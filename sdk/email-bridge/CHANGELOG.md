# Changelog

## [Unreleased]

### Added
- **HTML Email Support**: Agent 回复现在会自动将 Markdown 转换为格式化的 HTML 邮件
  - 添加 `convertMarkdownToHtml()` 函数用于 Markdown → HTML 转换
  - 使用 `marked` 库解析 Markdown，支持 GFM (GitHub Flavored Markdown)
  - 包含 GitHub 风格的样式：代码块、表格、列表、引用等
  - 邮件回复自动使用 HTML 格式，提供更好的阅读体验
  
### Changed
- 邮件回复从纯文本格式改为 HTML 格式
- 日志输出从 "Replied" 改为 "Replied (HTML)" 以标识使用了 HTML 格式

### Dependencies
- 新增依赖：`marked@18.0.5`

## [0.1.0] - 2026-06-XX

### Added
- Initial release
- Email polling and routing based on subject prefix
- Profile dispatcher with session management
- Retry mechanism for failed messages
- Self-sent email filtering
