# security-hardening-abc — Status

> 最后更新：2026-07-09

## 当前状态

**进度：** M2 实现完成，待对抗审查  
**分支：** `fix/security-hardening-abc`  
**Worktree：** `/Users/kongjie/projects/ilink-hub/.worktrees/fix/security-hardening-abc/`

## 里程碑

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1 CORS + vtoken claim-window | ✅ | claim-window 修复已落地；见 reviews/m1/ |
| M2 shell/log/loopback | 🔄 待对抗审查 | High（保留 dangerous flag）；见 reviews/m2/ |
| M3 God 模块拆分 ≥2 | ⏳ | C=3 |
| M4 文档 + 归档 | ⏳ | |

## 恢复指引

```bash
cd /Users/kongjie/projects/ilink-hub/.worktrees/fix/security-hardening-abc
# 续跑：对抗审查 M2，或开始 M3
```

## 用户确认

- A+B 做；dangerous flag **保留**
- C=3 真拆多个 god 模块
