//! Postgres + pgvector adapter — the MVP's only first-class memory backend.
//!
//! Phase 1 (week 3): bulk segment build from a snapshot of current state.
//! Phase 2 (week 4): logical-decoding streamer for real-time segment writes.
//! Phase 3 (week 5): atomic snapshot via advisory lock + COW of the active head.
//!
//! This file currently sketches the surface area. Each method returns
//! `unimplemented!()` until its scheduled week.

use crate::adapter::{MemoryAdapter, SnapshotHandle};
use crate::segment::SegmentManifest;
use crate::Result;

use sqlx::PgPool;

pub struct PostgresAdapter {
    pool: PgPool,
    tables: Vec<String>,
}

impl PostgresAdapter {
    pub async fn connect(url: &str, tables: Vec<String>) -> Result<Self> {
        let pool = PgPool::connect(url).await?;
        Ok(Self { pool, tables })
    }
}

#[async_trait::async_trait]
impl MemoryAdapter for PostgresAdapter {
    async fn init(&mut self) -> Result<()> {
        // Week 3–4: validate pgvector, create replication slot, install
        // helper functions, begin streaming.
        unimplemented!("PostgresAdapter::init lands in week 3")
    }

    async fn snapshot(&self) -> Result<SnapshotHandle> {
        unimplemented!("PostgresAdapter::snapshot lands in week 5")
    }

    async fn restore(&self, _target: &SnapshotHandle) -> Result<()> {
        unimplemented!("PostgresAdapter::restore lands in week 8")
    }

    async fn current_schema_version(&self) -> Result<String> {
        // Read from a helper function we install during init.
        let row: (String,) = sqlx::query_as("SELECT agentic_schema_version()")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }
}

// Reference the unused field so clippy doesn't complain pre-implementation.
impl PostgresAdapter {
    #[allow(dead_code)]
    fn tracked_tables(&self) -> &[String] {
        &self.tables
    }

    #[allow(dead_code)]
    fn placeholder_manifest(&self) -> SegmentManifest {
        SegmentManifest { entries: vec![] }
    }
}
