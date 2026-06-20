---
type: Playbook
title: force-dev 工作流
description: 用 force-dev flow 启动、续跑 feature 分支开发，自动管理 worktree 隔离。
tags: [workflow, force-dev, feature, worktree]
timestamp: 2026-06-18T10:00:00+08:00
---

# force-dev 工作流

force-dev 是基于 flowcast 的 feature 开发 flow，自动创建 git worktree 隔离、执行质量门检查、推送 PR。

## 前置条件

```bash
# 确认 flowcast 已 npm link（只需首次）
cd ~/projects/flowx && npm link

# 确认 resolver 路径
ls ~/projects/flowx/bin/flowcast-resolver.mjs
```

## 启动新 Feature

```bash
# 推荐：准备好 prompt 文件后批量启动（跳过 HITL 确认）
FLOWCAST_PKG_INDEX=~/projects/flowx/index.js \
  node --import ~/projects/flowx/bin/flowcast-resolver.mjs \
  ~/projects/force-lab/flows/force-dev.js \
  --feature <feature-name> \
  --repo /Users/kongjie/projects/ilink-hub \
  --prompt-file /tmp/<feature-name>-prompt.md \
  --hitl terminal \
  2>&1 &

# 交互模式（flow 在 prompt.md 生成后暂停等确认）
FLOWCAST_PKG_INDEX=~/projects/flowx/index.js \
  node --import ~/projects/flowx/bin/flowcast-resolver.mjs \
  ~/projects/force-lab/flows/force-dev.js \
  --feature <feature-name> \
  --repo /Users/kongjie/projects/ilink-hub \
  --hitl terminal
```

> `<feature-name>` 用 kebab-case，如 `db-migration-version-tracking`

## Worktree 行为

force-dev 自动：
1. 创建 `.worktrees/feat/<feature-name>/`（已加入 `.gitignore`）
2. 在 worktree 中切出 `feat/<feature-name>` 分支
3. 所有代码、文档、commit 在 worktree 内完成，**主仓库始终在 main**
4. PR 推送后自动清理 worktree

## 断点续跑

```bash
# 查看所有 run
FLOWCAST_PKG_INDEX=~/projects/flowx/index.js \
  node --import ~/projects/flowx/bin/flowcast-resolver.mjs \
  ~/projects/force-lab/flows/force-dev.js \
  --repo /Users/kongjie/projects/ilink-hub --list

# 续跑（--run-id 从 list 输出取）
FLOWCAST_PKG_INDEX=~/projects/flowx/index.js \
  node --import ~/projects/flowx/bin/flowcast-resolver.mjs \
  ~/projects/force-lab/flows/force-dev.js \
  --run-id run-XXXX \
  --feature <feature-name> \
  --repo /Users/kongjie/projects/ilink-hub \
  --hitl terminal
```

## Prompt 文件格式

```markdown
# Feature: <feature-name>

## 目标
（用户视角，一句话描述做什么）

## 完成标准
- [ ] 条件 1（可用命令验证）
- [ ] 条件 2

## 非目标
- 不做 X

## 背景 / 约束
（可选：相关代码路径、已知限制）
```

## 相关文档

- [质量门](/project/quality-gates.md) — force-dev 运行的 CI 检查项
- [代码规范与约定](/project/conventions.md) — 并发开发约定
