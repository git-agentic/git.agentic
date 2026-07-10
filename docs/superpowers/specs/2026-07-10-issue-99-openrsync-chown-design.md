# Issue #99 — deploy.sh on stock macOS (conditional `--chown`) — design

Date: 2026-07-10
Issue: [#99](https://github.com/git-agentic/git.agentic/issues/99)
Related: OpenWolf bug-123 (2026-07-09 suite-site deploy failure)

## Problem

`website/scripts/deploy.sh` passes `--chown=www-data:www-data` to rsync.
macOS ships openrsync, which does not support `--chown`,
so on a stock machine the deploy dies after the build step.
Every other flag the script uses (`-avz`, `--delete`, `-e`, `--rsync-path`) is supported by openrsync;
`--chown` is the only portability break.

The end state that matters: served files under `/var/www/git-agentic.com` owned `www-data:www-data`,
nginx tested and reloaded, script prints "✓ Deployed."

## Decision

Conditional `--chown` (approach B, chosen over unconditional server-side chown):

- **Detection** is a capability probe, not a vendor sniff:
  `rsync --help 2>&1 | grep -q -- '--chown'`.
  GNU rsync ≥ 3.1.0 lists `--chown` in its help; openrsync does not.
  This also handles old GNU rsync (< 3.1.0) correctly.
- **Capable rsync:** invocation identical to today, `--chown` included, no extra SSH command.
  Zero behavior change on machines with GNU rsync (acceptance criterion 3).
- **Incapable rsync (openrsync):** same invocation minus `--chown`,
  then one extra SSH command after the sync and before the nginx test + reload:
  `sudo chown -R www-data:www-data $DOCROOT`.
  This is the exact sequence verified manually on the 2026-07-09 deploy (bug-123).

## Implementation shape

macOS `/bin/bash` is 3.2, where expanding an empty array under `set -u` errors,
so the conditional flag is a scalar (the flag contains no spaces):

```bash
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

The fallback path announces itself with the `echo`
so an operator understands why an extra SSH command ran;
the GNU path stays silent, as today.

## Error handling

Nothing new: `set -euo pipefail` is already in place,
so a failed server-side chown aborts before the nginx reload,
the same as any other step.

## Verification

- `bash -n` and `shellcheck` on the script.
- Probe logic checked locally against `/usr/bin/rsync` (openrsync)
  and, if installed, Homebrew GNU rsync.
- The full acceptance criteria (deploy runs to "✓ Deployed." with only openrsync on PATH;
  served files end up `www-data:www-data`) require a real deploy against the server,
  triggered by the operator — the script change alone cannot verify them.

## Out of scope

- Requiring or recommending Homebrew GNU rsync (rejected approach C).
- Unconditional server-side chown (rejected approach A — chosen B preserves
  the literal GNU-rsync behavior, at the cost of a second code path).
- Any other deploy.sh behavior (build, SSR-artifact pruning, docroot bootstrap, nginx reload).

## Housekeeping

- Update `.wolf/buglog.json` bug-123 once the fix lands (fix is now scripted, not manual).
- Cerebrum Do-Not-Repeat entry: bash 3.2 errors on empty-array expansion under `set -u`;
  use a scalar + `${VAR:+"$VAR"}` for conditional single flags in portable scripts.
