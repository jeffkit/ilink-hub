# ilink-bridge-profile

Python SDK for writing [iLink Hub Bridge](../../docs/bridge/profile-spec.md) profile handlers.

Implements the **P0 exec protocol** — reads `ILINK_*` env vars injected by the bridge,
calls your async handler, writes P0-formatted output to stdout. Works on macOS, Linux, and Windows.

## Install

```bash
pip install ilink-bridge-profile
```

## Quick start

```python
# my_profile.py
from ilink_bridge import create_profile

async def handler(ctx):
    # call any LLM API here
    reply = await my_llm(ctx.message)
    return reply  # plain str, or ProfileResult(response=reply)

create_profile(handler)
```

Configure in `ilink-hub-bridge.yaml`:

```yaml
profiles:
  my-ai:
    command: python
    args: [/path/to/my_profile.py]
    stdin: none
    timeout_secs: 120
```

## With session continuity

```python
from ilink_bridge import create_profile, load_history, append_history, HistoryEntry, ProfileResult

async def handler(ctx):
    history = load_history(ctx.session_id)

    messages = [
        {"role": e.role, "content": e.content}
        for e in history
    ] + [{"role": "user", "content": ctx.message}]

    reply = await call_openai(messages)

    append_history(ctx.session_id, [
        HistoryEntry(role="user", content=ctx.message),
        HistoryEntry(role="assistant", content=reply),
    ])

    return ProfileResult(response=reply, session_id=ctx.session_id)

create_profile(handler)
```

History is stored in `~/.ilink-hub/sessions/<session_id>.jsonl` (one JSON object per line).

## API

### `create_profile(handler)`

Runs the P0 protocol loop: reads env vars → calls `handler(ctx)` → writes stdout → exits.

**`ctx`** (`ProfileContext`) fields:
| Field | Env var | Description |
|-------|---------|-------------|
| `message` | `ILINK_MESSAGE` | User message text |
| `session_id` | `ILINK_SESSION_ID` | Hub-persisted backend session UUID |
| `session_name` | `ILINK_SESSION_NAME` | Human-readable session name |
| `from_user` | `ILINK_FROM_USER` | Sender user ID |
| `context_token` | `ILINK_CONTEXT_TOKEN` | Hub context token |

**Return**: `ProfileResult(response, session_id?)` or plain `str`.

### `load_history(session_id, session_dir?) → List[HistoryEntry]`

Load conversation history from `~/.ilink-hub/sessions/<session_id>.jsonl`.

### `append_history(session_id, entries, session_dir?)`

Append `[HistoryEntry(role, content, ts?)]` entries to the JSONL history file.
