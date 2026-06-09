# 让 AI 帮你安装与配置

> 最后更新：2026-06-09

不想看文档？让你的 AI 助手（Claude Code、Cursor 等）直接读取 iLink Hub 的安装 Skill，帮你完成全部配置。

---

## 什么是 Skill？

Skill 是一段结构化的操作手册，AI 读取后可以自主完成多步骤任务——安装软件、生成配置文件、测试、发布——无需你逐条查阅文档。

iLink Hub 提供两个官方 Skill：

| Skill | 用途 | 原始文件 |
|-------|------|---------|
| **ilink-hub-setup** | 安装 Hub、绑定微信、启动 bridge manager | [SKILL.md](/ilink-hub/skills/ilink-hub-setup/SKILL.md) |
| **bridge-profile** | 创建/测试/发布 Profile，用 Python/JS SDK 开发自定义 handler | [SKILL.md](/ilink-hub/skills/bridge-profile/SKILL.md) |

---

## 在 Claude Code 中使用

### 方式一：一次性让 AI 读取执行（无需安装）

打开 Claude Code，输入：

```
请读取 https://jeffkit.github.io/ilink-hub/skills/ilink-hub-setup/SKILL.md，
然后帮我完成 ilink-hub 的安装与配置。
```

AI 会获取最新的 Skill 内容，引导你完成全部步骤。

### 方式二：安装到本地（持久生效）

把 Skill 安装到 `~/.claude/skills/`，以后直接用 `/ilink-hub-setup` 命令触发：

```bash
# 创建 skill 目录并下载
mkdir -p ~/.claude/skills/ilink-hub-setup
curl -Lo ~/.claude/skills/ilink-hub-setup/SKILL.md \
  https://jeffkit.github.io/ilink-hub/skills/ilink-hub-setup/SKILL.md

mkdir -p ~/.claude/skills/bridge-profile
curl -Lo ~/.claude/skills/bridge-profile/SKILL.md \
  https://jeffkit.github.io/ilink-hub/skills/bridge-profile/SKILL.md
```

安装后在 Claude Code 中输入：

```
/ilink-hub-setup
```

或者

```
/bridge-profile
```

---

## 典型对话示例

**安装 ilink-hub 并连接微信：**

> 你：帮我安装 ilink-hub 并配置好，我用的是 MacBook M3
>
> AI：（读取 Skill 后）我来帮你完成安装。首先确认你的环境……

**创建一个接 Claude Code 的 bridge profile：**

> 你：帮我创建一个 bridge profile，接 Claude Code，项目目录是 ~/projects/myapp
>
> AI：（读取 Skill 后）好的，我来生成配置文件……

**用 Python SDK 开发自定义 handler：**

> 你：我想用 Python 写一个接 OpenAI 的 bridge profile，支持多轮对话
>
> AI：（读取 Skill 后）我来创建 handler.py 和对应的 YAML……

---

## Skill 文件直接链接

如果你的 AI 工具支持直接读取 URL，可以把以下链接粘贴给它：

- **安装配置**：`https://jeffkit.github.io/ilink-hub/skills/ilink-hub-setup/SKILL.md`
- **Profile 开发**：`https://jeffkit.github.io/ilink-hub/skills/bridge-profile/SKILL.md`

Skill 文件随版本更新，始终与最新文档保持同步。
