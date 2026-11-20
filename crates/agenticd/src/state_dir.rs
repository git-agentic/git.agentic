//! Ownership checks for the daemon's state directory.
//!
//! In the ADR-0004 sidecar the worker shares a volume with the daemon and
//! is adversarial. Refs, objects and `schema/*.down.sql` (which a plain
//! rollback executes against Postgres) live under `<repo>/.agentic`, so a
//! worker that can write there can steer the daemon. The daemon therefore
//! requires `<repo>` and `<repo>/.agentic` to be its own, `.agentic` to be
//! mode 0700, and `<repo>` not to be group- or world-writable. The worker
//! can still read `<repo>/prompts`, but must run as a different uid.

use std::path::Path;

/// Create or tighten `<repo>/.agentic` and verify ownership. Unix only;
/// elsewhere there is no uid model to check and this is a no-op.
#[cfg(unix)]
pub fn secure_state_dir(repo: &Path) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(repo)
        .with_context(|| format!("creating repo directory {}", repo.display()))?;
    let repo_meta = std::fs::metadata(repo).with_context(|| format!("stat {}", repo.display()))?;
    if repo_meta.uid() != euid {
        bail!(
            "repo directory {} is owned by uid {}, not the daemon's uid {euid}; refusing to start",
            repo.display(),
            repo_meta.uid()
        );
    }
    if repo_meta.mode() & 0o022 != 0 {
        bail!(
            "repo directory {} is group- or world-writable (mode {:o}); refusing to start. \
             Run `chmod go-w` on it",
            repo.display(),
            repo_meta.mode() & 0o7777
        );
    }

    let dir = repo.join(".agentic");
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_symlink() => {
            bail!("{} is a symlink; refusing to start", dir.display())
        }
        Ok(m) if !m.is_dir() => bail!("{} is not a directory; refusing to start", dir.display()),
        Ok(m) => {
            if m.uid() != euid {
                bail!(
                    "state directory {} is owned by uid {}, not the daemon's uid {euid}; refusing to start",
                    dir.display(),
                    m.uid()
                );
            }
            if m.mode() & 0o7777 != 0o700 {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("setting mode 0700 on {}", dir.display()))?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("stat {}", dir.display())),
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn secure_state_dir(_repo: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn creates_state_dir_0700() {
        let t = tempfile::tempdir().unwrap();
        secure_state_dir(t.path()).unwrap();
        assert_eq!(mode(&t.path().join(".agentic")), 0o700);
    }

    #[test]
    fn tightens_existing_state_dir() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join(".agentic");
        std::fs::create_dir(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).unwrap();
        secure_state_dir(t.path()).unwrap();
        assert_eq!(mode(&d), 0o700);
    }

    #[test]
    fn refuses_group_writable_repo() {
        let t = tempfile::tempdir().unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
        let err = secure_state_dir(t.path()).unwrap_err().to_string();
        assert!(
            err.contains("writable") && err.contains(&*t.path().to_string_lossy()),
            "{err}"
        );
        assert!(!t.path().join(".agentic").exists());
    }

    #[test]
    fn refuses_symlinked_state_dir() {
        let t = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), t.path().join(".agentic")).unwrap();
        let err = secure_state_dir(t.path()).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }
}
