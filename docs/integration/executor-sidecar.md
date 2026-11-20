# Platform-partner integration — sidecar `agenticd` integration

**Status:** Draft. Implements [ADR-0004](../adr/0004-realtime-agenticd-for-executor.md).
**Audience:** Platform-partner integrators / Coding-worker authors wiring `agenticd` into a Cloud Run instance.

## Topology

The Coding worker runs as a scale-to-zero Cloud Run service with `containerConcurrency: 1`. `agenticd` runs **in the same Cloud Run instance** as a sidecar container, communicates with the worker over a shared Unix domain socket, and persists the commit DAG to a Google Cloud Storage bucket. No network surface, no multi-tenant, same `agentic-proto` wire the LangGraph integration already uses.

## Image

```bash
docker build -f crates/agenticd/Dockerfile.sidecar -t agenticd:sidecar .
```

`debian:bookworm-slim` base, runs as non-root uid 65532, exposes no ports. Targeted size ~70 MB after `strip`. Build for `linux/amd64` (Cloud Run) and `linux/arm64` (Apple Silicon dev) via `docker buildx`.

## Trust boundary

The worker runs model-driven code and is treated as adversarial. The layout below keeps the daemon's state out of its reach:

- The worker image sets a `USER` with a uid other than the sidecar's 65532 (Cloud Run runs each container as its image `USER`). The examples use 10001.
- The daemon starts with `--allowed-uid 10001` and refuses to start without an allowlist (ADR-0012). It admits only that uid on the socket and records it on each commit. When the allowlist names a uid other than the daemon's own, the daemon creates the socket mode 0666, since connecting needs write permission on the file. The `SO_PEERCRED` check is what gates access.
- `--repo` lives on its own emptyDir (`state`) that only the `agenticd` container mounts. The shared volume (`ipc`) carries the socket and nothing else. A worker that can write to a directory above the repo can rename the repo and plant its own `.agentic`, so the state volume must not be writable by the worker.
- At startup the daemon resolves `--repo` to its canonical path and refuses to start, naming the path, if:
  - the repo is owned by another uid, or has any group or other write bit (the sticky bit and gid do not matter for the repo itself);
  - any ancestor up to `/` is owned by neither the daemon nor root, or is world-writable without the sticky bit, or is group-writable with a group other than the daemon's. A sticky ancestor such as a root-owned `/tmp` passes when its child on the path is owned by the daemon or root;
  - `.agentic` is a symlink, is owned by another uid, or is already group- or world-writable.

  Otherwise it creates `.agentic` at mode 0700 or tightens an existing one to 0700.
- Group-write with the daemon's own gid is allowed on ancestors, so user-private-group checkouts under umask 002 keep working. The residual is that the worker must not share the daemon's primary group (the sidecar image uses gid 65532).
- `--trusted-volume-root <dir>` exempts one ancestor, matched by canonical path, from the ancestor check. Use it when only agenticd mounts that directory. Kubernetes-style emptyDir roots are often world-writable, and without the flag the daemon refuses to start. It never exempts the repo itself.
- The socket path lives on the shared volume, so the worker can replace or remove the socket file. Nothing other than the worker's own client should trust that path.
- Refs, objects, the GCS cache and `schema/*.down.sql` live in `.agentic`, so the worker cannot edit them. If the worker must read the prompts that rollback restores, mount the `state` volume into the worker read-only. Do not mount it writable.
- Credentials arrive as files from Secret Manager volumes mounted into the `agenticd` container only. This removes the argv and environment exposure. It does not isolate the secrets from the worker: all containers in one Cloud Run instance share the service identity, so the worker can fetch the same secrets or call the bucket directly. Real isolation needs a separate identity for the sidecar (ADR-0015, Proposed).

**Not verified on Cloud Run.** We have not confirmed the owner and mode of an emptyDir volume root, or whether it can be mounted read-only into one container. If the root is world-writable, the daemon refuses to start unless you pass `--trusted-volume-root /state`. Pass it only because the `state` volume is mounted into the `agenticd` container alone. Test the exact deployment before relying on it.

## Cloud Run service definition (excerpt)

```yaml
spec:
  template:
    spec:
      containerConcurrency: 1
      containers:
        - name: worker
          # The image must set USER 10001 (any uid other than 65532).
          image: gcr.io/<your-registry>/coding-worker:<tag>
          env:
            - { name: AGENTICD_SOCKET, value: /ipc/agenticd.sock }
          volumeMounts:
            - { name: ipc, mountPath: /ipc }
            # Only if the worker must read restored prompts, and only read-only:
            # - { name: state, mountPath: /state, readOnly: true }
        - name: agenticd
          image: gcr.io/<your-registry>/agenticd-sidecar:<tag>
          args:
            - --repo
            - /state/work
            - --trusted-volume-root
            - /state
            - --socket
            - /ipc/agenticd.sock
            - --allowed-uid
            - "10001"
            - --object-store
            - gcs://<your-bucket>/<tenant>
            - --postgres-url-file
            - /secrets/postgres/url
            - --tables
            - <table>:<pk>
          env:
            - { name: AGENTIC_GCS_TOKEN_FILE, value: /secrets/gcs/token }
          volumeMounts:
            - { name: ipc, mountPath: /ipc }
            - { name: state, mountPath: /state }
            - { name: gcs-token, mountPath: /secrets/gcs, readOnly: true }
            - { name: postgres-url, mountPath: /secrets/postgres, readOnly: true }
      volumes:
        - name: ipc
          emptyDir: { medium: Memory }
        - name: state
          emptyDir: { medium: Memory }
        - name: gcs-token
          secret:
            secretName: gcs-bearer
            items: [{ key: latest, path: token }]
        - name: postgres-url
          secret:
            secretName: postgres-url
            items: [{ key: latest, path: url }]
```

Drop the `--postgres-url-file` and `--tables` lines if the deployment has no memory backend.

Both emptyDirs are tmpfs; durability lives in GCS. Because `<repo>/.agentic/schema` is empty on tmpfs at boot, reverse migrations need the sidecar image to provide the `.down.sql` files, for example copied into `/state/work/.agentic/schema` by the sidecar's own entrypoint. The worker must never supply them: a plain rollback executes those files against Postgres.

## Flags and env vars `agenticd` reads

| Input | Meaning | Default |
|---|---|---|
| `--allowed-uid <uid>` | Uid allowed on the socket; repeatable. Required unless `--insecure-allow-any-uid` is passed (ADR-0012). | _none_; startup fails |
| `--trusted-volume-root <dir>` | Ancestor of `--repo` that the operator asserts only agenticd can write; exempt from the ancestor write check. Never exempts the repo. | _none_ |
| `--postgres-url-file <path>` | File holding the Postgres URL; surrounding whitespace is trimmed and an empty file is an error. Conflicts with `--postgres`. | _none_ |
| `AGENTIC_GCS_TOKEN_FILE` | Path to a file holding the GCS bearer token, trimmed the same way. Setting it together with `AGENTIC_GCS_TOKEN` is an error. | _none_ |
| `AGENTIC_GCS_ENDPOINT` | Override GCS host (for `tests/fixtures/fake-gcs.yml`) | public GCS |
| `AGENTIC_GCS_TOKEN` | Bearer for the GCS JSON API, as a plain env value. Fine for the demo and tests; prefer the file form in the sidecar. | _none_; required for real GCS unless the file form is used |
| `RUST_LOG` | tracing filter | `info` |

`--postgres <url>` still works for the demo and tests, but puts the password in argv. Use `--postgres-url-file` in the sidecar.

The sidecar does NOT call ADC or the GCE metadata server itself. In v1.0 the only supported wiring is to **mount the token as a Secret Manager volume** into the `agenticd` container, as in the YAML excerpt above. ("Have the worker write it to `/ipc`" is intentionally NOT supported: both containers start at the same time, `/ipc` is empty on boot, and the sidecar would race the worker's first checkpoint. If token rotation past the Cloud Run instance lifetime becomes a requirement, file a follow-up, since that needs a refresh thread inside `agenticd`.)

## Worker code

```python
from agentic import AgenticClient
client = AgenticClient(socket_path=os.environ["AGENTICD_SOCKET"])
# wire into Claude Agent SDK's checkpoint hooks per ADR-0004 Decision 3
```

## Failure semantics

Per ADR-0004 Decision 4 there is no degraded mode. If the sidecar is unreachable when the worker tries to write a checkpoint, the worker MUST fail the ticket loudly with the last-successful-checkpoint hash and the sidecar exit code.

## Out of scope

- Wire auth beyond the peer-uid allowlist. The socket admits only the uids named by `--allowed-uid` (ADR-0012); there is no token or mTLS exchange.
- Multi-region failover — single-region in v1.0; document with design partners.
- Concurrent writers — `containerConcurrency: 1` is a hard assumption.

## Verification

1. `docker build -f crates/agenticd/Dockerfile.sidecar -t agenticd:sidecar .` succeeds.
2. `docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/.state:/state" -v "$PWD/.ipc:/ipc" agenticd:sidecar --repo /state/work --socket /ipc/agenticd.sock --insecure-allow-any-uid` (the default `fs` object store lives under `/state/work/.agentic`; the container uid must own both mounted directories, and the insecure flag is for this local check only) binds the socket and serves a `Ping`.
3. Same as (2) but with `--object-store gcs://...` against `tests/fixtures/fake-gcs.yml` completes one commit-and-rollback cycle. Covered by the `gcs` CI job today.
