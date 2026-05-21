//! Secret scanner: pattern + entropy detection as a put_raw pre-hook.
//! See ADR-0013.

use crate::hash::Hash;
use crate::scanner_patterns::PATTERNS;
use regex::bytes::{Regex, RegexSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const ENTROPY_THRESHOLD: f64 = 4.5;
const MIN_RUN_LENGTH: usize = 20;

/// Bytes considered part of the base64-ish alphabet for entropy scanning.
fn is_base64ish(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub kind: HitKind,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name")]
pub enum HitKind {
    Pattern(String),
    HighEntropy,
}

/// Blob-hash-scoped allowlist. An entry whitelists exactly one blob's
/// content hash; the scanner suppresses every pattern + entropy hit on
/// that specific blob.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    blob_hashes: BTreeSet<Hash>,
}

impl Allowlist {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_toml(toml_str: &str) -> Result<Self, AllowlistError> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            ignore: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            blob_hash: String,
        }
        let file: File = toml::from_str(toml_str)?;
        let mut blob_hashes = BTreeSet::new();
        for entry in file.ignore {
            let h: Hash = entry.blob_hash.parse()?;
            blob_hashes.insert(h);
        }
        Ok(Self { blob_hashes })
    }

    pub fn load_from_path(path: &std::path::Path) -> Result<Self, AllowlistError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn contains(&self, h: &Hash) -> bool {
        self.blob_hashes.contains(h)
    }

    pub fn len(&self) -> usize {
        self.blob_hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blob_hashes.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AllowlistError {
    #[error("reading allowlist file: {0}")]
    Io(#[from] std::io::Error),
    #[error("parsing allowlist TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("parsing blob_hash: {0}")]
    Hash(#[from] crate::hash::ParseHashError),
}

/// Compiled scanner. Construct once per ObjectStore; reuse across puts.
pub struct Scanner {
    regex_set: RegexSet,
    regexes: Vec<Regex>,
    pattern_names: Vec<&'static str>,
}

impl Scanner {
    pub fn new() -> Self {
        let patterns: Vec<&str> = PATTERNS.iter().map(|p| p.regex).collect();
        let regex_set =
            RegexSet::new(&patterns).expect("PATTERNS must compile (covered by unit tests)");
        let regexes = PATTERNS
            .iter()
            .map(|p| Regex::new(p.regex).expect("PATTERN must compile"))
            .collect();
        let pattern_names = PATTERNS.iter().map(|p| p.name).collect();
        Self {
            regex_set,
            regexes,
            pattern_names,
        }
    }

    /// Returns the list of hits found in `bytes`. Empty Vec means clean.
    pub fn scan(&self, bytes: &[u8]) -> Vec<Hit> {
        let mut hits = Vec::new();

        // Patterns
        let matches = self.regex_set.matches(bytes);
        for pattern_idx in matches.iter() {
            for m in self.regexes[pattern_idx].find_iter(bytes) {
                hits.push(Hit {
                    kind: HitKind::Pattern(self.pattern_names[pattern_idx].to_string()),
                    offset: m.start(),
                    length: m.end() - m.start(),
                });
            }
        }

        // Entropy (single pass for runs of base64ish chars)
        let mut run_start: Option<usize> = None;
        for (i, &b) in bytes.iter().enumerate() {
            if is_base64ish(b) {
                if run_start.is_none() {
                    run_start = Some(i);
                }
            } else if let Some(start) = run_start.take() {
                self.maybe_emit_entropy_hit(bytes, start, i, &mut hits);
            }
        }
        if let Some(start) = run_start {
            self.maybe_emit_entropy_hit(bytes, start, bytes.len(), &mut hits);
        }

        hits
    }

    fn maybe_emit_entropy_hit(
        &self,
        bytes: &[u8],
        start: usize,
        end: usize,
        hits: &mut Vec<Hit>,
    ) {
        let len = end - start;
        if len < MIN_RUN_LENGTH {
            return;
        }
        let h = shannon_entropy(&bytes[start..end]);
        if h > ENTROPY_THRESHOLD {
            hits.push(Hit {
                kind: HitKind::HighEntropy,
                offset: start,
                length: len,
            });
        }
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    let mut h = 0.0;
    for &c in counts.iter() {
        if c > 0 {
            let p = c as f64 / len;
            h -= p * p.log2();
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_catches_github_pat() {
        let s = Scanner::new();
        let blob = b"some prefix ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa trailing";
        let hits = s.scan(blob);
        assert!(
            hits.iter()
                .any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "github_pat")),
            "should catch ghp_ token; got {hits:?}"
        );
    }

    #[test]
    fn scanner_catches_aws_access_key() {
        let s = Scanner::new();
        let blob = b"AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let hits = s.scan(blob);
        assert!(hits
            .iter()
            .any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "aws_access_key_id")));
    }

    #[test]
    fn scanner_catches_anthropic_key() {
        let s = Scanner::new();
        let blob = b"key: sk-ant-api-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let hits = s.scan(blob);
        assert!(hits
            .iter()
            .any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "anthropic_api_key")));
    }

    #[test]
    fn scanner_catches_pem_header() {
        let s = Scanner::new();
        let blob = b"-----BEGIN RSA PRIVATE KEY-----\nMIIB...";
        let hits = s.scan(blob);
        assert!(hits
            .iter()
            .any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "private_key_pem_header")));
    }

    #[test]
    fn scanner_catches_gcp_service_account_marker() {
        let s = Scanner::new();
        let blob = br#"{"type": "service_account", "project_id": "x"}"#;
        let hits = s.scan(blob);
        assert!(hits
            .iter()
            .any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "gcp_service_account_marker")));
    }

    #[test]
    fn entropy_detector_catches_high_entropy_run() {
        // 30-char base64-ish string with near-uniform char distribution.
        let s = Scanner::new();
        let blob = b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi";
        let hits = s.scan(blob);
        assert!(
            hits.iter().any(|h| h.kind == HitKind::HighEntropy),
            "should catch high-entropy run; got {hits:?}"
        );
    }

    #[test]
    fn entropy_does_not_flag_repetitive_runs() {
        // 30 repeating 'a's: very low entropy.
        let s = Scanner::new();
        let blob = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hits = s.scan(blob);
        assert!(
            !hits.iter().any(|h| h.kind == HitKind::HighEntropy),
            "should not flag low-entropy; got {hits:?}"
        );
    }

    #[test]
    fn entropy_does_not_flag_short_runs() {
        // 15-char base64 string under the 20-char min.
        let s = Scanner::new();
        let blob = b"short: aB3xQ9zPmK7nR"; // 15 chars after the prefix
        let hits = s.scan(blob);
        assert!(
            !hits.iter().any(|h| h.kind == HitKind::HighEntropy),
            "should not flag short runs; got {hits:?}"
        );
    }

    #[test]
    fn clean_blob_produces_no_hits() {
        let s = Scanner::new();
        let blob = b"this is some normal English text without any secrets.";
        let hits = s.scan(blob);
        assert!(hits.is_empty(), "should be clean; got {hits:?}");
    }

    #[test]
    fn allowlist_loads_and_matches() {
        // Construct a known blob, get its Hash, put the hash in the allowlist,
        // confirm contains.
        let bytes = b"sample blob bytes";
        let h = Hash::of(bytes);
        let toml_text = format!(
            r#"
            [[ignore]]
            blob_hash = "{}"
            reason = "test"
        "#,
            h.to_hex()
        );
        let al = Allowlist::from_toml(&toml_text).unwrap();
        assert!(al.contains(&h));
        assert_eq!(al.len(), 1);
    }

    #[test]
    fn allowlist_missing_file_yields_empty() {
        let al = Allowlist::load_from_path(std::path::Path::new(
            "/nonexistent/scanner-allowlist.toml",
        ))
        .unwrap();
        assert_eq!(al.len(), 0);
    }
}
