"""LangGraph integration: drop-in checkpointer backed by ``agenticd``.

Every ``put`` becomes a Commit on a per-thread branch; ``get_tuple``
re-reads the most recent Checkpoint. The Checkpoint payload is encoded
through LangGraph's ``JsonPlusSerializer`` (msgpack under the hood) and
base64-wrapped into a single JSON blob the daemon stores under the
commit's ``prompts`` tree at ``__langgraph__/<thread-hash>/checkpoint.json``.

Usage::

    from langgraph.graph import StateGraph
    from agentic import AgenticClient
    from agentic.langgraph import AgenticCheckpointer

    client = AgenticClient(socket_path=".agentic/agenticd.sock")
    cp = AgenticCheckpointer(client=client, repo=".")
    app = graph.compile(checkpointer=cp)
    app.invoke({"input": "..."}, config={"configurable": {"thread_id": "u42"}})

Then ``agentic log`` shows one commit per graph step, ``agentic diff``
shows the checkpoint deltas, and ``agentic rollback <ref>`` restores
both the prompts and (if memory is attached) the agent's memory state
to that point.

MVP scope
---------

The full checkpointer surface is ``put`` / ``get_tuple`` / ``list`` /
``put_writes`` plus async variants. This module ships the resume cycle
the broken-prompt demo needs:

* ``put`` serialises + commits and returns a checkpoint-id config.
* ``get_tuple`` returns the latest checkpoint for the thread by reading
  the on-disk blob (and reports the commit hash as ``checkpoint_id``).
* Specific-``checkpoint_id`` lookups, time-travel ``list``, and the
  ``put_writes`` accumulator land alongside a daemon ``ReadBlob`` RPC
  in a follow-up. They raise / no-op explicitly today so callers see a
  predictable failure rather than silent breakage.
"""

from __future__ import annotations

import base64
import hashlib
import json
import logging
from pathlib import Path
from typing import Any, Iterator, Optional, Sequence

from langchain_core.runnables import RunnableConfig
from langgraph.checkpoint.base import (
    BaseCheckpointSaver,
    ChannelVersions,
    Checkpoint,
    CheckpointMetadata,
    CheckpointTuple,
)
from langgraph.checkpoint.serde.jsonplus import JsonPlusSerializer

from .client import AgenticClient

_log = logging.getLogger(__name__)

#: Magic blob path under the commit's ``prompts`` tree. Deliberately
#: distinctive so a human inspecting ``agentic show <ref>`` can tell
#: this is a LangGraph artefact rather than a user prompt.
CHECKPOINT_BLOB_PREFIX = "__langgraph__"

#: Bumped when the on-disk envelope format breaks compatibility.
ENVELOPE_VERSION = 1


class AgenticCheckpointer(BaseCheckpointSaver):
    """A LangGraph checkpointer backed by ``agenticd``.

    One branch per ``thread_id``. One commit per checkpoint. The
    Checkpoint + metadata are serialised through
    :class:`JsonPlusSerializer`, base64-wrapped, and persisted as a
    single blob under the commit's ``prompts`` tree.
    """

    def __init__(
        self,
        *,
        client: AgenticClient | None = None,
        repo: str | Path = ".",
    ) -> None:
        super().__init__()
        self.client = client or AgenticClient.default()
        self.repo = Path(repo)
        self.serde = JsonPlusSerializer()

    # ------------------------------------------------------------------ put

    def put(
        self,
        config: RunnableConfig,
        checkpoint: Checkpoint,
        metadata: CheckpointMetadata,
        new_versions: ChannelVersions,
    ) -> RunnableConfig:
        thread_id = _require_thread_id(config)
        branch = _branch_for_thread(thread_id)

        envelope = _serialise_envelope(self.serde, checkpoint, metadata, thread_id)
        blob_path = _checkpoint_blob_path(thread_id)
        envelope_json = json.dumps(envelope, separators=(",", ":"))

        # Mirror the envelope to disk so ``get_tuple`` (and ``agentic
        # rollback``) can read it back without a ReadBlob RPC. This is
        # the same path the daemon writes prompts to on rollback, so
        # restore-and-resume "just works".
        on_disk = self.repo / "prompts" / blob_path
        on_disk.parent.mkdir(parents=True, exist_ok=True)
        on_disk.write_text(envelope_json)

        commit = self.client.commit(
            message=f"langgraph step {metadata.get('step', '?')} ({metadata.get('source', '?')})",
            prompts={blob_path: envelope_json},
            no_memory=True,
            branch=branch,
            author="langgraph",
        )

        return {
            "configurable": {
                "thread_id": thread_id,
                "checkpoint_ns": _ns(config),
                "checkpoint_id": commit.hash,
            }
        }

    # ------------------------------------------------------------ get_tuple

    def get_tuple(self, config: RunnableConfig) -> Optional[CheckpointTuple]:
        thread_id = _require_thread_id(config)
        requested_id = config.get("configurable", {}).get("checkpoint_id")
        branch = _branch_for_thread(thread_id)

        head = self.client.resolve(branch)
        if head is None:
            return None

        if requested_id and requested_id != head:
            # Historical lookup needs a daemon `ReadBlob` op — coming
            # in a follow-up PR. Make the limit explicit instead of
            # silently returning the wrong thing.
            raise NotImplementedError(
                "AgenticCheckpointer does not yet support resuming from a "
                "specific checkpoint_id other than the branch head. "
                "Use `agentic rollback <ref>` to move the branch tip, then "
                "call get_tuple without a checkpoint_id."
            )

        blob_path = self.repo / "prompts" / _checkpoint_blob_path(thread_id)
        if not blob_path.exists():
            _log.warning(
                "branch %s has head %s but no on-disk checkpoint at %s; "
                "checkpoint was committed without disk mirror or the "
                "working tree was wiped",
                branch,
                head,
                blob_path,
            )
            return None

        envelope = json.loads(blob_path.read_text())
        checkpoint, metadata = _deserialise_envelope(self.serde, envelope)

        return CheckpointTuple(
            config={
                "configurable": {
                    "thread_id": thread_id,
                    "checkpoint_ns": _ns(config),
                    "checkpoint_id": head,
                }
            },
            checkpoint=checkpoint,
            metadata=metadata,
            parent_config=None,
            pending_writes=None,
        )

    # ----------------------------------------------------------------- list

    def list(
        self,
        config: Optional[RunnableConfig],
        *,
        filter: Optional[dict[str, Any]] = None,
        before: Optional[RunnableConfig] = None,
        limit: Optional[int] = None,
    ) -> Iterator[CheckpointTuple]:
        """Iterate historical checkpoints for the thread.

        Time-travel ``list`` needs to materialise each historical
        Checkpoint, which in turn requires a daemon ``ReadBlob`` op we
        haven't shipped yet. Today this yields the current head
        (matching ``get_tuple``) and stops; that's enough for "resume
        from the latest" but not for "walk every step".
        """
        head = self.get_tuple(config) if config else None
        if head is not None and limit != 0:
            yield head

    # ----------------------------------------------------------- put_writes

    def put_writes(
        self,
        config: RunnableConfig,
        writes: Sequence[tuple[str, Any]],
        task_id: str,
        task_path: str = "",
    ) -> None:
        """Pending intra-step writes between checkpoints.

        Intentional no-op for MVP — LangGraph's runtime carries
        pending writes in-memory and flushes them through ``put`` at
        the end of each super-step, so the resume cycle stays correct.
        Durable replay of intra-step writes lands with ``ReadBlob``.
        """
        return None

    # ------------------------------------------------- async pass-through

    async def aput(
        self,
        config: RunnableConfig,
        checkpoint: Checkpoint,
        metadata: CheckpointMetadata,
        new_versions: ChannelVersions,
    ) -> RunnableConfig:
        return self.put(config, checkpoint, metadata, new_versions)

    async def aget_tuple(self, config: RunnableConfig) -> Optional[CheckpointTuple]:
        return self.get_tuple(config)

    async def alist(
        self,
        config: Optional[RunnableConfig],
        *,
        filter: Optional[dict[str, Any]] = None,
        before: Optional[RunnableConfig] = None,
        limit: Optional[int] = None,
    ):
        for tup in self.list(config, filter=filter, before=before, limit=limit):
            yield tup

    async def aput_writes(
        self,
        config: RunnableConfig,
        writes: Sequence[tuple[str, Any]],
        task_id: str,
        task_path: str = "",
    ) -> None:
        return self.put_writes(config, writes, task_id, task_path)


# ============================================================ helpers


def _require_thread_id(config: RunnableConfig) -> str:
    try:
        return config["configurable"]["thread_id"]
    except (KeyError, TypeError) as exc:
        raise ValueError(
            "AgenticCheckpointer requires config['configurable']['thread_id']"
        ) from exc


def _ns(config: RunnableConfig) -> str:
    return config.get("configurable", {}).get("checkpoint_ns", "") or ""


def _branch_for_thread(thread_id: str) -> str:
    """Map a thread_id to a filesystem-safe branch name.

    Thread ids can contain any characters; branch names can't. We hash
    to keep the mapping deterministic + safe and prefix so the name is
    recognisable in ``agentic log``.
    """
    h = hashlib.blake2b(thread_id.encode("utf-8"), digest_size=8).hexdigest()
    return f"langgraph/{h}"


def _checkpoint_blob_path(thread_id: str) -> str:
    h = hashlib.blake2b(thread_id.encode("utf-8"), digest_size=8).hexdigest()
    return f"{CHECKPOINT_BLOB_PREFIX}/{h}/checkpoint.json"


def _serialise_envelope(
    serde: JsonPlusSerializer,
    checkpoint: Checkpoint,
    metadata: CheckpointMetadata,
    thread_id: str,
) -> dict[str, Any]:
    ckpt_type, ckpt_bytes = serde.dumps_typed(checkpoint)
    meta_type, meta_bytes = serde.dumps_typed(metadata)
    return {
        "v": ENVELOPE_VERSION,
        "thread_id": thread_id,
        "checkpoint": {
            "type": ckpt_type,
            "data_b64": base64.b64encode(ckpt_bytes).decode("ascii"),
        },
        "metadata": {
            "type": meta_type,
            "data_b64": base64.b64encode(meta_bytes).decode("ascii"),
        },
    }


def _deserialise_envelope(
    serde: JsonPlusSerializer,
    envelope: dict[str, Any],
) -> tuple[Checkpoint, CheckpointMetadata]:
    if envelope.get("v") != ENVELOPE_VERSION:
        raise ValueError(
            f"unsupported langgraph envelope version: {envelope.get('v')!r} "
            f"(expected {ENVELOPE_VERSION})"
        )
    ckpt = envelope["checkpoint"]
    meta = envelope["metadata"]
    checkpoint = serde.loads_typed(
        (ckpt["type"], base64.b64decode(ckpt["data_b64"]))
    )
    metadata = serde.loads_typed(
        (meta["type"], base64.b64decode(meta["data_b64"]))
    )
    return checkpoint, metadata
