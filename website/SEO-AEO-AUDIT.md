# git-agentic.com SEO and AEO audit

**Audit date:** 2026-07-18  
**Scope:** Live technical audit, on-page SEO, answer-engine comprehension,
content architecture, built-output validation, and deployment follow-up.

## Executive summary

The live site was fast, crawlable, and technically clean, but it exposed too
few discovery and comprehension signals. The homepage appeared in a sampled
`site:git-agentic.com` search, while the three product pages did not appear in
that result set. The most important concrete defect was a missing XML sitemap:
both `/sitemap.xml` and `/sitemap-index.xml` returned 404. The site also had no
structured data, no social preview image metadata, no article surface, no RSS
feed, and no direct question-and-answer sections for search or answer engines.

This branch fixes the in-repository issues. It does not claim that markup alone
will produce rankings or citations. Search-engine submission, crawl inspection,
backlinks, and ongoing content distribution remain owner-operated work after
deployment.

## Baseline findings

| Area | Live finding before this change | Priority | Resolution |
|---|---|---:|---|
| Indexability | `robots.txt` returned 200 and allowed all crawlers | Good | Preserved |
| Sitemap | `/sitemap.xml` and `/sitemap-index.xml` returned 404 | Critical | Added a canonical-only XML sitemap and advertised it in `robots.txt` |
| Canonicalization | Every page had a canonical; HTTP, `www`, `.dev`, and `.org` redirected to the HTTPS `.com` origin | Good | Preserved |
| Route behavior | Canonical no-trailing-slash routes returned 200; trailing-slash variants returned 404 | Low | Preserved intentionally to match the Astro/nginx route contract |
| Titles and descriptions | Unique page-level values existed, but several titles led with brand language instead of the problem/category | High | Rewritten around AI agent version control, parallel worktrees, and npm supply-chain security |
| Social previews | Open Graph title/description existed; image, image alt text, site name, locale, and complete Twitter metadata were absent | Medium | Added globally |
| Structured data | No JSON-LD on any page | High | Added Organization, WebSite, WebPage/Article, SoftwareSourceCode, BreadcrumbList, ItemList, and visible FAQ-linked markup |
| Answer readiness | Product pages were detailed but did not answer common questions in a compact, extractable form | High | Added visible, product-specific FAQs and direct definitions |
| Content surface | Four launch-ready articles existed in the repository but were not published or internally linked by the site | High | Added `/learn`, four clean article routes, Article schema, dates, and internal links |
| Feed/discovery | No RSS or alternate feed discovery | Medium | Added `/rss.xml` and a head-level alternate link |
| Machine-readable context | No concise project map for language-model consumers | Medium | Added `/llms.txt` as a supplemental, non-standard discovery aid |
| Performance | Live Lighthouse: 100 Performance, 100 Best Practices, 100 SEO | Good | Preserved |
| Accessibility | Live Lighthouse: 90, due to low text contrast | Medium | Increased muted-text contrast and made inline links non-color-dependent |
| Ongoing validation | No automated SEO regression check | High | Added `npm run check:seo` |

The sampled `site:` query is an observation, not authoritative index-coverage
data. Google Search Console is the correct source for per-URL indexing status.

## Search and answer topic map

The revised page architecture assigns one clear intent to each canonical URL:

| Page | Primary topic / question answered |
|---|---|
| `/` | What open-source infrastructure exists for software built by AI agents? |
| `/git-agentic` | How do I version and atomically roll back AI agent behavior? |
| `/src-control` | How can parallel AI agents use isolated in-memory worktrees, encrypted collaboration, Git interoperability, and native history browsing? |
| `/sentinel` | How can an AI agent inspect npm package risk before installation? |
| `/learn/why-git-revert-does-not-fix-ai-agent-regressions` | Why does `git revert` leave a production AI agent broken? |
| `/learn/version-control-for-ai-agents` | What should version control for AI agents capture? |
| `/learn/six-dimensions-of-ai-agent-behavior` | Which six dimensions determine AI agent behavior? |
| `/learn/version-control-layer-for-agentic-software` | What substrate does agentic software need beneath Git workflows? |

The pages use natural topic language rather than repeated exact-match phrases.
Each product schema and FAQ is backed by visible page content and repository
documentation.

## Validation performed

- Verified live response codes and redirects for the canonical domain, `www`,
  HTTP, `.dev`, and `.org`.
- Verified all four live HTML routes returned 200.
- Ran Lighthouse against the live homepage.
- Built the revised static site with `npm run build`.
- Validated nine built pages for unique titles, descriptions, canonical URLs,
  exactly one `h1`, complete Open Graph/Twitter tags, parseable JSON-LD,
  working internal links, and exact sitemap/canonical parity.
- Ran Lighthouse against the revised homepage, flagship product page, and an
  article. Final flagship result: 100 Performance, 100 Accessibility,
  100 Best Practices, and 100 SEO.
- Inspected desktop and narrow-layout renders.

Run the repeatable checks with:

```bash
cd website
npm ci
npm run build
npm run check:seo
```

## Required actions after deployment

These actions require account ownership or external publication and therefore
are not performed by the code change:

1. Add or verify the `git-agentic.com` domain property in Google Search Console.
2. Submit `https://git-agentic.com/sitemap.xml` and inspect all nine canonical
   URLs, starting with the three product pages and guide hub.
3. Add the site in Bing Webmaster Tools, import Search Console verification if
   convenient, and submit the same sitemap. Consider IndexNow when the site
   begins publishing or updating URLs frequently.
4. Validate deployed product and article URLs in Google's Rich Results Test and
   Schema.org Validator. JSON syntax is build-tested, but deployed fetch and
   search-engine interpretation should still be confirmed.
5. Link to the canonical product pages—not only GitHub—from repository READMEs,
   package metadata, social profiles, launch posts, relevant talks, and partner
   documentation. External authority cannot be created with on-page markup.
6. Re-request indexing after deployment. Allow several days to weeks for search
   systems to recrawl and reprocess titles and structured data.

## Measurement plan

Review monthly for the first three months after deployment:

- indexed canonical URLs versus the nine URLs in the sitemap;
- impressions, clicks, click-through rate, and average position by landing page;
- queries containing `AI agent version control`, `AI agent rollback`,
  `in-memory agent worktrees`, and `npm security for AI agents`;
- referring domains and links to canonical website pages;
- Bing/Copilot grounding citations and a small fixed set of answer-engine test
  questions, recorded with date and exact prompt;
- crawl errors, duplicate canonical selections, and structured-data warnings.

Do not use rank or AI-citation guarantees as success criteria. The useful
leading indicator is whether search systems discover all canonical pages and
start showing impressions for the page's assigned topic.

## Primary guidance used

- [Google Search Central: sitemaps](https://developers.google.com/search/docs/crawling-indexing/sitemaps/overview)
- [Google Search Central: title links](https://developers.google.com/search/docs/appearance/title-link)
- [Google Search Central: structured data introduction](https://developers.google.com/search/docs/appearance/structured-data/intro-structured-data)
- [Google Search Central: organization structured data](https://developers.google.com/search/docs/appearance/structured-data/organization)
- [Bing Webmaster Guidelines](https://www.bing.com/webmasters/help/bing-webmaster-guidelines-30fba23a)
- [Bing Webmaster Tools: sitemaps](https://www.bing.com/webmasters/help/sitemaps-3b5cf6ed)
- [Bing Webmaster Tools: IndexNow](https://www.bing.com/webmasters/help/indexnow-0z209wby)

`llms.txt` is included only as a lightweight supplemental summary. It is not a
search-ranking mechanism or a universally adopted standard. Crawlability,
canonical content, clear answers, primary-source links, and external authority
remain the durable SEO/AEO foundation.
