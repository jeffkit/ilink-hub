# implement.md — mutation-test-coverage-p2

## M1 — commands.rs：命令解析逻辑

### Decisions
- 新增 `make_hub_state()` 辅助函数，构造无 client 的最小 `Arc<HubState>`
- 针对 `handle_cmd_broadcast`、`handle_cmd_status`、`handle_cmd_session_list` 的函数替换变异（→ String::new()）
- 针对 `handle_hub_command` 中 `!ctx.is_empty()` 的 `!` 删除变异

### Problems
- `resp.ret` 返回码检查（3 个变异）需要 mock upstream，暂缓

### Outcome
新增测试 4 个：
- `broadcast_to_no_online_clients_returns_zero_count`
- `status_with_no_clients_returns_hub_status_string`
- `session_list_with_no_backend_returns_no_backend_message`
- `handle_hub_command_with_empty_context_token_returns_early`

初始 Mutation Score 75% → 修复 7/10 未捕获，3 个暂缓（需 mock upstream）

---

## M2 — pairing.rs：配对流程逻辑

### Decisions
- 针对 `pre_check_confirm` 中 `constant_time_eq` CSRF 比较（安全关键）
- 针对 `is_expired` 时间判断（confirmed 状态）
- 针对 `purge_expired`（wait/scanned 状态清除）
- 针对 `session_cap` 容量计算（confirmed 计入总数）

### Problems
- 14 个时间窗口边界相关变异（`Instant::now` 精度问题）暂缓

### Outcome
新增测试 6 个：
- `pre_check_confirm_rejects_wrong_csrf`（CSRF 安全验证）
- `pre_check_confirm_accepts_correct_csrf`
- `confirmed_session_is_not_expired_after_pairing_ttl`
- `purge_expired_removes_stale_wait_sessions`
- `purge_expired_removes_stale_scanned_sessions`
- `session_cap_includes_confirmed_sessions`

初始 Mutation Score 65.8% → 覆盖关键安全路径

---

## M3 — dispatch.rs：消息分发核心

### Decisions
- 针对 `build_no_backend_reply` 函数替换变异（3 类分支：non-command / None / command）
- 针对 `push_to_queue_pub` 的 no-op 变异（dispatched 指标计数）
- 针对 `resolve_quote_from_footer` 的 `||` → `&&` 逻辑运算符变异
- 针对 `build_hub_ext_for_vctx` 的 `!` 删除变异（session_id 非空判断）

### Problems
- `dispatch_message` 的 ctx 空检查 + API 返回码（~11 个变异）需要 mock upstream，暂缓

### Outcome
新增测试 6 个：
- `build_no_backend_reply_non_command_returns_no_backend_online`
- `build_no_backend_reply_none_returns_no_backend_online`
- `build_no_backend_reply_command_returns_unrecognized_command`
- `push_to_queue_pub_increments_dispatched_metric`
- `resolve_quote_from_footer_session_prefix_uses_session_path`
- `build_hub_ext_for_vctx_session_override_returns_non_empty_session_id`

初始 Mutation Score 63% → 覆盖分发核心逻辑路径

---

## Phase 6 扩展模块（新增）

### relay/protocol.rs
初始扫描即 100% mutation score，无需补测。

### hub/quote_route.rs
初始扫描即 100% mutation score，无需补测。

### relay/device.rs
初始 Mutation Score：43.3%（17 个 missed mutants）

新增测试 4 个：
- `validate_device_id_length_boundaries`（长度边界：< 8 和 > 64）
- `device_identity_device_id_getter_returns_actual_id`（getter 替换）
- `device_identity_verifying_key_matches_signing_key`（密钥派生）
- `device_identity_public_key_and_sign_register_are_non_trivial`（签名非空验证）

使用 `DeviceIdentity::for_testing` 构造器，避免文件系统交互。

### hub/messages.rs
初始 Mutation Score：63.2%（14 个 missed mutants）

新增测试 9 个：
- `session_new_created_switch_failed_contains_name_and_error`
- `session_new_failed_contains_error`
- `session_use_failed_contains_error`
- `session_use_slot_create_failed_contains_error`
- `session_use_query_failed_contains_error`
- `session_delete_failed_contains_error`
- `session_list_failed_contains_error`
- `hub_status_truncation_boundary_exactly_30_chars_not_truncated`（截断边界 = 30 不截）
- `hub_status_truncation_boundary_31_chars_is_truncated`（截断边界 31 必截）

### ilink/login.rs
初始 Mutation Score：0%（11 个 missed mutants）

延迟原因：所有函数均为 HTTP I/O，需 mockito/wiremock 接入后才可有效测试。
