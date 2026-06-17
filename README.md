# iLink Hub

**iLink-compatible multiplexer hub for WeChat ClawBot** — connect one WeChat account to multiple AI agent backends running on different machines or workspaces, with zero client-side code changes.

[![CI](https://github.com/jeffkit/ilink-hub/actions/workflows/ci.yml/badge.svg)](https://github.com/jeffkit/ilink-hub/actions)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

---

## The Problem

WeChat ClawBot's [iLink API](https://ilinkai.weixin.qq.com) enforces an exclusive lock: only **one process** can poll `getupdates` at a time. If you run Recursive on your Mac, Recursive on a server, and OpenClaw on your laptop — they all fight for the same connection, and only one wins.

## The Solution

iLink Hub is a **transparent iLink proxy**:

```
[WeChat User]
      ↕ real iLink protocol
[iLink Hub]  ← the sole connection holder
      ↕ emulated iLink API (same HTTP endpoints, same protocol)
  ┌───────────────┐  ┌────────────────────┐  ┌────────────────┐
  │Recursive (Mac)│  │Recursive (Server)  │  │OpenClaw (etc.) │
  │base_url=hub   │  │base_url=hub        │  │base_url=hub    │
  │token=vhub_abc │  │token=vhub_def      │  │token=vhub_xyz  │
  └───────────────┘  └────────────────────┘  └────────────────┘
```

**Clients don't need any code changes** — just point `WEIXIN_BASE_URL` at the Hub and use a virtual token. The Hub handles multiplexing, routing, and token mapping transparently.

---

## Features

- **iLink-compatible API** — any existing iLink client works out-of-the-box
- **Multi-backend routing** — route messages to different backends via WeChat commands
- **Context-token mapping** — real context tokens never leak to clients; persisted across restarts
- **QR code login** — scan once, token saved to DB
- **Multi-database** — SQLite (default), PostgreSQL, MySQL via `DATABASE_URL`
- **Full persistence** — client registrations, routing state, and context mappings survive restarts
- **Web admin panel** — manage clients and copy config at `/hub/ui`
- **Admin auth** — protect `/hub/` endpoints with `ILINK_ADMIN_TOKEN` env var
- **Bounded queues** — per-client message buffer capped at 200 to prevent OOM
- **Prometheus metrics** — counters and gauges at `/metrics`
- **Friendly fallback** — when all backends are offline, WeChat users get an instant reply
- **Pre-built binaries** — download from GitHub Releases (Linux/macOS/Windows), no Rust required
- **Health checks** — auto-marks offline clients after 90s idle
- **CLI bridge (`ilink-hub-bridge`)** — connect as a Hub backend and run a local CLI per message ([`docs/bridge/README.md`](docs/bridge/README.md))
- **Docker support** — single-command deployment, multi-arch image (amd64 + arm64)

### Desktop app (Tauri)

A **Tauri 2** desktop shell lives under [`desktop/ilink-hub-desktop/`](desktop/ilink-hub-desktop/): it embeds the same [`run_serve`](src/runtime/serve.rs) runtime as `ilink-hub serve` (default listen `127.0.0.1:8765`, SQLite under the OS app data dir). The root crate stays out of any workspace with this app, so `cargo build` / `cargo test` at the repo root are unchanged.

**Prebuilt installers** (`.dmg` / `.msi` / `.deb`, filenames prefixed with `ilink-hub-desktop-`) are attached to [GitHub Releases](https://github.com/jeffkit/ilink-hub/releases) when a `v*` tag is published (same workflow as CLI binaries). User-facing install notes: [docs — 安装（桌面版）](docs/guide/installation.md#desktop). Dev commands and data paths: [`desktop/ilink-hub-desktop/README.md`](desktop/ilink-hub-desktop/README.md). Roadmap: [`docs/desktop-tauri-roadmap.md`](docs/desktop-tauri-roadmap.md).

---

## Quick Start

### Option A: Pre-built Binary (fastest)

```bash
# Linux x86_64
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
chmod +x ilink-hub && sudo mv ilink-hub /usr/local/bin/

# macOS Apple Silicon
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-aarch64
chmod +x ilink-hub && sudo mv ilink-hub /usr/local/bin/

# macOS Intel
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-x86_64
chmod +x ilink-hub && sudo mv ilink-hub /usr/local/bin/
```

> Windows: download `ilink-hub-windows-x86_64.exe` from [Releases](https://github.com/jeffkit/ilink-hub/releases).

### Option B: Cargo (requires Rust)

```bash
cargo install ilink-hub

# Start Hub (QR login runs inline on first start if no token in DB)
ilink-hub serve --addr 0.0.0.0:8765

# Open web admin panel
# Visit http://your-hub.example.com:8765/hub/ui

# Register each backend (CLI or via the web UI)
ilink-hub register --hub-url http://your-hub.example.com:8765 \
  --name mac-home --label "Mac Home"
# Outputs:
#   WEIXIN_BASE_URL=http://your-hub.example.com:8765
#   WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxx
```

### Option C: Docker Compose

```yaml
# docker-compose.yml
services:
  ilink-hub:
    image: ghcr.io/jeffkit/ilink-hub:latest
    restart: unless-stopped
    ports:
      - "8765:8765"
    volumes:
      - ilink-hub-data:/data
    environment:
      DATABASE_URL: sqlite:/data/ilink-hub.db
      # 强烈建议设置 — 不设置时 /hub/ 管理端点仅当显式开启 ILINK_ADMIN_INSECURE_NO_AUTH=true 才允许无鉴权访问
      # Strongly recommended — if unset, /hub/ admin endpoints require ILINK_ADMIN_INSECURE_NO_AUTH=true
      # ILINK_ADMIN_TOKEN: your-secret-token
      # ILINK_TOKEN: your-token  # Optional: skip QR login if you have a token

volumes:
  ilink-hub-data:
```

```bash
docker compose up -d
# First-time iLink bind: follow logs for the QR code
docker compose logs -f ilink-hub
```

### Option D: PostgreSQL backend

```bash
DATABASE_URL=postgres://user:pass@localhost/ilink_hub ilink-hub serve
```

> [!NOTE]
> When compiling `ilink-hub` from source (e.g., `cargo install` or `cargo build`), only the `sqlite` driver is enabled by default to reduce binary size and compilation times. To enable PostgreSQL or MySQL support, you must compile with the corresponding feature flags:
> ```bash
> cargo build --release --features postgres
> # or
> cargo build --release --features mysql
> ```
> Pre-built binaries and official Docker images are compiled with all features enabled.

---

## WeChat Commands

Send these from WeChat to control the Hub:

| Command | Effect |
|---------|--------|
| `/list` | List all registered backends and their status |
| `/use <name>` | Switch active backend (e.g. `/use mac-home`) |
| `/broadcast <text>` | Send a message to all online backends |
| `/status` | Show Hub status (online/total clients) |

### Quote-reply routing (multi-session)

If you **quote-reply** to a bot message, iLink includes structured `ref_msg` data. The Hub:

1. Records each backend `sendmessage` by its outbound `client_id` (`ilink-hub:…`).
2. When the real iLink `getupdates` stream returns the bot copy (`message_type == 2`) with the same `client_id`, the Hub indexes that message’s per-item `msg_id` (and top-level `message_id`).
3. Your next user message that **quotes** that bot line is routed to the same backend (or the same Hub command, e.g. `/list`), **overriding** the current `/use` selection for that turn. Explicit `/…` hub commands in the new text still take priority over quote routing.

**Operational note:** routing depends on iLink echoing your bot sends on the upstream poll. If your tenant never echoes them, the index stays empty and quote routing has nothing to match.

**Optional display label:** by default, the Hub appends a short footer (`— workspace` or `— workspace · label`) to each **client** `sendmessage` text **only when more than one backend is registered** (so single-backend setups stay clean). Set `ILINKHUB_OUTBOUND_ORIGIN_LABEL=0` / `false` / `off` to disable, or `1` / `true` / `on` to **always** append (even with one client).

---

## Configuring Clients

### Recursive

```toml
# ~/.recursive/config.toml
[weixin]
base_url = "http://your-hub.example.com"
token = "vhub_xxxxxxxxxxxxxxxx"
```

Or via environment:
```bash
WEIXIN_BASE_URL=http://your-hub.example.com
WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxx
recursive weixin
```

### Any `wechatbot`-based Rust SDK

```rust
let bot = WeChatBot::new(BotOptions {
    base_url: Some("http://your-hub.example.com".to_string()),
    token: "vhub_xxxxxxxxxxxxxxxx".to_string(),
    ..Default::default()
});
```

### OpenClaw

```yaml
# ~/.openclaw/openclaw.json
{
  "channels": {
    "weixin": {
      "base_url": "http://your-hub.example.com",
      "token": "vhub_xxxxxxxxxxxxxxxx"
    }
  }
}
```

### ilink-hub-bridge (local CLI)

Run a **local** command (Claude Code, Cursor Agent, Codex, etc.) for each routed WeChat text message — same iLink virtual-token flow as other backends. **Usage guide (Chinese):** [bridge/USAGE](https://jeffkit.github.io/ilink-hub/bridge/USAGE.html). **Quick echo path:** [5-minute try](https://jeffkit.github.io/ilink-hub/bridge/quick-try.html). Full options: [`docs/bridge/README.md`](docs/bridge/README.md) and [`docs/bridge/examples/`](docs/bridge/examples/).

```yaml
# Example bridge config (ilink-hub-bridge.yaml)
profiles:
  echo:
    command: echo
    args: ["{{MESSAGE}}"] # Placeholder replaced with user message
    stdin: none
```

> [!WARNING]
> **安全警告 / Security Warning**:
> 绝不要将 `{{MESSAGE}}` 用于 shell `-c` 参数（例如 `args: ["-c", "run {{MESSAGE}}"]`），因为这会带来 shell 命令注入的安全隐患。推荐使用 `stdin: message` 模式，将消息内容通过标准输入安全地传递给子进程。

```bash
cp docs/bridge/examples/echo.example.yaml ./ilink-hub-bridge.yaml
# Default — no WEIXIN_TOKEN and no cred file yet: POST /hub/register, saves ~/.ilink-hub/bridge-credentials.json (ILINK_ADMIN_TOKEN if Hub requires it). If the file exists but is corrupt/empty, bridge errors instead of overwriting — use --force-register or delete the file.
WEIXIN_BASE_URL=http://127.0.0.1:8765 ilink-hub-bridge --config ./ilink-hub-bridge.yaml
# Optional — Hub client QR pairing instead: add --pair
# Optional — explicit vtoken: WEIXIN_TOKEN=vhub_xxx …
```

---

## Hub API Reference

The Hub exposes the full iLink API surface **plus** Hub-specific management endpoints:

### iLink-compatible endpoints (same as `ilinkai.weixin.qq.com`)

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/ilink/bot/getupdates` | Long-poll for messages (30s timeout) |
| `POST` | `/ilink/bot/sendmessage` | Send reply (context_token auto-translated) |
| `POST` | `/ilink/bot/sendtyping` | Send typing indicator |
| `POST` | `/ilink/bot/getconfig` | Get typing ticket |
| `POST` | `/ilink/bot/getuploadurl` | Get CDN upload URL |

**Authentication:** Same as real iLink — `Authorization: Bearer <vtoken>` header.

### Hub management endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/hub/register` | Register a new backend client |
| `GET` | `/hub/clients` | List all registered clients (includes vtoken) |
| `PATCH` | `/hub/clients/{name}` | Update a client's name and label |
| `DELETE` | `/hub/clients/{name}` | Delete an offline client |
| `GET` | `/hub/ui` | Web admin panel (browser UI) |
| `GET` | `/metrics` | Prometheus-format metrics |
| `GET` | `/health` | Health check |

**Admin auth（必填/Required）:** 部署时必须设置 `ILINK_ADMIN_TOKEN=<secret>`，客户端调用
`/hub/register` 或 `/hub/clients` 时需传递 `Authorization: Bearer <secret>`。未设置 token 时，
管理端点默认返回 403；若要允许无鉴权访问（**仅限本地开发环境**），需显式设置
`ILINK_ADMIN_INSECURE_NO_AUTH=true`。

> [!WARNING]
> **安全警告 / Security Warning**:
> 绝不要在生产环境设置 `ILINK_ADMIN_INSECURE_NO_AUTH=true`。该选项会完全移除 `/hub/` 管理端点的
> 身份验证，使任何人都能注册客户端、查看所有 vtoken 并操作 Hub。仅在本地开发或完全隔离的私有网络
> 中使用。

---

## Architecture

```
ilink-hub/
├── src/
│   ├── ilink/
│   │   ├── types.rs      — Complete iLink protocol types (mirrors ilinkai.weixin.qq.com)
│   │   ├── upstream.rs   — Real iLink poller (exponential backoff, auto-reconnect)
│   │   └── login.rs      — QR login flow (terminal QR rendering)
│   ├── hub/
│   │   ├── registry.rs   — Client registry (vtoken management)
│   │   ├── router.rs     — Message routing + WeChat command parser
│   │   ├── queue.rs      — Per-client queues + context_token mapping
│   │   └── health.rs     — Background health checker
│   ├── server/
│   │   └── routes.rs     — iLink-compatible HTTP handlers
│   ├── store/
│   │   └── mod.rs        — sqlx database layer (SQLite/PostgreSQL/MySQL)
│   └── main.rs           — CLI: serve / login / register / clients
├── Dockerfile             — Multi-stage build
└── .github/workflows/ci.yml
```

### Message flow

```
WeChat sends message
  ↓
Hub polls real iLink getupdates → receives InboundMessage
  ↓
Router: parse WeChat command or determine target client
  ↓
Map real context_token → virtual context_token (stored in DB)
  ↓
Push to target client's queue (notify waiting getupdates long-poll)
  ↓
Client's getupdates returns the message
  ↓
Client processes, sends sendmessage with virtual context_token
  ↓
Hub resolves virtual → real context_token
  ↓
Hub forwards sendmessage to real iLink
  ↓
WeChat receives reply ✓
```

---

## Design Trade-offs

### Broadcast persist is fire-and-forget

When a message lands on the **broadcast** path (no `/use` route resolved, or hub-level
`/broadcast <text>`), the Hub fans out to every online backend. Persisting the resulting
`real_ctx → vctx` mapping into `context_token_map` is done inside a `tokio::spawn` task —
the message is **not** held back from the per-client queue waiting for the DB write to
return. This is a deliberate trade-off:

- **Pro:** Tail latency on the dispatch hot-path stays at the speed of the queue push;
  a slow / contended database (or a one-off `SQLITE_BUSY` under load) cannot stall
  message delivery. The user keeps receiving replies while the persistence layer
  catches up.
- **Con:** If the persist call fails, the mapping is silently dropped. The next time
  the same user sends a message they may be assigned a new vctx, and any per-backend
  session that was keyed to the old vctx becomes orphaned in `backend_sessions_v2`.

To make this trade-off **observable** rather than silent, the Hub exposes two
counters on the in-process `Metrics` struct and on the Prometheus `/metrics`
endpoint as `ilink_hub_persist_fire_and_forget_failures_total{path="forward_to"}`
and `ilink_hub_persist_fire_and_forget_failures_total{path="broadcast"}`. The
per-message (`ForwardTo`) site bumps the `forward_to` counter on error; the
per-broadcast (`Broadcast`) site bumps the `broadcast` counter on error. A
non-zero rate on either label indicates context-token durability is being lost;
alert on `rate(...) > 0` rather than scraping absolute totals. The `path` label
lets operators distinguish single-row failures from per-broadcast batch failures.

If you require strict durability, replace the `tokio::spawn` in `src/hub/mod.rs` with
an awaited write (or wrap it in a retry-with-backoff task and a bounded
"persistence backlog" queue that the dispatcher drains before the next broadcast).

---

## Security Recommendations

- **Deploy behind HTTPS** — use a reverse proxy (Nginx, Caddy) with TLS
- **Restrict `/hub/` admin endpoints** — add IP allowlist or Bearer token auth to admin routes
- **Use PostgreSQL for production** — SQLite works but isn't suited for high-concurrency deployments
- **Rotate virtual tokens periodically** — re-register clients with a new name to get fresh vtokens
- **Keep Hub on private network** — only expose port 8765 if needed; ideally put Nginx in front
- **Configure CORS origins** — set `ILINK_CORS_ORIGINS` to a comma-separated list of allowed origins (e.g. `https://a.com,https://b.com`) to restrict cross-origin access to the bot API. When unset, the Hub falls back to permissive CORS (any origin allowed) with a WARN log. Origins must include the scheme (`https://` or `http://`); values without a scheme (e.g. `*`, `null`, bare hostnames) cause a startup error.

### Nginx example

```nginx
server {
    listen 443 ssl;
    server_name hub.example.com;

    # Only allow your backend IPs to access admin endpoints
    location /hub/ {
        allow 192.168.1.0/24;
        deny all;
        proxy_pass http://localhost:8765;
    }

    # iLink API open to registered clients
    location /ilink/ {
        proxy_pass http://localhost:8765;
        proxy_set_header Host $host;
    }

    location /health {
        proxy_pass http://localhost:8765;
    }
}
```

---

## Comparison with Similar Projects

| Project | Protocol for clients | Multi-machine | Standalone |
|---------|---------------------|---------------|------------|
| **iLink Hub** (this) | ✅ iLink-compatible | ✅ Yes | ✅ Yes |
| OpeniLink Hub | ❌ Custom WebSocket/SDK | ✅ Yes | ✅ Yes |
| HermesClaw | ❌ Local proxy only | ❌ No | ✅ Yes |
| wechat-clawbot | HTTP webhook | ✅ Yes | ✅ Yes |
| OpenClaw bindings | ❌ OpenClaw-specific | ❌ Same machine | ✅ Yes |

---

## License

MIT © 2026 jeffkit
