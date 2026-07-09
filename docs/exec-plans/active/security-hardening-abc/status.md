# security-hardening-abc — Status

> 最后更新：2026-07-09

## 当前状态

**进度：** M1 ✅ · M2 ✅ · M3 实现完成，待对抗审查 → M4 待做  
**分支：** `fix/security-hardening-abc`  
**Worktree：** `/Users/kongjie/projects/ilink-hub/.worktrees/fix/security-hardening-abc/`

## 里程碑

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1 CORS + vtoken claim-window | ✅ | 对抗审查 PASS |
| M2 shell/log/loopback | ✅ | 对抗审查 PASS（dangerous flag 保留） |
| M3 God 模块拆分 ≥2 | 🔄 待对抗审查 | dispatcher/ + desktop listen_addr/hub_commands/bridge_profiles |
| M4 文档 + 归档 | ⏳ | |

## 恢复指引

```bash
cd /Users/kongjie/projects/ilink-hub/.worktrees/fix/security-hardening-abc
# 续跑：对抗审查 M3，或开始 M4
```

## 用户确认

- A+B 做；dangerous flag **保留**
- C=3 真拆多个 god 模块
