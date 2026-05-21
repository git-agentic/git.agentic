# Six Things Determine What Your AI Agent Does

*For backend and infrastructure engineers new to agent operations.*

If you operate a microservice in production and it regresses, you know what to do. Diff the recent commits. Check the deploys. Look at the metrics. Roll back the bad commit. Done in twenty minutes.

If you operate an LLM agent in production and it regresses, none of that works. The commits look the same. The deploys look the same. The metrics show a different distribution of outputs but they don't tell you why. You roll back the code and the agent is still broken. You spend an afternoon and end up writing a Slack message that includes the phrase *we think it's resolved.*

The asymmetry isn't that agents are harder to debug. It's that the surface area of what determines an agent's behavior is wider than what version control was designed to track. A microservice's behavior at runtime is almost entirely determined by its code plus configuration. An agent's behavior is determined by at least six things, and your version control system probably tracks one of them.

The six are:

**Code.** Same as ever. Orchestration logic, business rules, glue between components. Git already does this part.

**Prompts.** System prompts, few-shot examples, instructions embedded in source files or pulled from disk at startup. Sometimes in Git. Often not — pulled from a database, an experiment platform, a CMS, or a colleague's hand-edit.

**Tools.** MCP servers, function specs, the JSON schemas of inputs and outputs. The pinned version of each tool matters: a tool that returned `{"price": "USD 12.50"}` last week and returns `{"price": {"amount": 12.50, "currency": "USD"}}` this week will silently break any agent that pattern-matched on the old shape.

**Model.** Which model, which checkpoint, which sampling settings. Providers update models. Default parameters drift. Two weeks from now, the same prompt produces different outputs.

**Memory.** Whatever the agent has written down, learned, or vector-indexed. Memory accumulates across requests. Once a wrong fact is in there, future requests will keep retrieving it.

**Schema.** The shape of memory rows and tool I/O. Schema migrations are silent commits; they change behavior without ever showing up in `git diff`.

Change any one of these between Tuesday and Thursday and your agent's outputs change. Most production agent failures are some combination of two or three of them moving at once.

The reason this matters is operational, not academic. When the agent regresses, you need to know which of the six changed first. Today, most of those questions don't have answers because most of those dimensions aren't under version control. Prompts get edited in admin UIs. Memory writes happen on every request. Tool versions update when the MCP server's maintainer ships. Models update when the provider ships. Schema migrations run in the foreground and then the diff disappears.

The path forward is treating the six as one versioned unit. A commit, in that model, is a snapshot of the full tuple — code, prompts, tool fingerprints, model identifiers, memory segments, schema version — at a single moment, with a single content-addressed identifier. Diffing two commits shows the changes across all six dimensions, not just code. Rolling back means restoring all six together, including running reverse schema migrations, so there's no in-between state where the code is at the old commit and the memory is still at the new one.

That's what git.agentic is. The substrate is Git plus a content-addressed object store; the coordinator is a daemon that stages writes in a strict order so the word *atomic* means what it says. MVP targets are commit under two seconds, rollback under five, diff under one, on a workload that exercises all six dimensions changing at once.

The demo driving the whole project is deliberately narrow. Clean `git clone`. `docker-compose up`. A scripted change that breaks the agent across multiple dimensions in a way `git revert` cannot fix. Then a single command that puts everything back. Five seconds, end-to-end.

The interesting question is whether the abstraction holds up once people start using it on workloads that look nothing like the demo. The interesting question is *not* whether it's nicer than the current state of practice. The current state of practice is staring at logs.
