import assert from 'node:assert/strict';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = fileURLToPath(new URL('../dist/', import.meta.url));
const site = 'https://git-agentic.com';

function filesUnder(directory, extension) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return filesUnder(path, extension);
    return path.endsWith(extension) ? [path] : [];
  });
}

function matchOne(html, expression, label, file) {
  const match = html.match(expression);
  assert.ok(match, `${file}: missing ${label}`);
  return match[1];
}

function outputPathFor(url) {
  const path = new URL(url, site).pathname;
  if (path === '/') return join(dist, 'index.html');
  const withoutLeadingSlash = path.slice(1);
  return join(dist, path.endsWith('/')
    ? `${withoutLeadingSlash}index.html`
    : `${withoutLeadingSlash}.html`);
}

const htmlFiles = filesUnder(dist, '.html');
assert.ok(htmlFiles.length > 0, 'No built HTML files found; run npm run build first');

const titles = new Set();
const canonicals = new Set();

for (const file of htmlFiles) {
  const html = readFileSync(file, 'utf8');
  const name = relative(dist, file);
  const title = matchOne(html, /<title>(.*?)<\/title>/s, 'title', name);
  const description = matchOne(html, /<meta name="description" content="([^"]+)"/, 'meta description', name);
  const canonical = matchOne(html, /<link rel="canonical" href="([^"]+)"/, 'canonical URL', name);

  assert.ok(title.length <= 70, `${name}: title is longer than 70 characters`);
  assert.ok(description.length >= 100 && description.length <= 180, `${name}: description length is ${description.length}`);
  assert.ok(!titles.has(title), `${name}: duplicate title: ${title}`);
  assert.ok(!canonicals.has(canonical), `${name}: duplicate canonical: ${canonical}`);
  titles.add(title);
  canonicals.add(canonical);

  assert.equal((html.match(/<h1(?:\s|>)/g) ?? []).length, 1, `${name}: expected exactly one h1`);
  for (const required of [
    'name="robots"',
    'property="og:title"',
    'property="og:description"',
    'property="og:image"',
    'name="twitter:card"',
    'name="twitter:image"',
  ]) {
    assert.ok(html.includes(required), `${name}: missing ${required}`);
  }
  assert.ok(html.includes('name="twitter:card" content="summary"'), `${name}: square social image requires a summary card`);

  const jsonLdBlocks = [...html.matchAll(/<script type="application\/ld\+json">(.*?)<\/script>/gs)];
  assert.ok(jsonLdBlocks.length >= 3, `${name}: expected base structured data`);
  for (const [, json] of jsonLdBlocks) JSON.parse(json);

  for (const [, href] of html.matchAll(/<a\s[^>]*href="([^"]+)"/g)) {
    if (href.startsWith('mailto:')) continue;
    const target = new URL(href, canonical);
    if (target.origin !== site) continue;
    assert.ok(existsSync(outputPathFor(target)), `${name}: broken internal link ${href}`);
  }
}

const sitemap = readFileSync(join(dist, 'sitemap.xml'), 'utf8');
const sitemapUrls = new Set([...sitemap.matchAll(/<loc>(.*?)<\/loc>/g)].map(([, url]) => url));
assert.deepEqual(sitemapUrls, canonicals, 'Sitemap URLs must exactly match page canonicals');

const robots = readFileSync(join(dist, 'robots.txt'), 'utf8');
assert.ok(robots.includes('Sitemap: https://git-agentic.com/sitemap.xml'), 'robots.txt must advertise the sitemap');
assert.ok(statSync(join(dist, 'llms.txt')).size > 500, 'llms.txt is missing or unexpectedly small');

console.log(`SEO checks passed for ${htmlFiles.length} pages and ${sitemapUrls.size} sitemap URLs.`);
