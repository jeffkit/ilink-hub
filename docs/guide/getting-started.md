# 快速开始

本指南带你在 5 分钟内完成 iLink Hub 的安装、登录和客户端接入。

## 前提条件

- 一个已开通 iLink API 的微信账号（ClawBot）
- macOS / Linux / Windows 机器（可访问外网）
- 可选：Docker（最简单的部署方式）

## 第一步：安装 iLink Hub

根据你的系统选择一种安装方式：

::: code-group

```bash [Homebrew（macOS 推荐）]
brew tap jeffkit/tap
brew install ilink-hub
```

```bash [macOS Apple Silicon (M 系列)]
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-aarch64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

```bash [macOS Intel]
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

```bash [Linux x86_64]
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

```bash [Cargo（需要 Rust）]
cargo install ilink-hub
```

:::

> **Windows 用户**：从 [Releases 页面](https://github.com/jeffkit/ilink-hub/releases) 下载 `ilink-hub-windows-x86_64.exe`。

验证安装成功：

```bash
ilink-hub --version
# 输出：ilink-hub 0.1.4
```

## 第二步：扫码登录微信

```bash
ilink-hub login
```

终端会显示一个二维码，用**已开通 iLink API 的微信账号**扫码登录。登录成功后，Token 会自动保存到本地数据库，无需重复登录。

```
扫描以下二维码登录：
█████████████████████████████████
█ ▄▄▄▄▄ █▀▀▄██▄▀ ▄█ █ ▄▄▄▄▄ █
█ █   █ █▀█▀▀▀▀▄▀▀▀▄ █ █   █ █
...（二维码内容）...
✓ 登录成功！Token 已保存。
```

## 第三步：启动 Hub 服务

```bash
ilink-hub serve --addr 0.0.0.0:8765
```

服务启动后，你会看到类似输出：

```
2026-06-05T10:00:00Z  INFO ilink_hub: Starting iLink Hub v0.1.4
2026-06-05T10:00:00Z  INFO ilink_hub: Listening on 0.0.0.0:8765
2026-06-05T10:00:00Z  INFO ilink_hub: Admin UI: http://localhost:8765/hub/ui
2026-06-05T10:00:00Z  INFO ilink_hub: Polling upstream iLink...
```

::: tip 后台运行
使用 `nohup ilink-hub serve --addr 0.0.0.0:8765 &` 或者 systemd/screen 在后台保持运行。
:::

## 第四步：打开 Web 管理面板

在浏览器中访问：

```
http://localhost:8765/hub/ui
```

你会看到 iLink Hub 的管理界面，可以在这里注册客户端并复制配置。

## 第五步：注册 AI 客户端

每个需要接入的 AI 后端都需要注册一次，获取一个专属的虚拟 Token。

**通过 CLI 注册：**

```bash
ilink-hub register \
  --hub-url http://your-hub.example.com:8765 \
  --name mac-home \
  --label "Mac 本机"
```

**或通过 Web UI 注册**（在 `/hub/ui` 页面点击「注册新客户端」）

注册成功后会输出：

```
✓ 客户端注册成功！

客户端名称：mac-home
虚拟 Token：vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

请将以下配置添加到你的 AI 客户端：
  WEIXIN_BASE_URL=http://your-hub.example.com:8765
  WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

## 第六步：配置 AI 客户端

将上一步获得的配置填入你的 AI 客户端：

::: code-group

```toml [Recursive (~/.recursive/config.toml)]
[weixin]
base_url = "http://your-hub.example.com:8765"
token = "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
```

```bash [环境变量（通用）]
export WEIXIN_BASE_URL=http://your-hub.example.com:8765
export WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

```json [OpenClaw (~/.openclaw/openclaw.json)]
{
  "channels": {
    "weixin": {
      "base_url": "http://your-hub.example.com:8765",
      "token": "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    }
  }
}
```

:::

## 完成！测试一下

在微信中发送 `/status`，如果 Hub 正常运行，你会收到类似回复：

```
Hub 状态：在线
已注册客户端：2
在线客户端：1（mac-home）
```

---

## 下一步

- [了解微信命令](/reference/commands) — 学习 `/list`、`/use` 等命令
- [Docker 部署](/deployment/docker) — 更稳定的生产环境部署方式
- [安全建议](/deployment/security) — 如何保护你的 Hub 实例
