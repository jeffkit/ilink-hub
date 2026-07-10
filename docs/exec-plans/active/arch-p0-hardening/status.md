# status — arch-p0-hardening

- 分支: `fix/arch-p0-onto-main`
- 进度: 已移植到当前 main（含 routes/dispatch 拆分后路径）；配对保留 PR#19 claim-window

| 里程碑 | 状态 |
|--------|------|
| M1 锁顺序 + 锁内 await | done |
| M2 shutdown 传播 | done |
| M3 vctx 归属 | done |
| M4 insecure fail-closed | done |
| M5 配对限流 + vtoken claim-window（保留 PR#19） | done |
| M6 AdminGuard 统一 | done |
| M7 CORS Result + relay wss + executor warn + key SOP | done |
| M8 合入 main 冲突解决 | done |
