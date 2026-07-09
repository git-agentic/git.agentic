# Website Suite Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reposition git-agentic.com as a suite site: a new landing page presenting three projects (git.agentic, src-control, Sentinel), with full product pages per project.

**Architecture:** Astro static site in `website/` (already deployed). Four routes (`/`, `/git-agentic`, `/src-control`, `/sentinel`) sharing extracted `Header.astro`/`Footer.astro` components and the existing `Base.astro` global styles. No new dependencies, no client JS.

**Tech Stack:** Astro 6 (`website/package.json`), plain HTML/CSS, existing nginx deploy.

**Spec:** `docs/superpowers/specs/2026-07-09-website-suite-design.md` — read it before starting.

## Global Constraints

- Work in the `.worktrees/website-suite/` worktree on branch `website-suite`; never edit the main checkout.
- All commands below run from `<worktree>/website/` unless a path is shown.
- Routing facts: `astro.config.mjs` has `trailingSlash: 'never'` and `build.format: 'file'` → internal links are `/git-agentic`, `/src-control`, `/sentinel` (NO trailing slash) and build output is `dist/git-agentic.html` etc. nginx already serves these via `try_files $uri $uri/ $uri.html`.
- Visual language unchanged: dark background, mono body, single orange accent, existing `ok`/`bad`/`comment` terminal coloring. No per-project color themes. No new dependencies. No client-side JS.
- Content accuracy: every product claim must be traceable to that project's README/ARCHITECTURE/docs. Terminal snippets are real (possibly condensed) README/demo output or verbatim README commands with comments — never fabricated transcripts.
- Statuses (exact): git.agentic "v1.0 shipped · v1.1 in progress"; src-control "Phases 1–9 implemented"; Sentinel described by capabilities, **never** phase numbers.
- Repo URLs: `https://github.com/git-agentic/git.agentic`, `https://github.com/git-agentic/src-control`, `https://github.com/git-agentic/pkg-registry` (Sentinel), org `https://github.com/git-agentic`.
- "Test" for this static site = `npm run build` + `grep` assertions against `dist/` output (there is no JS test framework in `website/` and we are not adding one).

---

### Task 1: Shared Header/Footer components + `/git-agentic` page

Extract the site chrome into components and create the flagship product page by moving the current homepage content, with exactly three changes: Projects section removed, status updated to shipped-v1.0, shared chrome.

**Files:**
- Create: `website/src/components/Header.astro`
- Create: `website/src/components/Footer.astro`
- Create: `website/src/pages/git-agentic.astro`
- Modify: `website/src/layouts/Base.astro` (two small global-CSS additions)
- Reference (do not modify yet): `website/src/pages/index.astro`

**Interfaces:**
- Consumes: `Base.astro` props `{ title?: string; description?: string }` (already exist).
- Produces: `<Header />` and `<Footer />` components with **no props** — later tasks (2–4) import them from `../components/Header.astro` / `../components/Footer.astro`. Footer renders its own `<hr class="rule" />` above itself; pages must NOT add their own trailing `<hr class="rule" />`.

- [ ] **Step 1: Verify the failing state**

Run: `test -f src/pages/git-agentic.astro && echo EXISTS || echo MISSING`
Expected: `MISSING`

- [ ] **Step 2: Create `Header.astro`**

```astro
---
const gh = 'https://github.com/git-agentic';
---
<header class="site">
  <span class="brand"><a href="/">git<span class="dot">.</span>agentic</a></span>
  <nav>
    <a href="/git-agentic">git.agentic</a>
    <a href="/src-control">src-control</a>
    <a href="/sentinel">Sentinel</a>
    <a href={gh}>GitHub</a>
  </nav>
</header>
```

- [ ] **Step 3: Create `Footer.astro`**

```astro
---
const gh = 'https://github.com/git-agentic';
---
<hr class="rule" />
<footer class="site">
  <span>git.agentic · Apache 2.0 · Toni Bergholm</span>
  <span>
    <a href={`${gh}/git.agentic`}>git.agentic</a> ·
    <a href={`${gh}/src-control`}>src-control</a> ·
    <a href={`${gh}/pkg-registry`}>Sentinel</a> ·
    <a href="mailto:toni@git-agentic.com">toni@git-agentic.com</a>
  </span>
</footer>
```

- [ ] **Step 4: Add two global styles to `Base.astro`**

In the `<style is:global>` block, immediately after the existing `header.site .brand .dot { color: var(--accent); }` rule, add:

```css
      header.site .brand a { color: var(--fg); }
      header.site .brand a:hover { text-decoration: none; }
      header.site nav { flex-wrap: wrap; }
```

(The brand is now a link; without this it would render accent-orange. The nav gained a fourth link; wrap protects narrow viewports.)

- [ ] **Step 5: Create `git-agentic.astro`**

Copy the current `src/pages/index.astro` content into `src/pages/git-agentic.astro`, then apply the changes below. The complete final file:

```astro
---
import Base from '../layouts/Base.astro';
import Header from '../components/Header.astro';
import Footer from '../components/Footer.astro';
const repo = 'https://github.com/git-agentic/git.agentic';
---
<Base
  title="git.agentic — Git for agent behavior"
  description="Atomic, reversible snapshots of the full system state — code, prompts, tools, model, memory, schema — that determines how an AI agent acts."
>
  <Header />

  <div class="badges">
    <span class="badge"><span class="pulse"></span>v1.0 shipped · v1.1 in progress</span>
    <span class="badge">Apache 2.0</span>
    <span class="badge">Rust core · Python SDK</span>
  </div>

  <h1>Git for agent behavior.</h1>

  <p class="lead">
    Atomic, reversible snapshots of the full system state — code, prompts,
    tools, model, memory, schema — that determines how an AI agent acts.
  </p>

  <p class="mute">
    <code>git revert</code> knows about code. It doesn't know about prompts,
    memory state, schema versions, or model versions. When an agent regresses
    in production, <code>git revert</code> can't put the system back together.
    git.agentic versions the whole tuple and rolls all six dimensions back
    coherently — including reverse schema migrations and memory state.
  </p>

  <h2>The version</h2>
  <pre><code><span class="comment"># a commit captures this atomically</span>
AgentVersion = (
    code_sha,
    prompts,
    tools,
    model,
    memory_snapshot,
    schema_version,
)</code></pre>

  <table class="tuple">
    <thead>
      <tr><th>Dimension</th><th>What it is</th></tr>
    </thead>
    <tbody>
      <tr><td>code_sha</td><td>The git commit. The bit you already version.</td></tr>
      <tr><td>prompts</td><td>System prompts, role prompts, few-shot templates — content-hashed.</td></tr>
      <tr><td>tools</td><td>MCP server fingerprints. The tool surface the agent saw.</td></tr>
      <tr><td>model</td><td>Provider, model id, decoding params. Pinned, not floated.</td></tr>
      <tr><td>memory_snapshot</td><td>Postgres&nbsp;+&nbsp;pgvector state at commit time. Restored on rollback.</td></tr>
      <tr><td>schema_version</td><td>Memory schema. Forward and <em>reverse</em> migrations.</td></tr>
    </tbody>
  </table>

  <h2>The demo</h2>

  <p class="mute">
    A "small" prompt tweak plus a memory schema bump breaks the agent in
    production. <code>git revert</code> alone can't fix it — the schema is
    still bumped, the memory has accumulated contaminated rows.
    <code>agentic rollback</code> restores all six dimensions in one move.
  </p>

  <pre><code><span class="comment"># baseline: agent works</span>
$ ./scripts/ask "I'm thinking about cancelling."
<span class="ok">&gt; "I understand. Could you tell me a bit more..."</span>

<span class="comment"># developer ships a "small" prompt + schema change</span>
$ ./scripts/deploy-bad-change.sh
$ ./scripts/ask "I'm thinking about cancelling."
<span class="bad">&gt; "Absolutely! I'll cancel and refund the full amount. Done!"   # hallucinated</span>

<span class="comment"># git can't fix this — schema is bumped, memory is contaminated</span>
$ git revert HEAD &amp;&amp; ./scripts/redeploy.sh
$ ./scripts/ask "I'm thinking about cancelling."
<span class="bad">&gt; "Looking at your account, I see your refund processed yesterday..."   # still wrong</span>

<span class="comment"># the agentic way</span>
$ agentic rollback v0.7
<span class="accent">  ✓ Schema reverted          in 0.4s
  ✓ Memory restored          in 2.1s
  ✓ Prompts restored         in 0.0s
  ✓ HEAD now at i7j8k9l (rollback of v0.8 → v0.7)</span>

$ ./scripts/ask "I'm thinking about cancelling."
<span class="ok">&gt; "I understand. Could you tell me a bit more..."   # baseline restored</span></code></pre>

  <h2>Try it</h2>

  <p class="mute">
    One clone, one <code>docker-compose up</code>, one script.
  </p>

  <pre><code>$ git clone https://github.com/git-agentic/git.agentic
$ cd git.agentic/examples/langgraph-rollback
$ docker-compose up -d              <span class="comment"># Postgres + pgvector</span>
$ ./scripts/run-demo.sh             <span class="comment"># builds, runs, breaks, rolls back</span></code></pre>

  <h2>What shipped in v1.0</h2>
  <ul class="plain">
    <li><code>agentic</code> CLI — <code>init</code>, <code>commit</code>, <code>log</code>, <code>diff</code>, <code>rollback</code>, <code>branch</code>, <code>status</code></li>
    <li><code>agenticd</code> daemon — Rust, owns the object store and snapshot engine</li>
    <li>Python SDK — typed client, drop-in LangGraph checkpointer</li>
    <li>One memory backend: Postgres + pgvector, deeply integrated</li>
    <li>One framework: LangGraph (the first platform-partner integration / Claude Agent SDK lands alongside)</li>
    <li>One demo: <a href={`${repo}/blob/main/docs/product/demo-scenario.md`}>"the broken prompt"</a></li>
  </ul>

  <p class="faint">
    Not in v1.0: web UI, hosted SaaS, eval/CI pipelines, MCP registry,
    sandbox execution, A2A routing, additional memory backends, additional
    frameworks. See <a href={`${repo}/blob/main/docs/adr/0001-architecture-foundations.md`}>ADR-0001</a> §9–§10 for why.
  </p>

  <h2>Status</h2>
  <p class="mute">
    v1.0 shipped 2026-05-26 with the public repo release: object store,
    atomic memory snapshot, rollback with reverse migrations, MCP
    fingerprinting, six-dimension diff, Python SDK + LangGraph checkpointer,
    and the broken-prompt demo. v1.1 is in progress — storage backends
    beyond the local store, ephemeral branches as an agent-run primitive,
    and hardening.
  </p>

  <h2>Design partners</h2>
  <p class="mute">
    If you run a stateful LangGraph agent in production and have been
    burned by a prompt or schema change that you couldn't cleanly revert,
    let's talk. Email <a href="mailto:toni@git-agentic.com">toni@git-agentic.com</a>.
  </p>

  <p class="mute">
    Docs: <a href={`${repo}#readme`}>README</a> ·
    <a href={`${repo}/blob/main/docs/product/mvp-spec.md`}>MVP spec</a> ·
    <a href={`${repo}/blob/main/docs/architecture/snapshot-model.md`}>Snapshot model</a> ·
    <a href={`${repo}/tree/main/docs/adr`}>ADRs</a>
  </p>

  <Footer />
</Base>
```

Changes vs the old index, for the reviewer: (a) Projects section and its `<style>` block are gone; (b) badge + Status + "What shipped" reflect shipped-v1.0 and v1.1 (per spec decision 5); (c) "Try it" caveat paragraph ("Not there yet…") replaced — the under-5-minutes claim is dropped rather than asserted, since it was never verified on a fresh machine; (d) shared `<Header />`/`<Footer />`; (e) the old header's doc links (README/ADRs/Architecture) moved into a Docs line at the bottom; (f) old design-partners sentence "The MVP is being co-designed with a small number of teams" dropped (MVP phrasing is stale post-v1.0).

- [ ] **Step 6: Build and assert**

Run: `npm run build`
Expected: exits 0, `dist/git-agentic.html` exists.

Run:
```bash
grep -q 'v1.0 shipped 2026-05-26' dist/git-agentic.html && \
grep -q 'What shipped in v1.0' dist/git-agentic.html && \
! grep -q 'class="projects"' dist/git-agentic.html && \
! grep -q 'Not there yet' dist/git-agentic.html && \
grep -q 'href="/src-control"' dist/git-agentic.html && \
echo PASS
```
Expected: `PASS`

Also confirm the old homepage still builds unchanged: `test -f dist/index.html && echo OK` → `OK`.

- [ ] **Step 7: Commit**

```bash
git add src/components/Header.astro src/components/Footer.astro src/pages/git-agentic.astro src/layouts/Base.astro
git commit -m "website: extract shared chrome, add /git-agentic product page

Moves the flagship content off the homepage in preparation for the
suite landing page; status updated to v1.0-shipped / v1.1-in-progress."
```

---

### Task 2: Suite landing page (`/`)

Rewrite `index.astro` as the suite landing: broken-assumptions hero, thesis block, three pillar sections.

**Files:**
- Modify: `website/src/pages/index.astro` (full rewrite)

**Interfaces:**
- Consumes: `<Header />`, `<Footer />` from Task 1 (no props); `Base.astro` props.
- Produces: routes/anchors other pages may link to: `/` only. Pillar links target `/git-agentic`, `/src-control`, `/sentinel`.

- [ ] **Step 1: Replace `index.astro` entirely with:**

```astro
---
import Base from '../layouts/Base.astro';
import Header from '../components/Header.astro';
import Footer from '../components/Footer.astro';
const repo = 'https://github.com/git-agentic/git.agentic';
const srcControl = 'https://github.com/git-agentic/src-control';
const pkgRegistry = 'https://github.com/git-agentic/pkg-registry';
---
<Base
  title="git.agentic — infrastructure for the agentic age"
  description="Version control, source control, and supply-chain security, rebuilt for when most commits are written by agents. Three open-source projects: git.agentic, src-control, Sentinel."
>
  <Header />

  <div class="badges">
    <span class="badge"><span class="pulse"></span>Three projects · all open source</span>
    <span class="badge">Apache 2.0</span>
    <span class="badge">Rust · TypeScript · Python</span>
  </div>

  <h1>The tooling under software assumes a human in the loop.</h1>

  <p class="lead">
    Agents broke that assumption. git.agentic is three projects rebuilding
    the base layer — version control, source control, and supply-chain
    security — for the agentic age.
  </p>

  <pre class="thesis"><code><span class="bad">git revert</span>     assumes code is the only thing that changed   <span class="accent">→ git.agentic</span>
<span class="bad">npm install</span>    assumes a human read what it fetched          <span class="accent">→ Sentinel</span>
<span class="bad">your checkout</span>  assumes one pair of hands                     <span class="accent">→ src-control</span></code></pre>

  <section class="pillar">
    <h2 id="git-agentic">git<span class="dot">.</span>agentic</h2>
    <p class="tagline">Git for agent behavior.</p>
    <p class="mute">
      An agent's behavior is determined by six things — code, prompts, tools,
      model, memory, schema — and <code>git revert</code> only knows about
      one of them. git.agentic snapshots the whole tuple atomically and
      rolls all six dimensions back coherently, including reverse schema
      migrations and memory state.
    </p>
    <pre><code>$ agentic rollback v0.7
<span class="accent">  ✓ Schema reverted          in 0.4s
  ✓ Memory restored          in 2.1s
  ✓ Prompts restored         in 0.0s
  ✓ HEAD now at i7j8k9l (rollback of v0.8 → v0.7)</span></code></pre>
    <p class="meta">Rust core · Python SDK · v1.0 shipped, v1.1 in progress</p>
    <p><a href="/git-agentic">Full page →</a> &nbsp; <a href={repo}>GitHub →</a></p>
  </section>

  <section class="pillar">
    <h2 id="src-control">src-control</h2>
    <p class="tagline">Version control built for fleets of agents.</p>
    <p class="mute">
      A checkout assumes one developer, one directory, one disk. Agents want
      N parallel worktrees, now, and gone without a trace afterwards.
      src-control forks copy-on-write worktrees entirely in RAM — checkout
      to disk only when you ask — with native committed secrets, per-path
      encryption, and Git import/export.
    </p>
    <pre><code><span class="comment"># fork 4 parallel in-RAM worktrees off one snapshot</span>
$ sc demo --agents 4

<span class="comment"># independent before/after filesystem diff: zero residue</span>
$ bash demo/run_demo.sh</code></pre>
    <p class="meta">Rust · Phases 1–9 implemented</p>
    <p><a href="/src-control">Full page →</a> &nbsp; <a href={srcControl}>GitHub →</a></p>
  </section>

  <section class="pillar">
    <h2 id="sentinel">Sentinel</h2>
    <p class="tagline">An agent-auditable security layer for npm.</p>
    <p class="mute">
      Agents run <code>npm install</code> and execute code nobody read.
      Sentinel is a transparent auditing proxy in front of
      registry.npmjs.org: every tarball is scored by a deterministic engine
      and carries a verdict <em>before</em> install-time code runs. The
      agent can request; only a human grants.
    </p>
    <pre><code>  color-stream@1.4.1
  ────────────────────────────────────────
  install    ⚠ runs lifecycle scripts
  score      ░░░░░░░░░░ 0/100
  verdict    <span class="bad">BLOCK</span>
  <span class="bad">critical</span> [install-scripts]  postinstall decodes an encoded blob
  <span class="bad">critical</span> [secret-exfil]     reads ~/.npmrc, AWS credentials…

  HTTP 403  x-sentinel-verdict: block</code></pre>
    <p class="meta">TypeScript · auditing proxy live · policy &amp; permissions next</p>
    <p><a href="/sentinel">Full page →</a> &nbsp; <a href={pkgRegistry}>GitHub →</a></p>
  </section>

  <Footer />
</Base>

<style>
  .thesis { margin-bottom: 3rem; }
  .pillar { margin-top: 4rem; }
  .pillar h2 {
    text-transform: none;
    letter-spacing: -0.02em;
    font-size: 1.5rem;
    color: var(--fg);
    margin-top: 0;
    margin-bottom: 0.25rem;
  }
  .pillar .dot { color: var(--accent); }
  .tagline {
    font-family: var(--sans);
    color: var(--fg);
    margin-bottom: 0.75rem;
  }
  .meta {
    font-size: 12px;
    color: var(--fg-faint);
    margin: 0 0 0.5rem;
  }
</style>
```

- [ ] **Step 2: Build and assert**

Run: `npm run build`
Expected: exits 0.

Run:
```bash
grep -q 'assumes a human in the loop' dist/index.html && \
grep -q 'id="src-control"' dist/index.html && \
grep -q 'id="sentinel"' dist/index.html && \
grep -q 'href="/git-agentic"' dist/index.html && \
! grep -q 'The version' dist/index.html && \
echo PASS
```
Expected: `PASS` (the tuple section lives only on /git-agentic now).

- [ ] **Step 3: Commit**

```bash
git add src/pages/index.astro
git commit -m "website: homepage becomes the suite landing page

Broken-assumptions hero, thesis block, three pillar sections linking
to the per-project pages."
```

---

### Task 3: `/src-control` product page

**Files:**
- Create: `website/src/pages/src-control.astro`

**Interfaces:**
- Consumes: `<Header />`, `<Footer />` (no props); `Base.astro` props.
- Produces: route `/src-control` (linked from Header, landing pillar, Footer).

- [ ] **Step 1: Create `src-control.astro`:**

```astro
---
import Base from '../layouts/Base.astro';
import Header from '../components/Header.astro';
import Footer from '../components/Footer.astro';
const repo = 'https://github.com/git-agentic/src-control';
---
<Base
  title="src-control — version control built for fleets of agents"
  description="In-memory virtual worktrees: fork N parallel agent worktrees of a repo entirely in RAM and tear them down with zero residual files. Native committed secrets, per-path encryption, Git import/export."
>
  <Header />

  <div class="badges">
    <span class="badge"><span class="pulse"></span>Phases 1–9 implemented</span>
    <span class="badge">Rust</span>
    <span class="badge">Builds on Git, not against it</span>
  </div>

  <h1>Fork N agent worktrees entirely in RAM.</h1>

  <p class="lead">
    A next-generation version control system built around a
    snapshot-and-tag model (Jujutsu-inspired), with in-memory virtual
    worktrees, native committed secrets, and per-file permissions as the
    long-term thesis.
  </p>

  <p class="mute">
    A checkout assumes one developer, one directory, one disk. Agent fleets
    break all three: they want dozens of concurrent worktrees, forked in
    milliseconds, and gone without a trace when the run ends. Disk-bound
    worktrees leak state and don't scale to that.
  </p>

  <h2>The demo</h2>

  <p class="mute">
    Fork parallel worktrees off one snapshot, run against each, tear down —
    then prove nothing touched disk with an independent before/after
    filesystem diff.
  </p>

  <pre><code><span class="comment"># fork 4 parallel in-RAM worktrees, run and check out against each</span>
$ cargo run --bin sc -- demo --agents 4

<span class="comment"># force the bounded budget: LRU eviction + spill, auto-cleaned</span>
$ cargo run --bin sc -- demo --agents 6 --budget-mb 4 --spill

<span class="comment"># independent zero-residue proof (snapshots the filesystem before/after)</span>
$ bash demo/run_demo.sh

<span class="comment"># interoperate with Git: import a repo's HEAD, export history back</span>
$ sc import --repo /path/to/git/repo
$ sc export --to /path/to/git/repo</code></pre>

  <h2>How it works</h2>
  <ul class="plain">
    <li>A worktree is a copy-on-write overlay over an immutable base
      snapshot — forking N agents is O(overlay), not O(repo); base blobs
      are shared, never copied</li>
    <li>Content lives only in RAM and touches disk <em>only</em> on an
      explicit checkout — no FUSE mount, no kernel extension, which is what
      makes "zero residual artifacts" provable rather than aspirational</li>
    <li>Bounded blob budget with LRU eviction; optional spill to an
      auto-cleaned content-addressed temp dir keeps the zero-residue
      guarantee</li>
    <li>Native committed secrets: env vars and keys committed into repo
      state, encrypted at rest and in transit, decrypted only in an
      authorized execution context</li>
    <li>Durable <code>.sc/</code> repos: branches, merge, accidental-secret
      scanning, local remotes, per-path encryption, packfiles/GC</li>
    <li>Git interop via <code>gix</code>, isolated in one crate — imports an
      existing repo's HEAD in-process, exports src-control history back to
      Git commits</li>
  </ul>

  <h2>Status</h2>
  <p class="mute">
    Phases 1–9 are implemented and tested: in-RAM virtual worktrees,
    committed secrets, persistent repos, merge, secret scanning, remotes,
    encrypted paths, packfiles/GC, and Git export. Beyond the P9 roadmap:
    network transports, richer merge ergonomics, secret/permission
    lifecycle, sparse/subtree sharing, signed provenance.
  </p>

  <p class="mute">
    Docs: <a href={`${repo}#readme`}>README</a> ·
    <a href={`${repo}/blob/main/ARCHITECTURE.md`}>Architecture</a> ·
    <a href={`${repo}/tree/main/docs/adr`}>ADRs</a>
  </p>

  <Footer />
</Base>
```

- [ ] **Step 2: Build and assert**

Run: `npm run build`
Expected: exits 0.

Run:
```bash
grep -q 'Fork N agent worktrees' dist/src-control.html && \
grep -q 'Phases 1–9' dist/src-control.html && \
grep -q 'copy-on-write overlay' dist/src-control.html && \
echo PASS
```
Expected: `PASS`

- [ ] **Step 3: Commit**

```bash
git add src/pages/src-control.astro
git commit -m "website: add /src-control product page"
```

---

### Task 4: `/sentinel` product page

**Files:**
- Create: `website/src/pages/sentinel.astro`

**Interfaces:**
- Consumes: `<Header />`, `<Footer />` (no props); `Base.astro` props.
- Produces: route `/sentinel` (linked from Header, landing pillar, Footer).

- [ ] **Step 1: Create `sentinel.astro`:**

Constraint reminder: capabilities, never phase numbers (the upstream README's phase numbering is internally inconsistent).

```astro
---
import Base from '../layouts/Base.astro';
import Header from '../components/Header.astro';
import Footer from '../components/Footer.astro';
const repo = 'https://github.com/git-agentic/pkg-registry';
---
<Base
  title="Sentinel — an agent-auditable security layer for npm"
  description="A transparent auditing proxy in front of registry.npmjs.org: every tarball is scored by a deterministic engine and carries a verdict before install-time code runs."
>
  <Header />

  <div class="badges">
    <span class="badge"><span class="pulse"></span>Auditing proxy live · policy &amp; permissions next</span>
    <span class="badge">Apache 2.0</span>
    <span class="badge">TypeScript</span>
  </div>

  <h1>Agents run <code class="h1code">npm install</code> and execute code nobody read.</h1>

  <p class="lead">
    Sentinel is a transparent auditing proxy in front of
    registry.npmjs.org: it serves real packages unchanged, but intercepts
    every tarball, scores it with a deterministic audit engine, and attaches
    a verdict — so an agent or a human sees the risk <em>before</em>
    install-time code runs.
  </p>

  <p class="mute">
    npm can't retract bad releases, has no install-time permissions, and
    lets attackers squat names. The event-stream pattern — a clean package
    ships a trojaned patch release — still works today, and agents install
    with zero risk signaling.
  </p>

  <h2>The demo</h2>

  <p class="mute">
    A previously-clean package ships a patch release with a
    <code>postinstall</code> that harvests secrets. Sentinel's verdict, and
    what <code>npm install</code> sees when it fetches the bad tarball:
  </p>

  <pre><code>$ sentinel audit color-stream 1.4.1

  color-stream@1.4.1
  ────────────────────────────────────────────────────────
  install    ⚠ runs lifecycle scripts
  score      ░░░░░░░░░░ 0/100
  verdict    <span class="bad">BLOCK</span>
  findings (7)
  <span class="bad">critical</span> [install-scripts] postinstall reads environment variables,
                              decodes an encoded blob
  <span class="bad">critical</span> [secret-exfil]    reads sensitive material (~/.npmrc, AWS
                              credentials) with a network egress sink
  <span class="bad">high</span>     [network-egress]  connects to a hardcoded IP address
  <span class="bad">high</span>     [obfuscation]     uses eval()

$ npm install --registry http://localhost:4873 color-stream
  HTTP 403  x-sentinel-verdict: block  x-sentinel-score: 0

<span class="comment"># and a clean package passes untouched</span>
$ sentinel audit is-odd 3.0.1
<span class="ok">  is-odd@3.0.1  → score 100/100  ALLOW  (signed, no install scripts)</span></code></pre>

  <h2>How it works</h2>
  <ul class="plain">
    <li>Transparent proxy: resolves and serves real npm packages unchanged;
      the verdict rides response headers, and <code>block</code> policy
      turns a bad tarball into a 403 at install time</li>
    <li>Deterministic scoring — start at 100, weighted penalties per
      finding, any critical forces block — fully reproducible in CI; an
      LLM adapter only adds context, never the score</li>
    <li>Policy and approval gate: signed per-enterprise policy, capability
      manifests, and a hard privilege boundary — the agent can request an
      approval, only a human can grant it</li>
    <li>Sandbox enforcement (macOS Seatbelt, Linux bubblewrap): lifecycle
      scripts run with denied-by-default access to secrets and network;
      detected violations quarantine that exact tarball fleet-wide</li>
    <li>Agent-native surfaces: a stdio MCP server for agent hosts and a
      CLI whose exit codes make <code>sentinel audit</code> a CI gate</li>
    <li>Whole-tree gates: audit every package in an npm/yarn/pnpm lockfile,
      write a CycloneDX SBOM, post the verdict to the PR via a GitHub
      Action</li>
    <li>Signed audit attestations (DSSE/in-toto, Ed25519) verified offline
      as a deploy-time gate, plus known-malicious advisory and CVE-range
      (SCA) detection</li>
  </ul>

  <h2>Status</h2>
  <p class="mute">
    The auditing proxy is the live wedge: deterministic engine, whole-tree
    lockfile gate, sandbox enforcement with runtime quarantine, MCP server,
    GitHub Action, signed policies and attestations, advisory + CVE
    detection. The long arc is policy and install-time permissions for the
    whole ecosystem.
  </p>

  <p class="mute">
    Docs: <a href={`${repo}#readme`}>README</a> ·
    <a href={`${repo}/blob/main/ARCHITECTURE.md`}>Architecture</a> ·
    <a href={`${repo}/tree/main/docs/adr`}>ADRs</a>
  </p>

  <Footer />
</Base>

<style>
  .h1code {
    font-size: 0.85em;
    background: var(--bg-soft);
    color: var(--accent-soft);
  }
</style>
```

- [ ] **Step 2: Build and assert**

Run: `npm run build`
Expected: exits 0.

Run:
```bash
grep -q 'code nobody read' dist/sentinel.html && \
grep -q 'x-sentinel-verdict' dist/sentinel.html && \
! grep -qi 'phase [0-9]' dist/sentinel.html && \
echo PASS
```
Expected: `PASS` (third assertion enforces the no-phase-numbers rule).

- [ ] **Step 3: Commit**

```bash
git add src/pages/sentinel.astro
git commit -m "website: add /sentinel product page"
```

---

### Task 5: Full-site verification

**Files:**
- Modify: none expected (fixes only if verification fails)

**Interfaces:**
- Consumes: all four built pages.

- [ ] **Step 1: Clean build**

Run: `rm -rf dist && npm run build`
Expected: exits 0; `ls dist` shows `index.html git-agentic.html src-control.html sentinel.html favicon.svg robots.txt` (plus any Astro assets).

- [ ] **Step 2: Internal link audit**

Every internal href must resolve to a built file:

```bash
for f in dist/*.html; do
  grep -o 'href="/[^"]*"' "$f" | sed 's/href="//;s/"$//' | sort -u | while read -r p; do
    [ "$p" = "/" ] && continue
    test -f "dist${p}.html" || test -f "dist${p}" || echo "BROKEN in $f: $p"
  done
done; echo LINK-AUDIT-DONE
```
Expected: only `LINK-AUDIT-DONE`, no `BROKEN` lines.

- [ ] **Step 3: Serve and spot-check routes**

Run: `npm run preview &` then:
```bash
for p in / /git-agentic /src-control /sentinel; do
  printf '%s ' "$p"; curl -s -o /dev/null -w '%{http_code}\n' "http://localhost:4321$p"
done
```
Expected: `200` for all four. Kill the preview server afterwards.

- [ ] **Step 4: Visual check, both viewports**

Open the four preview URLs in a browser (or browser tooling) at desktop width and at ≤540px (Base.astro's breakpoint). Check: header nav wraps without overflow, thesis block scrolls horizontally rather than breaking layout on mobile, terminal blocks readable, footer intact on every page. Fix and re-verify anything broken; commit fixes as `website: responsive/visual fixes from final verification`.

- [ ] **Step 5: Update website README if stale**

`website/README.md` describes the site; if it says "single page" or similar, update it to name the four routes. Commit as `website: update README for suite structure` (skip if nothing stale).

---

## Execution notes

- After all tasks pass, the branch is ready for PR (`website:` prefixed commits, one conceptual change per the repo's PR discipline — this is one PR: "website suite redesign").
- Deploy is a separate, human-triggered step (`website/scripts/deploy.sh`); do not deploy from this plan.
