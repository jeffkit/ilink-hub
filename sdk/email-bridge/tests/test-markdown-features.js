#!/usr/bin/env node
'use strict';
/**
 * 测试各种 Markdown 特性的 HTML 渲染效果
 * 使用: node test-markdown-features.js
 */

const { convertMarkdownToHtml } = require('./src/index');
const fs = require('fs');
const path = require('path');

// 包含各种 Markdown 特性的测试内容
const testMarkdown = `
你好！

感谢提问！我来详细解答一下。

## 1. 功能概述

当前 \`email-bridge\` 已经支持以下功能：

### 核心特性

- **自动路由**：根据邮件主题前缀 \`[profile-name]\` 智能路由
- **会话管理**：同一邮件线程自动共享 AI 会话上下文
- **重试机制**：失败的邮件会自动重试，确保不丢失
- **防循环设计**：自动过滤自己发送的邮件，避免无限循环

## 2. 技术细节

### 代码示例

下面是一个简单的配置示例：

\`\`\`yaml
default: claude

profiles:
  claude:
    command: node
    args:
      - ./profiles/claude-handler.js
    trigger: claude
\`\`\`

### 数据流

\`\`\`
用户邮件 → 轮询检测 → 路由 → Profile 处理 → HTML 回复
\`\`\`

## 3. 对比表格

| 功能 | 微信通道 | 邮件通道 |
|-----|---------|---------|
| 触发方式 | WebSocket Push | 定时轮询 |
| 响应速度 | 实时 | 最快 15 秒 |
| Profile 兼容 | ✅ 完全兼容 | ✅ 完全兼容 |
| 会话持久化 | ✅ 支持 | ✅ 支持 |

## 4. 注意事项

> **重要提示**：邮件发送有频率限制，默认每天最多 50 封。请合理使用。

以下是建议的使用场景：

1. **日常咨询**
   - 代码审查
   - 技术答疑
   - 架构建议

2. **长对话**
   - 复杂问题讨论
   - 多轮交互
   - 项目规划

3. **异步处理**
   - 非紧急问题
   - 深度分析
   - 文档生成

## 5. 链接和引用

详细文档请参考：
- [iLink Hub 主仓库](https://github.com/youorg/ilink-hub)
- [Email Bridge README](./README.md)

---

**总结**：email-bridge 提供了与微信通道对称的邮件接入能力，代码零重复，Profile 完全复用。

希望这些信息对你有帮助！如果还有其他问题，随时通过邮件联系我。
`;

console.log('=== Testing Markdown Features ===\n');
console.log('Converting markdown with multiple features...\n');

const html = convertMarkdownToHtml(testMarkdown);

// 保存到临时文件
const outputPath = '/tmp/email-full-preview.html';
fs.writeFileSync(outputPath, html, 'utf8');

console.log('✓ Conversion complete');
console.log(`✓ Output saved to: ${outputPath}\n`);
console.log('Features tested:');
console.log('  - Multiple heading levels (H2, H3)');
console.log('  - Bold and italic text');
console.log('  - Inline code and code blocks');
console.log('  - Ordered and unordered lists');
console.log('  - Nested lists');
console.log('  - Tables with alignment');
console.log('  - Blockquotes');
console.log('  - Links');
console.log('  - Horizontal rules');
console.log('  - Mixed Chinese and English content\n');
console.log('Opening in browser...');

// 在浏览器中打开
const { exec } = require('child_process');
exec(`open "${outputPath}"`, (error) => {
  if (error) {
    console.log(`\nNote: Could not auto-open browser. Please open manually:\n  ${outputPath}`);
  }
});
