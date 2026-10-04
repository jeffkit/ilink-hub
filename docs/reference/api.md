# HTTP API 参考

iLink Hub 暴露两类 HTTP 端点：**兼容 iLink 协议的客户端端点**和 **Hub 专属管理端点**。

## iLink 兼容端点

这些端点与真实的 `ilinkai.weixin.qq.com` 完全兼容，AI 客户端无需修改代码。

**Base URL**: `http://your-hub.example.com:8765`

**认证**: HTTP 头 `Authorization: Bearer <虚拟 Token>`

### POST /ilink/bot/getupdates

长轮询，等待新消息。如 30 秒内无新消息则返回空结果。

**请求体：**

```json
{
  "token": "vhub_xxxxxxxx"
}
```

**响应（有消息时）：**

```json
{
  "errcode": 0,
  "messages": [
    {
      "context_token": "vhub_ctx_xxxxxxxx",
      "from_user": "user_openid",
      "content": "你好",
      "msg_type": "text",
      "timestamp": 1717545600
    }
  ]
}
```

### POST /ilink/bot/sendmessage

发送回复消息给微信用户。

**请求体：**

```json
{
  "token": "vhub_xxxxxxxx",
  "context_token": "vhub_ctx_xxxxxxxx",
  "content": "你好！有什么可以帮你的？",
  "msg_type": "text"
}
```

**响应：**

```json
{"errcode": 0}
```

### POST /ilink/bot/sendtyping

发送「正在输入」状态指示。

**请求体：**

```json
{
  "token": "vhub_xxxxxxxx",
  "context_token": "vhub_ctx_xxxxxxxx"
}
```

### POST /ilink/bot/getconfig

获取输入配置（typing ticket）。

### POST /ilink/bot/getuploadurl

获取媒体文件 CDN 上传地址。

### 出站限流（sendmessage / sendtyping / getconfig / getuploadurl / MCP call_agent）

所有租户共用同一个真实微信账号与同一条上游连接池，因此上游配额是**全局**的，
但虚拟 Token 是**按租户**签发的。为避免单个租户（或泄漏的 Token）打满共享配额、
殃及其他租户，Hub 对这五条出站路径按 vtoken 做**令牌桶**限流：

- 五条路径**共用同一个桶**——不能靠切换路由绕过自己的配额。
- 认证在限流之前：未注册/无效 Token 的请求不会创建桶。
- 被限流时返回 **HTTP 429**，响应体为 `{"ret": 429, "errmsg": "rate limit exceeded; retry after Ns"}`；
  MCP `call_agent` 则以 JSON-RPC error 返回同一句话。

默认值与调优（均为启动时读取的环境变量，改后需重启）：

| 环境变量 | 默认 | 含义 |
|----------|------|------|
| `ILINK_BOT_RATE_LIMIT_PER_SEC` | `20` | 每租户持续速率（请求/秒） |
| `ILINK_BOT_RATE_LIMIT_BURST` | `40` | 每租户突发容量（请求） |

观测：`GET /metrics` 导出每租户的 `ilink_hub_ratelimit_tokens`（剩余配额）、
`ilink_hub_ratelimit_burst`（桶容量）与 `ilink_hub_ratelimit_rejected_total`（被拒次数）。

---

## Hub 管理端点

这些端点用于注册和管理客户端。

**认证**: 若设置了 `ILINK_ADMIN_TOKEN`，需要携带 `Authorization: Bearer <admin-token>`。

### POST /hub/register

注册新客户端，获取虚拟 Token。

**请求体：**

```json
{
  "name": "mac-home",
  "label": "Mac 本机 Recursive"
}
```

**响应：**

```json
{
  "name": "mac-home",
  "label": "Mac 本机 Recursive",
  "token": "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "created_at": "2026-06-05T10:00:00Z"
}
```

::: warning
`token` 字段只在此处返回一次明文，之后无法从 Hub 恢复。请务必保存。
:::

### GET /hub/clients

列出所有已注册的客户端。

**响应：**

```json
{
  "clients": [
    {
      "name": "mac-home",
      "label": "Mac 本机 Recursive",
      "status": "online",
      "last_seen": "2026-06-05T10:01:30Z",
      "queue_size": 0
    },
    {
      "name": "openclaw-01",
      "label": "OpenClaw 实例 1",
      "status": "offline",
      "last_seen": "2026-06-05T09:55:00Z",
      "queue_size": 0
    }
  ],
  "total": 2,
  "online": 1
}
```

### POST /hub/clients/{name}/revoke

吊销一个客户端当前持有的全部授权，并轮换它的 vtoken。

**鉴权：** `Authorization: Bearer <ILINK_ADMIN_TOKEN>`

**行为：**

1. 删除旧 vtoken 的授权行（`routing_state`、`active_sessions`、`backend_sessions_v2`）；
2. 生成新的明文 vtoken，替换 registry / `clients` 表中的旧 hash；
3. 清空旧 vtoken 的路由、队列与 `last_seen`，并把该客户端标记为下线（bridge 仍持有旧凭据，需用新明文重新连接）。

消息历史（`messages`）保留不变 —— 历史记录不再构成发送授权。

**响应：**

```json
{
  "ret": 0,
  "name": "mac-home",
  "vtoken": "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
}
```

::: warning
`vtoken` 只在本次响应中返回一次明文，Hub 只保存 hash，无法恢复。撤销不可逆：若明文遗失，只能用 `DELETE /hub/clients/{name}` 删除后重新配对。
:::

**错误：** 名字为空 → `400`；客户端不存在 → `404`；存储失败 → `500`。

### GET /hub/ui

Web 管理面板（浏览器界面）。返回 HTML 页面。

---

## 其他端点

### GET /health

健康检查，可用于监控和负载均衡器探活。

**响应（正常）：**

```json
{
  "status": "ok",
  "upstream": "connected",
  "clients": {
    "online": 2,
    "total": 3
  }
}
```

**响应（上游断开）：**

```json
{
  "status": "degraded",
  "upstream": "reconnecting",
  "clients": {
    "online": 2,
    "total": 3
  }
}
```

HTTP 状态码：正常返回 `200`，降级状态也返回 `200`（便于区分完全故障）。

### GET /metrics

Prometheus 格式的指标数据。

详见 [Prometheus 指标](/reference/metrics)。
