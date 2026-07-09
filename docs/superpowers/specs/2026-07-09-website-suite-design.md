# Website suite redesign — design

**Date:** 2026-07-09
**Status:** Approved by Toni (brainstorming session 2026-07-09)
**Scope:** `website/` + `docs/superpowers/` only. No changes to product code or ADRs.

## Goal

Reposition git-agentic.com from a single-project product page into a suite
site presenting three projects — git.agentic, src-control, and Sentinel — as
one effort: rebuilding the infrastructure under software development for the
agentic age. Each project keeps a full-depth product page; the homepage tells
the suite story.

## Decisions made during brainstorming

1. **Site shape:** suite landing page at `/`, plus dedicated full product
   pages `/git-agentic`, `/src-control`, `/sentinel` (no trailing slash). The current homepage
   content moves to `/git-agentic`.
2. **Umbrella brand:** "git.agentic" names the whole effort as well as the
   flagship project (Docker-style company/product name sharing). Header brand
   is unchanged; no new suite name is coined.
3. **Narrative:** problem-led "broken assumptions" framing — every layer of
   dev infrastructure assumes a human in the loop; agents broke that
   assumption; each project fixes one layer.
4. **Page depth:** src-control and Sentinel get full product pages mirroring
   the current git.agentic page structure (hero, problem, terminal demo, how
   it works, status, links), not thin overview pages.
5. **git.agentic status:** the site states v1.0 **shipped** (May 2026) and
   v1.1 in progress. The current site's "v1.0 ships 2026-05-26" future-tense
   wording and "v1.0 target June 2026" badge are stale and must be replaced.

## Site structure

Astro static site, existing build/deploy pipeline (`npm run build`,
`scripts/deploy.sh`, nginx). Four routes:

| Route | Content |
|---|---|
| `/` | New suite landing page |
| `/git-agentic/` | Flagship product page (current index content, updated) |
| `/src-control/` | New full product page |
| `/sentinel/` | New full product page |

Shared chrome: extract `src/components/Header.astro` and
`src/components/Footer.astro` (four pages now share them). Header nav becomes
site-wide: the three project pages + GitHub org. `Base.astro` keeps its
per-page `title`/`description` props; every page sets its own for SEO/OG.
`Base.astro` global styles stay the single source of visual truth.

## Page designs

### `/` — suite landing

- **Hero:** "The tooling under software assumes a human in the loop. Agents
  broke that assumption." Lead paragraph: version control, source control,
  and supply-chain security, rebuilt for the agentic age.
- **Thesis block** (terminal-styled annotation block), the assumption → fix
  mapping:
  - `git revert` assumes code is the only thing that changed → **git.agentic**
  - `npm install` assumes a human read what it fetched → **Sentinel**
  - your checkout assumes one pair of hands → **src-control**
- **Three pillar sections**, ~1 screen each (replacing the current small
  project cards): project name + tagline, a "what breaks / what this fixes"
  paragraph, a compact real terminal snippet, a status/meta line, links to
  the full page and GitHub. Snippets:
  - git.agentic — the `agentic rollback v0.7` block (already on the site)
  - src-control — `sc demo --agents 4` fork/teardown + zero-residue proof
  - Sentinel — condensed `color-stream@1.4.1` BLOCK verdict panel
- **Footer:** suite-wide — three repos, email, Apache 2.0.

### `/git-agentic/` — flagship page

Current homepage content moves here nearly verbatim, with exactly three
changes:

1. **Projects section removed** (the landing page owns that now).
2. **Status updated:** badge and Status section say v1.0 shipped May 2026,
   v1.1 in progress. v1.1 themes drawn from `docs/product/v1.1-plan.md`.
   The "Try it" caveat ("Not there yet — see status below") is re-checked
   against the shipped status and reworded or dropped accordingly.
3. **Header/footer** swap to the shared suite components.

Everything else — tuple table, broken-prompt demo transcript, "What ships in
v1.0", design partners — stays.

### `/src-control/` — new page

All claims sourced from the src-control README and ARCHITECTURE.md.

- **Hero:** parallel-agents angle — "Fork N agent worktrees entirely in RAM.
  Tear them down leaving nothing."
- **Problem:** agents want dozens of concurrent checkouts; git worktrees are
  disk-bound, leaky, and shaped for one developer per checkout.
- **Terminal demo:** `sc demo --agents 4` and the independent before/after
  filesystem-diff zero-residue proof (`demo/run_demo.sh`).
- **How it works:** copy-on-write overlays over immutable base snapshots
  (fork is O(overlay), not O(repo)); bounded blob budget with LRU eviction
  and optional auto-cleaned spill; native committed secrets (encrypted at
  rest/in transit, decrypted only in an authorized execution context);
  per-path encryption; Git import/export via gix — builds on Git, doesn't
  replace it.
- **Status:** Phases 1–9 implemented and tested (in-RAM worktrees, secrets,
  durable repos, merge, secret scanning, remotes, encrypted paths,
  packfiles/GC, Git export). Next: network transports, richer merge
  ergonomics, signed provenance.
- **Links:** GitHub repo, ARCHITECTURE.md, docs/adr/.

### `/sentinel/` — new page

All claims sourced from the pkg-registry README and ARCHITECTURE.md.

- **Hero:** "Agents run `npm install` and execute code nobody read.
  Sentinel audits it first."
- **Problem:** zero risk signaling before install-time code runs; npm can't
  retract bad releases; name squatting.
- **Terminal demo:** the trojaned `color-stream@1.4.1` verdict panel
  (findings, score 0/100, BLOCK) ending in the 403 an installer receives.
- **How it works:** transparent auditing proxy in front of
  registry.npmjs.org; deterministic, reproducible scoring; policy +
  human-approval gate (the agent can request, never grant); sandbox
  enforcement (macOS Seatbelt / Linux bubblewrap) with runtime-violation
  quarantine; MCP server for agent hosts; GitHub Action tree gate; signed
  audit attestations; known-advisory + CVE (SCA) detection.
- **Status wording rule:** the README's phase numbering is internally
  inconsistent (its status paragraph says "Phases 1–14 built" while later
  sections document through Phase 22), so the site describes **capabilities,
  never phase numbers**.
- **Links:** GitHub repo, ARCHITECTURE.md, docs/adr/.

## Visual design

Keep the existing visual language exactly: dark background, monospace body,
sans headings, single orange accent, terminal blocks with `ok`/`bad`/
`comment` coloring. No per-project color themes — differentiation comes from
content. No new dependencies, no client-side JS.

## Content accuracy guardrails

- Every product claim on the site must be traceable to the project's own
  README, ARCHITECTURE.md, or docs — no invented capabilities or numbers.
- Terminal snippets are real (possibly condensed) output from the repos'
  READMEs/demos, not fabricated transcripts.
- Statuses: git.agentic "v1.0 shipped · v1.1 in progress"; src-control
  "Phases 1–9 implemented"; Sentinel capability-based wording (see above).

## Build, verification, process

- All work in a `.worktrees/website-suite/` worktree (repo discipline).
- Verification: `npm run build` in `website/` passes; manual pass over all
  internal links across the four pages; browser check of the built output
  (desktop + narrow viewport, since Base.astro has a 540px breakpoint).
- Deploy is unchanged (`website/scripts/deploy.sh`); Astro emits
  `dist/git-agentic/index.html` etc., which nginx serves as static files.
  No redirects needed: existing inbound links to `/` keep working.

## Out of scope

- Web UI for the products themselves (ADR-0001 Decision 9 stands).
- Blog integration, analytics, hosted docs.
- Any change to the three repos' READMEs (the Sentinel README's internal
  phase inconsistency is noted but fixed in its own repo, not here).
