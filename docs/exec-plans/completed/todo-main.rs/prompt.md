修复 main.rs 模块修复：CONS-01, CONS-02

## 待修复条目

  - [CONS-01] CLI 帮助全英文，文档与 GUI 全中文
     文件：src/main.rs, ilink-hub, src/bin/ilink-hub-bridge.rs, ilink-hub-bridge
     问题：
     修复方向：1. 短期：为命令/参数 `help` 文案补中文（或中英双语），至少覆盖 `serve` / `register` / `bridge` 的顶层描述。   2. 或在文档「快速开始」里明确「CLI 提示为英文属正常」，降低小白困惑（次选）。

  - [CONS-02] 「Hub 地址」三套环境变量与默认值不一致，默认监听 `0.0.0.0`
     文件：src/main.rs, serve --addr, ILINK_HUB_ADDR, 0.0.0.0:8765, register --hub-url, ILINK_HUB_URL, http://localhost:8765, src/bin/ilink-hub-bridge.rs, --hub-url, WEIXIN_BASE_URL, http://127.0.0.1:8765, docs/guide/getting-started.md:72, 0.0.0.0:8765, docs/bridge/quick-try.md:37, 127.0.0.1:8765
     问题：同一个「Hub 在哪」的概念散落成三套环境变量名（`ILINK_HUB_ADDR` / `ILINK_HUB_URL` / `WEIXIN_BASE_URL`），并混用 `localhost` / `127.0.0.1` / `0.0.0.0`；文档内部示例也不统一。其中 `serve` 默认监听 `0.0.0.0:8765` 意味着**默认对整个局域网开放**，对不懂网络的小白是潜在安全隐患（桌
     修复方向：1. 对外统一以 `WEIXIN_BASE_URL` 作为「Hub 地址」入口（与各后端/Bridge 一致），其余作为别名兼容。   2. 文档全部统一为 `127.0.0.1`（本机场景）；需要对外暴露时单独在「部署/安全」章节显式说明。   3. 评估把 `serve` 默认监听改为 `127.0.0.1:8765`，需要 LAN/容器暴露时由用户显式传 `0.0.0.0`（属行为变更，需在 CHANGELOG / 文档标注）。

## 完成标准
- [ ] CONS-01 修复已提交，相关测试通过
- [ ] CONS-02 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖