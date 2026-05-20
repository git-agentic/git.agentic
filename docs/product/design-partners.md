# Design Partners — outreach brief (v1.0)

**Owner:** Toni
**Target:** 3 onboarded teams by 2026-08-11 (roadmap week 12).
**Status:** Draft. Candidate list and outreach status start empty — fill in inline.

## Why this exists now

Roadmap week 12 requires three design partners who have actually run `agentic rollback` against their own staging environments. The lead time on B2B outreach is weeks, not days; an empty pipeline today means missing the kill-criterion in `roadmap.md` §"Kill criteria" ("If at the end of week 12 zero design partners are using the tool weekly in their own work, the wedge does not have product-market pull and we abandon the current scope").

Per the in-repo MVP spec, design partners are explicitly **not** about feature requests. They are about proving the wedge: does the broken-prompt scenario, in their environment, actually save them time?

## Target persona — hard filter

A candidate qualifies only if **all** of the following are true:

- **Stateful LangGraph agent** in production or staging (not just a prototype).
- **Postgres + pgvector** as the memory backend (not Pinecone, not Weaviate, not LanceDB — those are v1.1 backends and won't run on the MVP).
- **Team size 2–15 engineers**, at least one of whom has authority over the staging environment and is willing to spend an hour on setup.
- **They have already been bitten** by a regression that `git revert` alone could not fix — a prompt change that contaminated memory, a model swap that broke downstream behaviour, or similar. This is the qualifying question; if they answer "no" or "we haven't seen that yet," they are not a Week-12 partner.

Per the MVP spec, **coding-agent companies (Cursor / Cognition class) are explicitly disqualified** as design partners — they own their own infrastructure and use a different shape. The Codento Executor integration (ADR-0003) runs in parallel as a platform-led track, not through this design-partner pipeline.

## The ask

> 15-minute intro call: confirm the persona fit and the regression incident. If both match,
> a 60-minute joint setup session in their staging environment — `git clone`,
> `docker-compose up`, run the broken-prompt demo, then point `agenticd` at one of
> their own pgvector-backed agents. We do the typing; they watch and ask questions.

Out of band, they get:

- A pinned channel in Slack/Discord with direct line to the maintainers.
- Their name in the launch post (opt-out by default if they prefer anonymity).
- First look at the GCS-backed `ObjectStore` and the Executor integration (ADR-0003/0004), which they don't need but may find interesting.

No pricing conversation. No commercial commitment. The cost to them is one focused hour of one engineer.

## What we measure

For each onboarded partner:

1. Time from intro call to first successful rollback against their environment.
2. One regression-recovery incident captured before week 12 ends — even if synthetic, it has to be a real shape they would have hit. Written up in `docs/product/design-partners-feedback.md` (created when the first incident lands).
3. Whether they use it again unprompted in the four weeks following.

The week-12 kill-criterion fires on (3): if zero partners come back unprompted, the wedge is wrong.

## Outreach checklist (per candidate)

- [ ] First-touch: identify the right human (eng lead, not founder).
- [ ] 15-min intro scheduled.
- [ ] Persona-fit confirmed (Postgres + pgvector + LangGraph + prior regression).
- [ ] 60-min setup session scheduled.
- [ ] Setup session completed; first commit + rollback in their env.
- [ ] Slack/Discord channel opened.
- [ ] Returned unprompted within 4 weeks of setup (yes/no, with note).

## Candidate list

Fill this in by hand. Eight is the minimum to reach three onboarded; expect a ~3:1 funnel.

| # | Org | Contact | First touch | Status | Notes |
|---|---|---|---|---|---|
| 1 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |
| 2 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |
| 3 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |
| 4 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |
| 5 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |
| 6 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |
| 7 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |
| 8 | _TBD_ | _TBD_ | _TBD_ | _Cold_ | |

## Outreach template

```
Subject: 15 minutes — does this resonate with your LangGraph work?

Hi <name>,

We're building git.agentic — atomic, reversible snapshots of the full
(code + prompts + tools + model + memory + schema) tuple for AI agents.
The wedge is the regression that `git revert` alone can't fix: a small
prompt change that also contaminated agent memory, or a model swap that
broke downstream behaviour.

I'm looking for 3 design partners running stateful LangGraph agents on
Postgres + pgvector who have lived through one of those incidents. The
ask is 15 minutes to check the fit, then a 60-minute joint setup in
your staging environment where I do the typing and we run a rollback
end-to-end against your code. No commercial commitment.

The pre-MVP demo is here: <repo URL>
The broken-prompt scenario is here: docs/product/demo-scenario.md

Have 15 minutes <week>?

— <signature>
```

## What to NOT do

- Don't pitch architecture; pitch the regression they already hate.
- Don't take feature requests on the intro call — only check fit.
- Don't onboard candidates who don't match the persona, however enthusiastic. Misfits at week 12 read as adoption failure.
- Don't promise dates, SLAs, or hosted offerings. v1.0 is self-hosted.
