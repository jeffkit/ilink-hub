# 5 分钟体验：本地 CLI Bridge

走通 **Homebrew 安装 → Hub 启动 → Bridge（默认自动注册）→ 微信收 echo**。Hub 只把 bridge 当成普通下游客户端，**不需要**在 `ilink-hub` 里增加任何 bridge 专用逻辑。接 **Claude Code / Cursor / Codex** 请看 [使用指引](./USAGE.md)。

::: warning 适用范围
- 下文以 **macOS + Homebrew** 为例；Linux/Windows 可从 [GitHub Releases](https://github.com/jeffkit/ilink-hub/releases) 下载对应资产。  
- Bridge 必须能访问 Hub 的 HTTP 端口（本机常用 `127.0.0.1:8765`）。
:::

## 你需要

- 已开通 **微信 iLink（ClawBot）** 的账号  
- macOS 上已安装 [Homebrew](https://brew.sh/)

---

## 第一步：安装 iLink Hub（含 bridge）

```bash
brew tap jeffkit/tap
brew update
brew install ilink-hub
```

```bash
ilink-hub --version
ilink-hub-bridge --version
```

> 若找不到 `ilink-hub-bridge`，先 `brew update` / `brew upgrade ilink-hub`；仍无则从 [Releases](https://github.com/jeffkit/ilink-hub/releases) 下载 `ilink-hub-bridge-macos-*` 放入 PATH。

---

## 第二步：启动 Hub 并绑定微信

```bash
ilink-hub serve --addr 127.0.0.1:8765
```

用已开通 iLink 的微信完成绑定。**保持该终端运行。**  
（可选）打开 [http://127.0.0.1:8765/hub/ui](http://127.0.0.1:8765/hub/ui)。

---

## 第三步：准备 `ilink-hub-bridge.yaml`

在另一终端、任意目录新建：

```yaml
command: echo
args: ["{{MESSAGE}}"]
stdin: none
timeout_secs: 10
```

---

## 第四步：启动 bridge（默认自动注册，无需扫码）

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
# 若 Hub 配置了 ILINK_ADMIN_TOKEN，这里必须导出相同值，否则自动注册会 401：
# export ILINK_ADMIN_TOKEN=你的管理密钥

ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

- 第一次：进程会调用 Hub 已有的 **`POST /hub/register`**，按 **`local-<hostname>-<配置名>`** 注册稳定客户端名（如 `local-MacBook-ilink-claude`），把 `vhub_…` 写入 **`~/.ilink-hub/bridge-credentials.json`**。  
- 终端会打印一行中文提示，里面包含 **`/use <客户端名>`**，按提示在微信发送即可（若这是 Hub 上**第一个**客户端，通常已是默认路由，也可直接发测试消息）。  
- 第二次起：直接读凭证文件，**无需再注册**。

若你更希望**手机扫码**确认，可改用：`ilink-hub-bridge --pair --config ./ilink-hub-bridge.yaml`（仍走 Hub 通用配对接口）。

---

## 第五步：发一条普通文字

发**不以 `/` 开头**的文本，例如：`你好 hub`。  
预期：收到与内容一致的 echo 回复。

---

## 故障排查

- **自动注册 401**：为 bridge 进程设置与 Hub 一致的 **`ILINK_ADMIN_TOKEN`**。  
- **收不到消息**：`/list` 看是否在线；多客户端时是否已 `/use <名称>`。  
- **想固定客户端名**：`--register-name my-cli` 或环境变量 `ILINKHUB_BRIDGE_REGISTER_NAME`。  
- **凭证文件坏了 / token 空了，又不想删文件**：加 **`--force-register`**（会先删默认凭证路径再自动注册）；或手动删 `~/.ilink-hub/bridge-credentials.json` 后重跑。

更多说明见 [功能与配置](./README.md)、FAQ：[#bridge-no-msg](/guide/faq#bridge-no-msg)。

---

最后更新：2026-06-07
