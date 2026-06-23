修复 hub 模块修复：MEM-01, TO-02, S-01, C-01, A-01

## 待修复条目

  - [MEM-01] Broadcast 路径 item_list 深 clone N 次（N = 在线客户端数）
     文件：src/hub/mod.rs:369, msg_clone = msg.clone()
     问题：`WeixinMessage.item_list: Option<Vec<MessageItem>>` 在 Broadcast 路径中对每个在线客户端执行一次完整深 clone，`MessageItem.extra: serde_json::Value` 是 heap 分配的树结构，clone 代价高。3 个后端时深 clone 3 次，10 个后端时 10 次。
     修复方向：将 `item_list` 改为 `Option<Arc<Vec<MessageItem>>>`，clone 只复制 Arc 引用而非数据。需同步修改 `sendmessage` handler 中修改 `item_list` 的代码（写时复制）。

  - [TO-02] build_hub_ext_for_vctx DB 查询无超时
     文件：src/hub/mod.rs:808-838, build_hub_ext_for_vctx
     问题：`get_active_session_name` 和 `get_backend_session` 两次 DB 查询没有超时保护。若 DB 因 SQLITE_BUSY 或 PostgreSQL 连接池耗尽而挂起，`dispatch_message` 任务永久阻塞，积压所有后续消息。
     修复方向：用 `tokio::time::timeout(Duration::from_secs(5), ...)` 包裹两次 DB 查询，超时时 warn 并返回 `None`（降级为无 HubExt 的消息转发）。

  - [S-01] vtoken 在 debug! 日志中未经 redact 完整输出
     文件：src/hub/router.rs:159
     问题：
     修复方向：`vtoken = %&vtoken[..vtoken.len().min(8)]`

  - [C-01] Broadcast persist fire-and-forget 存在重启丢失窗口
     文件：src/hub/mod.rs:349-354
     问题：
     修复方向：接受现有语义时，至少添加 metrics counter 记录 fire-and-forget 失败次数；在 README 中说明该设计权衡。

  - [A-01] HubState 神对象，14 个字段无访问边界
     文件：src/hub/mod.rs:131-155
     问题：
     修复方向：按职责拆分为 `IlinkConnState`、`RoutingState` 等子结构，通过字段访问而非直接暴露全部状态。

## 完成标准
- [ ] MEM-01 修复已提交，相关测试通过
- [ ] TO-02 修复已提交，相关测试通过
- [ ] S-01 修复已提交，相关测试通过
- [ ] C-01 修复已提交，相关测试通过
- [ ] A-01 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖