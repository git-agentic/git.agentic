//! Library surface for `agenticd`.
//!
//! The daemon ships as a binary (`src/main.rs`), but its internal modules
//! are also exposed here so integration tests under `tests/` can drive them
//! against a real Postgres without standing up the full daemon.
//!
//! Only modules that an integration test or another in-workspace consumer
//! needs are re-exported. The public surface here is **not** stable across
//! versions — treat it as the daemon's internal API.

pub mod commit;
pub mod lifecycle;
pub mod limits;
pub mod mcp;
pub mod membackend;
pub mod migrate;
pub mod objstore;
pub mod peer_auth;
pub mod rollback;
pub mod server;
pub(crate) mod store_async;
pub mod wire_error;
