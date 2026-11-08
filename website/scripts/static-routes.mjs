import { readdirSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const dist = fileURLToPath(new URL('../dist/', import.meta.url));
const pages = readdirSync(dist, { recursive: true }).filter((path) => path.endsWith('.html'));
// Explicit rewrites accept both slash variants without Workers canonical redirects.
const rules = pages.flatMap((page) => {
  const route = page === 'index.html' ? '' : page.slice(0, -5);
  return route ? [`/${route} /${page} 200`, `/${route}/ /${page} 200`] : ['/ /index.html 200'];
});
writeFileSync(`${dist}_redirects`, `${rules.join('\n')}\n`);
console.log(`Wrote rewrites for ${pages.length} HTML pages.`);
