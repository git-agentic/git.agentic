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

from agentic import AgenticClient, AgenticError
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


def test_error_response_raises(short_tmp: Path):
    sock_path = short_tmp / "mock.sock"

    def handler(conn: socket.socket) -> None:
        envelope = read_frame(conn)
        write_frame(
            conn,
            {
                "correlation_id": envelope["correlation_id"],
                "payload": {"kind": "error", "message": "boom"},
            },
        )

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticError, match="boom"):
        client.ping()


def test_daemon_not_running_raises(short_tmp: Path):
    sock_path = short_tmp / "missing.sock"
    client = AgenticClient(socket_path=sock_path)
    with pytest.raises(AgenticError, match="daemon not reachable"):
        client.ping()


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
