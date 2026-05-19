# agentic-sdk

Python SDK for [git.agentic](../../README.md). Talks to a local `agenticd`
daemon over a Unix domain socket.

```bash
pip install agentic-sdk
pip install agentic-sdk[langgraph]   # with LangGraph integration
```

```python
import agentic
from agentic.langgraph import AgenticCheckpointer

agentic.commit(message="ship friendlier prompt", model="anthropic:claude-opus:2026-05-01")
print(agentic.diff("HEAD^", "HEAD"))
agentic.rollback("v0.7")

graph = StateGraph(...)
app = graph.compile(checkpointer=AgenticCheckpointer(repo=".agentic"))
```

See the [top-level README](../../README.md) for the full picture.
