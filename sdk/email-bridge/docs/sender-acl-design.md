# 设计提案：Email Bridge 发件人访问控制（Sender ACL）

| 项 | 值 |
|---|---|
| 状态 | **Draft / 待确认** |
| 创建 | 2026-06-25 |
| 前置依赖 | email-bridge 基础路由、pending-store、时间戳游标 poll |

---

## 1. 背景与问题

当前 email-bridge 完全开放：任何人只要知道 Agent 邮箱地址，发来邮件都会得到 AI 回复并消耗计算资源。唯一的过滤逻辑是 `filterSelfSent`（防止 Agent 回复自己造成死循环），本质上不是访问控制。

这带来两个风险：
1. **资源滥用**：任意外部邮件触发 AI 推理，成本不可控
2. **安全边界模糊**：Agent 可能响应恶意构造的邮件内容

需要一套轻量的访问控制机制，在不大幅增加配置复杂度的前提下满足以下场景：
- 个人用户：只允许自己的几个邮箱联系 Agent
- 团队场景：允许整个公司域名（`@company.com`）
- 精细控制：某些 profile 只对特定人开放

---

## 2. 设计目标与非目标

### 目标
- 支持**全局白名单**（allowed_senders）：不在列表里的发件人被静默忽略或收到提示
- 支持**全局黑名单**（denied_senders）：明确拒绝某些地址
- 支持**域名匹配**：`@company.com` 匹配整个域
- 支持**per-profile ACL**：某个 profile 只对特定发件人开放
- 配置在 `email-profiles.yaml` 中，无需改代码
- 被拒绝时可选择静默丢弃或发送一封礼貌的拒绝回信

### 非目标
- 不做身份认证（DKIM/DMARC 验证留给邮件服务商）
- 不做细粒度的操作权限（如"此人只能用 echo profile"由 per-profile ACL 覆盖）
- 不做动态通讯录（运行时增删联系人，本期不做）

---

## 3. 配置设计

### 3.1 全局 ACL（顶层字段）

```yaml
# email-profiles.yaml

# 全局白名单：只处理来自这些地址/域名的邮件
# 留空或不配置 = 不限制（当前行为）
allowed_senders:
  - bbmyth@gmail.com          # 精确匹配
  - "@company.com"            # 域名匹配（所有 company.com 邮箱）
  - "@trusted-partner.org"

# 全局黑名单：明确拒绝这些地址（优先级高于白名单）
denied_senders:
  - spam@example.com

# 被拒绝时的行为：
#   silent  — 静默丢弃，不回复（默认，避免泄露 Agent 存在）
#   notify  — 发送一封礼貌的拒绝通知邮件
deny_action: silent

default: claude-code

profiles:
  claude-code:
    command: node
    args: [./profiles/claude-code.js]
    trigger: claude
  ...
```

### 3.2 Per-Profile ACL

```yaml
profiles:
  claude-code:
    command: node
    args: [./profiles/claude-code.js]
    trigger: claude
    # 此 profile 的独立白名单，不在此列表的发件人即使过了全局 ACL 也无法使用
    # 留空 = 继承全局规则
    allowed_senders:
      - bbmyth@gmail.com
      - "@company.com"

  echo:
    command: node
    args: [./profiles/echo.js]
    trigger: echo
    # echo profile 仅供调试，只有自己能用
    allowed_senders:
      - bbmyth@gmail.com
```

### 3.3 匹配规则优先级

```
全局 denied_senders 命中  →  拒绝（最高优先级）
全局 allowed_senders 未命中（且配置了白名单）  →  拒绝
全局 ACL 通过  →  进入 profile 路由
Per-profile allowed_senders 未命中（且配置了白名单）  →  拒绝
Per-profile ACL 通过  →  正常处理
```

### 3.4 域名匹配语法

| 配置写法 | 匹配逻辑 |
|---------|---------|
| `user@example.com` | 精确匹配（大小写不敏感） |
| `@example.com` | 匹配该域名下所有地址 |
| `@*.example.com` | 匹配所有子域名（可选，二期实现） |

---

## 4. 实现方案

### 4.1 代码改动范围

改动集中在两个文件，不影响 agently-cli 调用逻辑：

**`src/index.js` — createEmailBridge**

```
loadProfilesConfig() 已有
↓
新增：buildAclChecker(config)  ← 从 yaml 构建 ACL 检查函数
↓
poll handler 中（现有 filterSelfSent 之后）：
  if (!globalAcl.allows(senderEmail)) → deny_action 处理
  ...
  resolveProfile() 后：
  if (!profileAcl.allows(profileName, senderEmail)) → deny_action 处理
```

**`src/dispatcher.js` — 不改动**（ACL 在进入 dispatch 之前就已判断）

### 4.2 核心逻辑伪代码

```js
function buildAclChecker(config) {
  const globalAllowed = config.allowed_senders || [];   // [] = 不限制
  const globalDenied  = config.denied_senders  || [];

  function matches(email, rules) {
    const lower = email.toLowerCase();
    return rules.some(rule => {
      if (rule.startsWith('@')) return lower.endsWith(rule.toLowerCase());
      return lower === rule.toLowerCase();
    });
  }

  return {
    // 返回 'allow' | 'deny'
    checkGlobal(senderEmail) {
      if (matches(senderEmail, globalDenied)) return 'deny';
      if (globalAllowed.length > 0 && !matches(senderEmail, globalAllowed)) return 'deny';
      return 'allow';
    },

    checkProfile(profileConfig, senderEmail) {
      const profileAllowed = profileConfig.allowed_senders || [];
      if (profileAllowed.length === 0) return 'allow';  // 无配置 = 继承全局
      return matches(senderEmail, profileAllowed) ? 'allow' : 'deny';
    },
  };
}
```

### 4.3 拒绝行为实现

```js
async function handleDenied(mail, msg, denyAction, reason) {
  process.stderr.write(
    `[email-bridge] ACL denied: "${msg.subject}" from ${msg.from?.email} (${reason})\n`
  );
  if (denyAction === 'notify') {
    // 发送礼貌拒绝邮件，不泄露系统细节
    mail.reply(msg.message_id,
      '感谢您的来信。您的邮件无法被处理，请联系管理员。',
      { bodyFormat: 'plain' }
    );
  }
  // silent: 不做任何事，但仍需 add 到 pending 并立即 markReplied
  // 避免被 retry sweep 反复重试
  pending.add(msg);
  pending.markReplied(msg.message_id);
}
```

---

## 5. 对现有行为的影响

| 场景 | 当前行为 | 改动后行为 |
|------|---------|-----------|
| 未配置 allowed_senders | 处理所有邮件 | 不变（完全兼容） |
| 配置了白名单，发件人在列表里 | — | 正常处理 |
| 配置了白名单，发件人不在列表里 | 处理 | 静默丢弃或通知 |
| 配置了黑名单，发件人命中 | 处理 | 静默丢弃或通知 |
| Per-profile ACL 拒绝 | — | 拒绝后 fallback 到 default profile（可选）或直接拒绝 |

> **兼容性**：`allowed_senders` 和 `denied_senders` 不配置时行为与当前完全一致，零迁移成本。

---

## 6. 待确认的设计问题

在实现前需要确认以下几点：

**Q1：Per-profile ACL 拒绝时，是否 fallback 到 default profile？**
- 方案 A：直接拒绝，走 `deny_action`
- 方案 B：降级到 default profile 处理（如果 default profile 的 ACL 允许）
- 建议：方案 A，行为更可预期

**Q2：被 ACL 拒绝的邮件是否需要标记为"已处理"防止 retry？**
- 建议：是，写入 pending store 并立即 markReplied，否则每次 retry sweep 都会重新判断拒绝

**Q3：`deny_action: notify` 的回复内容是否需要支持自定义模板？**
- 建议：一期用固定文案，二期再支持 `deny_message` 配置字段

**Q4：是否需要支持正则表达式匹配？**
- 建议：一期只做精确匹配和域名前缀（`@domain.com`），避免配置复杂化

---

## 7. 实现优先级建议

| 阶段 | 内容 | 工作量估算 |
|------|------|-----------|
| P0 | 全局 `allowed_senders` 白名单 + `silent` 丢弃 | ~2h |
| P1 | 全局 `denied_senders` 黑名单 + `notify` 拒绝回信 | ~1h |
| P2 | Per-profile `allowed_senders` | ~1h |
| P3 | 域名通配符 `@*.domain.com`、自定义拒绝模板 | 视需求 |

P0 覆盖了最核心的安全需求，建议优先落地。
