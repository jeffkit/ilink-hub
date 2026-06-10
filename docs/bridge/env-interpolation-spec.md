# Bridge Profile 环境变量插值 — 需求说明

> 状态：草案（draft）
> 最后更新：2026-06-10
> 关联：`docs/bridge/profile-spec.md`、`~/.ilaude/skills/bridge-profile/SKILL.md`

## 1. 背景与动机

当前 `env:` 字段只接受字面字符串（`env: HashMap<String, String>`，见
`src/bridge/config.rs`）。这导致以下问题：

1. **敏感信息被迫入仓**：用户若需在 profile 里注入 `ANTHROPIC_API_KEY`，
   只能把 key 直接写进 YAML，而 `~/.ilink-hub-bridge/profiles/` 在很多场景下
   会进 git / dotfiles 仓库 → **明文密钥泄露**。
2. **同一密钥多 profile 重复**：5 个 profile 都用同一个 key，要改时得 5 处同步。
3. **环境差异配置无法表达**：dev / prod 切换时要改 YAML，不能只换 shell env。

期望：YAML 写 `${VAR}`，运行时由 bridge 从**进程 env** 展开成实际值，**YAML 本身永远不出现明文密钥**。

## 2. 目标 / 非目标

### 目标
- YAML 中 `env: { FOO: ${BAR} }` 在启动子进程时，**`BAR` 来自 manager 进程的 env**。
- **找不到对应 env var 时报错并停止子进程启动**（fail-fast），不要静默把字面 `${BAR}` 传下去。
- **支持转义**：要传字面量 `$` 时写 `$$`。
- **向后兼容**：YAML 里**不写** `${...}` 的值保持原样（字面字符串），无回归。

### 非目标（本期不做）
- **不读 `~/.ilink-hub-bridge/.env` 文件**（避免扩大 surface；要 `.env` 走 shell 启动 manager）。
- **不递归插值**（`${A_${B}}` 不支持，避免实现复杂度爆炸）。
- **不读 launchd / systemd 的 env 块**——那是 manager 启动方式的问题，不是 profile 解析问题。
- **不提供默认值语法**（`${VAR:-default}`）—— 简单起见只支持 `${VAR}`，缺 var 报错。

## 3. 语法设计

### 3.1 插值 token

仅识别 `${IDENT}` 形式：

- `IDENT` 须匹配 `[A-Za-z_][A-Za-z0-9_]*`
- 不识别 `$VAR`（POSIX shell 风格）—— 避免误把 bash 单字母变量当插值
- 不识别 `${VAR:-default}` / `${VAR-default}` / `${VAR:?err}` —— 见 §2

### 3.2 转义

`$$` → 字面 `$`（一次替换，不递归）

- 例：`$$HOME` → `$HOME`（**不**展开 env）
- 例：`price is $$5` → `price is $5`

### 3.3 多次出现

同一字符串中可出现多次：

```yaml
env:
  GREETING: "hello ${USER}, your key ends in ${KEY_SUFFIX}"
```

## 4. 行为细则

### 4.1 解析时机

**仅在 spawn 子进程那一刻**展开，不在 manager 启动时一次性展开。理由：
manager 持有的是"模板 env map"，子进程 spawn 时才复制并展开 → 子进程拿到的是
那一刻的 env 快照，**与 manager 后续 env 变化解耦**。

### 4.2 缺失变量

| 情况 | 行为 |
|------|------|
| 模板里有 `${X}`，进程 env 无 `X` | **拒绝 spawn**，日志 `ERROR env var X not found for profile <name>`，子进程不启动 |
| 模板里有 `$$X` | 永远不当插值，按 §3.2 处理（不查 env） |
| 模板里**没有** `${...}` | 不查 env，原样传（兼容现有 YAML） |

### 4.3 空值 vs 未定义

- `export FOO=""` + 模板 `${FOO}` → 展开为**空字符串**，不报错
- 未 `export FOO` + 模板 `${FOO}` → 报错

区分"显式空"和"未定义"—— Unix 约定如此。

### 4.4 日志脱敏

**子进程 stdout / stderr 中可能包含展开后的密钥**。错误日志**绝不**打印展开后的 env 值，
只打印**变量名列表**（"expanded env vars: [ANTHROPIC_API_KEY]"），让用户知道哪些被读到了，
但不泄露值。

### 4.5 占位符 `{{MESSAGE}}` 不受影响

现有 `args` / `stdin` 模板里的 `{{MESSAGE}}` / `{{SESSION_ID}}` 走的是另一套
占位符系统（见 `profile-spec.md` §1），**与本插值机制完全独立**。两者可共存于同一 profile。

## 5. 配置层与依赖

| 配置层 | 决定 | 备注 |
|--------|------|------|
| Manager 启动 shell | 提供 env 变量 | 用户责任，文档说明推荐 `~/.zshenv` / `~/.bash_profile` |
| Bridge `env: HashMap` | 持有**模板** | YAML 解析阶段不展开 |
| Bridge spawn 子进程 | 展开 + 注入 | 本期要实现的逻辑 |

**不**新增任何配置文件（无 `.env` loader / 无 `secrets.yaml`）。理由：
iLink Hub 的设计哲学是"轻、stdout 协议、零额外配置面"——加 `.env` 是反方向。

## 6. 实现要点（给后续 PR 看的）

### 6.1 改动的文件

- `src/bridge/config.rs`：当前 `env: HashMap<String, String>` 不变，**新增**解析函数
  `expand_env(template: &str, process_env: &HashMap<String, String>) -> Result<String, EnvExpandError>`
- `src/bridge/manager.rs`：spawn 子进程前调用 `expand_env` 对每条 `env[k]` 展开
- `src/bridge/error.rs`：新增 `EnvExpandError { var: String, profile: String }`

### 6.2 单元测试覆盖

- ✅ 普通插值（`${X}` → `X` 的值）
- ✅ 多次出现同一变量
- ✅ 字面 `$$` 转义
- ✅ `${X}` 与字面量混合（前后缀）
- ✅ 未定义变量 → 错误
- ✅ 空字符串视为合法
- ✅ 模板里没有 `${...}` → 原样
- ✅ 非法 token 形态（`${}` / `${1FOO}` / `${VAR with space}`）→ 错误

### 6.3 错误信息

要**包含**：
- profile 名
- 缺失的变量名
- 出错的 YAML 字段（`env.ANTHROPIC_API_KEY` 这样的 path）

要**不包含**：
- 任何已展开的密钥值
- manager 完整 env dump

## 7. 迁移 / 兼容性

- **现有用户无感**：YAML 不写 `${...}` 时行为不变。
- **手动迁移**：以前写 `ANTHROPIC_API_KEY: "sk-ant-..."` 的 profile，要改成
  `ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}`，并在启动 manager 前 export。
- **文档同步**：`SKILL.md` 已先行改为 `${VAR}` 示范（见"Secrets & 环境变量"节），
  实际实现 PR 合并后此规范进入 GA。

## 8. 开放问题

- [ ] 需不需要支持 `${VAR:-default}`？当前设计为不，但用户后续可能要。**建议**：
  实现 GA 一段时间后看反馈再决定。
- [ ] 错误时 manager 是否要继续运行其他 profile？**建议**是：单 profile 失败不影响其他，
  日志 ERROR 后 manager 继续。这与现有"单 profile 启动失败不致命"行为一致。
- [ ] Windows 兼容：Windows 进程 env 行为与 Unix 一致（`std::env::var` 跨平台），
  无额外工作。

## 9. 参考

- Docker Compose `${VAR}` 插值（行为类似，但支持 `:-` 默认值——我们刻意不支持）
- GitHub Actions `secrets.*` 注入（强类型、不允许部分插值——我们是字符串模板）
- systemd `EnvironmentFile=`（不读文件，只读 env——我们一致）
