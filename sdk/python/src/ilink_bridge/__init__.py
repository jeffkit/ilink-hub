"""
ilink-bridge-profile — iLink Hub Bridge Profile SDK (Python)

Implements the P0 exec protocol so you can write a single async handler function
instead of manually reading env vars and formatting stdout.

P0 contract (read by the bridge):
  Input  — env vars: ILINK_MESSAGE, ILINK_SESSION_ID, ILINK_SESSION_NAME,
                     ILINK_FROM_USER, ILINK_CONTEXT_TOKEN
  Output — stdout: optional first line "ILINK_SESSION:<uuid>", then reply text
  Exit   — 0 = success, non-zero = error

Example::

    # my_profile.py
    from ilink_bridge import create_profile

    async def handler(ctx):
        reply = await my_llm(ctx.message)
        return reply  # or: return ProfileResult(response=reply, session_id=new_id)

    create_profile(handler)
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Awaitable, List, Optional, Union

__all__ = [
    "ProfileContext",
    "ProfileResult",
    "create_profile",
    "load_history",
    "append_history",
    "session_file_path",
]


# ---------------------------------------------------------------------------
# Data classes
# ---------------------------------------------------------------------------

@dataclass
class ProfileContext:
    """Input context passed to the profile handler."""

    message: str
    """User message text (ILINK_MESSAGE)."""

    session_id: str
    """Hub-persisted backend session UUID (ILINK_SESSION_ID)."""

    session_name: str
    """Human-readable session name (ILINK_SESSION_NAME)."""

    from_user: str
    """Sender user ID (ILINK_FROM_USER)."""

    context_token: str
    """Hub context token (ILINK_CONTEXT_TOKEN)."""

    async def send_partial(self, text: str) -> None:
        """Send a partial response chunk to the WeChat user immediately.

        Writes an ``ILINK_PARTIAL:<json>`` line to stdout and flushes the buffer.
        The bridge reads this line in real-time and forwards the decoded text to
        the Hub without waiting for the profile process to exit.

        The profile itself has no knowledge of iLink or Hub URLs — the bridge
        handles all protocol details.  This method is intentionally thin: it only
        writes to stdout and flushes.

        Example::

            async def handler(ctx):
                async for chunk in stream_ai(ctx.message):
                    await ctx.send_partial(chunk)
                return ProfileResult(session_id=new_session_id)
        """
        sys.stdout.write(f"ILINK_PARTIAL:{json.dumps(text)}\n")
        sys.stdout.flush()


@dataclass
class ProfileResult:
    """Return value from a profile handler.

    When all content has been sent incrementally via :meth:`ProfileContext.send_partial`,
    set ``response`` to an empty string so the bridge does not send an additional
    (empty) final message.
    """

    response: str = ""
    """Reply text to send back to the WeChat user.

    Set to an empty string when all content was already sent via
    :meth:`ProfileContext.send_partial`.
    """

    session_id: Optional[str] = None
    """New backend session ID to persist (optional)."""


@dataclass
class HistoryEntry:
    """A single conversation turn stored in the JSONL history file."""

    role: str
    content: str
    ts: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())


# ---------------------------------------------------------------------------
# Session history helpers (optional)
# ---------------------------------------------------------------------------

def _default_session_dir() -> Path:
    return Path.home() / ".ilink-hub" / "sessions"


def session_file_path(session_id: str, session_dir: Optional[Path] = None) -> Path:
    """Resolved path for a session JSONL file, keyed by the stable session UUID."""
    base = session_dir or _default_session_dir()
    return base / f"{session_id}.jsonl"


def load_history(
    session_id: str,
    session_dir: Optional[Path] = None,
) -> List[HistoryEntry]:
    """
    Load conversation history for a session from its JSONL file.
    Returns an empty list if the file does not exist.
    """
    if not session_id:
        return []
    path = session_file_path(session_id, session_dir)
    if not path.exists():
        return []
    entries = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
            entries.append(HistoryEntry(
                role=obj.get("role", ""),
                content=obj.get("content", ""),
                ts=obj.get("ts", ""),
            ))
        except json.JSONDecodeError:
            pass
    return entries


def append_history(
    session_id: str,
    entries: List[HistoryEntry],
    session_dir: Optional[Path] = None,
) -> None:
    """
    Append one or more entries to a session's JSONL history file.
    Creates the file (and parent directory) if it does not exist.
    """
    if not session_id or not entries:
        return
    path = session_file_path(session_id, session_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as f:
        for entry in entries:
            f.write(json.dumps({
                "role": entry.role,
                "content": entry.content,
                "ts": entry.ts,
            }) + "\n")


# ---------------------------------------------------------------------------
# create_profile — main entry point
# ---------------------------------------------------------------------------

HandlerResult = Union[ProfileResult, str]
Handler = Callable[[ProfileContext], Awaitable[HandlerResult]]


def create_profile(handler: Handler) -> None:
    """
    Run a profile handler following the P0 exec protocol.

    Reads ILINK_* env vars, invokes ``handler(ctx)``, writes P0-formatted
    output to stdout, and exits the process with code 0 (success) or 1 (error).

    :param handler: Async callable that receives a :class:`ProfileContext` and
        returns either a :class:`ProfileResult` or a plain string.
    """
    asyncio.run(_run(handler))


async def _run(handler: Handler) -> None:
    ctx = ProfileContext(
        message=os.environ.get("ILINK_MESSAGE", ""),
        session_id=os.environ.get("ILINK_SESSION_ID", ""),
        session_name=os.environ.get("ILINK_SESSION_NAME", "default"),
        from_user=os.environ.get("ILINK_FROM_USER", ""),
        context_token=os.environ.get("ILINK_CONTEXT_TOKEN", ""),
    )

    try:
        raw = await handler(ctx)
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"[ilink_bridge] handler error: {exc}\n")
        sys.exit(1)

    if isinstance(raw, str):
        result = ProfileResult(response=raw)
    else:
        result = raw

    # P0 output: optional session line first, then reply text
    if result.session_id:
        sys.stdout.write(f"ILINK_SESSION:{result.session_id}\n")
    sys.stdout.write(result.response)
    sys.stdout.flush()
    sys.exit(0)
