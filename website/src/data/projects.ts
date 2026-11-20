// One status vocabulary for the whole site. A badge never claims more than
// the project page states; "In production use" always appears next to `scope`.
export type BadgeKind = 'prod' | 'pre' | 'alpha' | 'mvp';

export const badgeLabel: Record<BadgeKind, string> = {
  prod: 'In production use',
  pre: 'Pre-1.0',
  alpha: 'Alpha',
  mvp: 'MVP complete, validation in progress',
};

export interface Project {
  name: string;
  href: string;
  repo: string;
  purpose: string;
  badges: BadgeKind[];
  scope: string;
  language: string;
  license: string;
}

const gh = 'https://github.com/git-agentic';

export const projects: Project[] = [
  {
    name: 'git.agentic',
    href: '/git-agentic',
    repo: `${gh}/git.agentic`,
    purpose: 'Version control and atomic rollback for AI agent behavior.',
    badges: ['mvp'],
    scope: 'Not in production use yet.',
    language: 'Rust, Python',
    license: 'Apache 2.0',
  },
  {
    name: 'src-control',
    href: '/src-control',
    repo: `${gh}/src-control`,
    purpose: 'Version control and a Git forge for fleets of AI agents.',
    badges: ['prod', 'pre'],
    scope: 'Production use covers sc-forge only, as the project Git host at Codento Engine.',
    language: 'Rust',
    license: 'Apache 2.0',
  },
  {
    name: 'Sentinel',
    href: '/sentinel',
    repo: `${gh}/pkg-registry`,
    purpose: 'npm supply-chain security for AI agents.',
    badges: ['prod', 'alpha'],
    scope: 'Runs at Codento Engine in observe mode. It scores every tarball and blocks none yet.',
    language: 'TypeScript',
    license: 'Apache 2.0',
  },
];

export const project = (name: string): Project => {
  const found = projects.find((item) => item.name === name);
  if (!found) throw new Error(`Unknown project: ${name}`);
  return found;
};
