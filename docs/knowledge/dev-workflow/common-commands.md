---
type: Reference
title: 常用命令速查
description: cargo test/clippy/fmt/build 等日常开发命令，含特殊场景说明。
tags: [commands, cargo, development, reference]
timestamp: 2026-06-18T10:00:00+08:00
---

# 常用命令速查

## 核心 Rust 命令

```bash
cargo test                          # 跑全部测试
cargo test -- --test-threads=1      # 串行测试（数据库测试需要）
cargo clippy -- -D warnings         # lint（零 warning 容忍）
cargo fmt                           # 格式化
cargo fmt --check                   # 检查格式（CI 用）
cargo build                         # 全量构建
```

## 带 Feature Flag 构建

```bash
cargo build --features postgres     # 启用 PostgreSQL 支持
cargo build --features mysql        # 启用 MySQL 支持
cargo build --all-features          # 启用所有数据库驱动
```

## 桌面端命令

```bash
cd desktop/ilink-hub-desktop
npm run build                       # 构建前端
npm run dev                         # 开发模式

cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml
# 检查 Tauri 后端（比 build 快）
```

## 质量门一键验证

```bash
cargo fmt --check && \
  cargo clippy -- -D warnings && \
  cargo test -- --test-threads=1 && \
  cargo build && \
  (cd desktop/ilink-hub-desktop && npm run build) && \
  cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml
```

## 相关文档

- [质量门](/project/quality-gates.md) — 每项检查的失败策略
- [force-dev 工作流](force-dev.md) — 自动化开发流程
