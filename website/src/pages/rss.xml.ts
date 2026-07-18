import { articles, articleUrl } from '../data/articles';
import { escapeXml } from '../utils/xml';

const site = 'https://git-agentic.com';

export const GET = () => {
  const items = [...articles].reverse().map((article) => {
    const url = `${site}${articleUrl(article)}`;
    return `    <item>
      <title>${escapeXml(article.title)}</title>
      <link>${url}</link>
      <guid isPermaLink="true">${url}</guid>
      <pubDate>${new Date(`${article.published}T12:00:00Z`).toUTCString()}</pubDate>
      <description>${escapeXml(article.description)}</description>
    </item>`;
  }).join('\n');

  return new Response(`<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>git.agentic articles</title>
    <link>${site}/learn</link>
    <description>Technical guides to version control and infrastructure for AI agents.</description>
    <language>en</language>
${items}
  </channel>
</rss>\n`, {
    headers: { 'Content-Type': 'application/rss+xml; charset=utf-8' },
  });
};
