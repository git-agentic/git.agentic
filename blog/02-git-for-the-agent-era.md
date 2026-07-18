# Git for the Agent Era

*For technical founders and dev-tool builders.*

What does version control need to become when most commits are written by agents?

That question has a different answer depending on how seriously you take it. The shallow answer is *the same thing as today, with some agent-friendly tooling bolted on.* Better commit-message generators. PR templates. A bot that runs the linter. This is where most of the agent-developer-experience tooling lives today, and it's a reasonable place to live if your model of agents is *fast junior developer.*

The deeper answer is that agents change a different shape of thing than humans do. A human PR is mostly code, occasionally code plus a migration, very occasionally code plus a config change. An agent run can change code, prompts, tool definitions, memory contents, vector indices, and schema in a single causal step — and the *prompts* aren't necessarily in the repo, the *memory* is in a database, the *tools* are in MCP servers running somewhere else, and the *model* is whichever snapshot the platform served on that request. Most of what determined the agent's behavior is not under version control at all.

This is the substrate problem that git.agentic is built around. The bet is that the right primitive isn't a wrapper around Git but a richer commit object that Git happens to be the substrate for. A commit, in this model, is a content-addressed snapshot of the full tuple: code, prompts, tool fingerprints, model identifiers, memory segments, and schema version. Git is the proven, durable, content-addressed log that the system stores Commit blobs into; the extended Commit object is what the rest of the system — SDKs, daemons, platform integrations — actually trades in.

That distinction matters for a reason most dev-tool companies don't get to engage with for a while: it makes the Commit object an API contract.

When the platforms that run agents — LangGraph, Claude Agent SDK, whatever shows up next — want to integrate with a version-control substrate, they don't want to learn Git internals. They want to hand the substrate a structured object that describes what happened in a step and get back an immutable identifier they can pass around. They want *diff* and *rollback* as operations on that object, not on a tree of files. The Commit object is the shape of that contract.

This has a strategic consequence the MVP spec is explicit about. The product is not pitched downstream — at end users writing their own agent loops — but laterally, at the platforms themselves. The first non-LangGraph integration target is the Claude Agent SDK, and it's framed as a partner integration, not a competitive feature. The substrate's job is to be the layer *underneath* agent platforms, not another agent platform. The SDK's public surface deliberately doesn't expose Git refs, object-store paths, or internal segment IDs; those would tie integrators to internal storage choices and preclude the v2+ storage swap to GCS that's already on the roadmap.

Two non-obvious commitments follow from this framing. The first is that destructive operations have to be honest. Forward schema migrations require a reverse migration or the commit doesn't land. Memory writes are bucketed into segments so rollback is a bounded operation. The two-phase staging order — blobs → Commit → Git push → ref update — is enforced by failure-injection tests because *atomic* is a load-bearing word in the pitch and an aspirational one in most systems. The second is that the demo discipline is brutal. A single named demo — the broken-prompt demo — has veto power over every feature in v1.0. If a proposed addition doesn't make that demo crisper, faster, or more honest, it ships in v1.1.

If you're building developer tools right now, the interesting strategic question isn't *can you build features on top of Git for agent developers?* You can; many people are. The more interesting question is whether the commit primitive itself has run out of room and what should replace it underneath. git.agentic is a bet that the answer is yes, and that the replacement is a content-addressed object whose schema is wide enough to capture everything that determines agent behavior — and narrow enough to remain a contract platforms can integrate against for years.

The repo has been public since May 22, 2026. The interesting part of the bet isn't the technology. It's the API surface.
