# 注册客户端

每个需要接入 Hub 的 AI 后端（如不同机器上的 Recursive、OpenClaw 等）都需要注册一次，获得一个专属的**虚拟 Token（vtoken）**。

## 通过 CLI 注册

```bash
ilink-hub register \
  --hub-url http://your-hub.example.com:8765 \
  --name mac-home \
  --label "Mac 本机 Recursive"
```

### 参数说明

| 参数 | 必填 | 说明 |
|------|------|------|
| `--hub-url` | 是 | Hub 服务的地址（包含端口） |
| `--name` | 是 | 客户端唯一标识（字母、数字、连字符，用于微信命令 `/use <name>`） |
| `--label` | 否 | 可读的显示名称（默认与 `--name` 相同） |

### 成功输出示例

```
✓ 客户端注册成功！

  名称：mac-home
  标签：Mac 本机 Recursive
  虚拟 Token：vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

请将以下配置添加到你的 AI 客户端：
  WEIXIN_BASE_URL=http://your-hub.example.com:8765
  WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

⚠️  请妥善保存虚拟 Token，之后无法从 Hub 查回明文。
```

::: warning Token 安全
虚拟 Token 只在注册时返回一次明文。之后 Hub 只存储其哈希值。如果丢失，需要重新注册（旧 Token 会失效）。
:::

## 通过 Web UI 注册

访问 `http://your-hub.example.com:8765/hub/ui`，点击「注册新客户端」按钮，填写名称和标签即可。Web UI 会直接显示可复制的配置。

## 查看所有已注册客户端

```bash
ilink-hub clients --hub-url http://your-hub.example.com:8765
```

输出示例：

```
已注册客户端（3 个）：

  NAME         LABEL                 STATUS    LAST_SEEN
  mac-home     Mac 本机 Recursive    online    2 秒前
  server-prod  生产服务器 Recursive  online    15 秒前
  openclaw-01  OpenClaw 实例 1       offline   3 分钟前
```

或访问 `/hub/ui` 在 Web 界面查看。

## 如何区分多个客户端？

建议按**运行环境**或**用途**命名：

| 名称 | 标签 | 场景 |
|------|------|------|
| `mac-home` | Mac 本机（家） | 家里电脑的 Recursive |
| `mac-office` | Mac 本机（公司） | 公司电脑的 Recursive |
| `server-main` | 主服务器 | 云服务器上的 OpenClaw |
| `test` | 测试用途 | 开发调试用的实例 |

## 切换活跃客户端

微信中所有消息默认路由到当前活跃客户端。使用微信命令切换：

```
/use mac-home
```

详见 [微信命令](/reference/commands)。
