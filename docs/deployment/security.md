# 安全建议

iLink Hub 作为微信账号的认证代理，安全性至关重要。以下是生产环境的最佳实践。

## 1. 保护管理接口

设置 `ILINK_ADMIN_TOKEN` 环境变量来保护 `/hub/` 管理端点：

```bash
ILINK_ADMIN_TOKEN=your-strong-random-secret ilink-hub serve
# 或 Docker：
# -e ILINK_ADMIN_TOKEN=your-strong-random-secret
```

设置后，调用 `/hub/register`、`/hub/clients` 需要携带 HTTP 头：

```
Authorization: Bearer your-strong-random-secret
```

Web UI `/hub/ui` 也会要求输入 Token 才能访问。

::: warning 生产必须设置
未设置 `ILINK_ADMIN_TOKEN` 时，管理端点完全开放，任何人都能注册新客户端。**生产环境必须设置此变量。**
:::

## 2. 使用 HTTPS

通过反向代理（Nginx 或 Caddy）为 Hub 添加 TLS 加密：

### Nginx + Certbot

```nginx
server {
    listen 443 ssl http2;
    server_name hub.example.com;

    ssl_certificate /etc/letsencrypt/live/hub.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hub.example.com/privkey.pem;

    # 限制管理接口只允许内网访问
    location /hub/ {
        allow 192.168.1.0/24;
        allow 10.0.0.0/8;
        deny all;
        proxy_pass http://localhost:8765;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }

    # iLink API 对注册客户端开放
    location /ilink/ {
        proxy_pass http://localhost:8765;
        proxy_set_header Host $host;
        proxy_read_timeout 35s;  # 长轮询需要稍长的超时
    }

    location /health {
        proxy_pass http://localhost:8765;
    }

    location /metrics {
        # 指标接口仅内网访问
        allow 10.0.0.0/8;
        deny all;
        proxy_pass http://localhost:8765;
    }
}
```

### Caddy（自动 HTTPS）

```
hub.example.com {
    # 管理接口 IP 限制
    handle /hub/* {
        @blocked not remote_ip 192.168.1.0/24 10.0.0.0/8
        abort @blocked
        reverse_proxy localhost:8765
    }

    handle /ilink/* {
        reverse_proxy localhost:8765 {
            transport http {
                read_timeout 35s
            }
        }
    }

    handle {
        reverse_proxy localhost:8765
    }
}
```

## 3. 网络隔离

- **Hub 本身无需直接暴露到公网**：只要 iLink 客户端（AI 后端）能访问到 Hub 即可
- 如果客户端和 Hub 在同一局域网/VPC，只需内网 IP，不需要公网端口
- 考虑使用 Tailscale 或 WireGuard 组建私有网络
- **暴露到局域网/容器**：Hub 默认只监听在 `127.0.0.1` 以确保安全。如果需要对外暴露（例如在 Docker 容器、虚拟机或局域网中运行），必须显式传 `--addr 0.0.0.0:8765`。

## 4. 日志安全

Hub 的日志**不会**输出以下敏感信息：

- 真实 context_token
- 虚拟 Token 明文
- 微信用户标识符
- 密码

但仍建议：

- 限制日志文件的访问权限（`chmod 600`）
- 生产环境设置合适的日志级别（`RUST_LOG=info`，不用 `debug`/`trace`）

## 5. 定期轮换虚拟 Token

如果某个客户端的 Token 疑似泄露：

```bash
# 重新注册同名客户端（旧 Token 自动失效）
ilink-hub register --hub-url http://... --name mac-home --label "Mac 本机"
# 然后更新该客户端的配置文件，使用新 Token
```

## 6. 数据库备份

如果使用 SQLite，定期备份数据文件：

```bash
# 简单备份（路径与 DATABASE_URL 一致；本机默认常为当前目录下的 ilink-hub.db）
cp ./ilink-hub.db ~/backups/ilink-hub-$(date +%Y%m%d).db

# cron 每日备份（Docker 常见挂载路径）
0 2 * * * cp /data/ilink-hub.db /backup/ilink-hub-$(date +\%Y\%m\%d).db
```

数据库中存储的是敏感数据（Token 映射），请妥善保管备份文件。
