import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdirSync, readFileSync } from 'node:fs';

const dist = new URL('../dist/', import.meta.url);
const base = process.argv[2] ?? 'http://localhost:8787';
for (const file of readdirSync(dist, { recursive: true }).map((path) => path.replaceAll('\\', '/')).filter((file) => file.endsWith('.html'))) {
  const route = file === 'index.html' ? '/' : `/${file.slice(0, -5)}`;
  for (const path of new Set([route, route === '/' ? route : `${route}/`, `/${file}`])) {
    const body = execFileSync('curl', ['--silent', '--show-error', '--max-time', '15', `${base}${path}`], { encoding: 'utf8' });
    assert.equal(body, readFileSync(new URL(file, dist), 'utf8'), `${path}: wrong page body`);
    check(path, 200);
  }
}
for (const path of ['/sitemap.xml', '/rss.xml', '/robots.txt', '/llms.txt']) check(path, 200);
check('/missing-page-check', 404);
check('/learn/missing-page-check', 404);
function check(path, expected) {
  const result = execFileSync('curl', ['--silent', '--show-error', '--max-time', '15', '--location', '--output', '/dev/null', '--write-out', '%{http_code} redirects=%{num_redirects}', `${base}${path}`], { encoding: 'utf8' });
  assert.equal(result, `${expected} redirects=0`, path);
  console.log(`${path} ${result}`);
}
