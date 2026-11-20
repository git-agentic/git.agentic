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
//!
//! ADR-0019 adds a second scope on the same token text format. A
//! [`ApprovalScope::DataLoss`] token signs the input above, unchanged, so
//! tokens from older issuers keep verifying for `accept_data_loss`. A
//! [`ApprovalScope::ReverseMigration`] token signs the same fields under a
//! distinct domain string, so it can never authorize `accept_data_loss`.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Symmetric-token time-to-live, in seconds (Decision 3). The window is
/// symmetric — a token is rejected if `|now - timestamp|` exceeds it — so
/// a slightly fast/slow daemon clock can't accept arbitrarily-future
/// tokens. It bounds how long a token is accepted; the daemon adds a
/// per-process single-use set on top (see `VerifiedToken`).
pub const APPROVAL_TOKEN_TTL_SECONDS: u64 = 300;

/// HMAC domain-separation string. A future signed-message format using
/// the same key MUST use a different domain string (Decision 2).
const DOMAIN: &str = "git.agentic/rollback-approval/v1";

/// Domain string of the reverse-migration scope (ADR-0019). It differs from
/// [`DOMAIN`] at the byte after `v1` (`/` against `:`), so no choice of
/// commit hash, uid or timestamp can make the two signed inputs equal.
const DOMAIN_REVERSE_MIGRATION: &str = "git.agentic/rollback-approval/v1/reverse-migration";

/// What an approval token authorizes (ADR-0019).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalScope {
    /// `accept_data_loss = true`: the pre-ADR-0019 signing input. Legacy
    /// tokens are this scope. It also covers a plain reverse-migration
    /// rollback, the weaker action.
    DataLoss,
    /// A rollback that runs reverse migrations without `accept_data_loss`.
    ReverseMigration,
}

impl ApprovalScope {
    /// Stable name for audit events.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalScope::DataLoss => "data_loss",
            ApprovalScope::ReverseMigration => "reverse_migration",
        }
    }
}

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
    #[error("approval token was already used for a destructive rollback")]
    Replayed,
}

impl ApprovalRejection {
    /// The audit-event `decision` token for this rejection.
    pub fn audit_decision(&self) -> &'static str {
        match self {
            ApprovalRejection::Malformed => "rejected_malformed",
            ApprovalRejection::Expired { .. } => "rejected_expired",
            ApprovalRejection::InvalidSignature => "rejected_invalid_signature",
            ApprovalRejection::Replayed => "rejected_replayed",
        }
    }
}

/// The canonical signed payload for a `(commit_hash, peer_uid, timestamp)`
/// tuple. `commit_hash` is normalized to lowercase hex by the caller.
fn signing_input(
    scope: ApprovalScope,
    commit_hash: &str,
    peer_uid: u32,
    timestamp: u64,
) -> Vec<u8> {
    let domain = match scope {
        ApprovalScope::DataLoss => DOMAIN,
        ApprovalScope::ReverseMigration => DOMAIN_REVERSE_MIGRATION,
    };
    format!("{domain}:{commit_hash}:{peer_uid}:{timestamp}").into_bytes()
}

/// Compute the raw HMAC-SHA256 tag for a tuple.
fn tag(
    key: &ApprovalKey,
    scope: ApprovalScope,
    commit_hash: &str,
    peer_uid: u32,
    timestamp: u64,
) -> Vec<u8> {
    // INVARIANT: HMAC accepts a key of any length; `new_from_slice` only
    // errors for algorithms with a fixed key size, which HMAC is not — so
    // this never returns Err. The `expect` documents that.
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(&signing_input(scope, commit_hash, peer_uid, timestamp));
    mac.finalize().into_bytes().to_vec()
}

/// Issue a token binding `commit_hash` + `peer_uid` at `now` (Decision 2).
/// Used by the operator's CLI issuer; the daemon only ever verifies.
///
/// `commit_hash` is normalized to lowercase so a token issued for an
/// upper/mixed-case hash still verifies against the daemon's normalized
/// target hash.
pub fn generate_token(key: &ApprovalKey, commit_hash: &str, peer_uid: u32, now: u64) -> String {
    generate_token_scoped(key, ApprovalScope::DataLoss, commit_hash, peer_uid, now)
}

/// [`generate_token`] for a given scope (ADR-0019). The token text format is
/// the same for every scope.
pub fn generate_token_scoped(
    key: &ApprovalKey,
    scope: ApprovalScope,
    commit_hash: &str,
    peer_uid: u32,
    now: u64,
) -> String {
    let commit_hash = commit_hash.to_ascii_lowercase();
    let mac = tag(key, scope, &commit_hash, peer_uid, now);
    format!("{now}:{}", hex::encode(mac))
}

/// A token that passed `verify_token`. `tag` is the decoded 32-byte HMAC,
/// the canonical identity of the token: the wire string is not canonical
/// (hex decoding is case-insensitive and the timestamp parse accepts a
/// leading `+` and leading zeros), so replay tracking must key on `tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    pub timestamp: u64,
    pub tag: [u8; 32],
    /// The scope whose signing input the tag verified under.
    pub scope: ApprovalScope,
}

impl VerifiedToken {
    /// Unix second after which the token fails the time window for every
    /// possible `now`. Symmetric window, so a future-dated token verifies
    /// until `timestamp + TTL`, not `issue time + TTL`.
    pub fn expires_at(&self) -> u64 {
        self.timestamp.saturating_add(APPROVAL_TOKEN_TTL_SECONDS)
    }
}

/// Verify a token against the expected `(commit_hash, peer_uid)` at time
/// `now`, for `accept_data_loss` ([`ApprovalScope::DataLoss`], which is what
/// legacy tokens are). The comparison is constant-time: the supplied hex tag
/// is decoded and checked with `Mac::verify_slice`, never a short-circuiting
/// string compare of the hex (Decision 2).
///
/// `commit_hash` is normalized to lowercase before verification, matching
/// `generate_token`.
pub fn verify_token(
    key: &ApprovalKey,
    commit_hash: &str,
    peer_uid: u32,
    token: &str,
    now: u64,
) -> Result<VerifiedToken, ApprovalRejection> {
    verify_token_any(
        key,
        &[ApprovalScope::DataLoss],
        commit_hash,
        peer_uid,
        token,
        now,
    )
}

/// [`verify_token`] accepting a token of any of `scopes` (ADR-0019). A plain
/// reverse-migration rollback passes `[ReverseMigration, DataLoss]`, because
/// a data-loss approval also covers the weaker action; `accept_data_loss`
/// passes `[DataLoss]` only. A token that fits none is `InvalidSignature`.
pub fn verify_token_any(
    key: &ApprovalKey,
    scopes: &[ApprovalScope],
    commit_hash: &str,
    peer_uid: u32,
    token: &str,
    now: u64,
) -> Result<VerifiedToken, ApprovalRejection> {
    let (ts_str, hex_tag) = token.split_once(':').ok_or(ApprovalRejection::Malformed)?;
    let timestamp: u64 = ts_str.parse().map_err(|_| ApprovalRejection::Malformed)?;
    let provided_tag = hex::decode(hex_tag).map_err(|_| ApprovalRejection::Malformed)?;
    // An HMAC-SHA256 tag is exactly 32 bytes; anything else (empty, short,
    // long) is structurally malformed, not a mis-signed token.
    let tag: [u8; 32] = provided_tag
        .as_slice()
        .try_into()
        .map_err(|_| ApprovalRejection::Malformed)?;

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
    for scope in scopes {
        let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
        mac.update(&signing_input(*scope, &commit_hash, peer_uid, timestamp));
        // Constant-time comparison of the decoded tag bytes.
        if mac.verify_slice(&tag).is_ok() {
            return Ok(VerifiedToken {
                timestamp,
                tag,
                scope: *scope,
            });
        }
    }
    Err(ApprovalRejection::InvalidSignature)
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
        assert_eq!(
            ApprovalRejection::Replayed.audit_decision(),
            "rejected_replayed"
        );
    }

    #[test]
    fn verified_token_exposes_canonical_tag_across_wire_variants() {
        let hash = "a".repeat(64);
        let tok = generate_token(&key(), &hash, 1, 100);
        let (ts, hex_tag) = tok.split_once(':').unwrap();
        let canonical = verify_token(&key(), &hash, 1, &tok, 100).unwrap();
        assert_eq!(canonical.timestamp, 100);
        assert_eq!(canonical.expires_at(), 100 + APPROVAL_TOKEN_TTL_SECONDS);
        // Uppercase hex, `+`-prefixed and zero-padded timestamps all verify
        // to the same tag.
        for variant in [
            format!("{ts}:{}", hex_tag.to_ascii_uppercase()),
            format!("+{ts}:{hex_tag}"),
            format!("000{ts}:{hex_tag}"),
        ] {
            assert_eq!(
                verify_token(&key(), &hash, 1, &variant, 100),
                Ok(canonical.clone()),
                "variant {variant:?}"
            );
        }
    }

    // ADR-0019: the scopes sign different inputs for the same tuple, and a
    // token of one scope does not verify as the other.
    #[test]
    fn scopes_sign_distinct_inputs() {
        let h = "a".repeat(64);
        assert_ne!(
            signing_input(ApprovalScope::DataLoss, &h, 1000, 5_000),
            signing_input(ApprovalScope::ReverseMigration, &h, 1000, 5_000)
        );
        let dl = generate_token_scoped(&key(), ApprovalScope::DataLoss, &h, 1000, 5_000);
        let rm = generate_token_scoped(&key(), ApprovalScope::ReverseMigration, &h, 1000, 5_000);
        assert_ne!(dl, rm);
    }

    #[test]
    fn legacy_signing_input_is_unchanged() {
        // Pinned byte for byte: tokens minted by older CLIs must keep verifying.
        assert_eq!(
            signing_input(ApprovalScope::DataLoss, "abc", 1000, 5),
            b"git.agentic/rollback-approval/v1:abc:1000:5".to_vec()
        );
        // And a token built the way the old code built it verifies as DataLoss.
        let mut mac = HmacSha256::new_from_slice(&[7u8; APPROVAL_KEY_LEN]).unwrap();
        mac.update(b"git.agentic/rollback-approval/v1:abc:1000:5");
        let legacy = format!("5:{}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_token(&key(), "abc", 1000, &legacy, 5).is_ok());
        assert_eq!(legacy, generate_token(&key(), "abc", 1000, 5));
    }

    #[test]
    fn a_reverse_migration_token_does_not_verify_for_data_loss() {
        let h = "a".repeat(64);
        let rm = generate_token_scoped(&key(), ApprovalScope::ReverseMigration, &h, 1000, 5_000);
        assert_eq!(
            verify_token(&key(), &h, 1000, &rm, 5_000),
            Err(ApprovalRejection::InvalidSignature)
        );
        let both = [ApprovalScope::ReverseMigration, ApprovalScope::DataLoss];
        assert!(verify_token_any(&key(), &both, &h, 1000, &rm, 5_000).is_ok());
        // A data-loss token also covers the weaker action.
        let dl = generate_token(&key(), &h, 1000, 5_000);
        assert!(verify_token_any(&key(), &both, &h, 1000, &dl, 5_000).is_ok());
    }

    #[test]
    fn verified_token_reports_the_scope_it_matched() {
        let h = "a".repeat(64);
        let both = [ApprovalScope::ReverseMigration, ApprovalScope::DataLoss];
        for scope in both {
            let t = generate_token_scoped(&key(), scope, &h, 1, 100);
            let v = verify_token_any(&key(), &both, &h, 1, &t, 100).unwrap();
            assert_eq!(v.scope, scope);
        }
        assert_eq!(ApprovalScope::DataLoss.as_str(), "data_loss");
        assert_eq!(
            ApprovalScope::ReverseMigration.as_str(),
            "reverse_migration"
        );
    }
}
