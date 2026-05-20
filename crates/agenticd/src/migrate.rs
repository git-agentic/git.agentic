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

/// Compute and load the reverse-migration steps needed to move from the live
/// schema version down to `target_version`.
///
/// Uses `adapter` to query `agentic_migrations` for the authoritative list of
/// applied migrations, then maps each to its `.down.sql` file under
/// `<agentic_dir>/schema/`. Returns steps in execution order (most-recent
/// migration first).
///
/// Returns an empty vec when no migration is needed (versions are equal).
/// Returns an error if any step is missing its `.down.sql` or is marked
/// `-- IRREVERSIBLE`.
pub async fn plan_reverse(
    adapter: &PostgresAdapter,
    agentic_dir: &Path,
    target_version: &str,
) -> anyhow::Result<Vec<MigrationStep>> {
    let names = adapter
        .migrations_after(target_version)
        .await
        .context("querying applied migrations")?;

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
    for name in &names {
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
pub async fn run_reverse(adapter: &PostgresAdapter, steps: &[MigrationStep]) -> anyhow::Result<()> {
    for step in steps {
        adapter
            .apply_down_migration(&step.name, &step.sql)
            .await
            .with_context(|| format!("applying reverse migration {:?}", step.name))?;
        tracing::info!(migration = %step.name, "reverse migration applied");
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
    fn missing_down_sql_path_is_detectable() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "001_init", "DROP TABLE foo;");
        // 002 is absent — path.exists() returns false
        let missing_path = dir.join("002_missing.down.sql");
        assert!(!missing_path.exists());
    }

    #[test]
    fn down_sql_irreversible_is_caught_after_read() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "003_bad", "-- IRREVERSIBLE\nDROP TABLE data;");
        let sql = fs::read_to_string(dir.join("003_bad.down.sql")).unwrap();
        let err = check_irreversible("003_bad", &sql).unwrap_err();
        assert!(err.to_string().contains("003_bad"), "{err}");
    }
}
