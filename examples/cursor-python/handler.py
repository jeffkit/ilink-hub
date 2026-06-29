"""
Cursor Agent Bridge Profile (Python SDK)

通过 ilink-bridge-profile SDK 接入 Cursor Agent CLI（agent 命令），
支持多轮对话（--resume）和流式输出（--output-format stream-json）。

依赖：
    pip install -r requirements.txt

本地测试（不需要启动 bridge）：
    ILINK_MESSAGE="你好，介绍一下自己" \\
    ILINK_SESSION_ID="" \\
    ILINK_SESSION_NAME="default" \\
    ILINK_FROM_USER="test" \\
    ILINK_CONTEXT_TOKEN="test-token" \\
    python3 handler.py

接入 bridge：
    ilink-hub-bridge --config profiles.yaml
"""

from __future__ import annotations

import asyncio
import json
import os
from typing import AsyncIterator

from ilink_bridge import ProfileContext, ProfileResult, create_profile

# agent CLI 单次调用的最大等待时间（秒）
TIMEOUT_SECS = 1800


async def stream_cursor_agent(
    message: str, session_id: str
) -> AsyncIterator[tuple[str, str]]:
    """流式调用 Cursor Agent CLI（--output-format stream-json）。

    在 agent 工作期间，逐行解析 stream-json 事件：
    - ``type == "assistant"`` 的文本内容作为阶段性进展 yield（供 send_partial 使用）
    - ``type == "result"`` 作为最终结果 yield（result 字段 + session_id）

    Yields:
        (text, session_id, is_final) — is_final=True 表示这是最终结果消息。
    """
    cmd = ["agent", "--print", "--trust", "--yolo", "--output-format", "stream-json"]

    model = os.environ.get("CURSOR_MODEL")
    if model:
        cmd += ["--model", model]

    if session_id:
        cmd += ["--resume", session_id]

    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )

    assert proc.stdin is not None
    proc.stdin.write(message.encode())
    await proc.stdin.drain()
    proc.stdin.close()

    assert proc.stdout is not None
    deadline = asyncio.get_event_loop().time() + TIMEOUT_SECS

    while True:
        remaining = deadline - asyncio.get_event_loop().time()
        if remaining <= 0:
            proc.kill()
            raise RuntimeError(f"Cursor Agent timed out after {TIMEOUT_SECS}s")

        try:
            line_bytes = await asyncio.wait_for(proc.stdout.readline(), timeout=remaining)
        except asyncio.TimeoutError:
            proc.kill()
            raise RuntimeError(f"Cursor Agent timed out after {TIMEOUT_SECS}s") from None

        if not line_bytes:
            break  # EOF

        line = line_bytes.decode(errors="replace").strip()
        if not line:
            continue

        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue

        event_type = event.get("type")

        if event_type == "assistant":
            # Intermediate progress text from the agent during its work process.
            # Sent as send_partial so the user sees real-time progress.
            content = event.get("message", {}).get("content", [])
            text = "".join(
                block.get("text", "")
                for block in content
                if block.get("type") == "text"
            )
            if text:
                yield text, "", False  # (text, session_id, is_final)

        elif event_type == "result":
            new_session_id = event.get("session_id", "")
            result_text = event.get("result", "")
            # The result event carries the FINAL answer, distinct from intermediate progress.
            yield result_text, new_session_id, True

    await proc.wait()
    if proc.returncode is not None and proc.returncode != 0:
        assert proc.stderr is not None
        stderr_text = (await proc.stderr.read()).decode(errors="replace").strip()
        raise RuntimeError(f"agent exited with code {proc.returncode}: {stderr_text}")


async def call_cursor_agent(message: str, session_id: str) -> tuple[str, str]:
    """非流式调用 Cursor Agent CLI，返回 (回复文本, 新 session_id)。

    保留作备用路径，在 agent 不支持 stream-json 时使用。
    """
    cmd = ["agent", "--print", "--trust", "--yolo", "--output-format", "json"]

    model = os.environ.get("CURSOR_MODEL")
    if model:
        cmd += ["--model", model]

    if session_id:
        cmd += ["--resume", session_id]

    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout_bytes, stderr_bytes = await asyncio.wait_for(
            proc.communicate(input=message.encode()),
            timeout=TIMEOUT_SECS,
        )
    except asyncio.TimeoutError:
        raise RuntimeError(f"Cursor Agent timed out after {TIMEOUT_SECS}s") from None

    if proc.returncode != 0:
        stderr_text = stderr_bytes.decode(errors="replace").strip()
        raise RuntimeError(f"agent exited with code {proc.returncode}: {stderr_text}")

    raw = stdout_bytes.decode(errors="replace").strip()
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as e:
        raise RuntimeError(
            f"failed to parse agent JSON output: {e}\nraw output: {raw[:500]}"
        ) from e

    return data.get("result", ""), data.get("session_id", "")


async def handler(ctx: ProfileContext) -> ProfileResult:
    response_text, new_session_id = await call_cursor_agent(ctx.message, ctx.session_id)
    return ProfileResult(
        response=response_text,
        session_id=new_session_id or ctx.session_id or None,
    )


create_profile(handler)
