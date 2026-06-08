# ilink-bridge-profile (Node.js)

Node.js SDK for writing [iLink Hub Bridge](../../docs/bridge/profile-spec.md) profile handlers.

Implements the **P0 exec protocol** — reads `ILINK_*` env vars injected by the bridge,
calls your async handler, writes P0-formatted output to stdout. Works on macOS, Linux, and Windows.

## Install

```bash
npm install ilink-bridge-profile
```

## Quick start

```js
// my-profile.js
const { createProfile } = require('ilink-bridge-profile');

createProfile(async ({ message, sessionId, sessionName }) => {
  // call any LLM API here
  const reply = await myLLM(message);
  return { response: reply };
});
```

Configure in `ilink-hub-bridge.yaml`:

```yaml
profiles:
  my-ai:
    command: node
    args: [/path/to/my-profile.js]
    stdin: none
    timeout_secs: 120
```

## With session continuity

```js
const { createProfile, loadHistory, appendHistory } = require('ilink-bridge-profile');

createProfile(async ({ message, sessionId }) => {
  const history = loadHistory(sessionId);

  const messages = [
    ...history.map(e => ({ role: e.role, content: e.content })),
    { role: 'user', content: message },
  ];

  const reply = await callOpenAI(messages);

  appendHistory(sessionId, [
    { role: 'user', content: message },
    { role: 'assistant', content: reply },
  ]);

  return { response: reply, sessionId };
});
```

History is stored in `~/.ilink-hub/sessions/<sessionId>.jsonl` (one JSON object per line).

## API

### `createProfile(handler)`

Runs the P0 protocol loop: reads env vars → calls `handler(ctx)` → writes stdout → exits.

**`ctx`** fields:
| Field | Env var | Description |
|-------|---------|-------------|
| `message` | `ILINK_MESSAGE` | User message text |
| `sessionId` | `ILINK_SESSION_ID` | Hub-persisted backend session UUID |
| `sessionName` | `ILINK_SESSION_NAME` | Human-readable session name |
| `fromUser` | `ILINK_FROM_USER` | Sender user ID |
| `contextToken` | `ILINK_CONTEXT_TOKEN` | Hub context token |

**Return value**: `{ response: string, sessionId?: string }` or plain `string`.

### `loadHistory(sessionId, sessionDir?)`

Load conversation history from `~/.ilink-hub/sessions/<sessionId>.jsonl`.

### `appendHistory(sessionId, entries, sessionDir?)`

Append `[{ role, content, ts? }]` entries to the JSONL history file.
