#!/usr/bin/env bash
# Deploy the static site.
#
# Usage: ./scripts/deploy.sh
#
# Requires a .env file in the website/ directory (copy from .env.example).
#
# Assumes:
#   - npm and astro are available locally
#   - SSH access to $SSH_TARGET on port $SSH_PORT is configured
#   - /var/www/git-agentic.com exists on the server (idempotent: created on first run)
#   - /etc/nginx/sites-available/git-agentic exists on the server (one-time bootstrap; see deploy/nginx/git-agentic.conf)

set -euo pipefail

cd "$(dirname "$0")/.."

if [[ -f .env ]]; then
  # shellcheck source=/dev/null
  source .env
fi

: "${SSH_TARGET:?Set SSH_TARGET in website/.env (see .env.example)}"
: "${SSH_PORT:=22}"
DOCROOT="/var/www/git-agentic.com"

echo "→ Building Astro site..."
npm run build >/dev/null

# Astro's static build leaves some unused SSR scaffolding (chunks/, pages/,
# manifest_*.mjs, renderers.mjs, _noop-middleware.mjs). Strip them — the
# rendered HTML is fully self-contained.
echo "→ Pruning unused SSR artifacts from dist/..."
rm -rf dist/chunks dist/pages dist/manifest_*.mjs dist/renderers.mjs dist/_noop-middleware.mjs 2>/dev/null || true

echo "→ Ensuring docroot on server..."
ssh -p "$SSH_PORT" "$SSH_TARGET" "sudo install -d -o www-data -g www-data -m 0755 $DOCROOT"

echo "→ Syncing dist/ → $SSH_TARGET:$DOCROOT ..."
rsync -avz --delete \
  -e "ssh -p $SSH_PORT" \
  --rsync-path='sudo rsync' \
  --chown=www-data:www-data \
  dist/ "$SSH_TARGET:$DOCROOT/"

echo "→ Reloading nginx (config test first)..."
ssh -p "$SSH_PORT" "$SSH_TARGET" "sudo nginx -t && sudo systemctl reload nginx"

echo "✓ Deployed."
