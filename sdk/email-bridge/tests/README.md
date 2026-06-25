# Email Bridge 测试

## 测试文件说明

### 1. `test-html-conversion.js`
测试 Markdown → HTML 转换的基础功能

```bash
node tests/test-html-conversion.js
```

**输出**：
- 打印原始 Markdown 和转换后的 HTML
- 保存预览文件到 `/tmp/email-preview.html`
- 自动在浏览器中打开预览

---

### 2. `test-integration.js`
验证 HTML 转换功能是否正确集成到 email-bridge

```bash
node tests/test-integration.js
```

**检查项**：
- ✓ `convertMarkdownToHtml` 是否正确导出
- ✓ Markdown 转换功能是否工作
- ✓ 输出是否包含 HTML 和样式
- ✓ `AgentlyMailClient` 是否支持 HTML bodyFormat

---

### 3. `test-markdown-features.js`
全面测试各种 Markdown 特性的渲染效果

```bash
node tests/test-markdown-features.js
```

**测试的 Markdown 特性**：
- 多级标题 (H2, H3)
- 加粗/斜体
- 行内代码和代码块
- 有序/无序列表
- 嵌套列表
- 表格（带对齐）
- 引用块
- 链接
- 分隔线
- 中英文混排

**输出**：
- 保存完整预览到 `/tmp/email-full-preview.html`
- 自动在浏览器中打开

---

### 4. `test-run.js`
端到端测试：连接真实邮箱，测试完整流程

```bash
# 干运行模式（不实际发邮件）
node tests/test-run.js --dry-run

# 真实运行（会实际发送邮件）
node tests/test-run.js
```

**注意事项**：
- 需要先运行 `agently-cli auth login` 登录授权
- 默认 15 秒轮询间隔（快速测试）
- 关闭了自发邮件过滤（`filterSelfSent: false`），允许处理自己发送的邮件
- 使用 `email-profiles.example.yaml` 配置

**测试流程**：
1. 启动脚本
2. 给自己发一封测试邮件（主题包含 `[echo]` 等前缀）
3. 等待 15 秒（最多）
4. 检查收件箱，确认收到 HTML 格式的回复

---

## 快速验证所有功能

运行所有测试：

```bash
# 1. 基础转换测试
node tests/test-html-conversion.js

# 2. 集成测试
node tests/test-integration.js

# 3. Markdown 特性测试
node tests/test-markdown-features.js

# 4. 端到端测试（干运行）
node tests/test-run.js --dry-run
```

全部通过表示 HTML 邮件功能已正常工作。

---

## 浏览器预览文件

测试脚本会生成以下预览文件：
- `/tmp/email-preview.html` - 简单示例
- `/tmp/email-full-preview.html` - 完整功能展示

可以直接在浏览器中打开查看实际邮件效果。
