---
type: Playbook
title: 代码规范与开发约定
description: Rust 编码规范、并发开发约定、数据库变更规则。
tags: [conventions, rust, workflow, required]
timestamp: 2026-06-18T10:00:00+08:00
---

# 代码规范与开发约定

## Rust 编码规范

### 错误处理

- 优先用 `thiserror` 定义错误类型
- **禁止**在生产路径裸用 `unwrap()`
- 异步任务（`tokio::spawn`）的错误必须 `.await` 或通过 channel 传递，不可静默丢弃

### 并发与锁

- 使用 `parking_lot::Mutex`，避免 `std::sync::Mutex::lock().unwrap()`
- 避免在锁持有期间做 IO 或 await

### 数据库

- 迁移文件放 `migrations/`
- DDL 变更需同步更新 MySQL 和 SQLite 兼容路径
- 集成测试写完后用 `--test-threads=1` 验证串行通过

## 并发开发约定

- **禁止**在 `main` 分支直接提交特性代码
- 所有特性开发通过 force-dev 自动创建 worktree 隔离
- Worktree 路径：`.worktrees/feat/<feature-name>/`（已在 `.gitignore`）
- 执行计划路径：`docs/exec-plans/active/<feature>/`（在 worktree 的 feature 分支上）
- 多个 feature 可以同时启动，互不干扰

## 提交规范

- **禁止**在 commit message 中添加 `Co-authored-by` 信息
- Commit message 用中文或英文均可，描述变更意图而非内容

## 相关文档

- [质量门](/project/quality-gates.md)
- [force-dev 工作流](/dev-workflow/force-dev.md)
