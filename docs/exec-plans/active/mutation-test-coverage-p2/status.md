# status.md — mutation-test-coverage-p2

## 当前状态

**进度**：Phase 5 — 全部完成 ✅

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1 commands.rs | ✅ done | 40 变异体，75% → 补 4 个测试，7/10 未捕获已修复，3 暂缓（需 mock upstream） |
| M2 pairing.rs | ✅ done | 67 变异体，65.8% → 补 6 个测试（含安全关键 CSRF 验证） |
| M3 dispatch.rs | ✅ done | 49 变异体，63% → 补 6 个测试（build_no_backend_reply, push_to_queue_pub, build_hub_ext_for_vctx 等） |

## 分支信息

- 分支：`main`（代码直接在 main 分支开发）
- 工作目录：`/Users/kongjie/projects/ilink-hub`
- Exec-plan：`docs/exec-plans/active/mutation-test-coverage-p2/`

## 关键发现

- **M2 CSRF 安全盲点（已修复）**：`constant_time_eq` 比较未被测试，攻击者可绕过配对验证
- **M3 hub_ext 空值判断（已修复）**：728:18 `!t.is_empty()` 删除 `!` 变异会使非空 session 值被丢弃
- **暂缓项**：dispatch_message 的 ctx 空检查 + API 返回码检查（均需 mock upstream，共约 10 个变异体）

## Phase 6 扩展模块（2026-07-06）

| 模块 | 状态 | 说明 |
|------|------|------|
| relay/protocol.rs | ✅ | 初扫即 100%，无需补测 |
| hub/quote_route.rs | ✅ | 初扫即 100%，无需补测 |
| relay/device.rs | ✅ | 新增 4 个测试（长度边界/getter/密钥派生/签名验证） |
| hub/messages.rs | ✅ | 新增 9 个测试（session 错误函数 × 7 + 截断边界 × 2） |
| ilink/login.rs | ⏳ | 延迟：需 mock HTTP 框架（mockito/wiremock） |

## 恢复指引

Phase 5 + Phase 6 已全部完成（login.rs 除外）。如需继续：
- 接入 mockito 后补测 login.rs 的 11 个 missed mutants
- 补测 commands.rs / dispatch.rs 暂缓的 ~14 个 mock upstream 相关变异
- 扩展到 store 层（session store 业务逻辑）
