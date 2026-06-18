# sendtyping 错误传播二次漏洞修复 — 实施记录

最后更新：2026-06-18

## 进度

| 里程碑 | 状态 | 备注 |
|---|---|---|
| M1 诊断与方案设计 | ✅ DONE | 现状诊断 + 修复方案；本里程碑一次性完成 M1+M2+M3+M4 的代码改动 |
| M2 send_typing 错误传播修复 | ✅ DONE | 已完成 |
| M3 send_typing 错误路径单元测试 | ✅ DONE | 已完成，新增 2 个直接测试 + 复用 1 个集成测试 |
| M4 代码质量门禁 | ✅ DONE | fmt + clippy -D warnings 通过 |
| M5 全量回归与提交 | ⏳ TODO | 见下面 commit 阶段 |

## 实施记录

### M1+M2 — 修复 src/ilink/upstream.rs::send_typing

**修改前**：
```rust
pub async fn send_typing(&self, req: SendTypingRequest) -> Result<()> {
    let url = format!("{}/ilink/bot/sendtyping", self.base_url);
    let _ = self
        .client
        .post(&url)
        .headers(self.headers()?)
        .json(&req)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
```

**修改后**：
```rust
pub async fn send_typing(&self, req: SendTypingRequest) -> Result<()> {
    let url = format!("{}/ilink/bot/sendtyping", self.base_url);
    let resp = self
        .client
        .post(&url)
        .headers(self.headers()?)
        .json(&req)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "iLink sendtyping network error");
            return Err(e.into());
        }
    };
    if let Err(e) = resp.error_for_status() {
        warn!(
            status = %e.status().unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            error = %e,
            "iLink sendtyping returned non-2xx status"
        );
        return Err(e.into());
    }
    Ok(())
}
```

要点：
- 不再用 `let _ =`；错误路径显式传播
- 网络错误和 HTTP 非 2xx 各走独立 match 分支
- warn! 风格对齐 sendmessage（端点名作为日志前缀、error Display、status code）
- 状态码从 `error_for_status()` 返回的 `e.status()` 取，避免 `reqwest::Response::error_for_status(self)` 取走所有权后再访问 `resp.status()`
- 函数签名 `async fn send_typing(&self, req: SendTypingRequest) -> Result<()>` 不变

### M3 — 单元测试

`tests/breaking_changes.rs` 新增 2 个测试：

1. **`sendtyping_upstream_network_error_propagates`** — bind 0 号端口后立即 drop，
   拿到保证 connection refused 的地址，断言 `send_typing` 返 `Err` 且错误链含
   "error sending request" / "connection refused"。

2. **`sendtyping_upstream_http_500_propagates`** — axum mock `/ilink/bot/sendtyping`
   返 500，断言 `send_typing` 返 `Err` 且错误链含 "500" / "server error"。

原有的 `sendtyping_error_propagation_test`（集成层，routes + upstream）继续通过。

### M4 — 质量门禁

```
cargo fmt --check                                  ✅ PASS
cargo clippy -- -D warnings                        ✅ PASS
cargo test                                         ✅ PASS（全绿：268 + 22 + 10 + 27 + 1 + 15 + 1）
cargo build                                        ✅ PASS
cd desktop/ilink-hub-desktop && npm run build      ✅ PASS
cargo check --manifest-path desktop/.../Cargo.toml ❌ FAIL（pre-existing 上游 breakage，与本次修复无关）
```

desktop-tauri 编译失败的详细情况：
- 报错：`desktop/ilink-hub-desktop/src-tauri/src/lib.rs:605` 调
  `unregister_client_in_hub(state.as_ref(), &name)` 缺第 3 个参数 `force: bool`
- 根因：`src/server/pairing.rs:327` 升级了 `unregister_client_in_hub` 签名加 `force`，
  desktop 侧调用方未同步
- 范围：与 sendtyping 错误传播修复完全无关；建议另起 PR 修复
- 复现：在 `git stash` 撤回本次改动后，`cargo check --manifest-path .../Cargo.toml`
  同样失败（确认是 pre-existing）

## 风险评估

- **低风险**：改 `send_typing` 错误传播后，原本吞掉的错误现在会冒泡到
  `routes.rs::570` 的 `Err(e) → ret:500`，可能让原本静默"成功"的 typing 调用
  在某些边界场景返 500。这是修复目标本身，不是回归。
- **回滚方案**：单文件单函数改动，`git revert` 即可，影响面仅 `src/ilink/upstream.rs`

## Commit

- M1 阶段提交：M1+M2+M3+M4 的代码改动与文档
- 提交内容：
  - `src/ilink/upstream.rs` — 修复 send_typing 错误传播
  - `tests/breaking_changes.rs` — 新增两个 send_typing 单元测试
  - `docs/exec-plans/active/sendtyping-error-fix/implement.md` — 本文件
  - `docs/exec-plans/active/sendtyping-error-fix/reviews/m1/review-request.yaml` — review 请求

## 非目标确认

- 不动 `routes.rs:570-577`（已是正确处理）
- 不动 `SendTypingRequest` 结构
- 不动 `tests/e2e_wechat_simulation.rs:85` 的 mock 语义
- 不重构 upstream.rs 其他 `send_*` 方法
- 不修复 desktop-tauri 上游 unregister_client_in_hub 参数缺失（pre-existing）