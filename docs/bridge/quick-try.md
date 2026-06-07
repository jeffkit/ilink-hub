# 5 分钟体验：本地 CLI Bridge

用系统自带的 `echo` 做「假 AI」，走通 **Hub → 虚拟 Token → `ilink-hub-bridge` → 本机命令 → 回微信** 全链路。无需安装 Claude / Codex；确认链路正常后，再把 `command` / `args` 换成真实编程 CLI 即可。

::: warning 适用范围
Bridge 必须和 Hub **网络互通**（本机最常见：`127.0.0.1`）。在另一台机器上跑 Hub 时，请把下文中的地址换成可访问的 Hub URL，并注意防火墙。
:::

## 你需要

- 已按 [快速开始](/guide/getting-started) 跑起来 Hub，且微信能收到 `/status` 一类回复  
- 本机已安装 **Rust 工具链**（推荐：用 `cargo` 从源码编 bridge），或已安装/下载的 `ilink-hub` 发行包中**带有** `ilink-hub-bridge` 可执行文件（以 [Releases](https://github.com/jeffkit/ilink-hub/releases) 资产说明为准）  
- 会发微信消息、能执行 `/use`（见 [微信命令](/reference/commands)）

## 第一步：注册专用后端

在 **能访问 Hub** 的终端执行（按你的 Hub 地址改 `--hub-url`）：

```bash
ilink-hub register \
  --hub-url http://127.0.0.1:8765 \
  --name local-bridge \
  --label "CLI bridge 体验"
```

记下输出的 `WEIXIN_TOKEN`（`vhub_…`）。若 Hub 配置了 `ILINK_ADMIN_TOKEN`，注册时需带管理端 Bearer，见 [环境变量配置](/reference/configuration)。

## 第二步：准备配置文件

在任意工作目录新建 `ilink-hub-bridge.yaml`，内容如下（可直接复制）：

```yaml
command: echo
args: ["{{MESSAGE}}"]
stdin: none
timeout_secs: 10
```

含义：每条微信文本消息会执行 `echo <消息内容>`，stdout 原样发回微信。

## 第三步：编译并启动 bridge

**从本仓库源码构建**（适合正在开发或克隆了仓库的情况）：

```bash
cd /path/to/ilink-hub
cargo build --release --bin ilink-hub-bridge
```

启动（把 `vhub_xxx` 换成你的虚拟 Token）：

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
export WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxx
./target/release/ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

若 `cargo install ilink-hub` 安装的版本已包含 `ilink-hub-bridge`（与 `cargo install` 所装版本一致），也可直接：

```bash
WEIXIN_BASE_URL=http://127.0.0.1:8765 WEIXIN_TOKEN=vhub_xxx ilink-hub-bridge
```

终端出现类似 `ilink-hub-bridge connected; waiting for getupdates` 即表示长轮询已挂上。

## 第四步：把微信路由到这个后端

在微信里向机器人发送：

```
/use local-bridge
```

（`local-bridge` 须与第一步 `--name` 一致。）

## 第五步：发一条普通文字

发一句 **不以 `/` 开头** 的文本，例如：

```
你好 hub
```

预期：几秒内收到一条回复，内容为 `你好 hub`（即 echo 的 stdout）。

::: tip 没反应？
- 先发 `/list`，确认 `local-bridge` 为 **在线**  
- 确认当前活跃后端是 bridge：`/use local-bridge`  
- 看 bridge 终端日志是否有 `getupdates` 或 `spawn` 报错  
:::

## 接下来做什么？

- 阅读 [功能与配置说明](./README.md)：占位符、`stdin: message`、超时、截断、接 Claude / Codex 等  
- 与 [配置 AI 客户端](/guide/client-config) 里其他后端并列：bridge 占用一个虚拟 Token，仍用 `/use` 切换  
- 仓库内还有示例 YAML：[echo](https://github.com/jeffkit/ilink-hub/tree/main/docs/bridge/examples/echo.example.yaml)、[claude-code / codex 模板](https://github.com/jeffkit/ilink-hub/tree/main/docs/bridge/examples)（需按本机 CLI 修改）

---

若你愿意把真实 CLI 接到生产环境，务必阅读 [安全建议](/deployment/security)：`bridge` 会在本机执行配置的命令，请使用**参数数组**、避免拼 `sh -c` 与用户可控字符串注入。
