修复 bridge 模块修复：MGR-01, MGR-02

## 待修复条目

  - [MGR-01] 重启退避重置阈值等于最大退避，快速崩溃绕过退避
     文件：src/bridge/manager.rs:512-515
     问题：重启退避在子进程存活时间超过一定阈值后重置为初始值。若该阈值等于最大退避值（如 60 秒），进程在每次退避结束后立刻崩溃（存活 < 1s），但退避计时器恰好在上一次等待中消耗完，下次会从头开始，无法真正惩罚持续快速崩溃。
     修复方向：将"健康存活"阈值设为显著大于最大退避（如最大退避的 3 倍），确保进程需要真正稳定运行一段时间才能重置退避计数。

  - [MGR-02] handle drop 检测延迟一个 reconcile 周期
     文件：src/bridge/manager.rs:198-212
     问题：`BridgeManagerHandle` drop 后，manager 需要等到下次 `reconcile_once` 轮询时才检测到，最多延迟一个轮询间隔（默认数秒）。期间子进程仍在运行并接收消息。
     修复方向：使用 `tokio::sync::watch::Sender`，handle drop 时发送关闭信号，manager 在 `tokio::select!` 中同时等待该信号和定时器，做到即时响应。

## 完成标准
- [ ] MGR-01 修复已提交，相关测试通过
- [ ] MGR-02 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖