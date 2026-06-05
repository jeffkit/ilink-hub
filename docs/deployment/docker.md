# Docker 部署

Docker 是生产环境部署 iLink Hub 的推荐方式，开箱即用，无需安装 Rust。

## 快速启动（Docker Compose）

创建 `docker-compose.yml`：

```yaml
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
      # ILINK_TOKEN: your-token      # 可选：跳过 QR 登录
      # ILINK_ADMIN_TOKEN: secret    # 推荐：保护管理接口
      # ILINK_HUB_ADDR: 0.0.0.0:8765

volumes:
  ilink-hub-data:
```

启动服务：

```bash
docker compose up -d
```

## 首次登录（QR 码扫码）

```bash
# 需要 -it 显示 QR 码（交互式终端）
docker compose exec ilink-hub ilink-hub login
```

登录成功后 Token 保存在 `/data/ilink-hub.db`，下次启动无需重新登录。

## 使用 PostgreSQL 数据库

适合多实例或需要高并发的场景：

```yaml
services:
  ilink-hub:
    image: ghcr.io/jeffkit/ilink-hub:latest
    restart: unless-stopped
    ports:
      - "8765:8765"
    environment:
      DATABASE_URL: postgres://ilink:password@db:5432/ilink_hub
      ILINK_ADMIN_TOKEN: your-admin-secret
    depends_on:
      db:
        condition: service_healthy

  db:
    image: postgres:16-alpine
    restart: unless-stopped
    environment:
      POSTGRES_DB: ilink_hub
      POSTGRES_USER: ilink
      POSTGRES_PASSWORD: password
    volumes:
      - pg-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U ilink -d ilink_hub"]
      interval: 5s
      timeout: 5s
      retries: 5

volumes:
  pg-data:
```

## 使用 Docker Run（不用 Compose）

```bash
# 创建数据卷
docker volume create ilink-hub-data

# 首次登录
docker run -it --rm \
  -v ilink-hub-data:/data \
  -e DATABASE_URL=sqlite:/data/ilink-hub.db \
  ghcr.io/jeffkit/ilink-hub:latest login

# 后台运行服务
docker run -d \
  --name ilink-hub \
  --restart unless-stopped \
  -p 8765:8765 \
  -v ilink-hub-data:/data \
  -e DATABASE_URL=sqlite:/data/ilink-hub.db \
  -e ILINK_ADMIN_TOKEN=your-secret \
  ghcr.io/jeffkit/ilink-hub:latest
```

## 查看日志

```bash
docker compose logs -f ilink-hub
# 或
docker logs -f ilink-hub
```

## 更新到新版本

```bash
docker compose pull
docker compose up -d
```

## 健康检查

Docker 镜像内置健康检查，访问 `http://localhost:8765/health` 会返回：

```json
{"status":"ok","upstream":"connected","clients":{"online":2,"total":3}}
```

Compose 可添加健康检查配置：

```yaml
services:
  ilink-hub:
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:8765/health"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s
```

## 反向代理（推荐生产环境）

配合 Nginx 或 Caddy，为 Hub 添加 HTTPS 支持，详见 [安全建议](/deployment/security)。
