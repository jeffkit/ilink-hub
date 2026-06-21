---
type: Playbook
title: 质量门（Quality Gates）
description: 每次代码变更必须全部通过的 CI 检查，配置在 .flowx/config.json。
tags: [ci, quality, cargo, required]
timestamp: 2026-06-18T10:00:00+08:00
---

# 质量门（Quality Gates）

配置在 `.flowx/config.json`，**每次代码变更必须全部通过**。

## 检查项

| 门禁 | 命令 | 失败策略 |
|------|------|---------|
| fmt | `cargo fmt --check` | autofix（先跑 `cargo fmt`） |
| clippy | `cargo clippy -- -D warnings` | resume-fix |
| test | `cargo test` | resume-fix |
| build | `cargo build` | rollback |
| desktop-frontend | `cd desktop/ilink-hub-desktop && npm run build` | resume-fix |
| desktop-tauri | `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | resume-fix |

## 快速修复流程

```bash
# 1. 格式问题 → 自动修复
cargo fmt

# 2. Clippy 警告 → 查看具体报错修复
cargo clippy -- -D warnings

# 3. 测试失败 → 数据库测试需串行运行
cargo test -- --test-threads=1

# 4. 桌面端构建
cd desktop/ilink-hub-desktop && npm run build
```

## 注意事项

- `clippy` 对所有 warning 零容忍（`-D warnings`），提交前必须全部清除
- 数据库集成测试之间有状态依赖，并发跑会互相干扰，用 `--test-threads=1`
- desktop-tauri 只跑 `cargo check`（不是 build），速度快，每次都要过

## 相关文档

- [常用命令速查](/dev-workflow/common-commands.md)
- [force-dev 工作流](/dev-workflow/force-dev.md)
