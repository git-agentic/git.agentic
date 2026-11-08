# Cloudflare Workers Static Assets

The site is static Astro output; no Worker script or backend is needed.
DNS, domain attachment, redirect rules and the nginx host are manual cutover work.
Do not run the old rsync deployment during cutover.

## One-time setup

1. Choose the Cloudflare account and enable its workers.dev subdomain.
2. Create an API token restricted to that account with **Account / Workers Scripts / Edit**.
   No DNS permission is needed for this workflow; attach domains manually with an appropriately privileged dashboard login.
3. In GitHub repository Settings → Secrets and variables → Actions, add
   `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` (account ID from the Cloudflare dashboard).
   Do not commit either value.
4. From `website/`, run `npm ci`, `npm run build`, then an authorized initial
   `npx wrangler deploy` to create `git-agentic-site` before PR version uploads.
   The initial deployment only exposes workers.dev; this config contains no domain routes.
5. Enable Version/Preview URLs for the Worker (also declared with `preview_urls: true`).
   Main pushes affecting website files, root-level `blog/**` article sources, or the deployment workflow run `wrangler deploy`.
   Same-repository PRs run `wrangler versions upload --preview-alias pr-N`;
   the URL is printed in the Actions log and does not change the active deployment.
   When either secret is missing, PR preview upload skips with an Actions warning; production deploy still fails.
   Fork PRs build and check without secrets or preview uploads. Never use `pull_request_target` to execute PR code with credentials.

## Routes and local proof

`output: static`, `build.format: file`, and `trailingSlash: never` retain slashless canonical URLs.
The build script generates exact `_redirects` 200 rewrites from every built HTML file.
Wrangler `html_handling: none` prevents automatic canonical redirects; both slash variants serve identical HTML.
Unknown paths use `not_found_handling: 404-page`; there is no custom 404 in this site, so Workers returns its standard 404.
No SPA fallback is used.

The nine page routes are:

- `/`
- `/git-agentic`
- `/src-control`
- `/sentinel`
- `/learn`
- `/learn/why-git-revert-does-not-fix-ai-agent-regressions`
- `/learn/version-control-for-ai-agents`
- `/learn/six-dimensions-of-ai-agent-behavior`
- `/learn/version-control-layer-for-agentic-software`

The build also emits `/sitemap.xml`, `/rss.xml`, and public assets including robots.txt, llms.txt and security.txt.
Canonical tags and sitemap stay on `https://git-agentic.com`.

```sh
cd website
npm ci
npm run build
npm run check:seo
npx wrangler dev --port 8787
# In another terminal:
node scripts/check-routes.mjs
# After cutover, repeat against production:
node scripts/check-routes.mjs https://git-agentic.com
```

The check uses curl to verify every HTML output's canonical, slash and `.html` URL,
compares page bodies against the build, and requires zero redirects and 200 statuses.
It also checks feeds and missing paths (404).
HTML/default responses cache for five minutes with revalidation; hashed `/_astro/*` assets cache immutably for a year.
Styles are currently inlined, so the current build has no hashed Astro assets.

## Custom domains (manual, after preview verification)

Keep a record of current DNS records and nginx configuration before any changes.
The domains must be active zones in the selected Cloudflare account.
In Workers & Pages → git-agentic-site → Settings → Domains & Routes → Add → Custom Domain,
attach `git-agentic.com` and `www.git-agentic.com`.
Cloudflare provisions certificates and DNS for these domains; resolve conflicting records only during the approved cutover.
These are **Custom Domains**, not route patterns pointing back to nginx.
Leave domain bindings out of wrangler.jsonc so CI cannot perform the cutover.

## Alias redirect rules (documented only)

On each `.org` and `.dev` Cloudflare zone, create a Single Redirect rule:

- Match `http.host in {"git-agentic.org" "www.git-agentic.org"}` (use `.dev` for the other zone).
- Dynamic target expression: `concat("https://git-agentic.com", http.request.uri.path)`.
- Status: **301**. Enable **Preserve query string**.

The alias hostnames must have proxied DNS records for rules to execute, with HTTPS certificates active.
Test both HTTP and HTTPS and a nested path with a query string; expect one redirect to the same path/query on `.com`.
Optionally apply the equivalent rule for `www.git-agentic.com` in the `.com` zone after attaching both custom domains,
so canonical traffic goes directly to the apex. Do not match the apex in that rule.

## Cutover and rollback

1. Deploy to workers.dev, inspect the preview and run the route check there.
   Confirm headers, feeds, nested pages, `/learn`, `/learn/`, and missing paths.
2. Record the known-good Worker version, current DNS and TTLs, then attach the two custom domains during the approved window.
   Keep the Hetzner nginx vhost, files and certificates intact until production is verified.
3. Check public HTTPS/certificates on apex and www, repeat the route check on apex,
   and verify security/cache headers and representative assets. Then enable/test the alias redirect rules.
4. Monitor traffic/errors through at least the previous DNS TTL before retiring anything on the old host.
5. For a bad content release, select the previous known-good Worker deployment in the dashboard
   (or revert the commit and let CI deploy). For a hosting cutover failure, remove the new Custom Domain bindings,
   restore the recorded apex/www DNS records to the old host, and disable any newly introduced redirect rules as needed.
   Allow DNS caches to expire, then verify the old site's TLS and pages. The old `/learn` failure will remain until separately fixed there.

## References

- [HTML handling](https://developers.cloudflare.com/workers/static-assets/routing/advanced/html-handling/)
- [Static redirects and 200 rewrites](https://developers.cloudflare.com/workers/static-assets/redirects/)
- [Custom domains](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/)
- [Version URLs](https://developers.cloudflare.com/workers/versions-and-deployments/version-urls/)
