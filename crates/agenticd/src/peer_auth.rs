//! Peer-UID enforcement policy for the daemon's socket.
//!
//! Constructed once at startup from CLI flags (`--allowed-uid` /
//! `--insecure-allow-any-uid`), then held in `DaemonState` so the accept
//! loop can consult it on every incoming connection. Per ADR-0012.

use std::collections::BTreeSet;

/// What the accept loop does with peer UIDs on each connection.
#[derive(Clone, Debug)]
pub enum PeerAuthPolicy {
    /// Reject any connection whose UID is not in this set.
    Allowlist(BTreeSet<u32>),
    /// Accept every connection. Set by --insecure-allow-any-uid.
    InsecureAllowAny,
}

impl PeerAuthPolicy {
    pub fn is_allowed(&self, uid: u32) -> bool {
        match self {
            PeerAuthPolicy::Allowlist(set) => set.contains(&uid),
            PeerAuthPolicy::InsecureAllowAny => true,
        }
    }
}

#[cfg(test)]
mod peer_auth_tests {
    use super::*;

    #[test]
    fn allowlist_admits_listed_uids_only() {
        let policy = PeerAuthPolicy::Allowlist([1000, 65532].into_iter().collect());
        assert!(policy.is_allowed(1000));
        assert!(policy.is_allowed(65532));
        assert!(!policy.is_allowed(0));
        assert!(!policy.is_allowed(99));
    }

    #[test]
    fn insecure_mode_admits_everything() {
        let policy = PeerAuthPolicy::InsecureAllowAny;
        assert!(policy.is_allowed(0));
        assert!(policy.is_allowed(u32::MAX));
    }
}
