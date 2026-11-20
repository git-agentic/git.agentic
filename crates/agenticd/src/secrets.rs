//! Credential inputs: file-based alternatives to argv and env secrets.
//!
//! A Postgres URL on argv and a bearer token in the environment are both
//! readable by other processes sharing the container's process or volume
//! namespace. The `*_FILE` forms let a secret volume carry them instead.

use std::path::Path;

use anyhow::{anyhow, Context};

/// Env var naming a file that holds the GCS bearer token.
pub const GCS_TOKEN_FILE_ENV: &str = "AGENTIC_GCS_TOKEN_FILE";

/// Read a secret from `path`, trimming surrounding whitespace (secret
/// mounts and `echo` both leave a trailing newline). An empty result is
/// an error: silently running unauthenticated is worse than refusing.
pub fn read_secret_file(path: &Path, what: &str) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {what} file {}", path.display()))?;
    let value = raw.trim();
    anyhow::ensure!(!value.is_empty(), "{what} file {} is empty", path.display());
    Ok(value.to_string())
}

/// Resolve the GCS bearer token from the plain env value and the file
/// env value. Setting both is ambiguous and refused.
pub fn resolve_gcs_token(
    token: Option<String>,
    token_file: Option<String>,
) -> anyhow::Result<Option<String>> {
    match (token, token_file) {
        (Some(_), Some(_)) => Err(anyhow!(
            "AGENTIC_GCS_TOKEN and {GCS_TOKEN_FILE_ENV} are both set; set only one"
        )),
        (Some(t), None) => Ok(Some(t)),
        (None, Some(f)) => read_secret_file(Path::new(&f), GCS_TOKEN_FILE_ENV).map(Some),
        (None, None) => Ok(None),
    }
}

/// Resolve the Postgres URL from `--postgres` or `--postgres-url-file`
/// (clap already rejects both together).
pub fn resolve_postgres_url(
    url: Option<&str>,
    url_file: Option<&Path>,
) -> anyhow::Result<Option<String>> {
    match (url, url_file) {
        (Some(_), Some(_)) => Err(anyhow!(
            "--postgres and --postgres-url-file are mutually exclusive"
        )),
        (Some(u), None) => Ok(Some(u.to_string())),
        (None, Some(f)) => read_secret_file(f, "--postgres-url-file").map(Some),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret");
        std::fs::write(&p, content).unwrap();
        (dir, p)
    }

    #[test]
    fn trims_trailing_newline() {
        let (_d, p) = file_with("postgres://u:p@h/db\n");
        assert_eq!(read_secret_file(&p, "x").unwrap(), "postgres://u:p@h/db");
    }

    #[test]
    fn empty_file_is_an_error() {
        let (_d, p) = file_with(" \n");
        let err = read_secret_file(&p, "x").unwrap_err().to_string();
        assert!(err.contains("is empty"), "{err}");
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(read_secret_file(Path::new("/nonexistent/secret"), "x").is_err());
    }

    #[test]
    fn gcs_token_and_file_conflict() {
        let err = resolve_gcs_token(Some("a".into()), Some("/f".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("set only one"), "{err}");
    }

    #[test]
    fn gcs_token_sources() {
        assert_eq!(resolve_gcs_token(None, None).unwrap(), None);
        assert_eq!(
            resolve_gcs_token(Some("t".into()), None)
                .unwrap()
                .as_deref(),
            Some("t")
        );
        let (_d, p) = file_with("tok\n");
        assert_eq!(
            resolve_gcs_token(None, Some(p.to_string_lossy().into()))
                .unwrap()
                .as_deref(),
            Some("tok")
        );
    }

    #[test]
    fn postgres_url_sources() {
        assert_eq!(resolve_postgres_url(None, None).unwrap(), None);
        assert_eq!(
            resolve_postgres_url(Some("postgres://x"), None)
                .unwrap()
                .as_deref(),
            Some("postgres://x")
        );
        let (_d, p) = file_with("postgres://u:p@h/db\n");
        assert_eq!(
            resolve_postgres_url(None, Some(&p)).unwrap().as_deref(),
            Some("postgres://u:p@h/db")
        );
        assert!(resolve_postgres_url(Some("a"), Some(&p)).is_err());
    }
}
