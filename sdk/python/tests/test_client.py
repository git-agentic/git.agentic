"""Tests for the Python SDK client.

Two layers:

* Framing + protocol tests use an in-process mock Unix server. They run
  everywhere — no Rust required.

* :func:`test_against_real_daemon` launches a real ``agenticd`` against
  a temporary repo and walks ping/init/commit/log/diff/rollback through
  the SDK. Gated by ``AGENTICD_BIN``; skipped otherwise.
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time
from pathlib import Path

import pytest

from agentic import (
    AgenticClient,
    AgenticConcurrencyError,
    AgenticError,
    AgenticInternalError,
    AgenticMemoryError,
    AgenticNotFoundError,
    AgenticProtocolError,
    AgenticStorageError,
    AgenticValidationError,
)
from agentic._framing import read_frame, write_frame


@pytest.fixture
def short_tmp():
    """Like ``tmp_path`` but rooted under ``/tmp`` so Unix-socket paths
    fit in AF_UNIX's 108-byte sun_path limit (macOS default tmp dirs
    overflow it)."""
    d = tempfile.mkdtemp(prefix="agentic-test-", dir="/tmp")
    try:
        yield Path(d)
    finally:
        shutil.rmtree(d, ignore_errors=True)


# ---------------------------------------------------------------------------
# Framing unit tests — mock Unix server, no daemon required.
# ---------------------------------------------------------------------------


def _spawn_mock_daemon(socket_path: Path, handler) -> threading.Thread:
    """Start a one-shot Unix server that accepts a single connection,
    invokes ``handler(conn)``, then exits."""
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(socket_path))
    server.listen(1)

    def serve():
        try:
            conn, _ = server.accept()
            try:
                handler(conn)
            finally:
                conn.close()
        finally:
            server.close()

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    return t


def test_framing_roundtrip(short_tmp: Path):
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {"kind": "pong"},
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    assert client.ping() is True


def test_correlation_mismatch_raises(short_tmp: Path):
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"] + "-tampered",
                "payload": {"kind": "pong"},
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticError, match="correlation id mismatch"):
        client.ping()


def test_error_response_raises_class_specific(short_tmp: Path):
    """ADR-0010 Decision 1: the SDK raises a class-specific subclass of
    AgenticError so callers can route on it without substring-matching."""
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {
                    "kind": "error",
                    "class": "not_found",
                    "code": "ref_not_found",
                    "message": "ref not found: foo",
                    "retryable": False,
                },
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticNotFoundError) as excinfo:
        client.ping()
    # Subclass relationship preserved for catch-all callers.
    assert isinstance(excinfo.value, AgenticError)
    assert excinfo.value.code == "ref_not_found"
    assert excinfo.value.retryable is False
    assert "ref not found: foo" in str(excinfo.value)


def test_retryable_attribute_surfaces(short_tmp: Path):
    """ADR-0010 Decision 1: the retryable flag is the load-bearing hint
    AgenticSessionStore reads to decide whether to back off and retry."""
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {
                    "kind": "error",
                    "class": "concurrency",
                    "code": "commit_lock_busy",
                    "message": "another commit in progress",
                    "retryable": True,
                },
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticConcurrencyError) as excinfo:
        client.ping()
    assert excinfo.value.retryable is True


def test_unknown_class_falls_back_to_internal(short_tmp: Path):
    """Forward-compat: an ErrorClass added in a future ADR that this SDK
    hasn't seen yet should still surface as a subclass of AgenticError."""
    from agentic import AgenticInternalError

    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {
                    "kind": "error",
                    "class": "future_class_not_yet_known",
                    "code": "x",
                    "message": "hi",
                    "retryable": False,
                },
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticInternalError):
        client.ping()


def test_envelope_carries_protocol_version(short_tmp: Path):
    """ADR-0010 Decision 5: every outbound envelope carries
    protocol_version=1 so the daemon doesn't route through the v0
    coexistence shim."""
    sock_path = short_tmp / "mock.sock"
    captured: dict[str, object] = {}

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        captured["envelope"] = envelope
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {"kind": "pong"},
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    client.ping()
    assert captured["envelope"]["protocol_version"] == 1


def test_commit_base64_encodes_prompts(short_tmp: Path):
    """ADR-0010 Decision 3: prompts cross the wire as base64-encoded
    bytes. The SDK accepts str or bytes from callers and encodes."""
    import base64

    sock_path = short_tmp / "mock.sock"
    captured: dict[str, object] = {}

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        captured["payload"] = envelope["payload"]
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {
                    "kind": "commit",
                    "commit_hash": "f" * 64,
                    "branch": "main",
                },
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    client.commit(
        message="hi",
        prompts={"system.md": "you are helpful", "blob.bin": b"\x00\x01\xff"},
        no_memory=True,
    )
    sent = captured["payload"]["prompts"]
    assert sent["system.md"] == base64.b64encode(b"you are helpful").decode("ascii")
    assert sent["blob.bin"] == base64.b64encode(b"\x00\x01\xff").decode("ascii")


def test_daemon_not_running_raises_protocol_error(short_tmp: Path):
    """Transport failures route through AgenticProtocolError so callers can
    write `except AgenticProtocolError:` and catch wire-level issues as a
    class. AgenticError subclass relationship is preserved."""
    sock_path = short_tmp / "missing.sock"
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticProtocolError) as excinfo:
        client.ping()
    assert isinstance(excinfo.value, AgenticError)
    assert excinfo.value.retryable is True
    assert excinfo.value.code == "daemon_unreachable"


@pytest.mark.parametrize(
    "class_token,expected_exc",
    [
        ("protocol", AgenticProtocolError),
        ("validation", AgenticValidationError),
        ("not_found", AgenticNotFoundError),
        ("storage", AgenticStorageError),
        ("memory", AgenticMemoryError),
        ("concurrency", AgenticConcurrencyError),
        ("internal", AgenticInternalError),
    ],
)
def test_error_class_token_routes_to_subclass(
    short_tmp: Path, class_token: str, expected_exc: type[AgenticError]
):
    """Every ErrorClass token from the ADR-0010 taxonomy must route to
    its dedicated subclass. A typo in the dispatch table would silently
    promote callers to AgenticInternalError; this parametrised test
    catches that."""
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {
                    "kind": "error",
                    "class": class_token,
                    "code": "x",
                    "message": "hi",
                    "retryable": False,
                },
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(expected_exc):
        client.ping()


def test_concurrency_error_forces_retryable_true_even_if_daemon_says_false(
    short_tmp: Path,
):
    """ADR-0010 Decision 2 names Concurrency as always-retryable. A
    malformed daemon response (or future-version daemon that drops the
    invariant) must not make AgenticSessionStore stop retrying transient
    contention."""
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {
                    "kind": "error",
                    "class": "concurrency",
                    "code": "lock_busy",
                    "message": "lock held",
                    "retryable": False,
                },
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticConcurrencyError) as excinfo:
        client.ping()
    assert excinfo.value.retryable is True, (
        "Concurrency errors must remain retryable even if the wire says otherwise"
    )


def test_retryable_strict_truthiness(short_tmp: Path):
    """The SDK uses `is True` rather than `bool(...)` so a daemon (or
    future protocol version) sending retryable as the JSON string
    "false" doesn't silently flip into retry-forever behaviour."""
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {
                    "kind": "error",
                    "class": "storage",
                    "code": "x",
                    "message": "hi",
                    "retryable": "false",  # ← JSON string, not boolean
                },
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticStorageError) as excinfo:
        client.ping()
    assert excinfo.value.retryable is False, (
        "non-boolean retryable values must not be coerced to True"
    )


def test_frame_format_matches_rust():
    """Hand-roll a known frame and confirm the SDK reads it. This is the
    cross-implementation contract: 4-byte BE length + UTF-8 JSON bytes."""
    payload = {"correlation_id": "py-test-1", "payload": {"kind": "pong"}}
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    framed = struct.pack(">I", len(body)) + body

    a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    a.sendall(framed)
    a.close()
    got = read_frame(b)
    b.close()
    assert got == payload


# ---------------------------------------------------------------------------
# Integration: real daemon. Skipped unless AGENTICD_BIN is set.
# ---------------------------------------------------------------------------


def _daemon_bin() -> Path | None:
    raw = os.environ.get("AGENTICD_BIN")
    if not raw:
        return None
    p = Path(raw)
    return p if p.exists() else None


@pytest.mark.skipif(
    _daemon_bin() is None,
    reason="set AGENTICD_BIN=/path/to/agenticd to run the integration test",
)
def test_against_real_daemon(short_tmp: Path):
    bin_path = _daemon_bin()
    assert bin_path is not None

    repo = short_tmp / "repo"
    (repo / "prompts").mkdir(parents=True)
    (repo / "prompts" / "system.txt").write_text("you are helpful\n")

    agentic_dir = repo / ".agentic"
    agentic_dir.mkdir()
    sock_path = agentic_dir / "agenticd.sock"

    proc = subprocess.Popen(
        [str(bin_path), "--repo", str(repo)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        deadline = time.time() + 5.0
        while time.time() < deadline:
            if sock_path.exists():
                break
            if proc.poll() is not None:
                stderr = (proc.stderr.read() or b"").decode()
                pytest.fail(f"agenticd exited early: {stderr}")
            time.sleep(0.05)
        else:
            pytest.fail(f"agenticd did not create {sock_path} in 5s")

        client = AgenticClient(socket_path=sock_path)
        assert client.ping() is True

        commit_a = client.commit(
            message="initial",
            prompts={"system.txt": "you are helpful\n"},
            model="anthropic:claude-opus:2026-05-01",
            no_memory=True,
        )
        assert len(commit_a.hash) == 64
        assert commit_a.branch == "main"

        commit_b = client.commit(
            message="tweak",
            prompts={"system.txt": "you are extra helpful\n"},
            model="anthropic:claude-opus:2026-05-01",
            no_memory=True,
        )
        assert commit_b.hash != commit_a.hash

        log = client.log(limit=10)
        assert [e.message for e in log[:2]] == ["tweak", "initial"]

        d = client.diff(from_ref=commit_a.hash, to_ref=commit_b.hash)
        assert d.from_ref == commit_a.hash
        assert d.to_ref == commit_b.hash
        assert any("system.txt" in line for line in d.prompts)

        plan = client.rollback(target=commit_a.hash, dry_run=True)
        assert plan.executed is False
        assert any(commit_a.hash[:7] in step for step in plan.planned_steps)

        applied = client.rollback(target=commit_a.hash)
        assert applied.executed is True
        assert applied.new_head_hash and len(applied.new_head_hash) == 64

        # Prompt should be restored to baseline content on disk.
        assert (repo / "prompts" / "system.txt").read_text() == "you are helpful\n"
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(repo, ignore_errors=True)
