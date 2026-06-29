# status.md — agentproc-protocol-align

## 当前进度
- Phase 1 ✅ Scope + prompt.md（已提交 6dc468c）
- Phase 2 ✅ plan.md（已提交 63aacb2）
- Phase 3 🔄 M1 实现中

## 里程碑表
| 里程碑 | 状态 | 验证 | 审查 | E2E |
|--------|------|------|------|-----|
| M1 Rust bridge 协议改名 | ✅ completed | ✅ 全过 | 降级自审 | not-ready |

Step 2 ✅ 删除 sdk/ + examples（ilink-bridge-profile 依赖），commit b7d9c57
Step 3 ✅ 文档同步（AGENT_* 协议名 + agentproc 包名），commit 577cb99
Step 4 ✅ builtin 归属：保留 Rust builtin，不重写为 agentproc profile 脚本（决策确认，无代码改动）

## 完成
所有 4 步已完成，进入 Phase 4 归档。

## 恢复指引
- Worktree: `/Users/kongjie/projects/ilink-hub/.worktrees/feat/agentproc-protocol-align/`
- 分支: `feat/agentproc-protocol-align`
- 协议映射表见 `plan.md`
- 例外清单（保留原名）见 `plan.md`「保留原名」段
