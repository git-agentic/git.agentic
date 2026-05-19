"""The client that talks to `agenticd` over a Unix domain socket.

MVP transport: length-prefixed JSON. We'll move to protobuf in v1.1 once
the wire format stabilizes.

Real socket I/O lands in week 10. For now the client raises
`NotImplementedError` from every method so callers see a clear failure
mode while the engine is being built.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Optional

from .types import Commit, Diff, LogEntry, RollbackPlan

DEFAULT_SOCKET_PATH = Path(os.environ.get("AGENTIC_SOCKET", ".agentic/agenticd.sock"))


class AgenticClient:
    """Synchronous client. Async variant coming in week 10."""

    _default: "AgenticClient | None" = None

    def __init__(self, socket_path: Path | str = DEFAULT_SOCKET_PATH) -> None:
        self.socket_path = Path(socket_path)

    @classmethod
    def default(cls) -> "AgenticClient":
        if cls._default is None:
            cls._default = cls()
        return cls._default

    # ---------------- public surface ----------------

    def commit(
        self,
        *,
        message: str,
        prompts: dict[str, str],
        tools: list[str],
        model: Optional[str],
        no_memory: bool,
    ) -> Commit:
        raise NotImplementedError(
            "AgenticClient.commit lands in week 10. "
            "Until then, use the `agentic` CLI for end-to-end testing."
        )

    def log(self, *, limit: int) -> list[LogEntry]:
        raise NotImplementedError("AgenticClient.log lands in week 10")

    def diff(self, *, from_ref: str, to_ref: str) -> Diff:
        raise NotImplementedError("AgenticClient.diff lands in week 10")

    def rollback(self, *, target: str, dry_run: bool, accept_data_loss: bool) -> RollbackPlan:
        raise NotImplementedError("AgenticClient.rollback lands in week 10")

    def status(self) -> dict:
        raise NotImplementedError("AgenticClient.status lands in week 10")
