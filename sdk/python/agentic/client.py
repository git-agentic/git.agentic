"""Synchronous client for the local ``agenticd`` daemon.

The client opens a Unix socket, exchanges one length-prefixed JSON
envelope per request, and parses the typed response. Wire format is
identical to the Rust CLI (``crates/agentic-cli/src/client.rs``); both
speak ``crates/agentic-proto`` directly.

An async variant lives in :mod:`agentic.async_client` once the LangGraph
integration lands; for the MVP synchronous calls are enough.
"""

from __future__ import annotations

import base64
import itertools
import os
import socket
import threading
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

from ._framing import FrameError, read_frame, write_frame
from .types import Commit, Diff, LogEntry, RollbackPlan

DEFAULT_SOCKET_PATH = Path(os.environ.get("AGENTIC_SOCKET", ".agentic/agenticd.sock"))

# Wire protocol version this SDK speaks. Bumped by ADRs that change the
# Envelope, Request, Response, or any nested type's wire shape.
# See `docs/adr/0010-wire-protocol-error-model.md`.
PROTOCOL_VERSION = 1


class AgenticError(Exception):
    """Raised when the daemon returns ``Response::Error`` or transport fails.

    The class hierarchy mirrors ADR-0010's ``ErrorClass`` taxonomy.
    Callers can catch the broad ``AgenticError`` to handle any daemon
    failure, or one of the class-specific subclasses
    (:class:`AgenticNotFoundError`, :class:`AgenticStorageError`, etc.)
    to make routing decisions.

    The ``retryable`` attribute is the load-bearing hint:
    ``AgenticSessionStore``'s retry loop reads it directly to decide
    whether to back off and retry vs surface the failure.
    """

    code: str = ""
    retryable: bool = False
    # ErrorClass tag from the wire; populated on raise. Public attribute
    # because forward-compat callers (a Python SDK at version N talking
    # to a daemon at version N+1 that introduced a new ErrorClass) need
    # to read it off the AgenticInternalError fallback to recover the
    # original tag.
    class_token: str = ""

    def __init__(
        self,
        message: str,
        *,
        code: str = "",
        retryable: bool = False,
        class_token: str = "",
    ) -> None:
        super().__init__(message)
        self.code = code
        self.retryable = retryable
        self.class_token = class_token


class AgenticProtocolError(AgenticError):
    """Wire-level: framing, version, malformed envelope, oversize frame."""


class AgenticValidationError(AgenticError):
    """Input validation rejected by the daemon."""


class AgenticNotFoundError(AgenticError):
    """Semantic absence: ref / commit / migration not found."""


class AgenticStorageError(AgenticError):
    """Object store, refs, filesystem. Often retryable."""


class AgenticMemoryError(AgenticError):
    """Postgres backend failure. ``retryable`` discriminates per occurrence."""


class AgenticConcurrencyError(AgenticError):
    """Daemon-internal serialisation. Always retryable per ADR-0010 D2."""

    def __init__(
        self,
        message: str,
        *,
        code: str = "",
        retryable: bool = True,
        class_token: str = "concurrency",
    ) -> None:
        # ADR-0010 Decision 2 names Concurrency as always-retryable. If
        # a malformed daemon response sets retryable=false on a
        # Concurrency-class error, override it loudly here: the alternative
        # is silently making AgenticSessionStore stop retrying transient
        # contention.
        super().__init__(
            message,
            code=code,
            retryable=True,
            class_token=class_token,
        )
        # The caller-supplied `retryable` is intentionally ignored; the
        # parent constructor has already pinned `self.retryable = True`.
        # Assert the invariant here so a refactor that re-introduces a
        # silent override is caught at construction time, not when an
        # agent run silently stops retrying.
        assert self.retryable is True, "Concurrency-class errors must remain retryable"


class AgenticInternalError(AgenticError):
    """Last-resort. Treat as non-retryable until reclassified upstream."""


_ERROR_CLASS_TO_EXCEPTION: dict[str, type[AgenticError]] = {
    "protocol": AgenticProtocolError,
    "validation": AgenticValidationError,
    "not_found": AgenticNotFoundError,
    "storage": AgenticStorageError,
    "memory": AgenticMemoryError,
    "concurrency": AgenticConcurrencyError,
    "internal": AgenticInternalError,
}


def _raise_from_error_response(response: dict[str, Any]) -> None:
    """Raise the class-specific :class:`AgenticError` subclass that matches
    the daemon's structured error response. Falls back to
    :class:`AgenticInternalError` if the daemon sent an unrecognised
    ``class`` field (forward-compat: an ErrorClass added in a future ADR
    that this SDK hasn't seen yet)."""
    class_token = response.get("class", "internal")
    code = response.get("code", "")
    message = response.get("message", "daemon returned Error")
    # Strict truthiness so a daemon (or future-version client) sending
    # retryable as the JSON string "false" doesn't silently flip into
    # retry-forever behaviour. JSON booleans deserialise as Python bools;
    # anything else is treated as not-retryable.
    raw_retryable = response.get("retryable", False)
    retryable = raw_retryable is True
    exc_cls = _ERROR_CLASS_TO_EXCEPTION.get(class_token, AgenticInternalError)
    raise exc_cls(
        message,
        code=code,
        retryable=retryable,
        class_token=class_token,
    )


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

    def __init__(
        self,
        socket_path: Path | str = DEFAULT_SOCKET_PATH,
        *,
        connect_timeout: float = 5.0,
        request_timeout: float = 30.0,
    ) -> None:
        self.socket_path = Path(socket_path)
        self.connect_timeout = connect_timeout
        self.request_timeout = request_timeout

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
        except AgenticNotFoundError:
            return {"head": None}
        return {"head": reply.get("hash")}

    def resolve(self, name: str) -> Optional[str]:
        """Resolve a ref name to a commit hash, or ``None`` if not found.

        A daemon-side ``not_found`` class error is collapsed to ``None``;
        transport failures and other error classes propagate so callers
        can tell "missing ref" apart from "daemon is down".
        """
        try:
            reply = self._request({"op": "resolve_ref", "name": name})
        except AgenticNotFoundError:
            return None
        return reply.get("hash")

    def commit(
        self,
        *,
        message: str,
        prompts: dict[str, str | bytes] | None = None,
        tools: list[str] | None = None,
        model: Optional[str] = None,
        no_memory: bool = False,
        author: Optional[str] = None,
        code_sha: Optional[str] = None,
        branch: Optional[str] = None,
    ) -> Commit:
        # ADR-0010 Decision 3: prompts cross the wire as base64-encoded
        # bytes. Accept either ``str`` (encoded as UTF-8 first) or
        # ``bytes`` (passed straight through) so the typed-Python API
        # surface still lets callers write text without thinking about
        # encoding.
        encoded_prompts: dict[str, str] = {}
        for name, body in (prompts or {}).items():
            if isinstance(body, str):
                body_bytes = body.encode("utf-8")
            else:
                body_bytes = bytes(body)
            encoded_prompts[name] = base64.b64encode(body_bytes).decode("ascii")
        payload = {
            "op": "commit",
            "message": message,
            "prompts": encoded_prompts,
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
        envelope = {
            "correlation_id": correlation_id,
            "protocol_version": PROTOCOL_VERSION,
            "payload": payload,
        }
        # Wire-level failures (transport, framing, correlation mismatch,
        # malformed payload) raise AgenticProtocolError so callers can
        # write `except AgenticProtocolError` and catch them as a class.
        # Inheritance from AgenticError is preserved for catch-all sites.
        # Tracks which timeout bound is in force so a TimeoutError can name
        # the number that actually applied: connect() is bounded by
        # connect_timeout, everything after by request_timeout. Without
        # this a timeout during connect() would misreport request_timeout,
        # naming the wrong deadline in the error message.
        phase_timeout: float = self.connect_timeout
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.settimeout(self.connect_timeout)
                sock.connect(str(self.socket_path))
                phase_timeout = self.request_timeout
                sock.settimeout(self.request_timeout)
                write_frame(sock, envelope)
                reply = read_frame(sock)
        except (FileNotFoundError, ConnectionRefusedError) as e:
            raise AgenticProtocolError(
                f"daemon not reachable at {self.socket_path}; is `agenticd` running?",
                code="daemon_unreachable",
                retryable=True,
                class_token="protocol",
            ) from e
        except FrameError as e:
            raise AgenticProtocolError(
                str(e),
                code="framing_error",
                retryable=False,
                class_token="protocol",
            ) from e
        except TimeoutError as e:
            raise AgenticProtocolError(
                f"daemon socket operation timed out after {phase_timeout}s at "
                f"{self.socket_path} (per-operation idle timeout, not a total "
                f"deadline); the daemon may be stalled — see its log",
                code="timeout",
                retryable=True,
                class_token="protocol",
            ) from e
        except OSError as e:
            raise AgenticProtocolError(
                f"socket error: {e}",
                code="socket_error",
                retryable=True,
                class_token="protocol",
            ) from e

        if reply.get("correlation_id") != correlation_id:
            raise AgenticProtocolError(
                f"correlation id mismatch: sent {correlation_id} got "
                f"{reply.get('correlation_id')!r}",
                code="correlation_mismatch",
                retryable=False,
                class_token="protocol",
            )
        response = reply.get("payload", {})
        if not isinstance(response, dict):
            raise AgenticProtocolError(
                f"daemon returned non-dict payload ({type(response).__name__!r}); "
                "protocol version mismatch?",
                code="malformed_response",
                retryable=False,
                class_token="protocol",
            )
        if response.get("kind") == "error":
            _raise_from_error_response(response)
        return response

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
