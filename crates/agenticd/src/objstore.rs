//! Object-store factory for agenticd.
//!
//! Parses the `--object-store` flag into an [`ObjectStoreSpec`] and
//! opens it into a trait-object handle the daemon can stash in
//! [`crate::server::DaemonState`].
//!
//! Supported forms:
//! - `fs` — filesystem store rooted at `<repo>/.agentic/objects/`
//!   (the historical default; matches behaviour before ADR-0004).
//! - `fs:///abs/path` — filesystem store at an explicit absolute path
//!   (useful for shared-volume tests).
//! - `gcs://bucket[/prefix]` — GCS-backed store (ADR-0004 Decision 5),
//!   with a write-through local cache. Endpoint and bearer token come
//!   from `AGENTIC_GCS_ENDPOINT` and `AGENTIC_GCS_TOKEN` so the URL
//!   itself stays free of credentials.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentic_core::{FsObjectStore, GcsObjectStore, ObjectStore};
use anyhow::{anyhow, Context};

/// Env var carrying the GCS endpoint override. Used by integration
/// tests against `fake-gcs-server`; production sets nothing and falls
/// back to the public GCS host.
pub const ENDPOINT_ENV: &str = "AGENTIC_GCS_ENDPOINT";

/// Env var carrying a bearer token for the GCS JSON API. On Cloud Run
/// the sidecar pulls this from the GCE metadata server before exec —
/// the daemon itself never reaches for ADC in v1.0.
pub const TOKEN_ENV: &str = "AGENTIC_GCS_TOKEN";

/// Parsed `--object-store` URL. `open` constructs the live store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectStoreSpec {
    Fs {
        root: PathBuf,
    },
    Gcs {
        bucket: String,
        prefix: String,
        cache_dir: PathBuf,
    },
}

impl ObjectStoreSpec {
    /// Parse a `--object-store` argument.
    ///
    /// `agentic_dir` is `<repo>/.agentic/` — used to derive default
    /// paths (`objects/` for `fs`, `gcs-cache/` for `gcs`).
    pub fn parse(spec: &str, agentic_dir: &Path) -> anyhow::Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() || spec == "fs" {
            return Ok(Self::Fs {
                root: agentic_dir.join("objects"),
            });
        }
        if let Some(rest) = spec.strip_prefix("fs://") {
            if !rest.starts_with('/') {
                return Err(anyhow!(
                    "fs:// requires an absolute path, got {spec:?} (try `fs` for the default)"
                ));
            }
            return Ok(Self::Fs {
                root: PathBuf::from(rest),
            });
        }
        if let Some(rest) = spec.strip_prefix("gcs://") {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((b, p)) => (b, p),
                None => (rest, ""),
            };
            if bucket.is_empty() {
                return Err(anyhow!("gcs:// requires a bucket name, got {spec:?}"));
            }
            return Ok(Self::Gcs {
                bucket: bucket.to_string(),
                prefix: prefix.trim_matches('/').to_string(),
                cache_dir: agentic_dir.join("gcs-cache"),
            });
        }
        Err(anyhow!(
            "unrecognised --object-store {spec:?}; expected `fs`, `fs:///abs/path`, or `gcs://bucket[/prefix]`"
        ))
    }

    /// Open the parsed spec into a live trait object.
    pub fn open(self) -> anyhow::Result<Arc<dyn ObjectStore + Send + Sync>> {
        match self {
            Self::Fs { root } => {
                let store = FsObjectStore::open(&root)
                    .with_context(|| format!("opening fs object store at {}", root.display()))?;
                Ok(Arc::new(store))
            }
            Self::Gcs {
                bucket,
                prefix,
                cache_dir,
            } => {
                let endpoint = std::env::var(ENDPOINT_ENV).ok();
                let bearer = std::env::var(TOKEN_ENV).ok();
                let store =
                    GcsObjectStore::new(bucket.clone(), prefix, &cache_dir, endpoint, bearer)
                        .with_context(|| format!("opening gcs object store {bucket}"))?;
                Ok(Arc::new(store))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agentic() -> PathBuf {
        PathBuf::from("/tmp/repo/.agentic")
    }

    #[test]
    fn fs_default() {
        let spec = ObjectStoreSpec::parse("fs", &agentic()).unwrap();
        assert_eq!(
            spec,
            ObjectStoreSpec::Fs {
                root: PathBuf::from("/tmp/repo/.agentic/objects"),
            }
        );
    }

    #[test]
    fn fs_empty_string_is_default() {
        let spec = ObjectStoreSpec::parse("", &agentic()).unwrap();
        assert!(matches!(spec, ObjectStoreSpec::Fs { .. }));
    }

    #[test]
    fn fs_absolute_path() {
        let spec = ObjectStoreSpec::parse("fs:///var/objects", &agentic()).unwrap();
        assert_eq!(
            spec,
            ObjectStoreSpec::Fs {
                root: PathBuf::from("/var/objects"),
            }
        );
    }

    #[test]
    fn fs_relative_path_rejected() {
        let err = ObjectStoreSpec::parse("fs://relative/path", &agentic()).unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn gcs_bucket_only() {
        let spec = ObjectStoreSpec::parse("gcs://my-bucket", &agentic()).unwrap();
        assert_eq!(
            spec,
            ObjectStoreSpec::Gcs {
                bucket: "my-bucket".to_string(),
                prefix: String::new(),
                cache_dir: PathBuf::from("/tmp/repo/.agentic/gcs-cache"),
            }
        );
    }

    #[test]
    fn gcs_bucket_and_prefix() {
        let spec = ObjectStoreSpec::parse("gcs://my-bucket/repo-a/sub", &agentic()).unwrap();
        assert_eq!(
            spec,
            ObjectStoreSpec::Gcs {
                bucket: "my-bucket".to_string(),
                prefix: "repo-a/sub".to_string(),
                cache_dir: PathBuf::from("/tmp/repo/.agentic/gcs-cache"),
            }
        );
    }

    #[test]
    fn gcs_strips_trailing_slash_in_prefix() {
        let spec = ObjectStoreSpec::parse("gcs://b/p/", &agentic()).unwrap();
        match spec {
            ObjectStoreSpec::Gcs { prefix, .. } => assert_eq!(prefix, "p"),
            _ => panic!("expected Gcs"),
        }
    }

    #[test]
    fn gcs_requires_bucket() {
        let err = ObjectStoreSpec::parse("gcs://", &agentic()).unwrap_err();
        assert!(err.to_string().contains("bucket"));
    }

    #[test]
    fn unknown_scheme_rejected() {
        let err = ObjectStoreSpec::parse("s3://bucket", &agentic()).unwrap_err();
        assert!(err.to_string().contains("unrecognised"));
    }
}
