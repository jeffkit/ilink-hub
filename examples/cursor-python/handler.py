"""
Cursor Agent Bridge Profile (Python SDK)

通过 ilink-bridge-profile SDK 接入 Cursor Agent CLI（agent 命令），
支持多轮对话（--resume）。

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
import sys

from ilink_bridge import ProfileContext, ProfileResult, create_profile

# agent CLI 单次调用的最大等待时间（秒）
TIMEOUT_SECS = 300


async def call_cursor_agent(message: str, session_id: str) -> tuple[str, str]:
    """
    调用 Cursor Agent CLI（agent --print --output-format json），
    返回 (回复文本, 新 session_id)。

    参数：
        message    用户消息
        session_id Hub 存储的 Cursor session UUID（空字符串 = 新会话）

    返回：
        (response, new_session_id)
    """
    cmd = ["agent", "--print", "--trust", "--output-format", "json"]
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

    result = data.get("result", "")
    new_session_id = data.get("session_id", "")
    return result, new_session_id


async def handler(ctx: ProfileContext) -> ProfileResult:
    response, new_session_id = await call_cursor_agent(ctx.message, ctx.session_id)
    return ProfileResult(
        response=response,
        session_id=new_session_id or ctx.session_id or None,
    )


create_profile(handler)
