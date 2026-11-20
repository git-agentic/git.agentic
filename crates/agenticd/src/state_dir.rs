//! Ownership checks for the daemon's state directory.
//!
//! In the ADR-0004 sidecar the worker shares a volume with the daemon and
//! is adversarial. Refs, objects and `schema/*.down.sql` (which a plain
//! rollback executes against Postgres) live under `<repo>/.agentic`, and
//! the daemon reopens those paths for the life of the process. A worker
//! that can write there, or that can write to any directory above it and
//! so rename the repo out of the way, can steer the daemon. The daemon
//! therefore refuses to start unless:
//!
//! - `<repo>` is owned by its euid and has no group or other write bit;
//! - no ancestor up to `/` can be modified by another principal (see
//!   [`ancestor_writable_by_others`]);
//! - `<repo>/.agentic` is a real directory owned by it, with no group or
//!   other write bit; it is then tightened to 0700.
//!
//! The worker may read `<repo>/prompts`, but must run as a different uid.

use std::path::{Path, PathBuf};

/// Create or tighten `<repo>/.agentic` and verify ownership, returning the
/// canonical repo path. The caller must use the returned path from here on,
/// so a symlink in the original path cannot be repointed after the check.
/// `trusted_root` (`--trusted-volume-root`) names an ancestor the operator
/// asserts only the daemon can write, exempting it (never the repo itself)
/// from the ancestor check. Unix only; elsewhere there is no uid model and
/// this returns `repo`.
#[cfg(unix)]
pub fn secure_state_dir(repo: &Path, trusted_root: Option<&Path>) -> anyhow::Result<PathBuf> {
    use anyhow::{bail, Context};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // SAFETY: geteuid and getegid have no preconditions and cannot fail.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(repo)
        .with_context(|| format!("creating repo directory {}", repo.display()))?;
    // Resolve symlinks first so every directory that can rename a path
    // component is on the chain checked below (macOS `/var` is a symlink).
    let repo = std::fs::canonicalize(repo)
        .with_context(|| format!("resolving repo directory {}", repo.display()))?;

    let trusted_root = trusted_root
        .map(|t| {
            std::fs::canonicalize(t)
                .with_context(|| format!("resolving --trusted-volume-root {}", t.display()))
        })
        .transpose()?;

    let repo_meta = std::fs::metadata(&repo).with_context(|| format!("stat {}", repo.display()))?;
    if repo_meta.uid() != euid {
        bail!(
            "repo directory {} is owned by uid {}, not the daemon's uid {euid}; refusing to start",
            repo.display(),
            repo_meta.uid()
        );
    }
    // The repo itself is strict: any group or other write bit lets another
    // principal plant or replace `.agentic`, sticky bit or not.
    if repo_meta.mode() & 0o022 != 0 {
        bail!(
            "repo directory {} is group- or world-writable (mode {:o}); refusing to start. \
             Run `chmod go-w` on it",
            repo.display(),
            repo_meta.mode() & 0o7777
        );
    }
    let chain: Vec<&Path> = repo.ancestors().collect();
    for pair in chain.windows(2) {
        let (child, dir) = (pair[0], pair[1]);
        if trusted_root.as_deref() == Some(dir) {
            continue;
        }
        let m = std::fs::metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
        let child_m =
            std::fs::metadata(child).with_context(|| format!("stat {}", child.display()))?;
        if let Some(why) = ancestor_writable_by_others(&m, &child_m, euid, egid) {
            bail!(
                "{} {why}; another uid could replace the daemon's state, refusing to start \
                 (if only agenticd mounts this directory, pass --trusted-volume-root)",
                dir.display()
            );
        }
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
            // Already group/other writable means someone else may have
            // planted content; tightening the mode would not undo that.
            if m.mode() & 0o022 != 0 {
                bail!(
                    "state directory {} is group- or world-writable (mode {:o}); \
                     its contents cannot be trusted, refusing to start",
                    dir.display(),
                    m.mode() & 0o7777
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
    Ok(repo)
}

/// Whether a principal other than the daemon (and root) could add, remove
/// or rename `child` through this strict ancestor, with a reason if so.
///
/// - Owned by neither the daemon nor root: the owner can chmod and write.
/// - Sticky: exempt from the group and other write checks, because a
///   sticky directory (a root-owned `/tmp`) only lets an entry's owner
///   rename or remove it. That holds only if `child` is owned by the daemon
///   or root, so no other uid created it.
/// - Otherwise other-writable is refused, and so is group-writable when the
///   directory's gid is not the daemon's egid. Group-write with the
///   process's own gid is allowed so a umask-002 checkout on a
///   user-private-group system keeps working; the worker must then not
///   share the daemon's primary group.
#[cfg(unix)]
fn ancestor_writable_by_others(
    m: &std::fs::Metadata,
    child: &std::fs::Metadata,
    euid: u32,
    egid: u32,
) -> Option<&'static str> {
    use std::os::unix::fs::MetadataExt;
    let mode = m.mode();
    if m.uid() != euid && m.uid() != 0 {
        Some("is owned by another uid")
    } else if mode & 0o1000 != 0 {
        (child.uid() != euid && child.uid() != 0)
            .then_some("is sticky but its child on the path to the repo is owned by another uid")
    } else if mode & 0o002 != 0 {
        Some("is world-writable without the sticky bit")
    } else if mode & 0o020 != 0 && m.gid() != egid {
        Some("is writable by a group other than the daemon's")
    } else {
        None
    }
}

#[cfg(not(unix))]
pub fn secure_state_dir(repo: &Path, _trusted_root: Option<&Path>) -> anyhow::Result<PathBuf> {
    Ok(repo.to_path_buf())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn set_mode(p: &Path, mode: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// A supplementary group of this process other than its egid, if any.
    fn foreign_gid() -> Option<u32> {
        // SAFETY: getegid/getgroups with a sized buffer are sound.
        let egid = unsafe { libc::getegid() };
        let mut groups = [0 as libc::gid_t; 256];
        let n = unsafe { libc::getgroups(256, groups.as_mut_ptr()) };
        groups[..n.max(0) as usize]
            .iter()
            .copied()
            .find(|g| *g != egid)
    }

    /// `<tmp>/outer/repo`, with `outer` set to `outer_mode` first.
    fn nested(outer_mode: u32) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let outer = t.path().join("outer");
        std::fs::create_dir(&outer).unwrap();
        set_mode(&outer, outer_mode);
        let repo = outer.join("repo");
        (t, outer, repo)
    }

    #[test]
    fn creates_state_dir_0700_and_returns_canonical_path() {
        let t = tempfile::tempdir().unwrap();
        let repo = secure_state_dir(t.path(), None).unwrap();
        assert_eq!(repo, std::fs::canonicalize(t.path()).unwrap());
        assert_eq!(mode(&repo.join(".agentic")), 0o700);
    }

    #[test]
    fn tightens_existing_0755_state_dir() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join(".agentic");
        std::fs::create_dir(&d).unwrap();
        set_mode(&d, 0o755);
        secure_state_dir(t.path(), None).unwrap();
        assert_eq!(mode(&d), 0o700);
    }

    #[test]
    fn refuses_group_or_world_writable_state_dir() {
        for m in [0o775, 0o777] {
            let t = tempfile::tempdir().unwrap();
            let d = t.path().join(".agentic");
            std::fs::create_dir(&d).unwrap();
            set_mode(&d, m);
            let err = secure_state_dir(t.path(), None).unwrap_err().to_string();
            assert!(
                err.contains("writable") && err.contains("agentic"),
                "{m:o}: {err}"
            );
            assert_eq!(mode(&d), m, "a refused dir must not be modified");
        }
    }

    #[test]
    fn refuses_world_writable_repo() {
        let t = tempfile::tempdir().unwrap();
        set_mode(t.path(), 0o777);
        let err = secure_state_dir(t.path(), None).unwrap_err().to_string();
        assert!(err.contains("world-writable"), "{err}");
        assert!(!t.path().join(".agentic").exists());
    }

    #[test]
    fn refuses_sticky_world_writable_repo() {
        let t = tempfile::tempdir().unwrap();
        set_mode(t.path(), 0o1777);
        let err = secure_state_dir(t.path(), None).unwrap_err().to_string();
        assert!(err.contains("writable"), "{err}");
    }

    #[test]
    fn refuses_group_writable_repo_with_own_gid() {
        let t = tempfile::tempdir().unwrap();
        set_mode(t.path(), 0o775);
        // SAFETY: getegid cannot fail.
        std::os::unix::fs::chown(t.path(), None, Some(unsafe { libc::getegid() })).unwrap();
        let err = secure_state_dir(t.path(), None).unwrap_err().to_string();
        assert!(err.contains("writable"), "{err}");
    }

    #[test]
    fn allows_sticky_ancestor_with_foreign_gid() {
        // A root-owned 1777 /tmp has gid 0, not the daemon's egid.
        let (_t, outer, repo) = nested(0o1777);
        if let Some(other) = foreign_gid() {
            let _ = std::os::unix::fs::chown(&outer, None, Some(other));
        }
        secure_state_dir(&repo, None).unwrap();
    }

    #[test]
    fn trusted_root_exempts_only_that_ancestor() {
        let (_t, outer, repo) = nested(0o777);
        assert!(secure_state_dir(&repo, None).is_err());
        secure_state_dir(&repo, Some(&outer)).unwrap();
        // Never exempts the repo itself.
        set_mode(&repo, 0o777);
        let err = secure_state_dir(&repo, Some(&repo))
            .unwrap_err()
            .to_string();
        assert!(err.contains("repo directory"), "{err}");
    }

    #[test]
    fn refuses_world_writable_ancestor_without_sticky() {
        let (_t, outer, repo) = nested(0o777);
        let err = secure_state_dir(&repo, None).unwrap_err().to_string();
        let outer = std::fs::canonicalize(&outer).unwrap();
        assert!(err.contains(&*outer.to_string_lossy()), "{err}");
    }

    #[test]
    fn allows_world_writable_ancestor_with_sticky() {
        let (_t, _outer, repo) = nested(0o1777);
        secure_state_dir(&repo, None).unwrap();
    }

    #[test]
    fn allows_group_writable_ancestor_with_own_gid() {
        let (_t, outer, repo) = nested(0o775);
        // SAFETY: getegid cannot fail.
        let egid = unsafe { libc::getegid() };
        std::os::unix::fs::chown(&outer, None, Some(egid)).unwrap();
        secure_state_dir(&repo, None).unwrap();
    }

    #[test]
    fn refuses_group_writable_ancestor_with_foreign_gid() {
        let Some(other) = foreign_gid() else {
            return; // no supplementary group to borrow on this machine
        };
        let (_t, outer, repo) = nested(0o775);
        if std::os::unix::fs::chown(&outer, None, Some(other)).is_err() {
            return;
        }
        let err = secure_state_dir(&repo, None).unwrap_err().to_string();
        assert!(err.contains("group other than"), "{err}");
    }

    #[test]
    fn refuses_symlinked_state_dir() {
        let t = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), t.path().join(".agentic")).unwrap();
        let err = secure_state_dir(t.path(), None).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }
}
