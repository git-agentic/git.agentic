# When `git revert` Lies to You

*For AI engineers building production agents.*

You shipped a change yesterday. The agent worked. You shipped another change this morning. The agent broke. You roll back the commit. The tests pass. The agent is still broken.

This is the failure mode that gets called *the broken-prompt problem*, and if you've shipped a non-trivial LLM agent to production you've almost certainly hit some version of it. The reason `git revert` doesn't help is that Git only versions one of the six things that actually determine what your agent does at runtime.

The full tuple looks like this:

1. **Code** — orchestration logic, business rules, glue.
2. **Prompts** — system prompts, few-shot blocks, instructions baked into source files or pulled from disk.
3. **Tools** — MCP servers and their pinned versions, function specs, JSON schemas, side effects.
4. **Model** — which model, which snapshot, which sampling settings.
5. **Memory** — whatever the agent has learned, written down, or vector-indexed.
6. **Schema** — the shape of memory rows and tool I/O. Migrations are silent commits.

Change any one of these between commit A and commit B and behavior changes. Roll back the code and the other five drift forward in time independently. Memory is the worst offender. Writes accumulate; once a contaminated row exists in the vector store, no amount of `git revert` removes it. Schema is right behind: a forward migration that ran at commit B is still applied at commit A unless you wrote — and ran — the reverse.

This is what git.agentic is built to fix. Every commit in the system is a content-addressed snapshot of the full six-dimensional tuple. Rollback is atomic: code, prompts, MCP fingerprints, memory rows, and schema all return to the snapshot together, including reverse migrations. There is no in-between state where the code is at A but the memory is at B.

The substrate is Git plus a content-addressed blob store, coordinated by a daemon called `agenticd`. Writes follow a strict order. Blobs go to the object store first. Their content hashes are collected and assembled into a `Commit` object. The Commit gets pushed to Git as the single atomic commit point. Only then is the branch ref updated. Failure-injection tests run at every boundary so the word *atomic* can mean something specific instead of something aspirational. The v1.0 performance targets are commit under two seconds, rollback under five, diff under one, with snapshot storage at less than 2× the changed data amortized. Demo-scale rollback is around one second; the under-five-second target at the committed one-million-row benchmark shape is still being verified.

The diff is the part most people don't expect. `agentic diff A B` shows you not just which files changed but which prompts changed, which MCP server fingerprints moved, which memory segments grew, and which schema versions advanced. When an agent regresses, the question stops being *what changed?* and becomes *which of these six things changed first?* That's tractable. The current state of practice — diff the code, stare at logs, guess — is not.

The MVP went public on May 22, 2026. The discipline behind every design decision is a single named demo: clean `git clone`, `docker-compose up`, a scripted bad change that breaks the agent across multiple dimensions in a way `git revert` cannot fix, then `agentic rollback` restoring the coherent baseline. CI now runs that demo end to end on a fresh Ubuntu runner in under two minutes with cached Rust dependencies. The first integrations are LangGraph (via the Python SDK and a LangGraph checkpointer) and the Claude Agent SDK, the latter running through a sidecar `agenticd` co-located with the agent's worker so snapshots can be taken atomically at tool-call boundaries.

The substrate has to be honest about destructive operations or the abstraction leaks. Forward schema migrations have to come with reverse migrations or the commit fails. Memory writes are bucketed into segments so rollback doesn't have to copy the world — it has to revert a small set of segments that changed since the snapshot. Secret scanning runs on every blob the daemon would write ([ADR-0013](https://github.com/git-agentic/git.agentic/blob/main/docs/adr/0013-secret-scanner.md)): a pattern + entropy detector hard-rejects matched blobs at the boundary rather than letting them surface three weeks later in Git history.

If you've ever spent an hour staring at agent logs trying to figure out why behavior regressed when *nothing changed*, this is what was missing from your stack. The repo is open. The demo is the only thing that matters.
