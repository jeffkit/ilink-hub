# Feature: review-fixes-minimal

## 目标

修复代码 review 发现的三个 Minimal 级别问题，不涉及运行时行为变更。

## 问题列表

### B-1: 安全文档与 Docker 示例完善
- `README.md` 和 `deploy/` 目录下的 Docker Compose 示例中，`ILINK_ADMIN_TOKEN` 未标注为必填
- `ILINK_ADMIN_INSECURE_NO_AUTH=true` 模式的风险在文档中未显著标注

### NEW-1: AUTH_ERROR_KEYWORDS 常量提取
- `src/bridge/mod.rs` 的 `handle_one_message` 和 `dry_run_profile` 两个函数各自维护一份相同的 auth 错误关键词列表
- 应提取为 `const AUTH_ERROR_KEYWORDS: &[&str]` 常量，消除 DRY 违反

### NEW-2: bridge 超时行为文档化
- `src/bridge/mod.rs::run_cli` 存在两层超时叠加：外层 `timeout_secs` + 内层 `child.wait()` 硬编码 10s
- 最坏情况等待 `timeout_secs + 10s`，但用户不知道，应在 `BridgeProfile` 字段注释中说明

## 完成标准

- [ ] Docker Compose 示例有醒目注释说明 ILINK_ADMIN_TOKEN 强烈建议设置
- [ ] `ILINK_ADMIN_INSECURE_NO_AUTH` 在 README 中有安全警告
- [ ] `AUTH_ERROR_KEYWORDS` 常量存在，两处函数均使用该常量
- [ ] `BridgeProfile::timeout_secs` 字段注释说明超时叠加行为
- [ ] `cargo clippy -- -D warnings` 零警告
- [ ] `cargo test` 全部通过

## 非目标

- 不改变任何运行时行为
- 不修改鉴权逻辑本身
- 不修改超时数值
