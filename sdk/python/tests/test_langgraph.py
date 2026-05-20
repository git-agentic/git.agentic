"""Tests for the LangGraph integration.

Three layers:

* Unit tests on the envelope serialiser run everywhere — no daemon and
  no LangGraph runtime required.
* :func:`test_checkpointer_against_real_daemon` exercises put +
  get_tuple against a real ``agenticd``; gated by ``AGENTICD_BIN``.
* :func:`test_real_graph_runs_against_checkpointer` compiles a tiny
  ``StateGraph`` with our checkpointer, runs two invocations on the
  same thread, asserts the second sees the first's state.
"""

import os
import shutil
import subprocess
import tempfile
import time
from operator import add
from pathlib import Path
from typing import Annotated, Any, TypedDict

import pytest

from agentic import AgenticClient
from agentic.langgraph import (
    AgenticCheckpointer,
    _branch_for_thread,
    _checkpoint_blob_path,
    _deserialise_envelope,
    _serialise_envelope,
)

from langgraph.checkpoint.serde.jsonplus import JsonPlusSerializer


@pytest.fixture
def short_tmp():
    """Avoid macOS /private/var paths overflowing the AF_UNIX 108-byte
    limit (same trick as test_client.py)."""
    d = tempfile.mkdtemp(prefix="agentic-lg-test-", dir="/tmp")
    try:
        yield Path(d)
    finally:
        shutil.rmtree(d, ignore_errors=True)


# ============================================================ unit tests


def test_envelope_roundtrip():
    serde = JsonPlusSerializer()
    ckpt = {
        "v": 4,
        "id": "c0",
        "ts": "2026-05-20T00:00:00Z",
        "channel_values": {"x": 1, "msg": "hello"},
        "channel_versions": {"x": "1"},
        "versions_seen": {},
        "updated_channels": ["x"],
    }
    meta = {
        "source": "input",
        "step": -1,
        "parents": {},
        "run_id": "r0",
        "counters_since_delta_snapshot": {},
    }
    env = _serialise_envelope(serde, ckpt, meta, "u42")
    assert env["v"] == 1
    assert env["thread_id"] == "u42"
    ckpt2, meta2 = _deserialise_envelope(serde, env)
    assert ckpt2 == ckpt
    assert meta2 == meta


def test_thread_id_to_branch_is_stable():
    a = _branch_for_thread("user-42")
    b = _branch_for_thread("user-42")
    c = _branch_for_thread("user-43")
    assert a == b
    assert a != c
    assert a.startswith("langgraph/")
    assert _checkpoint_blob_path("user-42").startswith("__langgraph__/")


def test_envelope_rejects_unknown_version():
    serde = JsonPlusSerializer()
    bogus = {
        "v": 99,
        "thread_id": "x",
        "checkpoint": {"type": "msgpack", "data_b64": ""},
        "metadata": {"type": "msgpack", "data_b64": ""},
    }
    with pytest.raises(ValueError, match="unsupported langgraph envelope"):
        _deserialise_envelope(serde, bogus)


# ===================================================== integration: daemon


def _daemon_bin() -> Path | None:
    raw = os.environ.get("AGENTICD_BIN")
    if not raw:
        return None
    p = Path(raw)
    return p if p.exists() else None


def _spawn_daemon(repo: Path, bin_path: Path):
    """Launch a real agenticd against ``repo`` and wait for the socket
    to appear. Returns ``(process, AgenticClient)``."""
    (repo / "prompts").mkdir(parents=True, exist_ok=True)
    (repo / ".agentic").mkdir(parents=True, exist_ok=True)
    sock = repo / ".agentic" / "agenticd.sock"
    proc = subprocess.Popen(
        [str(bin_path), "--repo", str(repo)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    deadline = time.time() + 5.0
    while time.time() < deadline:
        if sock.exists():
            break
        if proc.poll() is not None:
            stderr = (proc.stderr.read() or b"").decode()
            raise RuntimeError(f"agenticd exited early: {stderr}")
        time.sleep(0.05)
    else:
        proc.terminate()
        try:
            proc.wait(timeout=1.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        raise RuntimeError(f"agenticd did not create {sock} in 5s")
    return proc, AgenticClient(socket_path=sock)


@pytest.mark.skipif(
    _daemon_bin() is None,
    reason="set AGENTICD_BIN=/path/to/agenticd to run integration test",
)
def test_checkpointer_against_real_daemon(short_tmp: Path):
    repo = short_tmp / "repo"
    bin_path = _daemon_bin()
    assert bin_path is not None
    proc, client = _spawn_daemon(repo, bin_path)
    try:
        cp = AgenticCheckpointer(client=client, repo=repo)
        config = {"configurable": {"thread_id": "user-42"}}

        ckpt = {
            "v": 4,
            "id": "c0",
            "ts": "2026-05-20T00:00:00Z",
            "channel_values": {"counter": 1},
            "channel_versions": {"counter": "1"},
            "versions_seen": {},
            "updated_channels": ["counter"],
        }
        meta = {
            "source": "input",
            "step": -1,
            "parents": {},
            "run_id": "r0",
            "counters_since_delta_snapshot": {},
        }

        # First put creates the thread branch.
        cfg1 = cp.put(config, ckpt, meta, new_versions={"counter": "1"})
        assert "checkpoint_id" in cfg1["configurable"]
        assert len(cfg1["configurable"]["checkpoint_id"]) == 64

        # On-disk blob exists.
        blob = repo / "prompts" / _checkpoint_blob_path("user-42")
        assert blob.exists()

        # get_tuple round-trip.
        got = cp.get_tuple(config)
        assert got is not None
        assert got.checkpoint == ckpt
        assert got.metadata == meta
        assert got.config["configurable"]["checkpoint_id"] == cfg1["configurable"]["checkpoint_id"]

        # Second put extends the branch.
        ckpt2 = {**ckpt, "id": "c1", "channel_values": {"counter": 2}}
        meta2 = {**meta, "step": 0, "source": "loop"}
        cfg2 = cp.put(config, ckpt2, meta2, new_versions={"counter": "2"})
        assert cfg2["configurable"]["checkpoint_id"] != cfg1["configurable"]["checkpoint_id"]

        # The branch tip is the second commit; get_tuple sees ckpt2.
        got2 = cp.get_tuple(config)
        assert got2 is not None
        assert got2.checkpoint == ckpt2

        # Log shows two commits on the thread branch.
        log = client.log(limit=10)
        thread_commits = [
            e for e in log if "langgraph step" in e.message and e.author == "langgraph"
        ]
        assert len(thread_commits) == 2

        # Requesting a different checkpoint_id raises (documented limit).
        prev_cfg = {
            "configurable": {
                "thread_id": "user-42",
                "checkpoint_id": cfg1["configurable"]["checkpoint_id"],
            }
        }
        with pytest.raises(NotImplementedError):
            cp.get_tuple(prev_cfg)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()


# Module-level State so LangGraph's get_type_hints can resolve
# `Annotated`/`add` from this module's globals. Defining the class
# inside the test function fails with `NameError: name 'Annotated' is
# not defined` because LangGraph evaluates the forward refs against the
# class's __globals__, which for a locally-defined class is empty.
class _GraphState(TypedDict):
    counter: Annotated[int, add]


def _graph_bump(state: _GraphState) -> dict[str, Any]:
    return {"counter": 1}


@pytest.mark.skipif(
    _daemon_bin() is None,
    reason="set AGENTICD_BIN=/path/to/agenticd to run integration test",
)
def test_real_graph_runs_against_checkpointer(short_tmp: Path):
    """Compile a tiny StateGraph with our checkpointer and verify the
    invoke pipeline (which calls put internally) commits + resumes."""
    from langgraph.graph import END, START, StateGraph

    repo = short_tmp / "repo"
    proc, client = _spawn_daemon(repo, _daemon_bin())
    try:
        cp = AgenticCheckpointer(client=client, repo=repo)

        graph = StateGraph(_GraphState)
        graph.add_node("bump", _graph_bump)
        graph.add_edge(START, "bump")
        graph.add_edge("bump", END)
        app = graph.compile(checkpointer=cp)

        config = {"configurable": {"thread_id": "graph-test"}}
        out1 = app.invoke({"counter": 0}, config=config)
        assert out1 == {"counter": 1}

        # Run again on the same thread — state accumulates because of
        # the Annotated[int, add] reducer.
        out2 = app.invoke({"counter": 0}, config=config)
        assert out2 == {"counter": 2}

        # The latest checkpoint reflects accumulated state.
        head_cfg = {"configurable": {"thread_id": "graph-test"}}
        tup = cp.get_tuple(head_cfg)
        assert tup is not None
        assert tup.checkpoint["channel_values"]["counter"] == 2
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
