# Issue #99: deploy.sh conditional `--chown` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `website/scripts/deploy.sh` complete end-to-end on stock macOS (openrsync) while keeping the invocation byte-identical on machines with GNU rsync.

**Architecture:** Capability-probe the local rsync for `--chown` support (`rsync --help | grep -- --chown`). If supported, pass `--chown=www-data:www-data` exactly as today. If not, omit it and run one extra SSH command after the sync — `sudo chown -R www-data:www-data $DOCROOT` — before the existing nginx test + reload. Spec: `docs/superpowers/specs/2026-07-10-issue-99-openrsync-chown-design.md`.

**Tech Stack:** bash (must run on macOS `/bin/bash` 3.2), rsync (GNU ≥ 3.1.0 or openrsync), ssh, shellcheck for linting.

## Global Constraints

- Work happens in worktree `.worktrees/issue-99-openrsync-chown/` on branch `issue-99-openrsync-chown` (repo rule: never edit the main checkout).
- Script must work under bash 3.2: no empty-array expansion under `set -u` — use a scalar flag with `${CHOWN_FLAG:+"$CHOWN_FLAG"}`.
- No other behavior change in `deploy.sh` (build, SSR pruning, docroot bootstrap, nginx reload stay untouched).
- The full acceptance test (real deploy to "✓ Deployed." with only openrsync on PATH; served files owned `www-data:www-data`) touches the production server and is run by the operator, not by this plan.
- Commits: plain prose, imperative mood, no conventional-commits prefixes (repo style).

---

### Task 1: Conditional `--chown` in deploy.sh

**Files:**
- Modify: `website/scripts/deploy.sh:39-44` (the rsync invocation; insertion of fallback block before line 46)

**Interfaces:**
- Consumes: existing env handling in the script — `$SSH_PORT`, `$SSH_TARGET`, `$DOCROOT` are already defined at lines 23–25.
- Produces: n/a (leaf script; no other task depends on it).

- [ ] **Step 1: Verify the failure precondition (the "failing test")**

The script cannot be TDD'd against the real server, so the red step is demonstrating that (a) the local stock rsync lacks `--chown` and (b) the script currently passes it unconditionally.

Run (from the worktree root `.worktrees/issue-99-openrsync-chown/`):

```bash
env PATH=/usr/bin:/bin sh -c 'rsync --help 2>&1 | grep -q -- --chown && echo has-chown || echo no-chown'
grep -n -- '--chown' website/scripts/deploy.sh
```

Expected: first command prints `no-chown` (openrsync); second prints exactly one hit, line 43, inside the unconditional rsync invocation. If the first command prints `has-chown`, stop — the machine's `/usr/bin/rsync` is not openrsync and the openrsync-side verification in Step 4 needs a different machine.

- [ ] **Step 2: Edit the sync section of deploy.sh**

Replace lines 39–44 of `website/scripts/deploy.sh`, currently:

```bash
echo "→ Syncing dist/ → $SSH_TARGET:$DOCROOT ..."
rsync -avz --delete \
  -e "ssh -p $SSH_PORT" \
  --rsync-path='sudo rsync' \
  --chown=www-data:www-data \
  dist/ "$SSH_TARGET:$DOCROOT/"
```

with:

```bash
echo "→ Syncing dist/ → $SSH_TARGET:$DOCROOT ..."
# GNU rsync (>= 3.1.0) supports --chown; macOS's bundled openrsync does not.
# Probe the capability; without it, fix ownership server-side after the sync.
# Scalar (not array) on purpose: macOS bash 3.2 errors on empty-array
# expansion under `set -u`.
CHOWN_FLAG=""
if rsync --help 2>&1 | grep -q -- '--chown'; then
  CHOWN_FLAG="--chown=www-data:www-data"
fi

rsync -avz --delete \
  -e "ssh -p $SSH_PORT" \
  --rsync-path='sudo rsync' \
  ${CHOWN_FLAG:+"$CHOWN_FLAG"} \
  dist/ "$SSH_TARGET:$DOCROOT/"

if [[ -z "$CHOWN_FLAG" ]]; then
  echo "→ Local rsync lacks --chown (openrsync); fixing ownership server-side..."
  ssh -p "$SSH_PORT" "$SSH_TARGET" "sudo chown -R www-data:www-data $DOCROOT"
fi
```

Everything before (`echo "→ Ensuring docroot..."`, line 36–37) and after (`echo "→ Reloading nginx..."`, line 46–47) stays byte-identical.

- [ ] **Step 3: Syntax check and lint**

```bash
bash -n website/scripts/deploy.sh
command -v shellcheck >/dev/null && shellcheck website/scripts/deploy.sh || echo "shellcheck not installed — skipped"
```

Expected: `bash -n` silent (exit 0); shellcheck reports no new warnings (the `${CHOWN_FLAG:+"$CHOWN_FLAG"}` idiom is shellcheck-endorsed and must not be "fixed" by quoting the whole expansion).

- [ ] **Step 4: Verify both probe branches against real binaries**

```bash
# openrsync branch (stock macOS): expect "fallback"
env PATH=/usr/bin:/bin sh -c 'rsync --help 2>&1 | grep -q -- --chown && echo direct-chown || echo fallback'

# GNU branch, if Homebrew rsync is installed: expect "direct-chown"
if command -v brew >/dev/null && [ -x "$(brew --prefix 2>/dev/null)/bin/rsync" ]; then
  "$(brew --prefix)/bin/rsync" --help 2>&1 | grep -q -- --chown && echo direct-chown || echo fallback
else
  echo "GNU rsync not installed — GNU branch verified by inspection only"
fi
```

Expected: first prints `fallback`; second prints `direct-chown` or the not-installed notice.

- [ ] **Step 5: Commit**

```bash
git add website/scripts/deploy.sh
git commit -m "Make website deploy.sh work with openrsync (no --chown)

macOS ships openrsync, which lacks GNU rsync's --chown, so the deploy
died after the build step (issue #99). Probe the local rsync for
--chown support: with GNU rsync the invocation is unchanged; with
openrsync the flag is dropped and ownership is fixed server-side with
sudo chown -R www-data:www-data before the nginx reload — the sequence
verified manually on the 2026-07-09 deploy."
```

---

### Task 2: Close the loop in OpenWolf buglog

**Files:**
- Modify: `/Users/tonibergholm/Developer/github/git.agentic/.wolf/buglog.json` (bug-123 `fix` field — NOTE: main checkout path; `.wolf/` is untracked/gitignored, so this is not a worktree write and is not committed)

**Interfaces:**
- Consumes: Task 1's commit hash (`git -C .worktrees/issue-99-openrsync-chown log -1 --format=%h -- website/scripts/deploy.sh`).
- Produces: n/a.

- [ ] **Step 1: Update bug-123's fix field**

```bash
python3 - <<'EOF'
import json
p = '/Users/tonibergholm/Developer/github/git.agentic/.wolf/buglog.json'
data = json.load(open(p))
bugs = data if isinstance(data, list) else data.get('bugs', data)
for b in bugs:
    if b.get('id') == 'bug-123':
        b['fix'] = ("Scripted in deploy.sh on branch issue-99-openrsync-chown (issue #99): "
                    "capability-probe rsync --help for --chown; GNU rsync path unchanged, "
                    "openrsync path drops the flag and runs server-side "
                    "sudo chown -R www-data:www-data before nginx reload.")
        b['last_seen'] = '2026-07-10'
        print('updated bug-123')
        break
else:
    raise SystemExit('bug-123 not found')
json.dump(data, open(p, 'w'), indent=2)
EOF
```

Expected: prints `updated bug-123`.

- [ ] **Step 2: Log to .wolf/memory.md**

```bash
printf '| %s | Implemented issue #99 fix: conditional --chown in deploy.sh | website/scripts/deploy.sh | committed on issue-99-openrsync-chown | ~2k |\n' "$(date +%H:%M)" >> /Users/tonibergholm/Developer/github/git.agentic/.wolf/memory.md
```

Expected: exit 0.

---

## Operator follow-up (not part of this plan's execution)

- Open a PR from `issue-99-openrsync-chown` referencing issue #99.
- Run a real deploy from a machine with only `/usr/bin/rsync` on PATH; confirm "✓ Deployed." and `ssh $SSH_TARGET "stat -c '%U:%G' /var/www/git-agentic.com/index.html"` → `www-data:www-data`. These are acceptance criteria 1–2 of issue #99.
