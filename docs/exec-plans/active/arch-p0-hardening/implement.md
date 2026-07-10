# implement — arch-p0-hardening

## Decisions

- 锁策略优先「单锁 + clone」，而非强制走 `with_router_and_registry`（该 facade 仍保留给真正需要原子双锁视图的场景）
- vctx 归属以 `active_sessions` / `backend_sessions_v2` / `messages` 任一存在为准；dispatch 在 push 前同步 `set_active_session_with_depth` 避免首包竞态
- insecure fail-closed 覆盖 `0.0.0.0` / `::` / `[::]`

## Problems

- `all_clients()` 返回 `Vec<&ClientInfo>`，list 路径需 `.cloned()` 才能在释放锁后使用

## Outcome

- clippy `-D warnings` 通过
- 新增归属 / fail-closed 单测通过；hub commands / hub tests 26 项通过
