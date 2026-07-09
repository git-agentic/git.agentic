//! Reverse SQL migration planner (ADR-0002 §5).
//!
//! Loads and validates `.down.sql` files from `<agentic_dir>/schema/` so
//! the live database schema can be brought from its current version down
//! to a target commit's `schema_version`, as part of `agentic rollback`.
//! Execution happens in `MemoryAdapter::apply_reverse_migrations`; this
//! module only *plans* (loads and validates files).
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
//! rollback to fail loudly by default. The operator can override this with
//! `agentic rollback --accept-data-loss <ref>` to run the down.sql anyway,
//! accepting that the migration's original forward operation was destructive
//! and the reverse may not restore lost data.
//!
//! The ADR-0002 Decision 5 bounded-rollback path (restore from a snapshot
//! taken before the migration was applied) is a separate v1.1 work item;
//! `--accept-data-loss` does NOT trigger that path in v1.0.

use std::path::Path;

use agentic_memory::MigrationStep;
use anyhow::{anyhow, Context};

/// Load and validate the `.down.sql` files for the given migration names.
///
/// `names` must already be in execution order (most-recent first), as returned
/// by `PostgresAdapter::migrations_after`. This function is **synchronous** so
/// callers run this between adapter calls — it does blocking filesystem I/O.
///
/// When `accept_data_loss` is `true`, files marked `-- IRREVERSIBLE` are loaded
/// anyway and their SQL will run as written when `apply_reverse_migrations` executes — the
/// operator has explicitly accepted that the reverse may not restore lost data.
/// When `false` (the default), an IRREVERSIBLE marker fails the load with a
/// clear message instructing the operator on their options.
///
/// Returns an empty vec if `names` is empty. Errors if any file is missing,
/// unreadable, or (when `accept_data_loss=false`) marked `-- IRREVERSIBLE`, or
/// if a name contains path separators that would escape `<agentic_dir>/schema/`.
pub fn load_steps(
    agentic_dir: &Path,
    names: &[String],
    accept_data_loss: bool,
) -> anyhow::Result<Vec<MigrationStep>> {
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
        check_irreversible(name, &sql, accept_data_loss)?;
        steps.push(MigrationStep {
            name: name.clone(),
            sql,
        });
    }

    Ok(steps)
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

fn check_irreversible(name: &str, sql: &str, accept_data_loss: bool) -> anyhow::Result<()> {
    if accept_data_loss {
        return Ok(());
    }
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
                 \x20 2. Re-run rollback with --accept-data-loss to execute the .down.sql as \
                 written, accepting that the reverse may not restore lost data.\n\
                 \x20 3. Use the bounded-rollback path (ADR-0002 Decision 5): restore from the \
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
    use std::path::PathBuf;
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
        assert!(check_irreversible("001_init", "DROP TABLE foo;", false).is_ok());
    }

    #[test]
    fn irreversible_passes_empty_leading_lines() {
        assert!(
            check_irreversible("001_init", "\n\n-- regular comment\nDROP TABLE foo;", false)
                .is_ok()
        );
    }

    #[test]
    fn irreversible_rejects_marker() {
        let err = check_irreversible("002_drop_data", "-- IRREVERSIBLE\n-- nothing to do", false)
            .unwrap_err();
        assert!(err.to_string().contains("IRREVERSIBLE"), "{err}");
        assert!(err.to_string().contains("002_drop_data"), "{err}");
    }

    #[test]
    fn irreversible_rejects_marker_with_suffix() {
        let err = check_irreversible("003_x", "-- IRREVERSIBLE: column data was discarded", false)
            .unwrap_err();
        assert!(err.to_string().contains("IRREVERSIBLE"), "{err}");
    }

    // AC3a — accept_data_loss=true bypasses the IRREVERSIBLE check
    #[test]
    fn irreversible_bypassed_when_accept_data_loss_true() {
        assert!(
            check_irreversible("002_drop_data", "-- IRREVERSIBLE\nDROP TABLE data;", true).is_ok()
        );
        assert!(check_irreversible(
            "003_x",
            "-- IRREVERSIBLE: column data was discarded\nUPDATE t SET c = NULL;",
            true
        )
        .is_ok());
        // Normal SQL also still passes with the flag (no behavior change for non-IRREVERSIBLE).
        assert!(check_irreversible("004_normal", "DROP TABLE foo;", true).is_ok());
    }

    #[test]
    fn load_steps_errors_on_missing_file() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "001_init", "DROP TABLE foo;");
        // 002 is absent — load_steps should error with a clear message
        let err = load_steps(
            tmp.path(),
            &["001_init".into(), "002_missing".into()],
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("002_missing"), "{err}");
    }

    #[test]
    fn load_steps_returns_steps_in_given_order() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "002_b", "DROP TABLE b;");
        write_down(&dir, "001_a", "DROP TABLE a;");
        // names already in reverse order (most-recent first), as from migrations_after
        let steps = load_steps(tmp.path(), &["002_b".into(), "001_a".into()], false).unwrap();
        assert_eq!(steps[0].name, "002_b");
        assert_eq!(steps[1].name, "001_a");
        assert_eq!(steps[1].sql, "DROP TABLE a;");
    }

    #[test]
    fn load_steps_catches_irreversible_in_file() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "003_bad", "-- IRREVERSIBLE\nDROP TABLE data;");
        let err = load_steps(tmp.path(), &["003_bad".into()], false).unwrap_err();
        assert!(err.to_string().contains("003_bad"), "{err}");
        assert!(err.to_string().contains("IRREVERSIBLE"), "{err}");
    }

    // AC3a (end-to-end through load_steps) — accept_data_loss=true loads
    // an IRREVERSIBLE-marked file with its SQL populated for execution.
    #[test]
    fn load_steps_loads_irreversible_when_accept_data_loss_true() {
        let tmp = TempDir::new().unwrap();
        let dir = make_schema_dir(&tmp);
        write_down(&dir, "003_bad", "-- IRREVERSIBLE\nDROP TABLE data;");
        let steps = load_steps(tmp.path(), &["003_bad".into()], true).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "003_bad");
        assert!(
            steps[0].sql.contains("DROP TABLE data;"),
            "sql was: {:?}",
            steps[0].sql
        );
    }

    #[test]
    fn validate_name_rejects_path_separators() {
        assert!(validate_migration_name("../../etc/passwd").is_err());
        assert!(validate_migration_name("sub/dir").is_err());
        assert!(validate_migration_name("001_init").is_ok());
        assert!(validate_migration_name("003_add-embeddings").is_ok());
    }
}
