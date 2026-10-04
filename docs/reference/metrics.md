# Prometheus 指标

iLink Hub 在 `/metrics` 端点暴露 Prometheus 格式的监控指标。

## 访问指标

```bash
curl http://localhost:8765/metrics
```

::: tip 安全建议
生产环境建议通过反向代理限制 `/metrics` 端点只允许内网访问，防止指标数据泄露业务信息。
:::

## 指标列表

以下为当前实现中实际输出的指标名（与 `/metrics` 文本一致）。

### 客户端与队列

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `ilink_hub_clients_online` | Gauge | 当前在线后端数量 |
| `ilink_hub_clients_total` | Gauge | 已注册后端总数 |
| `ilink_hub_queue_size` | Gauge（带 `client` 标签） | 每个后端当前待下发队列长度；已删除客户端的 vtoken 归入单个 `client="unknown"` 序列（多客户端求和），字面名为 `unknown` 的客户端与之合并 |
| `ilink_hub_messages_rejected_total` | Counter（带 `client` 标签） | 因队列满被背压拒绝的消息条数（按客户端归因）；标签值按 exposition 规则转义，无名字映射的 vtoken 归入 `unknown` 桶 |

### 消息与上游

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `ilink_hub_messages_dispatched_total` | Counter | 已尝试下发到后端队列的消息条数（含广播分支中每个目标一条） |
| `ilink_hub_messages_dropped_total` | Counter | 因队列满或推送失败而丢弃的条数 |
| `ilink_hub_upstream_user_messages_total` | Counter | 从微信上游进入 Hub 并参与路由的消息条数（不含 `message_type == 2` 的 bot echo 副本） |
| `ilink_hub_upstream_polls_ok_total` | Counter | 上游 `getupdates` 长轮询成功次数 |
| `ilink_hub_upstream_polls_err_total` | Counter | 上游轮询失败或错误响应次数 |

### 每租户出站限流

所有租户共用同一个真实微信账号与同一条上游连接池，上游配额是全局的。Hub 对出站路径
（`sendmessage` / `sendtyping` / `getconfig` / `getuploadurl` / MCP `call_agent`）按
vtoken 做令牌桶限流，被限流时返回 HTTP 429。默认 20 req/s、突发 40，
可用 `ILINK_BOT_RATE_LIMIT_PER_SEC` / `ILINK_BOT_RATE_LIMIT_BURST` 调整。

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `ilink_hub_ratelimit_tokens` | Gauge（带 `client` 标签） | 该租户令牌桶剩余配额（越接近 0 越接近 429） |
| `ilink_hub_ratelimit_burst` | Gauge（带 `client` 标签） | 该租户令牌桶的突发容量 |
| `ilink_hub_ratelimit_rejected_total` | Counter（带 `client` 标签） | 该租户被限流拒绝的出站请求数 |

未产生过出站请求的后端没有令牌桶，因此不会出现在上述指标中——**没有序列表示「没有流量」，不是「没有限流」**。

## 示例 Prometheus 配置

```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'ilink-hub'
    static_configs:
      - targets: ['localhost:8765']
    metrics_path: /metrics
    scrape_interval: 30s
```

## 示例 Grafana 面板查询

**消息下发速率（每分钟）：**

```promql
rate(ilink_hub_messages_dispatched_total[1m]) * 60
```

**消息丢弃率（告警用）：**

```promql
rate(ilink_hub_messages_dropped_total[5m]) > 0
```

**在线客户端数量：**

```promql
ilink_hub_clients_online
```
