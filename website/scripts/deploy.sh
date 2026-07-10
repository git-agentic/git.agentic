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
# GNU rsync (>= 3.1.0) supports --chown; macOS's bundled openrsync does not.
# Probe the capability; without it, fix ownership server-side after the sync.
# Scalar (not array) on purpose: macOS bash 3.2 errors on empty-array
# expansion under `set -u`.
CHOWN_FLAG=""
if rsync --help 2>&1 | grep -q -- '--chown'; then
  CHOWN_FLAG="--chown=www-data:www-data"
fi

rsync -avz --delete \
  -e "ssh -p $SSH_PORT" \
  --rsync-path='sudo rsync' \
  ${CHOWN_FLAG:+"$CHOWN_FLAG"} \
  dist/ "$SSH_TARGET:$DOCROOT/"

if [[ -z "$CHOWN_FLAG" ]]; then
  echo "→ Local rsync lacks --chown (openrsync); fixing ownership server-side..."
  ssh -p "$SSH_PORT" "$SSH_TARGET" "sudo chown -R www-data:www-data $DOCROOT"
fi

echo "→ Reloading nginx (config test first)..."
ssh -p "$SSH_PORT" "$SSH_TARGET" "sudo nginx -t && sudo systemctl reload nginx"

echo "✓ Deployed."
