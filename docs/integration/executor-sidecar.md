# Codento Executor — sidecar `agenticd` integration

**Status:** Draft. Implements [ADR-0004](../adr/0004-realtime-agenticd-for-executor.md).
**Audience:** Codento Executor / Coding-worker authors wiring `agenticd` into a Cloud Run instance.

## Topology

The Coding worker runs as a scale-to-zero Cloud Run service with `containerConcurrency: 1`. `agenticd` runs **in the same Cloud Run instance** as a sidecar container, communicates with the worker over a shared Unix domain socket, and persists the commit DAG to a Google Cloud Storage bucket. No network surface, no multi-tenant, same `agentic-proto` wire the LangGraph integration already uses.

## Image

```bash
docker build -f crates/agenticd/Dockerfile.sidecar -t agenticd:sidecar .
```

`debian:bookworm-slim` base, runs as non-root uid 65532, exposes no ports. Targeted size ~70 MB after `strip`. Build for `linux/amd64` (Cloud Run) and `linux/arm64` (Apple Silicon dev) via `docker buildx`.

## Cloud Run service definition (excerpt)

```yaml
spec:
  template:
    spec:
      containerConcurrency: 1
      containers:
        - name: worker
          image: gcr.io/codento/coding-worker:<tag>
          env:
            - { name: AGENTICD_SOCKET, value: /shared/agenticd.sock }
          volumeMounts:
            - { name: shared, mountPath: /shared }
        - name: agenticd
          image: gcr.io/codento/agenticd-sidecar:<tag>
          args:
            - --repo
            - /shared/work
            - --socket
            - /shared/agenticd.sock
            - --object-store
            - gcs://codento-exec-sessions/<tenant>
          env:
            - name: AGENTIC_GCS_TOKEN
              valueFrom: { secretKeyRef: { name: gcs-bearer, key: token } }
          volumeMounts:
            - { name: shared, mountPath: /shared }
      volumes:
        - name: shared
          emptyDir: { medium: Memory }
```

`/shared` is tmpfs; durability lives in GCS.

## Env vars `agenticd` reads

| Variable | Meaning | Default |
|---|---|---|
| `AGENTIC_GCS_ENDPOINT` | Override GCS host (for `tests/fixtures/fake-gcs.yml`) | public GCS |
| `AGENTIC_GCS_TOKEN` | Bearer for the GCS JSON API | _none_; required for real GCS |
| `RUST_LOG` | tracing filter | `info` |

The sidecar does NOT call ADC or the GCE metadata server itself. In v1.0 the only supported wiring is to **inject the token via env at Cloud Run service startup** — the YAML excerpt above does that via a Secret Manager reference. ("Have the worker write it to `/shared`" is intentionally NOT supported: both containers start at the same time, `/shared` is empty on boot, and the sidecar would race the worker's first checkpoint. If token rotation past the Cloud Run instance lifetime becomes a requirement, file a follow-up — that needs a refresh thread inside `agenticd`.)

## Worker code

```python
from agentic import AgenticClient
client = AgenticClient(socket_path=os.environ["AGENTICD_SOCKET"])
# wire into Claude Agent SDK's checkpoint hooks per ADR-0004 Decision 3
```

## Failure semantics

Per ADR-0004 Decision 4 there is no degraded mode. If the sidecar is unreachable when the worker tries to write a checkpoint, the worker MUST fail the ticket loudly with the last-successful-checkpoint hash and the sidecar exit code.

## Out of scope

- Wire auth — none in v1.0 (ADR-0004 Decision 2).
- Multi-region failover — single-region in v1.0; document with design partners.
- Concurrent writers — `containerConcurrency: 1` is a hard assumption.

## Verification

1. `docker build -f crates/agenticd/Dockerfile.sidecar -t agenticd:sidecar .` succeeds.
2. `docker run --rm -v "$PWD/.shared:/shared" agenticd:sidecar --repo /shared/work --socket /shared/agenticd.sock --object-store fs:///shared/objects` binds the socket and serves a `Ping`.
3. Same as (2) but with `--object-store gcs://...` against `tests/fixtures/fake-gcs.yml` completes one commit-and-rollback cycle. Covered by the `gcs` CI job today.
