"""Typed data classes mirroring the daemon's wire protocol."""

from __future__ import annotations

from datetime import datetime
from typing import Optional

from pydantic import BaseModel


class Commit(BaseModel):
    hash: str
    branch: str
    parent: Optional[str] = None
    message: str
    author: str
    timestamp: datetime


class LogEntry(BaseModel):
    hash: str
    message: str
    author: str
    timestamp: datetime


class Diff(BaseModel):
    from_ref: str
    to_ref: str
    prompts: list[str]
    tools: list[str]
    model_changed: bool
    memory_summary: str
    schema_summary: str


class RollbackPlan(BaseModel):
    planned_steps: list[str]
    executed: bool
    new_head_hash: Optional[str] = None
