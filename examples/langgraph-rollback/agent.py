"""The broken-prompt demo agent.

A deliberately tiny LangGraph "customer-support" agent. We do not call
an LLM — the demo is about the rollback mechanism, not the model. The
"response generator" is a deterministic function that branches on the
system prompt's content and quotes recent episode rows, so:

* a wholesome system prompt yields an empathetic answer;
* a sycophantic system prompt yields a hallucinated refund;
* junk rows added to ``episodes`` visibly contaminate the response.

This is enough to show that `agentic rollback` restores both the prompt
on disk and the memory rows in Postgres — i.e. that `git revert` of
just the prompt is insufficient.

Usage::

    DATABASE_URL=postgres://… AGENTIC_SOCKET=.agentic/agenticd.sock \\
      python agent.py "I'm thinking about cancelling my subscription."
"""

from __future__ import annotations

import os
import sys
import uuid
from operator import add
from pathlib import Path
from typing import Annotated, Any, TypedDict

import psycopg
from langgraph.graph import END, START, StateGraph

from agentic import AgenticClient
from agentic.langgraph import AgenticCheckpointer

REPO_ROOT = Path(__file__).resolve().parent
PROMPT_PATH = REPO_ROOT / "prompts" / "system.txt"
THREAD_ID = "demo-broken-prompt"


# --------------------------------------------------------- agent state


class State(TypedDict):
    user_input: str
    memory_context: list[str]
    response: str
    transcript: Annotated[list[str], add]


# --------------------------------------------------------- nodes


def fetch_memory(state: State) -> dict[str, Any]:
    """Pull the most recent rows from the ``episodes`` table.

    This is the memory dimension the demo is rolling back. Junk rows
    inserted by the bad change appear here and contaminate the prompt.
    """
    url = _require_env("DATABASE_URL")
    rows: list[str] = []
    with psycopg.connect(url) as conn, conn.cursor() as cur:
        cur.execute("SELECT text FROM episodes ORDER BY id DESC LIMIT 3")
        rows = [r[0] for r in cur.fetchall()]
    return {"memory_context": rows}


def generate(state: State) -> dict[str, Any]:
    """Deterministic response generator.

    Branches on the system prompt to produce either an empathetic or a
    sycophantic answer; quotes the freshest episode so memory state is
    visible in the output.
    """
    system_prompt = PROMPT_PATH.read_text().strip()
    sp_lower = system_prompt.lower()

    # Discriminating sentinels: the baseline prompt contains "escalate"
    # (the safe-routing behaviour) and the bad prompt contains
    # "absolutely agreeable" (the sycophantic behaviour). Matching on
    # those keeps the two branches cleanly distinct — even though the
    # baseline prompt mentions "issue refunds" in passing (when listing
    # what the agent does NOT have authority to do), it does not match
    # the sycophantic branch.
    if "absolutely agreeable" in sp_lower or "can-do" in sp_lower:
        body = (
            "Absolutely! I'll cancel your subscription and refund the full "
            "amount you've paid this year. Done!"
        )
    elif "escalate" in sp_lower or "empathetic" in sp_lower:
        body = (
            "I understand. Could you tell me a bit more about why? I'd "
            "like to help find the right outcome for you."
        )
    else:
        body = "Thanks for getting in touch — let me look into that."

    if state["memory_context"]:
        body += (
            "\n\nLooking at your account history, I see: "
            f"{state['memory_context'][0]}"
        )

    return {
        "response": body,
        "transcript": [f"USER: {state['user_input']}", f"AGENT: {body}"],
    }


# --------------------------------------------------------- assembly


def build_app(client: AgenticClient):
    cp = AgenticCheckpointer(client=client, repo=REPO_ROOT)
    g = StateGraph(State)
    g.add_node("fetch_memory", fetch_memory)
    g.add_node("generate", generate)
    g.add_edge(START, "fetch_memory")
    g.add_edge("fetch_memory", "generate")
    g.add_edge("generate", END)
    return g.compile(checkpointer=cp)


def _require_env(name: str) -> str:
    v = os.environ.get(name)
    if not v:
        print(f"error: ${name} not set", file=sys.stderr)
        sys.exit(2)
    return v


def main() -> int:
    if len(sys.argv) < 2:
        print(f"usage: {sys.argv[0]} <user message>", file=sys.stderr)
        return 2
    user_input = sys.argv[1]

    sock = os.environ.get(
        "AGENTIC_SOCKET", str(REPO_ROOT / ".agentic" / "agenticd.sock")
    )
    client = AgenticClient(socket_path=sock)

    app = build_app(client)
    config = {
        "configurable": {
            "thread_id": THREAD_ID,
            "checkpoint_ns": "",
            "checkpoint_id": None,
        },
        "run_id": uuid.uuid4(),
    }
    out: State = app.invoke(
        {"user_input": user_input, "memory_context": [], "response": "", "transcript": []},
        config=config,
    )

    print(f"> {out['response']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
