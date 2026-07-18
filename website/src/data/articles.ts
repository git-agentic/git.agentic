export interface Article {
  source: string;
  slug: string;
  title: string;
  description: string;
  published: string;
  modified: string;
}

export const articles: Article[] = [
  {
    source: '01-when-git-revert-lies-to-you.md',
    slug: 'why-git-revert-does-not-fix-ai-agent-regressions',
    title: 'Why git revert Does Not Fix AI Agent Regressions',
    description:
      'Git only restores code. Learn why AI agent rollback must also restore prompts, tools, models, memory, and schema as one coherent version.',
    published: '2026-05-21',
    modified: '2026-07-18',
  },
  {
    source: '02-git-for-the-agent-era.md',
    slug: 'version-control-for-ai-agents',
    title: 'Version Control for AI Agents: A New Commit Primitive',
    description:
      'AI agents change more than code. A practical look at versioning prompts, tools, models, memory, and schema in one content-addressed commit.',
    published: '2026-05-21',
    modified: '2026-07-18',
  },
  {
    source: '03-six-things-determine-what-your-agent-does.md',
    slug: 'six-dimensions-of-ai-agent-behavior',
    title: 'The Six Dimensions That Determine AI Agent Behavior',
    description:
      'Code, prompts, tools, model, memory, and schema jointly determine an AI agent’s behavior. Here is why production teams must version all six.',
    published: '2026-05-21',
    modified: '2026-07-18',
  },
  {
    source: '04-version-control-layer-for-agentic-software.md',
    slug: 'version-control-layer-for-agentic-software',
    title: 'The Version Control Layer for Agentic Software',
    description:
      'Why software built and operated by AI agents needs a version-control substrate for behavior—not only a faster workflow around Git.',
    published: '2026-05-21',
    modified: '2026-07-18',
  },
];

export function articleUrl(article: Article): string {
  return `/learn/${article.slug}`;
}
