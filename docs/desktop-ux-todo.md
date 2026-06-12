# iLink Hub 桌面版 + Bridge — 小白体验问题清单（UX TODO）

> 生成日期：2026-06-12
> 来源：以「完全没用过的小白用户」视角体验桌面版（Tauri）与 Bridge 的一次走查。CLI / bridge 行为为实跑验证；桌面 GUI 因会与本机正在使用的微信 token 冲突，未真实启动，结论来自源码 + 文档审查。
> 状态说明：`open` = 待处理，`in_progress` = 进行中，`done` = 已完成
> 优先级口径：P1 = 小白几乎必然卡住、无自救路径；P2 = 体验割裂或有安全隐患；P3 = 文案 / 概念可优化

---

## 总览

| ID | 优先级 | 分类 | 简述 |
|----|--------|------|------|
| UX-01 | **P1** | 桌面/可用性 | 「停止服务」后无法在应用内重启（无 start/restart 入口） |
| UX-02 | **P1** | 桌面/可用性 | 端口被占用只能靠环境变量改，双击启动的 .app 无 GUI 改端口入口 |
| UX-03 | **P1** | 桌面/引导 | 扫码弹窗缺少 ClawBot / iLink 前置条件提醒 |
| CONS-01 | P2 | 一致性 | CLI 帮助全英文，文档与 GUI 全中文 |
| CONS-02 | P2 | 一致性/安全 | 「Hub 地址」三套环境变量与默认值不一致，默认监听 `0.0.0.0` |
| UX-04 | P2 | 桌面/可用性 | Bridge 模板保存后不探活 CLI，失败仅显示「重启 N」，无「测试」按钮 |
| DOC-01 | P3 | 文案/概念 | 「后端」与「Bridge」两个 Tab 关系未解释 |
| DOC-02 | P3 | 文案/概念 | Bridge 页黑话多（workspace/profile/manager/pid） |
| DOC-03 | P3 | 文案/概念 | 首页「用户上行次数」等指标用词偏技术 |

---

## P1 — 小白几乎必然卡住

### UX-01 · 桌面端「停止服务」后无法在应用内重启

- **状态**：done
- **文件**：`desktop/ilink-hub-desktop/index.html:84-91`（仅 `#btn-stop`），`desktop/ilink-hub-desktop/src/main.ts:1087-1101`（仅 `stop_hub`），`desktop/ilink-hub-desktop/src-tauri/src/lib.rs:1449`（仅 `stop_hub` 命令，无 `start_hub` / `restart_hub`）
- **问题**：首页底栏只有一个红色危险按钮「停止服务」。点击并二次确认后，前端 `setHubState("stopped")` 把按钮置灰，但界面上**没有任何「启动 / 重启服务」入口**，后端也未暴露 `start_hub`。小白只要手贱点了一次停止，就只能退出应用再重新打开才能恢复中转——而退出/重开对小白来说并不直观（容易以为应用坏了）。
- **修复方向**：
  1. 后端新增 `start_hub` / `restart_hub` Tauri command，复用 `run_serve` 启动逻辑（停止时保留可重启的句柄而非彻底拆除 runtime）。
  2. 首页在 `stopped` 状态下把「停止服务」按钮替换为「启动服务」主按钮；或常驻一个状态切换按钮（运行中=停止 / 已停止=启动）。
  3. 降低「停止服务」的误触权重：移出底栏一级位置，或停止后立刻给出「已停止，点此重新启动」的提示行。

### UX-02 · 端口被占用时只能改环境变量，GUI 无改端口入口

- **状态**：done
- **文件**：`desktop/ilink-hub-desktop/index.html:76-81`（`bind-hint` 提示设 `ILINK_HUB_ADDR`），`desktop/ilink-hub-desktop/src/main.ts:823-828`（仅在长时间 `starting` 时显示提示）
- **问题**：端口 8765 被占用（例如本机另开了 `ilink-hub serve`）时，桌面端会一直停在「启动中」，并提示「请设置 `ILINK_HUB_ADDR=127.0.0.1:8770` 后重启本应用」。但桌面应用是**双击启动**的 `.app`，普通用户根本不知道如何给一个 GUI 应用设置环境变量（需要 `launchctl setenv` 或终端导出后再启动），而应用内**没有任何「修改监听端口」的设置项**。这是一条对小白完全不可达的自救路径。
- **修复方向**：
  1. 在首页（或设置区）提供「监听端口」输入框，写入应用本地配置（如 Tauri store / 配置文件），下次启动读取。
  2. 端口占用导致 bind 失败时，直接弹出「端口被占用，换一个端口？」对话框，允许一键改端口并重试 bind，而不是停留在「启动中」。
  3. 提示文案去掉对环境变量的依赖描述（小白看不懂），改为指向 GUI 设置项。

### UX-03 · 扫码弹窗缺少 ClawBot / iLink 前置条件提醒

- **状态**：done
- **文件**：`desktop/ilink-hub-desktop/index.html:256-283`（QR 弹窗无前置说明），对照 `docs/guide/getting-started.md:9-11`、`docs/bridge/quick-try.md:11-13`（文档反复强调需先开启 ClawBot 龙虾插件）
- **问题**：文档里把「使用前需在微信开启 ClawBot（龙虾插件）、且必须是已开通 iLink 的微信账号」作为强提醒反复出现。但桌面端的「微信扫码登录」弹窗里**没有这一句**。小白下载 `.app` → 双击 → 弹出二维码 → 用普通微信扫 → 没反应，完全不知道是「账号没开通 iLink」导致的，极易误判为「这软件是不是坏了」。
- **修复方向**：
  1. 在 QR 弹窗顶部加一行前置提醒：「需先在微信『我 → 设置 → 插件』开启 ClawBot（龙虾插件），并使用已开通 iLink 的微信账号扫码。」
  2. 提供「扫了没反应？」可折叠帮助，复用 `getting-started.md` 里的排查清单（账号未开通 / 二维码过期 / 手机网络）。

---

## P2 — 体验割裂或安全隐患

### CONS-01 · CLI 帮助全英文，文档与 GUI 全中文

- **状态**：open
- **文件**：`src/main.rs`（`ilink-hub` clap 定义），`src/bin/ilink-hub-bridge.rs`（`ilink-hub-bridge` clap 定义）
- **现象**：`ilink-hub --help` / `ilink-hub serve --help` / `ilink-hub-bridge --help` 输出**全英文**（"Start the hub server"、"Register a backend client" 等），而文档站、桌面 GUI 全部中文。面向中文小白时，一旦从 GUI 掉到终端就出现语言割裂。
- **修复方向**：
  1. 短期：为命令/参数 `help` 文案补中文（或中英双语），至少覆盖 `serve` / `register` / `bridge` 的顶层描述。
  2. 或在文档「快速开始」里明确「CLI 提示为英文属正常」，降低小白困惑（次选）。

### CONS-02 · 「Hub 地址」三套环境变量与默认值不一致，默认监听 `0.0.0.0`

- **状态**：open
- **文件**：`src/main.rs`（`serve --addr` env `ILINK_HUB_ADDR` 默认 `0.0.0.0:8765`；`register --hub-url` env `ILINK_HUB_URL` 默认 `http://localhost:8765`），`src/bin/ilink-hub-bridge.rs`（`--hub-url` env `WEIXIN_BASE_URL` 默认 `http://127.0.0.1:8765`），`docs/guide/getting-started.md:72`（示例用 `0.0.0.0:8765`）对比 `docs/bridge/quick-try.md:37`（示例用 `127.0.0.1:8765`）
- **问题**：同一个「Hub 在哪」的概念散落成三套环境变量名（`ILINK_HUB_ADDR` / `ILINK_HUB_URL` / `WEIXIN_BASE_URL`），并混用 `localhost` / `127.0.0.1` / `0.0.0.0`；文档内部示例也不统一。其中 `serve` 默认监听 `0.0.0.0:8765` 意味着**默认对整个局域网开放**，对不懂网络的小白是潜在安全隐患（桌面端用的是 `127.0.0.1` 更稳妥）。
- **修复方向**：
  1. 对外统一以 `WEIXIN_BASE_URL` 作为「Hub 地址」入口（与各后端/Bridge 一致），其余作为别名兼容。
  2. 文档全部统一为 `127.0.0.1`（本机场景）；需要对外暴露时单独在「部署/安全」章节显式说明。
  3. 评估把 `serve` 默认监听改为 `127.0.0.1:8765`，需要 LAN/容器暴露时由用户显式传 `0.0.0.0`（属行为变更，需在 CHANGELOG / 文档标注）。

### UX-04 · Bridge 模板保存后不探活 CLI，失败仅显示「重启 N」

- **状态**：open
- **文件**：`desktop/ilink-hub-desktop/src-tauri/src/lib.rs:694-790`（模板生成 `claude` / `cursor`(=`agent`) / `codex` / `gemini`），`desktop/ilink-hub-desktop/src/main.ts:524-543`（子进程状态仅渲染 `pid … · 重启 N`）
- **问题**：小白在 Bridge 页选「Claude Code / Cursor / Codex / Gemini」模板、填项目目录、保存后，manager 立即拉起子进程。但若本机**没装对应 CLI 或没登录认证**，UI 只会显示「pid — · 重启 N」并不断退避重启，没有「未找到 `claude` 命令，请先安装并登录」这类可读提示，小白完全无法判断为什么不工作；也没有「测试一下」按钮先验证 CLI 能否产出 stdout。
- **修复方向**：
  1. 保存模板前/后做一次轻量探活：`which <command>`（不存在 → 明确提示「未找到 xxx 命令，请先安装」），并尽量识别「未认证」类错误。
  2. 子进程因 `command not found` / 非零退出连续失败时，把 `lastError` 以人话呈现在 profile 卡片上（区分「没装」「没登录」「配置错误」）。
  3. 模板编辑区加「测试」按钮：用一条样例消息跑一次 CLI，把 stdout/stderr 回显给用户。

---

## P3 — 文案 / 概念可优化

### DOC-01 · 「后端」与「Bridge」两个 Tab 关系未解释

- **状态**：open
- **文件**：`desktop/ilink-hub-desktop/index.html:24-28`（三个 Tab 并列），`docs/bridge/README.md:14`（Hub 侧并不区分调用方是不是 bridge）
- **问题**：首页有「后端」「Bridge」两个并列 Tab，但 Bridge 本质上**也是一种后端**（只是本机 CLI 桥接）。小白看不出二者关系，也不知道「我要接 Claude Code 到底该去哪个 Tab」。
- **修复方向**：在两个 Tab 顶部各加一句定位说明（如「后端：任意通过 Token 接入的客户端」「Bridge：把本机 CLI 一键接入，免手填 Token」），或在帮助折叠区画一张「Bridge 也是后端之一」的关系图。

### DOC-02 · Bridge 页黑话多

- **状态**：open
- **文件**：`desktop/ilink-hub-desktop/index.html:168-169,196-245`（"workspace 名"、"等待 manager 扫描"等），`desktop/ilink-hub-desktop/src/main.ts:520,528,532`（"等待 manager 扫描"、`pid … · 重启 N`）
- **问题**：Bridge 页大量出现 `workspace` / `profile` / `manager` / `pid` / 「等待 manager 扫描」等技术黑话，对小白几乎不可读。
- **修复方向**：用更口语化的中文替换面向用户的标签（如 workspace→「接入名」、profile→「配置」、「等待扫描」→「准备中…」），`pid/重启` 等调试信息收进「详情/高级」折叠区。

### DOC-03 · 首页指标用词偏技术

- **状态**：open
- **文件**：`desktop/ilink-hub-desktop/index.html:54-74`（"用户上行次数 = 从微信侧进入 Hub 的消息条数"、"对话次数 = 已转发至后端的消息条数"）
- **问题**：「用户上行次数」「上行」属网络/后端黑话，小白不易理解。
- **修复方向**：改为更直观的措辞，如「收到用户消息」「转发给 AI」，并保留一行小字解释。

---

## 体验中的亮点（保留，勿回退）

- Bridge「连不上 Hub」的错误是**中文 + 分步排查指引**（检查 URL / 远程 Hub / `WEIXIN_TOKEN` / `--pair`），体验很好。
  - 文件：`src/bin/ilink-hub-bridge.rs`（连接失败错误链）
- 自动注册 + 稳定客户端名（`local-<hostname>-<配置名>`），同名重启复用同一客户端，不会在 `/list` 堆积 `local-<uuid>`。
  - 文件：`docs/bridge/README.md:7`
- 桌面端有二维码弹窗、复制备用链接、危险操作二次确认、toast 反馈，整体打磨过。
  - 文件：`desktop/ilink-hub-desktop/index.html:256-295`

---

最后更新：2026-06-12
