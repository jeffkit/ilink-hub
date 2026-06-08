# QR 码登录

> 最后更新：2026-06-07

大多数情况下**不需要**单独使用本页的 `login` 子命令：直接执行 [`ilink-hub serve`](./getting-started.md)，若数据库里没有有效 iLink Token，Hub 会在启动阶段**自动**在终端打印二维码并完成绑定。

`ilink-hub login` 适用于：只想刷新/写入 Token、**不想**同时启动 HTTP 服务；或在某些环境下希望把「登录」与「常驻 serve」拆成两步。

## 与 `serve` 的关系

| 方式 | 行为 |
|------|------|
| `ilink-hub serve` | 无有效 Token 时内联 QR 登录，成功后保存并继续监听 |
| `ilink-hub login` | 仅执行 QR 登录并把 Token 写入数据库，然后退出 |

两者使用相同的 `DATABASE_URL` 与 `ILINK_BASE_URL`，写入的是同一份凭证。

## 执行独立登录

```bash
ilink-hub login
```

### 可选参数

```bash
ilink-hub login \
  --database-url sqlite:/path/to/custom.db
```

或通过环境变量：

```bash
DATABASE_URL=sqlite:/data/ilink-hub.db ilink-hub login
```

默认数据库为 **`~/.ilink-hub/ilink-hub.db`**（与 `serve` 一致），除非你通过 `--database-url` 或 `DATABASE_URL` 指定了其他路径。

## 登录流程

1. 终端显示 QR 码
2. 用**已开通 iLink API 的微信账号**扫码
3. 在手机上确认授权
4. 终端显示「登录成功」，Token 自动写入数据库

## 跳过扫码（已有 Token）

如果你已经通过其他方式获得了 iLink Token，可以通过环境变量直接注入，**启动 serve 时**跳过 QR 码：

```bash
ILINK_TOKEN=your-existing-token ilink-hub serve
```

## 常见问题

### 二维码不显示或显示乱码

终端不支持 Unicode 块字符时可能出现乱码。尝试：

- 使用 iTerm2（macOS）或 Windows Terminal
- Docker 场景下用 `docker compose logs -f` 查看容器标准输出中的二维码

### 登录后 Token 保存在哪里？

由 `DATABASE_URL` 决定，未设置时默认为 **`~/.ilink-hub/ilink-hub.db`**（SQLite 文件）。Docker 示例中常为挂载卷内的 `/data/ilink-hub.db`。

### Token 会过期吗？

iLink Token 本身不会自动过期，但微信账号的授权可能因安全原因被撤销。若 `serve` 启动后提示 Token 无效，可再次直接运行 `ilink-hub serve`（会重新出码），或先执行 `ilink-hub login` 仅更新凭证。

### Docker 环境中如何登录？

镜像默认入口为 `ilink-hub serve`。推荐**直接启动服务**，在日志里完成首次扫码：

```bash
docker compose up -d
docker compose logs -f ilink-hub
```

若你更习惯先单独登录再后台运行，可使用交互式一次性容器：

```bash
docker run -it --rm \
  -v ilink-hub-data:/data \
  -e DATABASE_URL=sqlite:/data/ilink-hub.db \
  ghcr.io/jeffkit/ilink-hub:latest login
```

随后用相同 `DATABASE_URL` 与数据卷运行 `serve`（例如 `docker compose up -d`）。
