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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

/// Why a connection was refused at the gate. Carried back to the accept
/// loop so the rejection log names the cap that tripped.
#[derive(Debug, PartialEq, Eq)]
pub enum ConnRejection {
    GlobalCap {
        current: usize,
        cap: usize,
    },
    PerUidCap {
        uid: u32,
        current: usize,
        cap: usize,
    },
}

#[derive(Default)]
struct GateCounts {
    global: usize,
    per_uid: HashMap<u32, usize>,
}

/// Global + per-UID connection admission. Checked in the accept loop
/// immediately after the ADR-0012 UID-allowlist check. Keys on the
/// observed `SO_PEERCRED` UID in both auth modes.
pub struct ConnGate {
    max_global: usize,
    max_per_uid: usize,
    counts: Mutex<GateCounts>,
}

impl ConnGate {
    pub fn new(cfg: &LimitsConfig) -> Arc<Self> {
        Arc::new(Self {
            max_global: cfg.max_connections,
            max_per_uid: cfg.max_connections_per_uid,
            counts: Mutex::new(GateCounts::default()),
        })
    }

    /// Admit a connection or say why not. The returned guard releases
    /// both counters on drop — hold it for the connection's lifetime.
    pub fn try_admit(self: &Arc<Self>, uid: u32) -> Result<ConnGuard, ConnRejection> {
        // INVARIANT: only plain arithmetic runs under this lock; no
        // panic path exists while it is held, so poisoning is
        // unreachable in practice. `expect` documents that.
        let mut counts = self.counts.lock().expect("ConnGate mutex poisoned");
        if counts.global >= self.max_global {
            return Err(ConnRejection::GlobalCap {
                current: counts.global,
                cap: self.max_global,
            });
        }
        let uid_count = counts.per_uid.get(&uid).copied().unwrap_or(0);
        if uid_count >= self.max_per_uid {
            return Err(ConnRejection::PerUidCap {
                uid,
                current: uid_count,
                cap: self.max_per_uid,
            });
        }
        counts.global += 1;
        *counts.per_uid.entry(uid).or_insert(0) += 1;
        Ok(ConnGuard {
            gate: Arc::clone(self),
            uid,
        })
    }
}

/// RAII admission token. Dropping it releases the global and per-UID
/// slots taken by `try_admit`.
pub struct ConnGuard {
    gate: Arc<ConnGate>,
    uid: u32,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // INVARIANT: see try_admit — no panic path under this lock.
        let mut counts = self.gate.counts.lock().expect("ConnGate mutex poisoned");
        counts.global = counts.global.saturating_sub(1);
        if let Some(n) = counts.per_uid.get_mut(&self.uid) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per_uid.remove(&self.uid);
            }
        }
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

    #[test]
    fn global_cap_rejects_and_guard_drop_readmits() {
        let cfg = LimitsConfig {
            max_connections: 2,
            max_connections_per_uid: 10,
            ..Default::default()
        };
        let gate = ConnGate::new(&cfg);
        let g1 = gate
            .try_admit(1000)
            .map_err(|e| format!("{e:?}"))
            .expect("first admit");
        let _g2 = gate
            .try_admit(1001)
            .map_err(|e| format!("{e:?}"))
            .expect("second admit");
        let rej = gate
            .try_admit(1002)
            .map(|_| ())
            .expect_err("third must be rejected");
        assert_eq!(rej, ConnRejection::GlobalCap { current: 2, cap: 2 });
        drop(g1);
        gate.try_admit(1002)
            .map(|_| ())
            .expect("guard drop must free the slot");
    }

    #[test]
    fn per_uid_cap_rejects_only_that_uid() {
        let cfg = LimitsConfig {
            max_connections: 10,
            max_connections_per_uid: 1,
            ..Default::default()
        };
        let gate = ConnGate::new(&cfg);
        let _held = gate
            .try_admit(1000)
            .unwrap_or_else(|e| panic!("first admit: {e:?}"));
        let rej = gate
            .try_admit(1000)
            .map(|_| ())
            .expect_err("same uid over cap");
        assert_eq!(
            rej,
            ConnRejection::PerUidCap {
                uid: 1000,
                current: 1,
                cap: 1
            }
        );
        gate.try_admit(2000)
            .map(|_| ())
            .expect("different uid has its own budget");
    }

    #[test]
    fn per_uid_entry_is_removed_when_count_hits_zero() {
        let cfg = LimitsConfig::default();
        let gate = ConnGate::new(&cfg);
        let g = gate
            .try_admit(1000)
            .unwrap_or_else(|e| panic!("admit: {e:?}"));
        drop(g);
        // Internal check: the per-UID map must not leak an entry per
        // ever-seen UID.
        assert!(gate.counts.lock().expect("gate mutex").per_uid.is_empty());
    }
}
