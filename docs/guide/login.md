# QR 码登录

iLink Hub 使用微信 iLink API 的 QR 码扫码授权流程。登录一次后，Token 会持久化保存到数据库，**无需重复登录**。

## 执行登录

```bash
ilink-hub login
```

### 可选参数

```bash
ilink-hub login \
  --db sqlite:/path/to/custom.db   # 指定数据库路径（默认：~/.local/share/ilink-hub/ilink-hub.db）
```

或通过环境变量：

```bash
DATABASE_URL=sqlite:/data/ilink-hub.db ilink-hub login
```

## 登录流程

1. 终端显示 QR 码
2. 用**已开通 iLink API 的微信账号**扫码
3. 在手机上确认授权
4. 终端显示「登录成功」，Token 自动写入数据库

```
扫描以下二维码登录微信 iLink API：

██████████████████████████████████
█ ▄▄▄▄▄ █▀▄▄▀██▀█ ▀▄▄▄▄ ▄▄▄▄▄ █
█ █   █ █ █▀ █▀▀▄▀█ █▄  █   █ █
█ █▄▄▄█ █▀█▄▀▀▀▄ ▀▀▀  █ █▄▄▄█ █
...
██████████████████████████████████

等待扫码...
✓ 登录成功！context_token 已保存到数据库。
```

## 跳过扫码（已有 Token）

如果你已经通过其他方式获得了 iLink Token，可以通过环境变量直接注入，跳过 QR 码登录：

```bash
ILINK_TOKEN=your-existing-token ilink-hub serve
```

## 常见问题

### 二维码不显示或显示乱码

终端不支持 Unicode 块字符时可能出现乱码。尝试：

- 使用 iTerm2（macOS）或 Windows Terminal
- 或直接访问登录页面（如果 Hub 提供了 Web 登录界面）

### 登录后 Token 保存在哪里？

默认保存在系统数据目录：
- **macOS/Linux**: `~/.local/share/ilink-hub/ilink-hub.db`
- **Docker**: 挂载的 `/data/ilink-hub.db`

### Token 会过期吗？

iLink Token 本身不会自动过期，但微信账号的授权可能因安全原因被撤销。如遇连接失败，重新执行 `ilink-hub login` 即可。

### Docker 环境中如何登录？

```bash
# 需要 -it 参数启用交互式终端以显示 QR 码
docker run -it --rm \
  -v ilink-hub-data:/data \
  -e DATABASE_URL=sqlite:/data/ilink-hub.db \
  ghcr.io/jeffkit/ilink-hub:latest login
```
