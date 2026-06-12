修复 runtime 模块修复：CHAN-02, SYNC-01

## 待修复条目

  - [CHAN-02] broadcast(256) 在 dispatcher 处理慢时静默丢消息
     文件：src/runtime/serve.rs:90, src/hub/mod.rs:330
     问题：`broadcast::channel::<WeixinMessage>(256)` 容量为 256。`spawn_dispatcher` 的 `dispatch_message` 包含 DB 查询（`get_hub_ext_batch`、`persist_context_tokens_batch`）。若 DB 慢（如 SQLite 锁争用），dispatcher 处理速度低于上游推送速度，超出 
     修复方向：增加通道容量（如 1024）；更重要的是将 DB 操作从 dispatcher 热路径移出（已有批量接口，继续优化将 DB 写入合并为后台 task）；并为 Lagged 事件增加 metrics counter，使丢失可观测。

  - [SYNC-01] 启动恢复的路由条目包含已删除 vtoken
     文件：src/runtime/serve.rs:257-268
     问题：`load_clients_from_db` 先恢复 clients，后恢复 routing_state。若某个 client 被删除但对应的 routing_state 行未清理（`clear_routes_for_vtoken` 仅在内存注销时调用，进程崩溃时可能跳过），重启后 `router` 内存中存在指向已删 vtoken 的路由，消息会被路由到不存在的 client 并静默丢弃。
     修复方向：`load_clients_from_db` 恢复路由后，过滤掉 registry 中不存在的 vtoken 对应的路由条目；或在 `upsert_client` 时同步清理同 vtoken 的旧路由。

## 完成标准
- [ ] CHAN-02 修复已提交，相关测试通过
- [ ] SYNC-01 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖