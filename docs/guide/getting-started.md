# 快速开始

本指南带你完成 iLink Hub 的安装、启动和客户端接入。

::: tip 不想用终端？
直接[下载桌面版](/guide/installation#desktop)，双击安装，无需任何命令行操作。桌面端的「Bridge」页也可以创建 Claude Code Profile、启动 / 停止 Bridge，并沿用 `~/.ilink-hub/ilink-hub-bridge.yaml`。
:::

::: warning 使用前确认
你需要在微信中开启 **ClawBot（龙虾插件）**。更新微信到最新版，进入「我 → 设置 → 插件」开启即可，无需申请审核。
:::

---

## 第一步：安装 iLink Hub

根据你的系统选择安装方式。

**不知道自己是哪种 Mac？** 点击屏幕左上角苹果图标 → 「关于本机」，看「芯片」一栏：
- 写着 **Apple M1 / M2 / M3 / M4** → 选 Apple Silicon
- 写着 **Intel** → 选 Intel

::: code-group

```bash [macOS（Homebrew，推荐）]
# 如果没有 Homebrew，先安装：https://brew.sh
brew tap jeffkit/tap
brew install ilink-hub
```

```bash [macOS Apple Silicon（M 系列）]
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

:::

> **Windows 用户**：从 [Releases 页面](https://github.com/jeffkit/ilink-hub/releases) 下载 `ilink-hub-windows-x86_64.exe`，重命名为 `ilink-hub.exe` 并放到 PATH 中。

验证安装成功：

```bash
ilink-hub --version
```

看到版本号（如 `ilink-hub 0.1.17`）就说明安装成功了。

::: details 提示「command not found」怎么办？
- **Homebrew 安装**：运行 `brew doctor` 检查环境
- **手动下载**：确认已执行 `chmod +x` 和 `sudo mv` 两步
- **所有方式**：尝试重新打开终端窗口
:::

---

## 第二步：启动 Hub（首次会出二维码扫码登录）

```bash
ilink-hub serve --addr 127.0.0.1:8765
```

**首次启动**，Hub 会自动在终端显示一个二维码：

```
首次启动需要绑定微信机器人，请扫描下方二维码登录。

█████████████████████
█ ▄▄▄▄▄ █▀▀▀▀▀█ ▄▄▄▄▄ █
█ █   █ █ ▄▄▄ █ █   █ █
...（二维码字符）

```

用你**已开通 iLink 的微信账号**扫码，手机上确认授权后，终端会显示：

```
iLink login successful, token saved
INFO ilink_hub: iLink Hub listening
```

Hub 启动成功。**保持这个终端窗口开着**，关掉就停止服务了。

::: details 二维码扫了没反应？
- 确认用的是**已开通 iLink 的微信账号**，普通微信账号无法授权
- 二维码有效期约 2 分钟，超时后重新运行命令即可
- 手机扫码后如果没有跳出授权页，检查手机网络是否正常
:::

::: details 提示「Address already in use」？
端口 8765 被占用了。换一个端口：
```bash
ilink-hub serve --addr 127.0.0.1:8766
```
:::

::: tip 想在后台运行？
```bash
nohup ilink-hub serve --addr 127.0.0.1:8765 > ilink-hub.log 2>&1 &
```
日志会写入 `ilink-hub.log`。生产环境推荐用 [Docker 部署](/deployment/docker)。
:::

---

## 第三步：打开 Web 管理面板

Hub 启动后，在浏览器访问：

```
http://127.0.0.1:8765/hub/ui
```

你会看到管理界面。这里可以查看运行状态、注册 AI 客户端、复制配置。

::: details 打不开页面？
- 确认 Hub 还在运行（终端没有报错）
- 如果 Hub 不在本机，把 `127.0.0.1` 换成实际 IP 地址
- 检查防火墙是否放行了 8765 端口
:::

---

## 第四步：注册 AI 客户端

每个需要接入的 AI 工具都需要注册一次，获取一个专属的**虚拟凭证（Token）**。

**方式一：通过 Web 界面注册（推荐）**

在 `/hub/ui` 管理面板点击「注册新客户端」，填写名称即可。注册完成后直接复制配置。

**方式二：通过命令行注册**

```bash
ilink-hub register \
  --hub-url http://127.0.0.1:8765 \
  --name my-claude \
  --label "我的 Claude"
```

注册成功后会显示：

```
✓ 客户端注册成功！

  名称：my-claude
  虚拟 Token：vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

请将以下配置添加到你的 AI 客户端：
  WEIXIN_BASE_URL=http://127.0.0.1:8765
  WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

> **请保存好这个 Token**。如果忘记了，可以在 Web 管理面板的客户端列表里查看。

---

## 第五步：配置你的 AI 工具

把上一步的两行配置填入你的 AI 工具：

::: code-group

```toml [Recursive（~/.recursive/config.toml）]
[weixin]
base_url = "http://127.0.0.1:8765"
token = "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
```

```bash [环境变量（大多数工具通用）]
export WEIXIN_BASE_URL=http://127.0.0.1:8765
export WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

```json [OpenClaw（~/.openclaw/openclaw.json）]
{
  "channels": {
    "weixin": {
      "base_url": "http://127.0.0.1:8765",
      "token": "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    }
  }
}
```

:::

---

## 完成！测试一下

在微信里发送 `/status`，正常情况下会收到回复：

```
iLink Hub 状态：1/1 个客户端在线
```

再发一条普通消息，看看你的 AI 工具是否收到并回复。

::: details AI 工具没收到消息？
1. 确认 AI 工具正在运行，没有报错
2. 在微信发 `/list`，查看客户端是否显示「在线」
3. 如果显示「离线」，重启 AI 工具
4. 如果注册了多个客户端，用 `/use my-claude` 切换到对应的那个
:::

---

## 常用微信命令

接入成功后，在微信里可以用这些命令管理 Hub：

| 命令 | 作用 |
|------|------|
| `/list` | 查看所有已注册的 AI 及在线状态 |
| `/use <名称>` | 切换当前使用的 AI（如 `/use my-claude`） |
| `/status` | 查看 Hub 整体状态 |
| `/help` | 显示帮助 |

---

## 下一步

- [接入 Claude Code](/guide/claude-code) — 完整教程：从安装到对话
- [5 分钟上手（echo）](/bridge/quick-try) — 先用 echo 命令验证完整链路
- [Docker 部署](/deployment/docker) — 服务器上 7×24 小时稳定运行
- [常见问题 FAQ](/guide/faq) — 遇到问题先查这里
