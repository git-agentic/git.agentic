"""agentic — the Python SDK for git.agentic.

This package is intentionally small. The interesting work happens in the
local `agenticd` daemon (a Rust binary); the SDK is a typed client.

MVP status: function signatures are defined and stubbed. Real daemon I/O
wires in over weeks 10–11 of the roadmap.
"""

from __future__ import annotations

from typing import Iterable, Mapping

from .client import AgenticClient, DEFAULT_SOCKET_PATH
from .types import Commit, Diff, LogEntry, RollbackPlan

__version__ = "0.1.0"

__all__ = [
    "__version__",
    "AgenticClient",
    "DEFAULT_SOCKET_PATH",
    "Commit",
    "Diff",
    "LogEntry",
    "RollbackPlan",
    "commit",
    "log",
    "diff",
    "rollback",
    "status",
]


def commit(
    *,
    message: str,
    prompts: Mapping[str, str] | None = None,
    tools: Iterable[str] | None = None,
    model: str | None = None,
    no_memory: bool = False,
) -> Commit:
    """Create a new commit capturing the current agent-state tuple.

    Equivalent to `agentic commit` from the shell. Uses the ambient daemon
    socket; configure via `AGENTIC_SOCKET` env var or
    `AgenticClient(...).commit(...)` for non-default paths.
    """
    return AgenticClient.default().commit(
        message=message,
        prompts=dict(prompts or {}),
        tools=list(tools or []),
        model=model,
        no_memory=no_memory,
    )


def log(limit: int = 20) -> list[LogEntry]:
    return AgenticClient.default().log(limit=limit)


def diff(from_ref: str, to_ref: str = "HEAD") -> Diff:
    return AgenticClient.default().diff(from_ref=from_ref, to_ref=to_ref)


def rollback(target: str, *, dry_run: bool = False, accept_data_loss: bool = False) -> RollbackPlan:
    return AgenticClient.default().rollback(
        target=target,
        dry_run=dry_run,
        accept_data_loss=accept_data_loss,
    )


def status() -> dict:
    return AgenticClient.default().status()
