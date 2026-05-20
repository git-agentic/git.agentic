//! Reverse SQL migration runner (ADR-0002 §5).
//!
//! Runs `.down.sql` files from `<agentic_dir>/schema/` to bring the live
//! database schema from its current version down to a target commit's
//! `schema_version`, as part of `agentic rollback`.
//!
//! ## Migration file convention
//!
//! Files live at `<agentic_dir>/schema/NNN_<name>.down.sql` (and
//! `NNN_<name>.up.sql` for the forward direction, not used here). The name
//! recorded in `agentic_migrations` is the full stem without the `.down.sql`
//! suffix — e.g. `"003_add_embeddings"`. NNN is a zero-padded decimal prefix
//! used only for human readability; ordering is determined by the `id` column
//! in `agentic_migrations`, which records insertion order.
//!
//! ## Irreversible marker
//!
//! A `.down.sql` whose first non-empty line is `-- IRREVERSIBLE` causes
//! rollback to fail loudly. The operator must either write a real reverse
//! migration or accept the ADR-0002 Decision 5 bounded-rollback path (restore
//! from the snapshot taken before the migration). The `--accept-data-loss`
//! flag is reserved for that second path; it is not yet implemented.

use std::path::{Path, PathBuf};

use agentic_memory::postgres::PostgresAdapter;
use anyhow::{anyhow, Context};

/// One step in a reverse-migration plan.
#[derive(Debug)]
pub struct MigrationStep {
    /// Migration stem name as recorded in `agentic_migrations`
    /// (e.g. `"003_add_embeddings"`).
    pub name: String,
    /// Absolute path to the `.down.sql` file (retained for diagnostics).
    #[allow(dead_code)]
    pub path: PathBuf,
    /// SQL content, pre-read so `run_reverse` doesn't need filesystem access.
    pub sql: String,
}

/// Load and validate the `.down.sql` files for the given migration names.
///
/// `names` must already be in execution order (most-recent first), as returned
/// by `PostgresAdapter::migrations_after`. This function is **synchronous** so
/// callers can release the `MutexGuard<PostgresAdapter>` before doing file I/O.
///
/// Returns an empty vec if `names` is empty. Errors if any file is missing,
/// unreadable, or marked `-- IRREVERSIBLE`, or if a name contains path
/// separators that would escape `<agentic_dir>/schema/`.
pub fn load_steps(agentic_dir: &Path, names: &[String]) -> anyhow::Result<Vec<MigrationStep>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }

    let schema_dir = agentic_dir.join("schema");
    if !schema_dir.exists() {
        return Err(anyhow!(
            "schema directory {} does not exist; cannot run reverse migrations \
             (create it and add the required .down.sql files)",
            schema_dir.display()
        ));
    }

    let mut steps = Vec::with_capacity(names.len());
    for name in names {
        validate_migration_name(name)?;
        let path = schema_dir.join(format!("{name}.down.sql"));
        if !path.exists() {
            return Err(anyhow!(
                "reverse migration file {} is missing; \
                 cannot roll back without it. \
                 Add the file or mark it -- IRREVERSIBLE if the migration cannot be reversed.",
                path.display()
            ));
        }
        let sql = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        check_irreversible(name, &sql)?;
        steps.push(MigrationStep {
            name: name.clone(),
            path,
            sql,
        });
    }

    Ok(steps)
}

/// Execute `steps` against the live database, in order.
///
/// Each step runs its SQL and removes the migration record from
/// `agentic_migrations` in a single transaction (see
/// `PostgresAdapter::apply_down_migration`). On any error the function
/// returns immediately; steps already completed are not rolled back.
///
/// The caller must hold a lock on the `PostgresAdapter` for the duration of
/// this call. Do not hold the lock while calling `load_steps` — that function
/// does blocking filesystem I/O and should run without the lock held.
pub async fn run_reverse(
    adapter: &PostgresAdapter,
    steps: Vec<MigrationStep>,
) -> anyhow::Result<()> {
    for step in steps {
        adapter
            .apply_down_migration(&step.name, &step.sql)
            .await
            .with_context(|| format!("applying reverse migration {:?}", step.name))?;
        tracing::info!(migration = %step.name, "reverse migration applied");
    }
    Ok(())
}

/// Reject migration names containing path separators or `..` components that
/// could escape the schema directory. Names come from `agentic_migrations`
/// which is controlled by the operator, but a compromised row should not be
/// able to cause arbitrary file reads.
fn validate_migration_name(name: &str) -> anyhow::Result<()> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(anyhow!(
            "migration name {name:?} contains path separators or '..' and would escape the \
             schema directory — refusing to proceed"
        ));
    }
    Ok(())
}

fn check_irreversible(name: &str, sql: &str) -> anyhow::Result<()> {
    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("-- IRREVERSIBLE") {
            return Err(anyhow!(
                "migration {:?} is marked -- IRREVERSIBLE and cannot be reversed automatically. \
                 Options:\n\
                 \x20 1. Write a real reverse migration and remove the IRREVERSIBLE marker.\n\
                 \x20 2. Use the bounded-rollback path (ADR-0002 Decision 5): restore from the \
                 snapshot taken before this migration was applied. \
                 That path is not yet implemented (v1.1 work item).",
                name
            ));
        }
        break; // only the first non-empty line matters
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_schema_dir(tmp: &TempDir) -> PathBuf {
        let dir = tmp.path().join("schema");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_down(dir: &Path, name: &str, sql: &str) {
        fs::write(dir.join(format!("{name}.down.sql")), sql).unwrap();
    }

    #[test]
    fn irreversible_passes_normal_sql() {
        assert!(check_irreversible("001_init", "DROP TABLE foo;").is_ok());
    }

    #[test]
    fn irreversible_passes_empty_leading_lines() {
        assert!(check_irreversible("001_init", "\n\n-- regular comment\nDROP TABLE foo;").is_ok());
    }

    #[test]
    fn irreversible_rejects_marker() {
        let err =
            check_irreversible("002_drop_data", "-- IRREVERSIBLE\n-- nothing to do").unwrap_err();
        assert!(err.to_string().contains("IRREVERSIBLE"), "{err}");
        assert!(err.to_string().contains("002_drop_data"), "{err}");
    }

    #[test]
    fn irreversible_rejects_marker_with_suffix() {
        let err =
            check_irreversible("003_x", "-- IRREVERSIBLE: column data was discarded").unwrap_err();
        assert!(err.to_string().contains("IRREVERSIBLE"), "{err}");
    }

    #[test]
    fn load_steps_errors_on_missing_file() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "001_init", "DROP TABLE foo;");
        // 002 is absent — load_steps should error with a clear message
        let err = load_steps(tmp.path(), &["001_init".into(), "002_missing".into()]).unwrap_err();
        assert!(err.to_string().contains("002_missing"), "{err}");
    }

    #[test]
    fn load_steps_returns_steps_in_given_order() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "002_b", "DROP TABLE b;");
        write_down(&dir, "001_a", "DROP TABLE a;");
        // names already in reverse order (most-recent first), as from migrations_after
        let steps = load_steps(tmp.path(), &["002_b".into(), "001_a".into()]).unwrap();
        assert_eq!(steps[0].name, "002_b");
        assert_eq!(steps[1].name, "001_a");
        assert_eq!(steps[1].sql, "DROP TABLE a;");
    }

    #[test]
    fn load_steps_catches_irreversible_in_file() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "003_bad", "-- IRREVERSIBLE\nDROP TABLE data;");
        let err = load_steps(tmp.path(), &["003_bad".into()]).unwrap_err();
        assert!(err.to_string().contains("003_bad"), "{err}");
        assert!(err.to_string().contains("IRREVERSIBLE"), "{err}");
    }

    #[test]
    fn validate_name_rejects_path_separators() {
        assert!(validate_migration_name("../../etc/passwd").is_err());
        assert!(validate_migration_name("sub/dir").is_err());
        assert!(validate_migration_name("001_init").is_ok());
        assert!(validate_migration_name("003_add-embeddings").is_ok());
    }
}
