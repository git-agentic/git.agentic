//! Destructive-rollback approval tokens (ADR-0014).
//!
//! A `Rollback { accept_data_loss: true }` request bypasses the
//! `IRREVERSIBLE`-migration check, so it is the one knob between a worker
//! and arbitrary data loss in the per-tenant database. This module is the
//! self-contained, stateless verification primitive that gates it: an
//! out-of-band operator holds an HMAC key and issues short-lived tokens
//! scoped to one `(commit_hash, peer_uid)`; the daemon verifies without
//! contacting the operator.
//!
//! Token wire format (Decision 2): `"<unix_ts_seconds>:<hex_hmac>"`.
//! The HMAC-SHA256 is computed over the canonical byte form of:
//!
//! ```text
//! "git.agentic/rollback-approval/v1" ":" commit_hash_hex ":"
//!   requesting_peer_uid_decimal ":" timestamp_unix_seconds_decimal
//! ```
//!
//! The `v1` domain-separation prefix guarantees a token minted for
//! rollback approval can never be reused for a different signed-message
//! format on the same key.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Symmetric-token time-to-live, in seconds (Decision 3). The window is
/// symmetric — a token is rejected if `|now - timestamp|` exceeds it — so
/// a slightly fast/slow daemon clock can't accept arbitrarily-future
/// tokens. It is the *only* replay defense; there is no persistent
/// anti-replay store.
pub const APPROVAL_TOKEN_TTL_SECONDS: u64 = 300;

/// HMAC domain-separation string. A future signed-message format using
/// the same key MUST use a different domain string (Decision 2).
const DOMAIN: &str = "git.agentic/rollback-approval/v1";

/// Required approval-key length in bytes (256-bit, Decision 6).
pub const APPROVAL_KEY_LEN: usize = 32;

type HmacSha256 = Hmac<Sha256>;

/// A 32-byte approval-signing key, zeroized on drop so it doesn't linger
/// in freed memory. Constructed by the operator's token issuer and by the
/// daemon at startup from `--approval-key-file`.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct ApprovalKey([u8; APPROVAL_KEY_LEN]);

impl ApprovalKey {
    /// Build a key from raw file bytes. Rejects any length other than
    /// exactly 32 (Decision 6) — the caller decides whether that aborts
    /// startup or fails closed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ApprovalKeyError> {
        if bytes.len() != APPROVAL_KEY_LEN {
            return Err(ApprovalKeyError::WrongLength {
                expected: APPROVAL_KEY_LEN,
                actual: bytes.len(),
            });
        }
        let mut key = [0u8; APPROVAL_KEY_LEN];
        key.copy_from_slice(bytes);
        Ok(Self(key))
    }
}

impl std::fmt::Debug for ApprovalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render key material, even in logs / panic messages.
        f.write_str("ApprovalKey(<redacted>)")
    }
}

/// Why an approval key could not be constructed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApprovalKeyError {
    #[error("approval key must be exactly {expected} bytes, got {actual}")]
    WrongLength { expected: usize, actual: usize },
}

/// Why a supplied token was rejected. Each variant maps 1:1 to a
/// `RollbackForcedDataLoss` audit `decision` string (Decision 5), so the
/// daemon can emit a precise audit event on every rejection branch.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApprovalRejection {
    #[error("approval token is malformed (expected \"<unix_ts>:<hex_hmac>\")")]
    Malformed,
    #[error("approval token expired or clock-skewed beyond the {ttl}s window")]
    Expired { ttl: u64 },
    #[error("approval token signature does not verify")]
    InvalidSignature,
}

impl ApprovalRejection {
    /// The audit-event `decision` token for this rejection.
    pub fn audit_decision(&self) -> &'static str {
        match self {
            ApprovalRejection::Malformed => "rejected_malformed",
            ApprovalRejection::Expired { .. } => "rejected_expired",
            ApprovalRejection::InvalidSignature => "rejected_invalid_signature",
        }
    }
}

/// The canonical signed payload for a `(commit_hash, peer_uid, timestamp)`
/// tuple. `commit_hash` is normalized to lowercase hex by the caller.
fn signing_input(commit_hash: &str, peer_uid: u32, timestamp: u64) -> Vec<u8> {
    format!("{DOMAIN}:{commit_hash}:{peer_uid}:{timestamp}").into_bytes()
}

/// Compute the raw HMAC-SHA256 tag for a tuple.
fn tag(key: &ApprovalKey, commit_hash: &str, peer_uid: u32, timestamp: u64) -> Vec<u8> {
    // INVARIANT: HMAC accepts a key of any length; `new_from_slice` only
    // errors for algorithms with a fixed key size, which HMAC is not — so
    // this never returns Err. The `expect` documents that.
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(&signing_input(commit_hash, peer_uid, timestamp));
    mac.finalize().into_bytes().to_vec()
}

/// Issue a token binding `commit_hash` + `peer_uid` at `now` (Decision 2).
/// Used by the operator's CLI issuer; the daemon only ever verifies.
///
/// `commit_hash` is normalized to lowercase so a token issued for an
/// upper/mixed-case hash still verifies against the daemon's normalized
/// target hash.
pub fn generate_token(key: &ApprovalKey, commit_hash: &str, peer_uid: u32, now: u64) -> String {
    let commit_hash = commit_hash.to_ascii_lowercase();
    let mac = tag(key, &commit_hash, peer_uid, now);
    format!("{now}:{}", hex::encode(mac))
}

/// Verify a token against the expected `(commit_hash, peer_uid)` at time
/// `now`. The comparison is constant-time: the supplied hex tag is decoded
/// and checked with `Mac::verify_slice`, never a short-circuiting string
/// compare of the hex (Decision 2).
///
/// `commit_hash` is normalized to lowercase before verification, matching
/// `generate_token`.
pub fn verify_token(
    key: &ApprovalKey,
    commit_hash: &str,
    peer_uid: u32,
    token: &str,
    now: u64,
) -> Result<(), ApprovalRejection> {
    let (ts_str, hex_tag) = token.split_once(':').ok_or(ApprovalRejection::Malformed)?;
    let timestamp: u64 = ts_str.parse().map_err(|_| ApprovalRejection::Malformed)?;
    let provided_tag = hex::decode(hex_tag).map_err(|_| ApprovalRejection::Malformed)?;
    // An HMAC-SHA256 tag is exactly 32 bytes; anything else (empty, short,
    // long) is structurally malformed, not a mis-signed token.
    if provided_tag.len() != 32 {
        return Err(ApprovalRejection::Malformed);
    }

    // Time-bound check before the cryptographic one. Symmetric window so a
    // skewed-future token is also rejected. Saturating so neither side
    // underflows near the epoch.
    let delta = now
        .saturating_sub(timestamp)
        .max(timestamp.saturating_sub(now));
    if delta > APPROVAL_TOKEN_TTL_SECONDS {
        return Err(ApprovalRejection::Expired {
            ttl: APPROVAL_TOKEN_TTL_SECONDS,
        });
    }

    let commit_hash = commit_hash.to_ascii_lowercase();
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(&signing_input(&commit_hash, peer_uid, timestamp));
    // Constant-time comparison of the decoded tag bytes.
    mac.verify_slice(&provided_tag)
        .map_err(|_| ApprovalRejection::InvalidSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ApprovalKey {
        ApprovalKey::from_bytes(&[7u8; APPROVAL_KEY_LEN]).unwrap()
    }

    #[test]
    fn key_rejects_wrong_length() {
        assert_eq!(
            ApprovalKey::from_bytes(&[0u8; 31]).unwrap_err(),
            ApprovalKeyError::WrongLength {
                expected: 32,
                actual: 31
            }
        );
        assert_eq!(
            ApprovalKey::from_bytes(&[0u8; 33]).unwrap_err(),
            ApprovalKeyError::WrongLength {
                expected: 32,
                actual: 33
            }
        );
        assert!(ApprovalKey::from_bytes(&[0u8; 32]).is_ok());
    }

    #[test]
    fn key_debug_is_redacted() {
        assert_eq!(format!("{:?}", key()), "ApprovalKey(<redacted>)");
    }

    #[test]
    fn generated_token_verifies() {
        let hash = "a".repeat(64);
        let tok = generate_token(&key(), &hash, 1000, 5_000);
        assert!(verify_token(&key(), &hash, 1000, &tok, 5_000).is_ok());
        // Within the window on either side.
        assert!(verify_token(&key(), &hash, 1000, &tok, 5_000 + 299).is_ok());
        assert!(verify_token(&key(), &hash, 1000, &tok, 5_000 - 299).is_ok());
    }

    #[test]
    fn commit_hash_case_is_normalized() {
        let tok = generate_token(&key(), &"AB".repeat(32), 1, 100);
        // Verifies against the lowercase form the daemon normalizes to.
        assert!(verify_token(&key(), &"ab".repeat(32), 1, &tok, 100).is_ok());
    }

    #[test]
    fn expired_token_rejected_both_directions() {
        let hash = "a".repeat(64);
        let tok = generate_token(&key(), &hash, 1, 10_000);
        assert_eq!(
            verify_token(&key(), &hash, 1, &tok, 10_000 + 301),
            Err(ApprovalRejection::Expired { ttl: 300 })
        );
        assert_eq!(
            verify_token(&key(), &hash, 1, &tok, 10_000 - 301),
            Err(ApprovalRejection::Expired { ttl: 300 })
        );
    }

    #[test]
    fn wrong_target_rejected() {
        let tok = generate_token(&key(), &"a".repeat(64), 1, 100);
        // Same key + uid + time, different commit hash → no verify.
        assert_eq!(
            verify_token(&key(), &"b".repeat(64), 1, &tok, 100),
            Err(ApprovalRejection::InvalidSignature)
        );
    }

    #[test]
    fn wrong_uid_rejected() {
        let hash = "a".repeat(64);
        let tok = generate_token(&key(), &hash, 1, 100);
        assert_eq!(
            verify_token(&key(), &hash, 2, &tok, 100),
            Err(ApprovalRejection::InvalidSignature)
        );
    }

    #[test]
    fn wrong_key_rejected() {
        let hash = "a".repeat(64);
        let tok = generate_token(&key(), &hash, 1, 100);
        let other = ApprovalKey::from_bytes(&[9u8; 32]).unwrap();
        assert_eq!(
            verify_token(&other, &hash, 1, &tok, 100),
            Err(ApprovalRejection::InvalidSignature)
        );
    }

    #[test]
    fn malformed_tokens_rejected() {
        let hash = "a".repeat(64);
        for bad in [
            "no-colon",
            "notanumber:deadbeef",
            "100:nothex!!",
            "100:",
            ":deadbeef",
        ] {
            assert_eq!(
                verify_token(&key(), &hash, 1, bad, 100),
                Err(ApprovalRejection::Malformed),
                "token {bad:?} should be Malformed"
            );
        }
    }

    #[test]
    fn domain_prefix_binds_the_token() {
        // A tag computed without the domain prefix must not verify — guards
        // against cross-protocol reuse of the same key.
        let hash = "a".repeat(64);
        let mut mac = HmacSha256::new_from_slice(&[7u8; 32]).unwrap();
        mac.update(format!("{hash}:1:100").as_bytes()); // no DOMAIN
        let forged = format!("100:{}", hex::encode(mac.finalize().into_bytes()));
        assert_eq!(
            verify_token(&key(), &hash, 1, &forged, 100),
            Err(ApprovalRejection::InvalidSignature)
        );
    }

    #[test]
    fn audit_decisions_are_distinct() {
        assert_eq!(
            ApprovalRejection::Malformed.audit_decision(),
            "rejected_malformed"
        );
        assert_eq!(
            ApprovalRejection::Expired { ttl: 300 }.audit_decision(),
            "rejected_expired"
        );
        assert_eq!(
            ApprovalRejection::InvalidSignature.audit_decision(),
            "rejected_invalid_signature"
        );
    }
}
