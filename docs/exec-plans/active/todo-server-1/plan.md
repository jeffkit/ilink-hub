# Execution Plan: Security Issues Fixes

本项目旨在修复 `ilink-hub` 的 5 项安全隐患（SEC-004, SEC-006, SEC-007, SEC-009, SEC-010）。本方案定义了分步实施的里程碑、对应的验证命令以及最终的 E2E 检查点。

---

## 里程碑列表

### 里程碑 1: 修复 SEC-006 (Admin token 非常量时间比较)
- **目标**: 引入 `subtle` crate 对 `admin_token` 进行常量时间比较（Constant-time comparison），防御计时攻击。
- **改动文件**:
  - `src/server/routes.rs` (修改 `check_admin_auth` 函数)
- **验证命令**:
  - 运行已有的 admin 认证测试：
    ```bash
    cargo test --test breaking_changes admin_endpoint_accessible_with_correct_token
    ```

### 里程碑 2: 修复 SEC-007 (/metrics 接口无认证)
- **目标**: 为 `/metrics` 接口加上 `check_admin_auth` 认证逻辑，或者在非安全模式下拒绝无 token 访问（与其它 admin 接口行为对齐）。
- **改动文件**:
  - `src/server/mod.rs`
  - `src/server/routes.rs` (修改 `metrics` 接口以校验 headers)
- **验证命令**:
  - 新增验证测试并执行：
    ```bash
    cargo test --test security_tests test_metrics_requires_auth
    ```

### 里程碑 3: 修复 SEC-004 (get_qrcode_status DoS 漏洞)
- **目标**: 
  1. 在 long-poll 循环开始前，校验传入的 `qrcode` 存在且处于活跃状态，若不存在则直接返回 `404 Not Found`。
  2. 对 `/ilink/bot/get_qrcode_status` 应用 per-IP 限速器。
- **改动文件**:
  - `src/server/pairing.rs` (修改 `get_qrcode_status` 函数)
- **验证命令**:
  - 新增验证测试并执行：
    ```bash
    cargo test --test security_tests test_qrcode_status_invalid_code
    ```

### 里程碑 4: 修复 SEC-009 (name/label 字段未限长与 Prometheus label 未转义)
- **目标**:
  1. 在 `register` 和 `pair_confirm` 的入口处校验 `name.len() <= 64` 且 `label.len() <= 256`。
  2. 在 Prometheus metrics 文本输出格式中，对 `client` label 里的 client name 进行转义（转义 `\`, `"`, `\n` 等特殊字符）。
- **改动文件**:
  - `src/server/routes.rs` (修改 `register` 和 `metrics` 格式化逻辑)
  - `src/server/pairing.rs` (修改 `pair_confirm` 接口)
- **验证命令**:
  - 新增长度校验与指标转义测试并执行：
    ```bash
    cargo test --test security_tests test_name_label_length_validation
    cargo test --test security_tests test_prometheus_label_escaping
    ```

### 里程碑 5: 修复 SEC-010 (未显式限制 HTTP Body 大小)
- **目标**: 在 `build_router` 中全局配置 Axum 的 `DefaultBodyLimit::max(256 * 1024)`（256KB 上限）。
- **改动文件**:
  - `src/server/mod.rs` (配置 `DefaultBodyLimit` layer)
- **验证命令**:
  - 新增 Body 大小限制测试并执行：
    ```bash
    cargo test --test security_tests test_http_body_limit
    ```

---

## E2E Checkpoint 标记 (E2E 验证点)

在所有里程碑完成后，必须执行以下综合检查，确保系统整体稳定且无 Regression 隐患。

### 检查项 1: 代码静态分析与规范校验
- **目标**: 确保新增和修改的代码完全符合 Rust 规范且无警告。
- **验证命令**:
  ```bash
  cargo clippy --all-targets --all-features -- -D warnings
  cargo fmt --all -- --check
  ```

### 检查项 2: 完整自动化测试套件通过 (E2E Checkpoint)
- **目标**: 确保原有的集成测试与新增的安全特性测试全部通过。
- **验证命令**:
  ```bash
  cargo test --all-targets --all-features
  ```
