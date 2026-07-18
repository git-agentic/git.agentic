import { articles, articleUrl } from '../data/articles';
import { escapeXml } from '../utils/xml';

const site = 'https://git-agentic.com';
const staticPages = [
  { path: '/', lastmod: '2026-07-18', priority: '1.0' },
  { path: '/git-agentic', lastmod: '2026-07-18', priority: '0.9' },
  { path: '/src-control', lastmod: '2026-07-18', priority: '0.8' },
  { path: '/sentinel', lastmod: '2026-07-18', priority: '0.8' },
  { path: '/learn', lastmod: '2026-07-18', priority: '0.8' },
];

export const GET = () => {
  const pages = [
    ...staticPages,
    ...articles.map((article) => ({
      path: articleUrl(article),
      lastmod: article.modified,
      priority: '0.7',
    })),
  ];
  const urls = pages.map((page) => `  <url>
    <loc>${escapeXml(`${site}${page.path}`)}</loc>
    <lastmod>${page.lastmod}</lastmod>
    <changefreq>monthly</changefreq>
    <priority>${page.priority}</priority>
  </url>`).join('\n');

  return new Response(`<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${urls}
</urlset>\n`, {
    headers: { 'Content-Type': 'application/xml; charset=utf-8' },
  });
};
