"""Synchronous client for the local ``agenticd`` daemon.

The client opens a Unix socket, exchanges one length-prefixed JSON
envelope per request, and parses the typed response. Wire format is
identical to the Rust CLI (``crates/agentic-cli/src/client.rs``); both
speak ``crates/agentic-proto`` directly.

An async variant lives in :mod:`agentic.async_client` once the LangGraph
integration lands; for the MVP synchronous calls are enough.
"""

from __future__ import annotations

import itertools
import os
import socket
import threading
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional, cast

from ._framing import FrameError, read_frame, write_frame
from .types import Commit, Diff, LogEntry, RollbackPlan

DEFAULT_SOCKET_PATH = Path(os.environ.get("AGENTIC_SOCKET", ".agentic/agenticd.sock"))


class AgenticError(Exception):
    """Raised when the daemon returns ``Response::Error`` or transport fails."""


class AgenticClient:
    """Synchronous, thread-safe daemon client.

    Every method opens a fresh socket, sends one request, reads one
    response, closes. That trades a small per-call latency for trivial
    concurrency semantics — no per-instance lock, no pooling, no
    in-flight bookkeeping. For agent-PR / commit-frequency workloads
    this is the right default.
    """

    _default: "AgenticClient | None" = None
    _correlation_counter = itertools.count(1)
    _lock = threading.Lock()

    def __init__(self, socket_path: Path | str = DEFAULT_SOCKET_PATH) -> None:
        self.socket_path = Path(socket_path)

    @classmethod
    def default(cls) -> "AgenticClient":
        # Double-checked locking on the singleton. The outer fast path
        # avoids lock contention once initialised; the inner check
        # re-validates after acquiring so concurrent first callers don't
        # construct twice.
        if cls._default is None:
            with cls._lock:
                if cls._default is None:
                    cls._default = cls()
        return cls._default

    # ---------------- public surface ----------------

    def ping(self) -> bool:
        """Return True if the daemon replies with ``Pong``."""
        reply = self._request({"op": "ping"})
        return reply.get("kind") == "pong"

    def status(self) -> dict[str, Any]:
        """Resolve ``HEAD`` to its commit hash. Returns ``{"head": <hash>}``
        or ``{"head": None}`` on a repo with no commits yet."""
        try:
            reply = self._request({"op": "resolve_ref", "name": "HEAD"})
        except AgenticError as exc:
            if str(exc) == "ref not found: HEAD":
                return {"head": None}
            raise
        return {"head": reply.get("hash")}

    def resolve(self, name: str) -> Optional[str]:
        """Resolve a ref name to a commit hash, or ``None`` if not found.

        Only the daemon's "ref not found: <name>" is collapsed to
        ``None`` — transport failures and other errors propagate so
        callers can tell "missing ref" apart from "daemon is down".
        """
        try:
            reply = self._request({"op": "resolve_ref", "name": name})
        except AgenticError as exc:
            if str(exc) == f"ref not found: {name}":
                return None
            raise
        return reply.get("hash")

    def commit(
        self,
        *,
        message: str,
        prompts: dict[str, str] | None = None,
        tools: list[str] | None = None,
        model: Optional[str] = None,
        no_memory: bool = False,
        author: Optional[str] = None,
        code_sha: Optional[str] = None,
        branch: Optional[str] = None,
    ) -> Commit:
        payload = {
            "op": "commit",
            "message": message,
            "prompts": prompts or {},
            "mcp_servers": tools or [],
            "model": model,
            "no_memory": no_memory,
            "author": author,
            "code_sha": code_sha,
            "branch": branch,
        }
        reply = self._request(payload)
        return Commit(
            hash=reply["commit_hash"],
            branch=reply["branch"],
            parent=None,
            message=message,
            author=author or "unknown",
            timestamp=datetime.now(timezone.utc),
        )

    def log(self, *, limit: int = 20) -> list[LogEntry]:
        reply = self._request({"op": "log", "limit": limit})
        return [
            LogEntry(
                hash=e["hash"],
                message=e["message"],
                author=e["author"],
                timestamp=_parse_rfc3339(e["timestamp"]),
            )
            for e in reply.get("entries", [])
        ]

    def diff(self, *, from_ref: str, to_ref: str = "HEAD") -> Diff:
        reply = self._request({"op": "diff", "from": from_ref, "to": to_ref})
        return Diff(
            from_ref=reply["from"],
            to_ref=reply["to"],
            prompts=reply.get("prompts", []),
            tools=reply.get("tools", []),
            model_changed=reply.get("model_changed", False),
            memory_summary=reply.get("memory_summary", ""),
            schema_summary=reply.get("schema_summary", ""),
        )

    def rollback(
        self,
        *,
        target: str,
        dry_run: bool = False,
        accept_data_loss: bool = False,
    ) -> RollbackPlan:
        reply = self._request(
            {
                "op": "rollback",
                "target": target,
                "dry_run": dry_run,
                "accept_data_loss": accept_data_loss,
            }
        )
        return RollbackPlan(
            planned_steps=reply.get("planned_steps", []),
            executed=reply.get("executed", False),
            new_head_hash=reply.get("new_head_hash"),
        )

    # ---------------- transport ----------------

    def _request(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Send one envelope, read one, validate correlation, return the
        unwrapped ``Response`` dict. Raises :class:`AgenticError` on
        either transport failure or a ``Response::Error`` from the
        daemon."""
        correlation_id = self._next_correlation_id()
        envelope = {"correlation_id": correlation_id, "payload": payload}
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.connect(str(self.socket_path))
                write_frame(sock, envelope)
                reply = read_frame(sock)
        except (FileNotFoundError, ConnectionRefusedError) as e:
            raise AgenticError(
                f"daemon not reachable at {self.socket_path}; is `agenticd` running?"
            ) from e
        except FrameError as e:
            raise AgenticError(str(e)) from e
        except OSError as e:
            raise AgenticError(f"socket error: {e}") from e

        if reply.get("correlation_id") != correlation_id:
            raise AgenticError(
                f"correlation id mismatch: sent {correlation_id} got "
                f"{reply.get('correlation_id')!r}"
            )
        response = reply.get("payload", {})
        if response.get("kind") == "error":
            raise AgenticError(response.get("message", "daemon returned Error"))
        return cast(dict[str, Any], response)

    @classmethod
    def _next_correlation_id(cls) -> str:
        with cls._lock:
            n = next(cls._correlation_counter)
        return f"py-{os.getpid()}-{n}"


def _parse_rfc3339(s: str) -> datetime:
    """Parse the daemon's RFC 3339 timestamp string into an aware datetime.

    Python 3.10's ``datetime.fromisoformat`` rejects the trailing ``Z``
    that strict RFC 3339 emitters use. Rust's chrono ``to_rfc3339()``
    currently produces ``+00:00`` for UTC (which 3.10 accepts), but
    normalising defensively keeps us compatible with any RFC 3339
    encoder upstream might pick later.
    """
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)
