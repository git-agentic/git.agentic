# PR-3 — Secret scanner — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Implement [ADR-0013](../../adr/0013-secret-scanner.md). Closes TM-009 (the "scanner exists" claim across CLAUDE.md / AGENTS.md / overview.md / competitive-brief becomes truthful).

**Architecture:** New module `agentic-core::scanner` runs as a pre-hook inside `agentic-core::store::put_raw`. Pattern detector (regex::RegexSet over a curated `&[TokenPattern]` array) + entropy detector (Shannon entropy > 4.5 bits/char on contiguous base64-alphabet runs ≥ 20 chars) share one scan pass. Hits return `Error::SecretDetected { hits }`; backend put is never reached. Allowlist is blob-hash-scoped (TOML at `.agentic/scanner-allowlist.toml`).

**Tech Stack:** Rust (`crates/agentic-core`); new workspace dependencies `regex` and `toml`. Existing `Hash` (BLAKE3) used for allowlist scoping.

**ADR-amendment note:** ADR-0013 D4 says "blob-SHA256-scoped" allowlist. This plan uses `agentic-core::Hash` (BLAKE3) instead, matching the rest of the system and avoiding a `sha2` dependency. ADR-0013 will be amended in a follow-up PR to reflect this.

---

## File Structure

**New:**
- `crates/agentic-core/src/scanner.rs` — `ScanResult`, `Hit`, `HitKind`, `Scanner` (compiled `RegexSet` + entropy state), `Allowlist`, `pub fn scan(bytes, &allowlist) -> ScanResult` (or as a `Scanner::scan` method — implementer's choice; one entry point either way).
- `crates/agentic-core/src/scanner_patterns.rs` — `pub struct TokenPattern { name, regex, description }` and `pub const PATTERNS: &[TokenPattern]`.

**Modified:**
- `crates/agentic-core/src/lib.rs` — `pub mod scanner; pub mod scanner_patterns;`; new `Error::SecretDetected { hits: Vec<scanner::Hit> }` variant.
- `crates/agentic-core/src/store.rs::put_raw` — call `scanner::scan` before delegating; return `Error::SecretDetected` on hit. The `FsObjectStore` and `GcsObjectStore` implementations of `put_raw` both gain the same pre-hook. Two ways: (a) add a default-method wrapper at trait level, OR (b) modify each impl. The trait can't have a default method easily because `Scanner` state lives in the store; prefer adding a new field to each store struct and modifying each `put_raw` body. Implementer's call.
- `crates/agentic-core/Cargo.toml` — add `regex = "1"` and `toml = "0.8"` to dependencies.
- `crates/agenticd/src/main.rs` — new `--scanner-allowlist <path>` flag (default `<repo>/.agentic/scanner-allowlist.toml`); load at startup; pass into ObjectStore constructors.

---

## Branch + Setup

### Task 0: Branch (controller has done this)

You are on `feat/pr3-secret-scanner`. The plan-only commit lands first; implementation follows.

---

## Task 1 — Pattern set + scanner_patterns module

### Task 1.1: Define `TokenPattern` and patterns

- [ ] **Step 1: Create `crates/agentic-core/src/scanner_patterns.rs`**

```rust
//! Curated set of high-precision secret patterns for the put_raw scanner.
//! See ADR-0013 Decision 5: patterns are compile-time, reviewable at PR
//! time, never loaded from disk.

#[derive(Debug, Clone, Copy)]
pub struct TokenPattern {
    pub name: &'static str,
    pub regex: &'static str,
    pub description: &'static str,
}

pub const PATTERNS: &[TokenPattern] = &[
    TokenPattern {
        name: "github_pat",
        regex: r"gh[poshu]_[A-Za-z0-9_]{36,}",
        description: "GitHub personal-access-token format (ghp_, gho_, ghs_, ghu_, ghp_)",
    },
    TokenPattern {
        name: "aws_access_key_id",
        regex: r"AKIA[0-9A-Z]{16}",
        description: "AWS access key ID",
    },
    TokenPattern {
        name: "anthropic_api_key",
        regex: r"sk-ant-(api|admin)-[A-Za-z0-9_-]{40,}",
        description: "Anthropic API or admin key",
    },
    TokenPattern {
        name: "openai_api_key",
        regex: r"sk-(proj-)?[A-Za-z0-9]{48,}",
        description: "OpenAI API key (standard and project)",
    },
    TokenPattern {
        name: "stripe_live_key",
        regex: r"(sk|pk)_live_[A-Za-z0-9]{24,}",
        description: "Stripe live secret or publishable key",
    },
    TokenPattern {
        name: "gcp_service_account_marker",
        regex: r#""type"\s*:\s*"service_account""#,
        description: "GCP service-account JSON marker",
    },
    TokenPattern {
        name: "private_key_pem_header",
        regex: r"-----BEGIN (RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----",
        description: "PEM-encoded private-key header",
    },
];
```

---

## Task 2 — Scanner module

### Task 2.1: Define types

- [ ] **Step 1: Create `crates/agentic-core/src/scanner.rs`**

```rust
//! Secret scanner: pattern + entropy detection as a put_raw pre-hook.
//! See ADR-0013.

use crate::hash::Hash;
use crate::scanner_patterns::{TokenPattern, PATTERNS};
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
    pub fn empty() -> Self { Self::default() }

    pub fn from_toml(toml_str: &str) -> Result<Self, AllowlistError> {
        #[derive(Deserialize)]
        struct File { #[serde(default)] ignore: Vec<Entry> }
        #[derive(Deserialize)]
        struct Entry { blob_hash: String }
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

    pub fn contains(&self, h: &Hash) -> bool { self.blob_hashes.contains(h) }

    pub fn len(&self) -> usize { self.blob_hashes.len() }
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
        let regex_set = RegexSet::new(&patterns).expect("PATTERNS must compile (covered by unit tests)");
        let regexes = PATTERNS.iter()
            .map(|p| Regex::new(p.regex).expect("PATTERN must compile"))
            .collect();
        let pattern_names = PATTERNS.iter().map(|p| p.name).collect();
        Self { regex_set, regexes, pattern_names }
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
                if run_start.is_none() { run_start = Some(i); }
            } else if let Some(start) = run_start.take() {
                self.maybe_emit_entropy_hit(bytes, start, i, &mut hits);
            }
        }
        if let Some(start) = run_start {
            self.maybe_emit_entropy_hit(bytes, start, bytes.len(), &mut hits);
        }

        hits
    }

    fn maybe_emit_entropy_hit(&self, bytes: &[u8], start: usize, end: usize, hits: &mut Vec<Hit>) {
        let len = end - start;
        if len < MIN_RUN_LENGTH { return; }
        let h = shannon_entropy(&bytes[start..end]);
        if h > ENTROPY_THRESHOLD {
            hits.push(Hit { kind: HitKind::HighEntropy, offset: start, length: len });
        }
    }
}

impl Default for Scanner {
    fn default() -> Self { Self::new() }
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() { return 0.0; }
    let mut counts = [0u32; 256];
    for &b in bytes { counts[b as usize] += 1; }
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
        assert!(hits.iter().any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "github_pat")), "should catch ghp_ token; got {hits:?}");
    }

    #[test]
    fn scanner_catches_aws_access_key() {
        let s = Scanner::new();
        let blob = b"AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let hits = s.scan(blob);
        assert!(hits.iter().any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "aws_access_key_id")));
    }

    #[test]
    fn scanner_catches_anthropic_key() {
        let s = Scanner::new();
        let blob = b"key: sk-ant-api-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let hits = s.scan(blob);
        assert!(hits.iter().any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "anthropic_api_key")));
    }

    #[test]
    fn scanner_catches_pem_header() {
        let s = Scanner::new();
        let blob = b"-----BEGIN RSA PRIVATE KEY-----\nMIIB...";
        let hits = s.scan(blob);
        assert!(hits.iter().any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "private_key_pem_header")));
    }

    #[test]
    fn scanner_catches_gcp_service_account_marker() {
        let s = Scanner::new();
        let blob = br#"{"type": "service_account", "project_id": "x"}"#;
        let hits = s.scan(blob);
        assert!(hits.iter().any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "gcp_service_account_marker")));
    }

    #[test]
    fn entropy_detector_catches_high_entropy_run() {
        // 30-char base64-ish string with near-uniform char distribution.
        let s = Scanner::new();
        let blob = b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi";
        let hits = s.scan(blob);
        assert!(hits.iter().any(|h| h.kind == HitKind::HighEntropy), "should catch high-entropy run; got {hits:?}");
    }

    #[test]
    fn entropy_does_not_flag_repetitive_runs() {
        // 30 repeating 'a's: very low entropy.
        let s = Scanner::new();
        let blob = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hits = s.scan(blob);
        assert!(!hits.iter().any(|h| h.kind == HitKind::HighEntropy), "should not flag low-entropy; got {hits:?}");
    }

    #[test]
    fn entropy_does_not_flag_short_runs() {
        // 15-char base64 string under the 20-char min.
        let s = Scanner::new();
        let blob = b"short: aB3xQ9zPmK7nR";  // 15 chars after the prefix
        let hits = s.scan(blob);
        assert!(!hits.iter().any(|h| h.kind == HitKind::HighEntropy), "should not flag short runs; got {hits:?}");
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
        let toml_text = format!(r#"
            [[ignore]]
            blob_hash = "{}"
            reason = "test"
        "#, h.to_hex());
        let al = Allowlist::from_toml(&toml_text).unwrap();
        assert!(al.contains(&h));
        assert_eq!(al.len(), 1);
    }

    #[test]
    fn allowlist_missing_file_yields_empty() {
        let al = Allowlist::load_from_path(std::path::Path::new("/nonexistent/scanner-allowlist.toml")).unwrap();
        assert_eq!(al.len(), 0);
    }
}
```

---

## Task 3 — Wire scanner into `put_raw`

### Task 3.1: Add `Error::SecretDetected`

- [ ] **Step 1: Extend `Error` enum in `crates/agentic-core/src/lib.rs`**

```rust
#[error("blob rejected by secret scanner: {hits:?}")]
SecretDetected { hits: Vec<scanner::Hit> },
```

Add `pub mod scanner; pub mod scanner_patterns;` at the top of the file.

### Task 3.2: Modify `FsObjectStore` and `GcsObjectStore`

- [ ] **Step 2: Each store gains a `Scanner + Allowlist`**

Both stores get two new fields:

```rust
scanner: Arc<scanner::Scanner>,
allowlist: Arc<scanner::Allowlist>,
```

(Use `Arc` so the daemon can share one scanner across multiple stores if it ever has more than one.)

Constructor signatures change. `FsObjectStore::open(root)` adds an `with_allowlist` builder method:

```rust
impl FsObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> { /* existing body, sets allowlist to empty */ }
    pub fn with_allowlist(mut self, allowlist: Allowlist) -> Self { self.allowlist = Arc::new(allowlist); self }
}
```

Same for `GcsObjectStore::new(...)`. The constructors default to an empty allowlist; `with_allowlist` is opt-in.

### Task 3.3: Pre-hook in `put_raw`

- [ ] **Step 3: Both `put_raw` impls call scanner first**

```rust
fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Hash> {
    let hits = self.scanner.scan(bytes);
    if !hits.is_empty() {
        let h = Hash::of(bytes);
        if !self.allowlist.contains(&h) {
            return Err(Error::SecretDetected { hits });
        }
    }
    // ... existing put body ...
}
```

The same pre-hook lives in both `FsObjectStore::put_raw` and `GcsObjectStore::put_raw`. This is duplication, but per the plan's File-Structure note it's the right shape for v1.0 — adding the hook to a default trait method would require Scanner state on the trait, which doesn't fit cleanly. The cost is one extra location to keep in sync.

### Task 3.4: Tests

- [ ] **Step 4: Integration test in `crates/agentic-core/src/store.rs` (or a new tests module)**

```rust
#[test]
fn put_raw_rejects_blob_with_secret() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsObjectStore::open(dir.path()).unwrap();
    let blob = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld";
    match store.put_raw(ObjectKind::Blob, blob) {
        Err(Error::SecretDetected { hits }) => {
            assert!(hits.iter().any(|h| matches!(&h.kind, scanner::HitKind::Pattern(n) if n == "aws_access_key_id")));
        }
        other => panic!("expected SecretDetected, got {other:?}"),
    }
    // Confirm no object was written.
    let h = Hash::of(blob);
    assert!(!store.has(&h));
}

#[test]
fn put_raw_allowlist_suppresses_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld";
    let h = Hash::of(blob);
    let mut al = scanner::Allowlist::empty();
    // Use the from_toml roundtrip to keep the public API consistent.
    let toml_text = format!(r#"
        [[ignore]]
        blob_hash = "{}"
    "#, h.to_hex());
    let al = scanner::Allowlist::from_toml(&toml_text).unwrap();
    let store = FsObjectStore::open(dir.path()).unwrap().with_allowlist(al);
    let hash = store.put_raw(ObjectKind::Blob, blob).expect("allowlisted blob should put cleanly");
    assert_eq!(hash, h);
    assert!(store.has(&h));
}

#[test]
fn put_raw_clean_blob_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsObjectStore::open(dir.path()).unwrap();
    let blob = b"normal-looking content";
    let h = store.put_raw(ObjectKind::Blob, blob).expect("clean blob should put");
    assert_eq!(h, Hash::of(blob));
    assert!(store.has(&h));
}
```

---

## Task 4 — agenticd CLI integration

### Task 4.1: New flag

- [ ] **Step 1: Add `--scanner-allowlist <path>` to `Args` in `crates/agenticd/src/main.rs`**

Default: `<repo>/.agentic/scanner-allowlist.toml`. The flag accepts an explicit override for unusual layouts.

### Task 4.2: Load allowlist at startup; pass to ObjectStore

- [ ] **Step 2: In `main()`, after building `agentic_dir`**

```rust
let allowlist_path = args.scanner_allowlist.unwrap_or_else(|| agentic_dir.join("scanner-allowlist.toml"));
let allowlist = agentic_core::scanner::Allowlist::load_from_path(&allowlist_path)
    .with_context(|| format!("loading scanner allowlist from {}", allowlist_path.display()))?;
tracing::info!(
    target: "agenticd::scanner",
    allowlist_entries = allowlist.len(),
    path = %allowlist_path.display(),
    "scanner allowlist loaded"
);
```

Then thread `allowlist` into the ObjectStore construction (`ObjectStoreSpec::parse` or wherever the store is built).

---

## Task 5 — Pre-flight + push + PR

### Task 5.1: Workspace verification

- [ ] **Step 1: Full checks**

```bash
cargo check --workspace 2>&1 | tail -3
cargo test --workspace --lib 2>&1 | tail -5
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --check 2>&1 | tail -2
```

All green.

- [ ] **Step 2: Demo path smoke**

```bash
docker compose -f examples/langgraph-rollback/docker-compose.yml up -d 2>&1 | tail -3
DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
    examples/langgraph-rollback/scripts/run-demo.sh 2>&1 | tail -5
```

The demo shouldn't trip the scanner (no real secrets in prompts), but if a test fixture in the prompts/ tree happens to look like a secret, the demo would fail. If that happens, document it as a follow-up rather than scrubbing the demo content in this PR.

### Task 5.2: Push + open PR

- [ ] **Step 3: Push and open PR**

```bash
git push -u origin feat/pr3-secret-scanner
gh pr create --title "agenticd: PR-3 secret scanner (ADR-0013; closes TM-009)" --body "..."
```

PR body summarizes (with verbatim test counts):

- Closes TM-009 — the scanner advertised in CLAUDE.md / AGENTS.md / overview.md / competitive-brief now exists in code.
- 9 unit tests in `scanner::tests` cover each pattern, entropy positive + negative cases, clean-blob, allowlist load + match, missing-file behavior.
- 3 store-level tests in `store.rs::tests` cover end-to-end put_raw rejection, allowlist suppression, clean-path.
- `Error::SecretDetected { hits }` is a new typed error variant.
- ADR-amendment note: ADR-0013 D4 used "blob-SHA256-scoped" allowlist; this PR uses BLAKE3 (existing `Hash::of`) for system consistency. Follow-up: amend ADR-0013 D4 to reflect.

---

## Self-Review

- All ADR-0013 decisions implemented: scanner-in-store (D1), patterns+entropy (D2), hard-reject (D3), blob-hash allowlist (D4 with hash-algorithm amendment), compile-time patterns (D5), failure-injection tests at the put_raw boundary (D6), v1.0 out-of-scope items honored (D7).
- No placeholders, no TBD, no "appropriate" weasel words. Code blocks have actual code.
- Test names are consistent between Task 2.1 and Task 3.4.
- The `Hash::of(&bytes)` and `Hash::to_hex` / `Hash::from_str` invariants are taken from existing code; the implementer should verify the parse direction (`"abc..".parse::<Hash>()`) works as expected in `hash.rs`.

---

## Done definition

- Branch `feat/pr3-secret-scanner` pushed; PR opened.
- 4 commits (or fewer if Tasks 1/2 bundle, or 3/4 bundle).
- `cargo test --workspace --lib` green; +12 new tests minimum.
- `cargo clippy --workspace --all-targets -- -D warnings` green.
- Demo runs end-to-end OR a documented exception logged.
- ADR-0013 amendment note included in PR description.
