# arch-p0-hardening

## 目标
修复架构 Review 中的 P0 正确性/安全项。

## 完成标准
1. handle_cmd_list 不再同时持有 registry+router（消除 AB-BA）
2. set_default 持久化不再在持 router 锁时 await DB
3. dispatcher / upstream polling 在 shutdown 信号下可及时退出
4. resolve_send_context / getconfig 校验 vctx 归属 vtoken
5. insecure_no_auth + 绑定 0.0.0.0 时拒绝启动
6. hub/mod.rs 与 pairing 注释锁顺序一致

## 非目标
- 不拆大文件（dispatcher/routes）
- 不做 master key 轮转 API
- 不强制 relay wss 启动检查
