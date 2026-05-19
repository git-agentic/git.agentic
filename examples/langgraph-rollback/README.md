# Example: the "broken prompt" demo

This directory will hold the canonical end-to-end demo (target: week 11 of the [roadmap](../../docs/product/roadmap.md)). See [`docs/product/demo-scenario.md`](../../docs/product/demo-scenario.md) for the full design.

Scheduled contents:

- `docker-compose.yml` — Postgres + pgvector + agenticd + the demo agent.
- `agent/` — a LangGraph customer-support agent using `AgenticCheckpointer`.
- `scripts/ask.sh`, `scripts/deploy-bad-change.sh`, `scripts/redeploy.sh` — the demo harness.
- `prompts/` — versioned prompt files (v0.7, v0.8 with the bad change).
- `migrations/` — paired up/down schema migrations including the `sentiment NOT NULL` change.
- `data/` — a seed knowledge base of 1000 fake support tickets.
- `README.md` — a five-minute quickstart.

Placeholder for now. Files land per the roadmap.
