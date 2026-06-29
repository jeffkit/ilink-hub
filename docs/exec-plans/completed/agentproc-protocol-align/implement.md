# implement.md — agentproc-protocol-align

## M1：Rust bridge 协议改名 + 最小新契约

### Decisions
- 协议名映射严格按 agentproc v0.3.0 spec
- `ILINK_CONTEXT_TOKEN` 及附件相关 `ILINK_ITEM_TYPE`/`ILINK_*_URL` 保留原名（ilink-hub 自有机制，agentproc spec 未覆盖）
- 新增常量 `AGENTPROC_PROTOCOL_VERSION = "0.3"`，executor 注入 `AGENT_PROTOCOL_VERSION`
- `AGENT_ERROR:` 解析：读到时按 spec 转发为错误回复（partial 已发则不重复发最终 body）

### Problems
- 子 Agent 资源不足（resource_exhausted），降级为编排者直接执行编码，未经独立对抗审查（PR body 已标注）
- 测试中 watch channel 时序问题：用 spawn_partial_collector + changed().await 替代 has_changed 轮询解决
- YAML args flow sequence 不支持含特殊字符的 shell 脚本，改用 tempfile 写入临时脚本文件

### Outcome
- 11 个文件，+277/-73 行，commit 4c79c4d
- 质量门：fmt ✅ / clippy 零 warning ✅ / 558 测试通过（含 3 个新端到端测试）✅
- 残留检查 CLEAN（src/bridge/ 内无 ILINK_MESSAGE/SESSION_ID/SESSION_NAME/FROM_USER/STREAMING/PARTIAL/SESSION: 残留）
- 自审发现并修复：probe.rs 补注入 AGENT_PROTOCOL_VERSION
- 已知限制标注于 CHANGELOG：AGENT_ERROR 用 partial 通道转发（非独立错误通道）、附件变量暂未对齐
