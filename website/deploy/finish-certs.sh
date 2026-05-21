#!/usr/bin/env bash
# Idempotent: issue any missing certs for git-agentic.{com,dev,org} and
# swap nginx to the TLS-enabled config once they are in place.
#
# Lives at /usr/local/bin/finish-git-agentic-certs.sh on bergholm.net.
# Safe to run repeatedly — it skips work that's already done.

set -uo pipefail

LOG=/var/log/git-agentic-finalize.log
exec >>"$LOG" 2>&1
echo
echo "=== $(date -u +%FT%TZ) run ==="

EMAIL=toni.bergholm@gmail.com
WEBROOT=/var/www/certbot

issue() {
  local primary=$1; shift
  local args=()
  for d in "$@"; do args+=(-d "$d"); done
  if [ -f "/etc/letsencrypt/live/$primary/fullchain.pem" ]; then
    echo "[$primary] already exists, skipping"
    return 0
  fi
  echo "[$primary] requesting cert for: $*"
  certbot certonly --webroot -w "$WEBROOT" --non-interactive \
    --agree-tos -m "$EMAIL" --key-type ecdsa --no-eff-email \
    "${args[@]}"
}

issue git-agentic.com  git-agentic.com  www.git-agentic.com  || true
issue git-agentic.dev  git-agentic.dev  www.git-agentic.dev  || true
# .org: try with www first; fall back to apex-only if www still on parking
if [ ! -f /etc/letsencrypt/live/git-agentic.org/fullchain.pem ]; then
  certbot certonly --webroot -w "$WEBROOT" --non-interactive \
    --agree-tos -m "$EMAIL" --key-type ecdsa --no-eff-email \
    -d git-agentic.org -d www.git-agentic.org \
    || certbot certonly --webroot -w "$WEBROOT" --non-interactive \
         --agree-tos -m "$EMAIL" --key-type ecdsa --no-eff-email \
         -d git-agentic.org \
    || true
fi

have() { [ -f "/etc/letsencrypt/live/$1/fullchain.pem" ]; }

CONF=/etc/nginx/sites-available/git-agentic
TMP=$(mktemp)

cat >"$TMP" <<'EOF'
# /etc/nginx/sites-available/git-agentic — managed by finish-git-agentic-certs.sh
#
# git-agentic.com → canonical
# git-agentic.dev/.org → 301 to https://git-agentic.com

# ---- port 80: ACME challenge + http→https redirect / apex bootstrap --------

server {
    listen 80;
    listen [::]:80;
    server_name git-agentic.com www.git-agentic.com;

    location /.well-known/acme-challenge/ {
        root /var/www/certbot;
    }
    location / {
__COM_PORT80_BODY__
    }
}

server {
    listen 80;
    listen [::]:80;
    server_name git-agentic.dev www.git-agentic.dev
                git-agentic.org www.git-agentic.org;

    location /.well-known/acme-challenge/ {
        root /var/www/certbot;
    }
    location / {
        return 301 https://git-agentic.com$request_uri;
    }
}
EOF

# .com port 80 behavior: redirect to https if cert is present, else serve directly.
if have git-agentic.com; then
  sed -i 's#__COM_PORT80_BODY__#        return 301 https://git-agentic.com$request_uri;#' "$TMP"
else
  sed -i 's#__COM_PORT80_BODY__#        root /var/www/git-agentic.com;\n        try_files $uri $uri/ $uri.html =404;\n        index index.html;#' "$TMP"
fi

# .com TLS server
if have git-agentic.com; then
  cat >>"$TMP" <<'EOF'

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name git-agentic.com www.git-agentic.com;

    ssl_certificate     /etc/letsencrypt/live/git-agentic.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/git-agentic.com/privkey.pem;
    include /etc/letsencrypt/options-ssl-nginx.conf;
    ssl_dhparam /etc/letsencrypt/ssl-dhparams.pem;

    # apex serves; www redirects to apex for canonicalization
    if ($host = www.git-agentic.com) {
        return 301 https://git-agentic.com$request_uri;
    }

    root /var/www/git-agentic.com;
    index index.html;

    location ~* \.(svg|png|jpg|jpeg|webp|woff2|css|js|mjs)$ {
        expires 30d;
        add_header Cache-Control "public, max-age=2592000, immutable" always;
    }
    location / {
        try_files $uri $uri/ $uri.html =404;
        add_header Cache-Control "public, max-age=300, must-revalidate" always;
    }
}
EOF
fi

# .dev TLS redirect
if have git-agentic.dev; then
  cat >>"$TMP" <<'EOF'

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name git-agentic.dev www.git-agentic.dev;

    ssl_certificate     /etc/letsencrypt/live/git-agentic.dev/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/git-agentic.dev/privkey.pem;
    include /etc/letsencrypt/options-ssl-nginx.conf;
    ssl_dhparam /etc/letsencrypt/ssl-dhparams.pem;

    return 301 https://git-agentic.com$request_uri;
}
EOF
fi

# .org TLS redirect
if have git-agentic.org; then
  cat >>"$TMP" <<'EOF'

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name git-agentic.org www.git-agentic.org;

    ssl_certificate     /etc/letsencrypt/live/git-agentic.org/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/git-agentic.org/privkey.pem;
    include /etc/letsencrypt/options-ssl-nginx.conf;
    ssl_dhparam /etc/letsencrypt/ssl-dhparams.pem;

    return 301 https://git-agentic.com$request_uri;
}
EOF
fi

# Install if changed.
if ! cmp -s "$TMP" "$CONF"; then
  cp "$CONF" "$CONF.bak.$(date +%Y%m%d-%H%M%S)"
  install -m 0644 "$TMP" "$CONF"
  if nginx -t; then
    systemctl reload nginx
    echo "nginx reloaded with updated config"
  else
    echo "nginx -t failed; restoring backup"
    cp "$CONF.bak."* "$CONF" 2>/dev/null
    rm -f "$TMP"
    exit 1
  fi
else
  echo "config unchanged"
fi
rm -f "$TMP"

echo "certs present:"
for d in git-agentic.com git-agentic.dev git-agentic.org; do
  if have "$d"; then echo "  ✓ $d"; else echo "  ✗ $d (missing)"; fi
done

# If everything's done, disarm any scheduled retries.
if have git-agentic.com && have git-agentic.dev && have git-agentic.org; then
  systemctl list-timers --all 2>/dev/null | grep -q "git-agentic-finalize" && \
    systemctl disable --now "$(systemctl list-timers --all | awk '/git-agentic-finalize/{print $NF; exit}').timer" 2>/dev/null || true
  echo "all certs present — finalize complete"
fi
