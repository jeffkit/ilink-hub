# implement.md — agentproc-protocol-align

## M1：Rust bridge 协议改名 + 最小新契约

### Decisions
- 协议名映射严格按 agentproc v0.3.0 spec
- `ILINK_CONTEXT_TOKEN` 及附件相关 `ILINK_ITEM_TYPE`/`ILINK_*_URL` 保留原名（ilink-hub 自有机制，agentproc spec 未覆盖）
- 新增常量 `AGENTPROC_PROTOCOL_VERSION = "0.3"`，executor 注入 `AGENT_PROTOCOL_VERSION`
- `AGENT_ERROR:` 解析：读到时按 spec 转发为错误回复（partial 已发则不重复发最终 body）

### Problems
- （实现中记录）

### Outcome
- （实现完成后记录）
