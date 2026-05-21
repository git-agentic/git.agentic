# git.agentic website

Static landing page for git-agentic.com (canonical), with .dev and .org redirecting to .com.

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

Deployed to bergholm.net via `scripts/deploy.sh` from the repo root, which
rsyncs `website/dist/` to `/var/www/git-agentic.com/` and reloads nginx if
the config has changed.

See `scripts/deploy.sh` and `deploy/nginx/git-agentic.conf` for the
serving setup.
