# iLink Hub

**iLink-compatible multiplexer hub for WeChat ClawBot** — route one WeChat account to multiple AI agent backends running on different machines or workspaces.

## What it solves

The WeChat ClawBot iLink API enforces an exclusive lock: only **one process** can poll `getupdates` at a time. If you run multiple AI agents (Recursive, OpenClaw, Claude Code…), only one gets the messages.

iLink Hub solves this by:
1. Holding the **single real iLink connection** to WeChat
2. Exposing an **iLink-compatible API server** — clients connect with the exact same protocol, just pointing at the Hub instead of `ilinkai.weixin.qq.com`
3. **Routing messages** between the WeChat user and the right backend

## Key design

```
[WeChat User]
     ↕ real iLink protocol
[iLink Hub]  ← single iLink connection holder
     ↕ emulated iLink API (same HTTP endpoints)
[Recursive on Mac]   [Recursive on Server]   [OpenClaw on Laptop]
 base_url=hub          base_url=hub            base_url=hub
 token=vhub_abc        token=vhub_def          token=vhub_xyz
```

**Clients don't need any code changes** — just set `WEIXIN_BASE_URL` and `WEIXIN_TOKEN` (virtual token from Hub).

## Quick start

### 1. Start the Hub (on a machine with public IP)

```bash
ILINK_TOKEN=your_real_ilink_token ilink-hub serve --addr 0.0.0.0:8765
```

### 2. Register each backend

```bash
ilink-hub register --hub-url https://hub.example.com --name mac-home --label "Mac Home"
# Outputs:
#   WEIXIN_BASE_URL=https://hub.example.com
#   WEIXIN_TOKEN=vhub_<random>
```

### 3. Configure each backend client

For any iLink-compatible tool, just set:
```bash
export WEIXIN_BASE_URL="https://hub.example.com"
export WEIXIN_TOKEN="vhub_<your-virtual-token>"
```

### 4. WeChat user commands

From WeChat:
```
/list          — show connected workspaces
/use mac-home  — switch to Mac Home workspace
/use server    — switch to server workspace
/broadcast hi  — send to all backends simultaneously
/status        — show hub status
```

## Architecture

```
src/
├── ilink/
│   ├── types.rs      — iLink protocol types (matching ilinkai.weixin.qq.com)
│   └── upstream.rs   — Real iLink poller (connects to WeChat)
├── hub/
│   ├── registry.rs   — Client registry (vtoken → client info)
│   ├── router.rs     — Message routing logic + hub command parser
│   └── queue.rs      — Per-client message queues + context_token mapping
├── server/
│   ├── routes.rs     — iLink-compatible HTTP handlers
│   └── mod.rs        — axum router setup
└── main.rs           — CLI (serve / register / clients)
```

## iLink API compatibility

| Endpoint | Status |
|----------|--------|
| `POST /ilink/bot/getupdates` | ✅ Long-poll, message fan-out |
| `POST /ilink/bot/sendmessage` | ✅ Proxied to real iLink |
| `POST /ilink/bot/sendtyping` | ✅ Proxied |
| `POST /ilink/bot/getconfig` | ✅ Proxied |
| `POST /ilink/bot/getuploadurl` | ✅ Proxied |
| Hub: `POST /hub/register` | 🆕 Hub-specific |
| Hub: `GET /hub/clients` | 🆕 Hub-specific |

## License

MIT
