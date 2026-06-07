# 5 分钟体验：本地 CLI Bridge

走通 **Homebrew 安装 → Hub 启动 →（可选扫码）Bridge → 微信收 echo**。无需先装 Recursive / OpenClaw；确认链路后再把 YAML 里的 `command` 换成 Claude Code、Codex 等。

::: warning 适用范围
- 下文以 **macOS + Homebrew** 为例；Linux/Windows 可从 [GitHub Releases](https://github.com/jeffkit/ilink-hub/releases) 下载 `ilink-hub` 与 `ilink-hub-bridge` 对应资产。  
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

验证（应能看到两个命令）：

```bash
ilink-hub --version
ilink-hub-bridge --version
```

> 若提示找不到 `ilink-hub-bridge`，说明当前 tap 里的 formula 尚未带上 bridge 子命令，请先 `brew update` 再试；仍没有则暂时用 [Releases](https://github.com/jeffkit/ilink-hub/releases) 里同版本的 `ilink-hub-bridge-macos-*` 手动下载到 PATH。

---

## 第二步：启动 Hub 并绑定微信

在终端执行（首次会在本机打印 **微信登录** 二维码，用已开通 iLink 的微信扫码）：

```bash
ilink-hub serve --addr 127.0.0.1:8765
```

看到日志里 Hub 已监听、且完成微信侧绑定即可。**保持该终端不要关**（或改用 `nohup`/后台服务）。

可选：浏览器打开 [http://127.0.0.1:8765/hub/ui](http://127.0.0.1:8765/hub/ui) 查看管理面板。

---

## 第三步：准备 bridge 的 YAML

在**另一终端**、任意目录新建 `ilink-hub-bridge.yaml`：

```yaml
command: echo
args: ["{{MESSAGE}}"]
stdin: none
timeout_secs: 10
```

含义：每条微信**用户文本**会执行 `echo <内容>`，stdout 原样回微信。

---

## 第四步：启动 bridge 并接入 Hub

仍在该目录执行（**不要**先设 `WEIXIN_TOKEN`，第一次走扫码）：

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

1. 终端会出现 **Hub 客户端配对** 二维码（与 OpenClaw 类似）。  
2. **手机浏览器**扫码打开确认页，填写**客户端名称**（例如 `local-bridge`）并确认。  
3. 成功后凭证写入默认路径 **`~/.ilink-hub/bridge-credentials.json`**（可用环境变量 `ILINKHUB_BRIDGE_CREDS` 改路径）。  
4. 再次启动 bridge 时，若文件已存在，会**自动读 token**，无需再扫码；若要重新配对，加 `--pair`。

::: tip 与 `ilink-hub register` 的关系
两种方式二选一即可：  
- **扫码配对**（推荐体验）：上面流程，手机页里起名即完成注册。  
- **CLI 注册**：`ilink-hub register --name local-bridge ...`，再 `export WEIXIN_TOKEN=vhub_...` 启动 bridge。  
:::

---

## 第五步：在微信里切到该后端

若 Hub 里已有多个客户端，发：

```
/use local-bridge
```

（把 `local-bridge` 换成你在配对页填写的**名称**。若这是第一个客户端，Hub 可能已设为默认，可先直接发下一条测试。）

---

## 第六步：发一条普通文字

发**不以 `/` 开头**的文本，例如：

```
你好 hub
```

预期：几秒内收到内容同样为 `你好 hub` 的回复（echo）。

---

## 故障排查

- **`ilink-hub-bridge` 一直等不到消息**：在微信发 `/list`，确认 `local-bridge` **在线**，并已 `/use local-bridge`。  
- **扫码后失败**：看 Hub 与 bridge 两边终端日志；确认手机能打开配对页（本机 Hub 常配合 [手机扫码配对](/guide/pairing-tunnel) 的中继说明）。  
- **想换 Hub 地址**：删掉凭证文件或指定 `ILINKHUB_BRIDGE_CREDS` 后，用 `--pair` 重新配对。

更多字段说明见 [功能与配置](./README.md)。FAQ：[客户端与 bridge](/guide/faq#bridge-no-msg)。

---

最后更新：2026-06-07
