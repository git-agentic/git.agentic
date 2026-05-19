//! BLAKE3-based content addresses.
//!
//! A `Hash` is a 32-byte BLAKE3 digest used as the identity of every object
//! in the store. Hashes are stable across machines and platforms — two
//! callers serializing the same content produce the same hash.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A 32-byte BLAKE3 content address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Compute the hash of a byte slice.
    pub fn of(bytes: &[u8]) -> Self {
        let digest = blake3::hash(bytes);
        Self(*digest.as_bytes())
    }

    /// Return the raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hex-encoded representation (64 lowercase chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Short form: first 7 hex chars, à la `git log --oneline`.
    pub fn short(&self) -> String {
        let s = self.to_hex();
        s[..7].to_string()
    }

    /// Shard prefix and remainder, used for on-disk layout: `ab/12cd34...`
    pub fn shard(&self) -> (String, String) {
        let s = self.to_hex();
        (s[..2].to_string(), s[2..].to_string())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseHashError {
    #[error("hash must be 64 hex characters, got {0}")]
    WrongLength(usize),

    #[error("hash contains non-hex characters")]
    InvalidHex,
}

impl FromStr for Hash {
    type Err = ParseHashError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(ParseHashError::WrongLength(s.len()));
        }
        let mut buf = [0u8; 32];
        hex::decode_to_slice(s, &mut buf).map_err(|_| ParseHashError::InvalidHex)?;
        Ok(Self(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hash_is_stable() {
        let h = Hash::of(b"");
        // BLAKE3 hash of the empty input is fixed by the algorithm spec.
        assert_eq!(
            h.to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn hash_roundtrip_via_hex() {
        let h = Hash::of(b"hello, world");
        let s = h.to_hex();
        let parsed: Hash = s.parse().unwrap();
        assert_eq!(h, parsed);
    }

    #[test]
    fn shard_partitions_correctly() {
        let h = Hash::of(b"shard me");
        let (prefix, rest) = h.shard();
        assert_eq!(prefix.len(), 2);
        assert_eq!(rest.len(), 62);
        assert_eq!(format!("{prefix}{rest}"), h.to_hex());
    }
}
