# ilink-hub full demo
# vhs demo-full.tape

Output demo-full.gif

Set Shell "zsh"
Set FontSize 14
Set Width 1100
Set Height 650
Set Theme "TokyoNight"
Set Padding 20
Set Framerate 24
Set PlaybackSpeed 1
# 加载 PATH（含 ~/.local/bin 中的 ilink-hub-bridge 0.1.11）
Type "source ~/.zshrc 2>/dev/null; clear"
Enter
Sleep 800ms

# ── 第 1 幕：安装 ──────────────────────────────────────────────────────────
Type "# 第一步：安装 ilink-hub（macOS）"
Enter
Sleep 400ms
Type "brew install jeffkit/tap/ilink-hub"
Sleep 400ms
Enter
Sleep 2s
Type "ilink-hub --version && ilink-hub-bridge --version"
Enter
Sleep 1500ms

# ── 第 2 幕：配置 Bridge Profile ──────────────────────────────────────────
Type "# 第二步：配置 Claude Code profile"
Enter
Sleep 300ms
Type "cat ~/ilink-claude.yaml"
Enter
Sleep 2500ms

# ── 第 3 幕：启动 Hub ──────────────────────────────────────────────────────
Type "# 第三步：启动 hub server"
Enter
Sleep 300ms
Type "ilink-hub serve --token $BOT_TOKEN"
Sleep 400ms
Enter
Sleep 3s
Ctrl+C
Sleep 600ms

# ── 第 4 幕：Bridge 测试 Claude 回复 ────────────────────────────────────
Type "# 第四步：bridge 直接调用 Claude Code"
Enter
Sleep 300ms
Type "AGENT_MESSAGE='用 Python 写快速排序' AGENT_SESSION_ID='' ILINK_CLAUDE_MODEL=sonnet ilink-hub-bridge profile claude-code"
Sleep 400ms
Enter
Sleep 12s

Sleep 3s
