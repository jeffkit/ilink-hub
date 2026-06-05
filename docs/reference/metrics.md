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

### 消息相关

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `ilink_hub_messages_routed_total` | Counter | 成功路由到客户端的消息总数 |
| `ilink_hub_messages_dropped_total` | Counter | 因队列满而丢弃的消息总数（head-drop） |
| `ilink_hub_messages_broadcast_total` | Counter | 广播消息总数 |
| `ilink_hub_upstream_poll_errors_total` | Counter | 上游轮询失败次数 |

### 客户端相关

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `ilink_hub_clients_active` | Gauge | 当前在线客户端数量 |
| `ilink_hub_clients_total` | Gauge | 已注册客户端总数 |
| `ilink_hub_queue_size` | Gauge（带 `client` 标签） | 每个客户端当前队列长度 |

### HTTP 请求

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `ilink_hub_http_requests_total` | Counter（带 `method`、`path`、`status` 标签） | HTTP 请求总数 |
| `ilink_hub_http_request_duration_seconds` | Histogram | HTTP 请求延迟分布 |

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

**消息路由速率（每分钟）：**
```promql
rate(ilink_hub_messages_routed_total[1m]) * 60
```

**消息丢弃率（告警用）：**
```promql
rate(ilink_hub_messages_dropped_total[5m]) > 0
```

**在线客户端数量：**
```promql
ilink_hub_clients_active
```

**API 请求 P95 延迟：**
```promql
histogram_quantile(0.95, rate(ilink_hub_http_request_duration_seconds_bucket[5m]))
```
