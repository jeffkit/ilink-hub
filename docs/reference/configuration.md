# 环境变量配置

iLink Hub 遵循 [12-Factor](https://12factor.net/config) 配置原则，所有配置均可通过环境变量注入。

## 核心变量

| 变量名 | 默认值 | 说明 |
|--------|--------|------|
| `DATABASE_URL` | `sqlite:~/.local/share/ilink-hub/ilink-hub.db` | 数据库连接字符串 |
| `ILINK_HUB_ADDR` | `0.0.0.0:8765` | Hub 监听地址和端口 |
| `ILINK_ADMIN_TOKEN` | （未设置） | 管理端点认证 Token，**生产环境必须设置** |
| `ILINK_TOKEN` | （未设置） | 跳过 QR 登录，直接使用已有的 iLink context_token |
| `ILINKHUB_RELAY_URL` | `https://ilinkhub.ai` | 公网配对中继地址；Hub 自动连接 `{url}/ws/pairing` |
| `ILINKHUB_RELAY` | （启用） | 设为 `0` 禁用出站中继，仅本机配对调试 |
| `HUB_PAIR_URL` | （自动） | 手动覆盖二维码公网前缀；设置后禁用自动中继 |
| `HUB_CLIENT_URL` | `http://127.0.0.1:8765` | 配对成功后返回给 SDK 的 API 地址（通常为本机 Hub） |
| `HUB_PUBLIC_URL` | （已废弃，等同 `HUB_PAIR_URL`） | 旧版配对公网 URL，仍可用作 `HUB_PAIR_URL` 的别名 |

## 数据库配置

### SQLite（默认）

```bash
DATABASE_URL=sqlite:/path/to/ilink-hub.db
# 或相对路径
DATABASE_URL=sqlite:./ilink-hub.db
```

### PostgreSQL

```bash
DATABASE_URL=postgres://user:password@host:5432/database_name
```

### MySQL

```bash
DATABASE_URL=mysql://user:password@host:3306/database_name
```

## 日志配置

| 变量名 | 默认值 | 说明 |
|--------|--------|------|
| `RUST_LOG` | `info` | 日志级别：`error`、`warn`、`info`、`debug`、`trace` |
| `RUST_LOG_FORMAT` | `pretty` | 日志格式：`pretty`（人类可读）或 `json`（结构化，适合日志聚合） |

```bash
# 生产环境推荐（结构化日志）
RUST_LOG=info RUST_LOG_FORMAT=json ilink-hub serve

# 开发调试（详细日志）
RUST_LOG=debug ilink-hub serve
```

## 示例：完整生产配置

```bash
# .env 文件
DATABASE_URL=postgres://ilink:secret@localhost/ilink_hub
ILINK_HUB_ADDR=0.0.0.0:8765
ILINK_ADMIN_TOKEN=your-very-strong-random-token-here
RUST_LOG=info
RUST_LOG_FORMAT=json
```

加载 `.env` 文件：

```bash
# 使用 dotenv 工具
dotenv ilink-hub serve

# 或手动 export
set -a && source .env && set +a
ilink-hub serve
```

## CLI 参数

部分配置也可以通过 CLI 参数传递：

```bash
ilink-hub serve --help

Options:
  --addr <ADDR>           监听地址 [env: ILINK_HUB_ADDR] [default: 0.0.0.0:8765]
  --db <DATABASE_URL>     数据库连接 [env: DATABASE_URL]
  --admin-token <TOKEN>   管理认证 Token [env: ILINK_ADMIN_TOKEN]
  --ilink-token <TOKEN>   直接注入 iLink Token [env: ILINK_TOKEN]
```

CLI 参数的优先级高于环境变量。
