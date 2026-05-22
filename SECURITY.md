# Security Policy

## Supported Versions

`git.agentic` shipped v1.0 with the repo going public on 2026-05-22. Only `main` and the latest tagged release receive security fixes during the immediate post-launch window; this table will track the supported release line as v1.0.x patches and v1.1 land.

| Version | Supported          |
| ------- | ------------------ |
| `main`  | :white_check_mark: |
| < 1.0   | :x:                |

## Reporting a Vulnerability

Please **do not** open public GitHub issues for security vulnerabilities. Report them privately by email to:

**toni@git-agentic.com**

Include as much of the following as is available:

- A description of the vulnerability and the impact you believe it has.
- Steps to reproduce, ideally with a minimal proof-of-concept.
- The commit hash or release version affected.
- Any suggested mitigations.

You can expect:

- An acknowledgement within **3 business days** of receipt.
- A status update within **7 business days** confirming whether the report is accepted, asking for more information, or explaining why it's out of scope.
- A coordinated disclosure timeline once a fix is identified; the default is **90 days** from accepted report to public disclosure, shorter if the fix lands sooner and a release is cut.

You are welcome (but not required) to use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability) on this repository as an alternative to email.
