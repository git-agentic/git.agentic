//! Socket admission control — issue #118.
//!
//! Everything that bounds a previously unbounded resource on the daemon
//! socket lives here: the limits configuration (CLI-flag backed), the
//! connection gate (global + per-UID caps), and the per-UID token-bucket
//! rate limiter. The commit-queue slot bound lives on `DaemonState`
//! because it wraps `commit_lock`, which lives there.
//!
//! Enforcement points:
//! * accept loop (`main.rs`) — `ConnGate`, log-and-close (no frame yet,
//!   nothing to attribute a structured reply to).
//! * connection loop (`server.rs`) — `RateLimiter`, structured
//!   `Concurrency`-class retryable reply; write-idle deadline.
//! * dispatch write path (`server.rs`) — commit-queue slots.

use std::time::Duration;

/// Tunable limits, one field per CLI flag. Static per process — reload
/// means bouncing the daemon, same as the ADR-0013 scanner allowlist.
#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// Global cap on concurrently open connections.
    pub max_connections: usize,
    /// Per-UID cap on concurrently open connections.
    pub max_connections_per_uid: usize,
    /// Per-UID request budget, requests/second. Burst capacity is 2×.
    pub rate_per_uid: u32,
    /// Max requests queued-or-executing on `commit_lock`.
    pub commit_queue_depth: usize,
    /// Deadline for reading one complete inbound frame (per-frame clock).
    pub read_idle: Duration,
    /// Deadline for writing one complete response frame.
    pub write_idle: Duration,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_connections_per_uid: 16,
            rate_per_uid: 200,
            commit_queue_depth: 8,
            read_idle: Duration::from_secs(30),
            write_idle: Duration::from_secs(30),
        }
    }
}

impl LimitsConfig {
    /// Reject configurations that would deny all service. Zero anywhere
    /// is an operator mistake; refuse loudly at startup rather than run
    /// a daemon that drops every connection.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.max_connections >= 1, "--max-connections must be >= 1");
        anyhow::ensure!(
            self.max_connections_per_uid >= 1,
            "--max-connections-per-uid must be >= 1"
        );
        anyhow::ensure!(self.rate_per_uid >= 1, "--rate-per-uid must be >= 1");
        anyhow::ensure!(
            self.commit_queue_depth >= 1,
            "--commit-queue-depth must be >= 1"
        );
        anyhow::ensure!(!self.read_idle.is_zero(), "--read-idle-secs must be >= 1");
        anyhow::ensure!(!self.write_idle.is_zero(), "--write-idle-secs must be >= 1");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = LimitsConfig::default();
        assert_eq!(c.max_connections, 64);
        assert_eq!(c.max_connections_per_uid, 16);
        assert_eq!(c.rate_per_uid, 200);
        assert_eq!(c.commit_queue_depth, 8);
        assert_eq!(c.read_idle, Duration::from_secs(30));
        assert_eq!(c.write_idle, Duration::from_secs(30));
        c.validate().expect("defaults must validate");
    }

    #[test]
    fn zero_values_are_rejected() {
        for cfg in [
            LimitsConfig {
                max_connections: 0,
                ..Default::default()
            },
            LimitsConfig {
                max_connections_per_uid: 0,
                ..Default::default()
            },
            LimitsConfig {
                rate_per_uid: 0,
                ..Default::default()
            },
            LimitsConfig {
                commit_queue_depth: 0,
                ..Default::default()
            },
            LimitsConfig {
                read_idle: Duration::ZERO,
                ..Default::default()
            },
            LimitsConfig {
                write_idle: Duration::ZERO,
                ..Default::default()
            },
        ] {
            assert!(cfg.validate().is_err(), "must reject: {cfg:?}");
        }
    }
}
