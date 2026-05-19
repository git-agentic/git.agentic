"""LangGraph integration: drop-in checkpointer that commits the agent
state tuple on every graph step.

The real implementation extends `langgraph.checkpoint.base.BaseCheckpointSaver`
and translates its `put` / `get_tuple` / `list` calls into agenticd commits
and lookups. We ship this in week 10 of the roadmap.

For now this module exposes the class with stubbed methods so downstream
imports don't break during scaffolding.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any


class AgenticCheckpointer:
    """A LangGraph checkpointer backed by `agenticd`.

    Usage::

        from agentic.langgraph import AgenticCheckpointer

        graph = StateGraph(...)
        app = graph.compile(checkpointer=AgenticCheckpointer(repo=".agentic"))
    """

    def __init__(self, repo: str | Path = ".agentic") -> None:
        self.repo = Path(repo)

    # LangGraph BaseCheckpointSaver interface — fully wired in week 10.

    def put(self, *args: Any, **kwargs: Any) -> Any:
        raise NotImplementedError("AgenticCheckpointer.put lands in week 10")

    def get_tuple(self, *args: Any, **kwargs: Any) -> Any:
        raise NotImplementedError("AgenticCheckpointer.get_tuple lands in week 10")

    def list(self, *args: Any, **kwargs: Any) -> Any:
        raise NotImplementedError("AgenticCheckpointer.list lands in week 10")
