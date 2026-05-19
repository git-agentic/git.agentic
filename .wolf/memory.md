# Memory

> Chronological action log. Hooks and AI append to this file automatically.
> Old sessions are consolidated by the daemon weekly.

## Session: 2026-05-19 16:15

| Time | Action | File(s) | Outcome | ~Tokens |
|------|--------|---------|---------|--------|
| 16:43 | Created ../../../.claude/plans/read-the-project-folder-s-functional-lerdorf.md | — | ~2996 |
| 17:?? | Created docs/adr/0002-substrate-and-supercommit.md | ADR-0002 (Proposed) locks in Approach C: Git core for code, content-addressed blob store for non-code, coordinator on top. Extends Commit object with intent/plan/transcript/evals/cost_cents/signatures. Fixes the 2PC staging order (blobs first, Git push as single commit point). Surfaces two design-partner honesty items (snapshot-capable storage; bounded destructive-migration rollback). Door to fully post-Git substrate in v2+ kept open via storage-independent platform API contract. | ~3400 |
| 17:?? | Accepted docs/adr/0002-substrate-and-supercommit.md | Status flipped Proposed → Accepted by Toni. ADR-0002 is now the authoritative substrate spec. Action items 1–8 are open work in the project backlog. | — |
| 17:?? | Rewrote CLAUDE.md | Replaced the 5-line OpenWolf bootstrap with a full standing-context file for AI assistants: identity, phase (12-week MVP to 2026-08-11), authoritative ADRs (0001 + 0002), strategic tension explicitly flagged (in-repo MVP spec disqualifies Cursor/Cognition; recent strategy work targets them as platform-led GTM — not yet reconciled), repo layout, build/test commands, code style, eight named anti-patterns including the 2PC staging-order invariant, demo-as-discipline framing, OpenWolf workflow rules preserved with @.wolf/OPENWOLF.md include. | ~2200 |
| 17:07 | Edited crates/agentic-core/src/object.rs | expanded (+31 lines) | ~690 |
| 17:07 | Edited crates/agentic-core/src/lib.rs | 7→10 lines | ~65 |
| 17:08 | Created crates/agentic-core/src/refs.rs | — | ~1614 |
| 17:10 | Created crates/agentic-core/src/commit.rs | — | ~2808 |
| 17:11 | Edited crates/agentic-proto/src/lib.rs | expanded (+12 lines) | ~264 |
| 17:12 | Edited crates/agentic-proto/Cargo.toml | 4→5 lines | ~31 |
| 17:12 | Edited crates/agentic-proto/src/lib.rs | 9→11 lines | ~94 |
| 17:13 | Created crates/agentic-proto/src/framing.rs | — | ~817 |
| 17:14 | Created crates/agenticd/src/server.rs | — | ~1581 |
| 17:14 | Edited crates/agenticd/src/main.rs | modified main() | ~593 |
| 17:15 | Created crates/agentic-cli/src/client.rs | — | ~678 |
| 17:17 | Created crates/agentic-cli/src/main.rs | — | ~2268 |
| 17:23 | Created .github/workflows/ci.yml | — | ~216 |
| 17:25 | Edited crates/agentic-core/src/object.rs | modified kind() | ~275 |
| 17:26 | Edited crates/agentic-core/src/commit.rs | inline fix | ~9 |
| 17:26 | Edited crates/agentic-core/src/commit.rs | 5→5 lines | ~51 |
| 17:26 | Edited crates/agentic-core/src/commit.rs | 11→11 lines | ~91 |
| 17:26 | Edited crates/agenticd/src/server.rs | 7→7 lines | ~73 |
| 17:30 | Edited crates/agentic-proto/src/lib.rs | 8→8 lines | ~51 |
| 17:30 | Edited crates/agenticd/src/server.rs | inline fix | ~11 |
| 17:30 | Edited crates/agentic-cli/src/main.rs | 22→22 lines | ~212 |
| 17:33 | Session end: 22 writes across 11 files (read-the-project-folder-s-functional-lerdorf.md, object.rs, lib.rs, refs.rs, commit.rs) | 17 reads | ~39774 tok |
