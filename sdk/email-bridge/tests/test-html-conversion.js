#!/usr/bin/env node
'use strict';
/**
 * 测试 Markdown → HTML 转换
 * 使用: node test-html-conversion.js
 */

const { convertMarkdownToHtml } = require('./src/dispatcher');

// 模拟 AI 回复的 Markdown 内容
const sampleMarkdown = `你好！

感谢问到建议！从目前代码看，\`feat/agent-email-bridge\` 这个分支已经相当成熟了。以下是几点建议：

**功能完善方面：**
1. **邮件附件处理** - 目前不清楚系统是否支持附件，如果需要处理图片、PDF 等附件，可以考虑集成 multimodal 能力
2. **邮件过滤规则** - 可以考虑添加更细粒度的过滤规则，比如根据发件人、主题关键词、优先级等自动分类处理
3. **上下文管理** - 长对话线索可能需要智能摘要或上下文窗口管理，避免超出 token 限制

**稳定性方面：**
- 重试策略优化 - 已有失败重试机制，可以考虑添加指数退避和最大重试次数限制
- 邮件发送速率限制 - 防止触发邮件服务商的频率限制

你觉得哪些方向对你当前的需求更重要？我可以帮你具体实现。
`;

console.log('=== Original Markdown ===');
console.log(sampleMarkdown);
console.log('\n=== Converted HTML ===');
const html = convertMarkdownToHtml(sampleMarkdown);
console.log(html);

// 保存到文件以便在浏览器中查看
const fs = require('fs');
const outputPath = '/tmp/email-preview.html';
fs.writeFileSync(outputPath, html, 'utf8');
console.log(`\n✓ HTML saved to: ${outputPath}`);
console.log('  Open it in your browser to preview');
