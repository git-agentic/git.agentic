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

    /// Returns the UID that should be attested onto a Commit shaped by this
    /// connection. Under `InsecureAllowAny` the UID has no security meaning,
    /// so attestation is suppressed (`None`); otherwise the connection's UID
    /// is attested. Centralises the "insecure mode suppresses attestation"
    /// invariant so it cannot drift between call sites.
    pub fn attestation_for(&self, uid: u32) -> Option<u32> {
        match self {
            PeerAuthPolicy::Allowlist(_) => Some(uid),
            PeerAuthPolicy::InsecureAllowAny => None,
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

    #[test]
    fn attestation_for_returns_uid_under_allowlist_and_none_under_insecure() {
        let allowlist = PeerAuthPolicy::Allowlist([1000].into_iter().collect());
        assert_eq!(allowlist.attestation_for(1000), Some(1000));
        // method doesn't gate on allowlist membership — by the time it's
        // called, is_allowed() has already admitted the connection.
        assert_eq!(allowlist.attestation_for(42), Some(42));

        let insecure = PeerAuthPolicy::InsecureAllowAny;
        assert_eq!(insecure.attestation_for(1000), None);
        assert_eq!(insecure.attestation_for(0), None);
    }
}
