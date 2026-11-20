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

The worker runs model-driven code and is treated as adversarial. It shares a volume with the daemon, so the layout below keeps the daemon's state out of its reach:

- The worker image sets a `USER` with a uid other than the sidecar's 65532 (Cloud Run runs each container as its image `USER`). The examples use 10001.
- The daemon starts with `--allowed-uid 10001` and refuses to start without an allowlist (ADR-0012). It admits only that uid on the socket and records it on each commit.
- The daemon creates and owns `--repo /shared/work`. At startup it creates `/shared/work/.agentic` with mode 0700, tightens an existing one to 0700, and refuses to start if the repo or `.agentic` is owned by another uid, if the repo is group- or world-writable, or if `.agentic` is a symlink. The error names the path.
- Refs, objects, the GCS cache and `schema/*.down.sql` live in `.agentic`, so the worker cannot edit them. It can still read the prompt files that rollback restores under `/shared/work/prompts`.
- Credentials arrive as files from Secret Manager volumes mounted into the `agenticd` container only. They never appear in argv or in the worker's environment.

The daemon cannot make the shared volume's root directory private. A worker that can write to `/shared` can rename `/shared/work` while the daemon is stopped or running, so the ownership check is a startup guard and not a substitute for a volume the worker cannot write. Keep the socket as the only thing the worker needs there.

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
            - { name: AGENTICD_SOCKET, value: /shared/agenticd.sock }
          volumeMounts:
            - { name: shared, mountPath: /shared }
        - name: agenticd
          image: gcr.io/<your-registry>/agenticd-sidecar:<tag>
          args:
            - --repo
            - /shared/work
            - --socket
            - /shared/agenticd.sock
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
            - { name: shared, mountPath: /shared }
            - { name: gcs-token, mountPath: /secrets/gcs, readOnly: true }
            - { name: postgres-url, mountPath: /secrets/postgres, readOnly: true }
      volumes:
        - name: shared
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

`/shared` is tmpfs; durability lives in GCS. Because `<repo>/.agentic/schema` is empty on tmpfs at boot, reverse migrations need the sidecar image to provide the `.down.sql` files, for example copied into `/shared/work/.agentic/schema` by the sidecar's own entrypoint. The worker must never supply them: a plain rollback executes those files against Postgres.

When an allowlist is enforced, the daemon sets the socket file to mode 0666 after binding, because connecting to a Unix socket needs write permission on the file and the worker is a different uid. The `SO_PEERCRED` check is what gates access.

## Flags and env vars `agenticd` reads

| Input | Meaning | Default |
|---|---|---|
| `--allowed-uid <uid>` | Uid allowed on the socket; repeatable. Required unless `--insecure-allow-any-uid` is passed (ADR-0012). | _none_; startup fails |
| `--postgres-url-file <path>` | File holding the Postgres URL; one trailing newline is trimmed and an empty file is an error. Conflicts with `--postgres`. | _none_ |
| `AGENTIC_GCS_TOKEN_FILE` | Path to a file holding the GCS bearer token, trimmed the same way. Setting it together with `AGENTIC_GCS_TOKEN` is an error. | _none_ |
| `AGENTIC_GCS_ENDPOINT` | Override GCS host (for `tests/fixtures/fake-gcs.yml`) | public GCS |
| `AGENTIC_GCS_TOKEN` | Bearer for the GCS JSON API, as a plain env value. Fine for the demo and tests; prefer the file form in the sidecar. | _none_; required for real GCS unless the file form is used |
| `RUST_LOG` | tracing filter | `info` |

`--postgres <url>` still works for the demo and tests, but puts the password in argv. Use `--postgres-url-file` in the sidecar.

The sidecar does NOT call ADC or the GCE metadata server itself. In v1.0 the only supported wiring is to **mount the token as a Secret Manager volume** into the `agenticd` container, as in the YAML excerpt above. ("Have the worker write it to `/shared`" is intentionally NOT supported: both containers start at the same time, `/shared` is empty on boot, and the sidecar would race the worker's first checkpoint. If token rotation past the Cloud Run instance lifetime becomes a requirement, file a follow-up, since that needs a refresh thread inside `agenticd`.)

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
2. `docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/.shared:/shared" agenticd:sidecar --repo /shared/work --socket /shared/agenticd.sock --insecure-allow-any-uid --object-store fs:///shared/objects` (the container uid must own the mounted directory, and the insecure flag is for this local check only) binds the socket and serves a `Ping`.
3. Same as (2) but with `--object-store gcs://...` against `tests/fixtures/fake-gcs.yml` completes one commit-and-rollback cycle. Covered by the `gcs` CI job today.
