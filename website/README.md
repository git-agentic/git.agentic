# git.agentic website

Static site for git-agentic.com (canonical), with .dev and .org redirecting to .com. Routes (including `/learn` and four nested articles; see [DEPLOY.md](DEPLOY.md) for the full inventory):

- `/` — suite landing page
- `/git-agentic` — git.agentic product page
- `/src-control` — src-control product page
- `/sentinel` — Sentinel product page

## Develop

```bash
cd website
npm install
npm run dev          # http://localhost:4321
```

## Build

```bash
npm run build        # writes static HTML/CSS to dist/
```

## Deploy

Cloudflare Workers Static Assets deployment and manual cutover instructions are in
[DEPLOY.md](DEPLOY.md). The legacy nginx deployment remains available for rollback.
