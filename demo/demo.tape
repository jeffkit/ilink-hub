# ilink-hub demo — WeChat → Claude Code
# vhs demo.tape

Output demo.gif

Set Shell "zsh"
Set FontSize 15
Set Width 1200
Set Height 600
Set Theme "TokyoNight"
Set Padding 20
Set Framerate 24
Set PlaybackSpeed 1

# 确保 PATH 包含新版 binary
Type "export PATH=$HOME/.local/bin:$PATH"
Enter
Sleep 500ms

# ── 第一幕：展示 bridge 版本 ──────────────────────────────────────────────
Type "ilink-hub-bridge --version"
Sleep 300ms
Enter
Sleep 1500ms

# ── 第二幕：展示 YAML 配置（就是这么简单）────────────────────────────────
Type "cat ~/ilink-claude.yaml"
Sleep 300ms
Enter
Sleep 2000ms

# ── 第三幕：直接测试 claude-code profile（单行命令）─────────────────────
Type `export AGENT_MESSAGE="用 Python 写快速排序，只要函数，不要解释"`
Enter
Sleep 200ms
Type "export AGENT_SESSION_ID=''"
Enter
Sleep 200ms
Type "export CLAUDE_MODEL=sonnet"
Enter
Sleep 200ms
Type "ilink-hub-bridge profile claude-code"
Sleep 300ms
Enter

# Claude 约需 8-12s
Sleep 12s

# ── 结尾停留 ─────────────────────────────────────────────────────────────
Sleep 3s
