# 提案：以 Project 为一等概念重构 Backend 组织

| 项 | 值 |
|---|---|
| 状态 | **Draft — 讨论中** |
| 作者 | jeffkit |
| 创建 | 2026-07-15 |
| 更新 | 2026-07-15 |
| 前置依赖 | 现有 `BridgeProfile.cwd`、wire 0.4 turn 对象、`ClientInfo` registry、`@<后端>` mention 路由 |

---

## 1. 背景：`/list` 为什么这么长

当前 `/list` 列出的"已注册后端"远多于真实可用的 agent 数量。根因是：

- `ClientInfo`（`src/hub/registry.rs:17`）本身**不携带 cwd / profile**，只有 `name / label / description / online`。
- 真正定义 agent 行为（command / env / **cwd**）的地方是 bridge 端的 `BridgeProfile`（`src/bridge/config.rs:65`），其中 `cwd` 是写死在 YAML 里的常量。
- 用户要在不同项目里跑同一个 agent CLI（例如 `claude` 在 `ilink-hub` 跑一次、又在 `Agentproc` 跑一次），就必须**注册多个 bridge backend**，每个绑定一份钉死 cwd 的 profile。

于是 `/list` 的条目数 ≈ `agent 数 × project 数`，是笛卡尔积。agent 和 project 本身都是有限的，但组合被预先枚举成独立后端，视觉上就爆炸了。

## 2. 参考实现：Agentproc Hub

`~/projects/Agentproc/hub/<name>/profile.yaml` 的设计精髓：

- profile **只定义 agent**（`command` / `args` / `env`），`cwd` **故意留空**。
- `cwd` 由调用方每次传入（`agentproc hub run` 默认用当前目录，也可 `--cwd` 显式指定）。
- 一个 profile 服务任意项目，没有笛卡尔积。

```yaml
# ~/projects/Agentproc/hub/claude-code/profile.yaml
agentproc:
  command: python3
  args: ["{{PROFILE_DIR}}/bridge.py"]
  # cwd is intentionally left unset.
  timeout_secs: 600
  streaming: true
```

关键启示：**把 cwd 从 profile 里拿出来，交给调用方按需传入。**

## 3. 关键决策：Project 必须在 bridge 端，不能在 Hub 端

> 林哥的判断：project 不应该在 Hub 端维护，而应该在用户的电脑端（bridge 那一端）维护。本地的 profile 才能用到本地的项目。

这个判断不仅"更合适"，而是**根本只能在 bridge 端**：

1. **Hub 看不到本地路径。** Hub 跑在服务器上（如 tcloud_gz），它的 `~/projects/foo` 和用户 Mac 上的 `~/projects/foo` 是两个东西。Hub 端存 project 表，路径是死的、对不上 bridge 实际能访问的目录。
2. **profile 本来就是 bridge 的本地资源。** `BridgeProfile` 里 `command / env / cwd / script` 全是 bridge 进程能直接访问的本地实体；project 作为 `cwd` 的语义来源，理应跟它们住在一起。
3. **多用户/多 bridge 时 Hub 端会再次爆炸。** 假设两台 Mac 都注册了名为 `ilink-hub` 的 project，Hub 端要按 `(user, path)` 去重？这相当于把笛卡尔积搬了个家。

**结论：bridge 维护 project 表（name → path），Hub 只透传"用哪个 project"这个字符串意图。Hub 永远不知道路径长什么样。**

## 4. 目标分层模型

```
bridge 端（本地）                        Hub 端（服务器）
─────────────────                       ──────────────
~/.ilink/
├── profiles/          ← agent 怎么跑     ClientInfo
│   ├── claude-code/      (command/env)    ├── name = bridge 名
│   └── codex/                              ├── online
└── projects.yaml      ← project 表        └── projects（仅名字列表）
    ilink-hub: ~/projects/ilink-hub
    agentproc:  ~/projects/Agentproc
```

- **Backend（Profile）层**：数量少、稳定。一个 backend 对应"一种 agent 怎么跑"（不含 cwd）。
- **Project 层**：数量多、动态、bridge 本地维护。一次具体调用 = backend + project（→ cwd）。
- 组合是**临时的、按消息构造的**，不是预先枚举的独立后端。

## 5. 交互流程

1. bridge 注册时，把"我能跑哪些 agent、我能访问哪些 project"作为元数据上报 Hub（塞进现有 `description` 或新增 `capabilities`）。
2. Hub 的 `ClientInfo` 增加可选 `projects: Vec<String>`（**只是名字列表，不是路径**），给 `/list` 展示用。
3. 用户发 `@claude-code #ilink-hub 修个 bug`：
   - Hub 路由到对应 bridge backend；
   - turn 对象里带上 `project: "ilink-hub"`；
   - **bridge 在本地查表把 `ilink-hub` 解析成 `~/projects/ilink-hub`，作为 cwd 传给 agent CLI。**
4. `/list` 输出形态（backend 有限、project 作为能力清单）：

   ```
   **已注册的后端：**
   🟢 1. `mac-workspace` — Claude Code / Codex
      projects: ilink-hub, agentproc, recursive
   🟢 2. `linux-box` — Gemini
      projects: server-stuff
   ```

## 6. 落地最小改动（待 impact 分析确认）

> 按 `CLAUDE.md` 要求，改动前必须先跑 `gitnexus_impact` 评估 `BridgeProfile.cwd` 和 `ClientInfo` 的 blast radius。

1. **bridge 端** 新增 `projects.yaml`（或塞进现有 bridge YAML 顶层），定义 `name → path`。
2. **bridge 端** `BridgeProfile.cwd` 降级为 fallback；turn 对象带 `project` 时查表覆盖 cwd。
3. **wire 协议** turn 对象增加可选字段 `project: Option<String>`（纯增量、向后兼容）。
4. **Hub 端** `ClientInfo` 增加可选 `projects: Vec<String>`，由 bridge 注册时上报，仅供 `/list` 展示。
5. **Hub 端** mention / quote_route 解析支持 `#project` 语法，塞进 turn 对象。

## 7. 未决问题 / 待讨论

- **project 名字冲突**：两个 bridge 都上报 `ilink-hub` 时，`#ilink-hub` 默认走哪个？是否需要 `@backend #project` 强制限定？
- **未指定 project 时的默认行为**：退回 `BridgeProfile.cwd` fallback？还是要求显式？
- **project 增删改的同步**：bridge 本地改了 `projects.yaml`，何时重新上报 Hub（启动时 / 文件变更 / 手动命令）？
- **MCP `list_agents` 的语义**：`description` / `capabilities` 字段如何向其他 Agent 暴露 project 维度？
- **阶段化**：是否先做"bridge 接受运行时 cwd"（阶段 1）跑通，再引入 project 抽象（阶段 2）？还是一步到位？

## 8. 参考资源

- `~/projects/Agentproc/hub/README.md` — profile 设计原则
- `~/projects/Agentproc/hub/claude-code/profile.yaml` — cwd 故意留空的实例
- `src/bridge/config.rs:65` — 现有 `BridgeProfile`（cwd 写死处）
- `src/hub/registry.rs:17` — 现有 `ClientInfo`（待扩展处）
- `src/hub/commands.rs:106` — `/list` 命令实现
