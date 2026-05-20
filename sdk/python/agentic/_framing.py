"""Length-prefixed JSON framing for the daemon socket.

Mirror of ``crates/agentic-proto/src/framing.rs`` on the Rust side. Every
frame is::

    [u32 BE length][json payload bytes]

Bounded by ``MAX_FRAME_BYTES`` so a malformed peer can't OOM us.

This module is intentionally private — clients touch ``AgenticClient``
or the top-level helpers in ``agentic`` instead.
"""

from __future__ import annotations

import json
import socket
import struct
from typing import Any

MAX_FRAME_BYTES = 16 * 1024 * 1024


class FrameError(Exception):
    """Raised on framing-layer failures (length cap, truncated reads, EOF)."""


def write_frame(sock: socket.socket, value: Any) -> None:
    """Serialize ``value`` as JSON and write it length-prefixed to ``sock``."""
    body = json.dumps(value, separators=(",", ":")).encode("utf-8")
    if len(body) > MAX_FRAME_BYTES:
        raise FrameError(f"frame too large: {len(body)} bytes (max {MAX_FRAME_BYTES})")
    header = struct.pack(">I", len(body))
    sock.sendall(header + body)


def read_frame(sock: socket.socket) -> Any:
    """Read a single length-prefixed JSON frame and return the decoded value."""
    header = _read_exact(sock, 4)
    (length,) = struct.unpack(">I", header)
    if length > MAX_FRAME_BYTES:
        raise FrameError(f"frame too large: {length} bytes (max {MAX_FRAME_BYTES})")
    body = _read_exact(sock, length)
    return json.loads(body)


def _read_exact(sock: socket.socket, n: int) -> bytes:
    """Read exactly ``n`` bytes from ``sock`` or raise ``FrameError``."""
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise FrameError(
                f"unexpected EOF after {len(buf)} of {n} bytes; daemon closed connection"
            )
        buf.extend(chunk)
    return bytes(buf)
