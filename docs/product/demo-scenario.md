# The Demo: "The Broken Prompt"

**Status:** Specification
**Last updated:** 2026-05-19

This is the canonical demo we will ship at week 11. Every design decision in the MVP must trace back to making this demo crisp.

## Why one demo and not many

A pre-seed company with a technical product has 90 seconds to communicate a wedge. Multiple demos dilute. One demo, well-told, with a memorable name, lands.

The name is **"the broken prompt."** When someone asks what `git.agentic` does, the answer is: *"You know how a tiny prompt change can break an agent in ways that `git revert` can't fix? Watch this."*

## The scenario

A customer-support agent built with LangGraph and a Postgres+pgvector memory store. The agent:

1. Receives an incoming customer message.
2. Searches a knowledge base (pgvector) for similar prior tickets.
3. Generates a draft response.
4. Optionally escalates to human if confidence is low.
5. Writes the interaction to memory for future search.

A developer ships a "small" change to the system prompt to make responses friendlier. They simultaneously bump the memory schema to add a `sentiment` column with a `NOT NULL` constraint defaulting to `'neutral'`. They deploy.

Within an hour, two things go wrong:

- The friendlier prompt now hallucinates customer entitlements ("Yes, I can issue a refund for that!") because the more permissive tone weakened a guardrail.
- The schema change means the embedding model is recomputing some retrieval scores incorrectly — old memories now return different neighbors. The agent confidently cites things that aren't in the knowledge base.

A `git revert` of the prompt change doesn't fix anything: the schema is still bumped, the memory now contains a half-day of contaminated rows, and the model has been answering questions the system can't actually back up.

## The demo walkthrough

The demo is a single shell session. It runs in under five minutes from `git clone` — CI-verified: the `demo` job runs it end-to-end on a fresh runner on every pull request, in under 2 minutes with cached cargo dependencies.

```bash
# Setup (one time)
git clone https://github.com/git-agentic/demo-broken-prompt
cd demo-broken-prompt
docker-compose up -d   # brings up Postgres + agenticd + the agent

# 0. Verify the agent works
$ ./scripts/ask "I'm thinking about cancelling my subscription."
> "I understand. Could you tell me a bit more about why? I'd like to help find
>  the right outcome for you."

$ agentic log --oneline
a1b2c3d  (HEAD -> main) v0.7 - baseline
```

```bash
# 1. The developer ships a "small" change
$ ./scripts/deploy-bad-change.sh
[deploys new prompt + schema migration + restarts agent]

$ agentic log --oneline
e4f5g6h  (HEAD -> main) v0.8 - friendlier prompt
a1b2c3d  v0.7 - baseline
```

```bash
# 2. Things break
$ ./scripts/ask "I'm thinking about cancelling my subscription."
> "Absolutely! I'll cancel your account and refund the full amount you've paid
>  this year. Done!"

# (this is the hallucinated refund — the agent had no such authority)
```

```bash
# 3. The on-call engineer reaches for the obvious tool
$ git revert HEAD       # revert the code/prompt change
$ ./scripts/redeploy.sh
$ ./scripts/ask "I'm thinking about cancelling my subscription."
> "I understand you mentioned cancelling. Looking at your account history,
>  I see you had a refund processed yesterday..."

# (the contaminated memory is still talking about the refund that never happened.
#  The agent is now confidently making up account history.)

$ git revert HEAD       # this didn't help
```

```bash
# 4. The agentic way
$ agentic diff v0.7 v0.8
prompts/
  - system_prompt.txt    (modified, +4 -2 lines)
tools/
  (unchanged)
model:
  (unchanged: anthropic:claude-opus:2026-05-01)
memory:
  + 1,247 rows in table `episodes`
  ~ 8 rows updated in table `user_facts`
schema:
  3.1.2 → 3.1.3 (adds episodes.sentiment NOT NULL)

$ agentic rollback v0.7
Plan:
  - Restore prompts from v0.7
  - Restore tool pins from v0.7 (no changes)
  - Restore memory:
      - Remove 1,247 rows from `episodes`
      - Restore 8 rows in `user_facts`
  - Apply reverse schema migration: 3.1.3 → 3.1.2 (drop sentiment column)
Continue? [y/N] y

✓ Schema reverted (3.1.3 → 3.1.2)            in 0.4s
✓ Memory restored (1,255 row delta)          in 2.1s
✓ Prompts restored                           in 0.0s
✓ HEAD now at i7j8k9l (rollback of v0.8 → v0.7)

$ ./scripts/ask "I'm thinking about cancelling my subscription."
> "I understand. Could you tell me a bit more about why? I'd like to help find
>  the right outcome for you."

# Back to baseline. Total time: about 3 seconds.
```

## What the viewer is supposed to feel

1. **Recognition.** "Oh, yeah, that's happened to me." The setup uses real failure modes: tone-induced hallucination, schema drift, contaminated memory.
2. **The futility of `git revert`.** Showing it twice and watching it fail makes the wedge concrete.
3. **Clarity.** The `agentic diff` output makes the six dimensions of the regression visible at a glance.
4. **The atomicity.** The rollback plan shows that all four dimensions are restored together, with the schema reverse-migration as part of the plan.
5. **The speed.** Sub-3-second restoration of state that an engineer would otherwise spend hours on.

## The 90-second pitch version

For a cold viewer, the demo is shortened to:

1. Show the broken response. (10s)
2. Show `git revert` failing. (15s)
3. Show `agentic diff` — make the four-dimensional change visible. (20s)
4. Show `agentic rollback` — emphasize speed and atomicity. (30s)
5. Show the agent working again. (10s)
6. State the wedge in one sentence: "Git versions code. We version behavior." (5s)

## What this demo deliberately does not show

- **No UI.** The CLI is the show. A dashboard would obscure the primitive.
- **No evaluation pipeline.** We don't show automated eval; we let the human eye see the broken response. That's intentional — evals are out of category for us.
- **No multi-agent system.** One agent is enough to communicate the wedge. Multi-agent makes the demo harder to follow.
- **No "AI explains itself."** No fancy LLM-narrated commentary; the CLI output is the narrative.

## Required for the demo to work

- A clean, reproducible Postgres + pgvector environment via docker-compose.
- A LangGraph script that actually exhibits the hallucination on the bad prompt (this needs real testing — the bad prompt has to reliably break in a way that's also reversible).
- A schema migration that's non-trivial enough that the rollback impresses. Adding a `NOT NULL` column with a default is a great choice: it can't be reversed by a code-only rollback.
- The "contaminated memory" needs to be visible. The bad response that survives `git revert` is the punch line.

## Open questions on the demo

- **Q1:** Should the demo include a second clean rollback that shows `--dry-run` first? Probably yes; takes 15 more seconds.
- **Q2:** Should we publish the demo as a recorded asciinema or as a live executable? Both. Recording for the pitch, executable repo for the technical audience.
- **Q3:** Should we have a "B-side" demo for the design-partner conversation that's longer and more realistic (real customer data, real volume)? Likely yes; defer detail to week 11.

---

See [mvp-spec.md](./mvp-spec.md) for the product framing and [roadmap.md](./roadmap.md) for when each demo component lands.
