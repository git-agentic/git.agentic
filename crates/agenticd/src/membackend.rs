//! Memory-backend factory for agenticd.
//!
//! Mirrors [`crate::objstore::ObjectStoreSpec`]'s parse/open split
//! (audit §S6): `DaemonState::open` derives a spec from its CLI-provided
//! configuration and delegates construction here, so future backends
//! (Mem0 / Zep / Letta, v1.1) land as new variants instead of new
//! inline construction paths.

use std::sync::Arc;

use agentic_core::ObjectStore;
use agentic_memory::postgres::{PgConfig, PostgresAdapter, TrackedTable};
use agentic_memory::MemoryAdapter;
use anyhow::Context;

/// Which memory backend to attach.
#[derive(Debug)]
pub enum MemoryBackendSpec {
    /// No memory backend; commits skip the memory-snapshot dimension.
    None,
    /// Postgres + pgvector — the v1.0 backend.
    Postgres {
        url: String,
        tables: Vec<TrackedTable>,
    },
}

impl MemoryBackendSpec {
    /// Derive the spec from the daemon's CLI configuration. Preserves
    /// the pre-factory behavior exactly: `--postgres` present requires
    /// at least one `--tables` entry; absent means no backend.
    pub fn from_flags(
        postgres_url: Option<&str>,
        tables: Vec<TrackedTable>,
    ) -> anyhow::Result<Self> {
        match postgres_url {
            None => Ok(Self::None),
            Some(url) => {
                if tables.is_empty() {
                    return Err(anyhow::anyhow!(
                        "--postgres requires at least one --tables entry"
                    ));
                }
                Ok(Self::Postgres {
                    url: url.to_string(),
                    tables,
                })
            }
        }
    }

    /// Connect and initialise the backend, returning the daemon-facing
    /// `Arc<dyn MemoryAdapter>`. `init` runs on the concrete adapter
    /// before the unsize coercion, per the trait's contract.
    pub async fn open(
        self,
        store: Arc<dyn ObjectStore + Send + Sync>,
    ) -> anyhow::Result<Option<Arc<dyn MemoryAdapter>>> {
        match self {
            Self::None => Ok(None),
            Self::Postgres { url, tables } => {
                let cfg = PgConfig::new(&url, tables);
                let mut adapter = PostgresAdapter::connect(cfg, store)
                    .await
                    .context("connecting Postgres memory backend")?;
                adapter
                    .init()
                    .await
                    .context("initialising memory backend")?;
                tracing::info!(
                    logical_decoding = adapter.logical_decoding_available(),
                    "memory backend attached"
                );
                Ok(Some(Arc::new(adapter)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_flags_none_when_no_url() {
        assert!(matches!(
            MemoryBackendSpec::from_flags(None, Vec::new()).unwrap(),
            MemoryBackendSpec::None
        ));
    }

    #[test]
    fn from_flags_requires_tables_with_url() {
        let err = MemoryBackendSpec::from_flags(Some("postgres://x"), Vec::new()).unwrap_err();
        assert!(
            err.to_string().contains("--tables"),
            "must keep the pre-factory error message; got: {err}"
        );
    }
}
