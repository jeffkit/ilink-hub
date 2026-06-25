'use strict';
/**
 * SenderAcl — 发件人访问控制
 *
 * 接收 AclConfig 提供的合并后规则，对每封邮件做两级检查：
 *   1. 全局检查（checkGlobal）
 *   2. Per-profile 检查（checkProfile）— 规则来自 email-acl.yaml 的 profile_acl 段
 *
 * 匹配语法：
 *   user@example.com   精确匹配（大小写不敏感）
 *   @example.com       域名下所有地址
 *   @*.example.com     所有子域名
 *
 * 优先级（高到低）：
 *   动态 denied  > 静态 denied  > 动态 allowed > 静态 allowed > 未配置白名单=放行
 *   （合并由 AclConfig._merge() 处理，此处只看合并后的结果）
 */

/**
 * @param {string} email  lowercase
 * @param {string} rule
 * @returns {boolean}
 */
function matchesRule(email, rule) {
  const r = rule.trim().toLowerCase();

  if (r.startsWith('@*.')) {
    const baseDomain = r.slice(3);
    const atIdx = email.indexOf('@');
    if (atIdx === -1) return false;
    const domain = email.slice(atIdx + 1);
    return domain === baseDomain || domain.endsWith('.' + baseDomain);
  }

  if (r.startsWith('@')) {
    return email.endsWith(r);
  }

  return email.toLowerCase() === r;
}

/**
 * @param {string}   email
 * @param {string[]} rules
 * @returns {boolean}
 */
function matchesAny(email, rules) {
  if (!rules || rules.length === 0) return false;
  const lower = email.toLowerCase();
  return rules.some((rule) => matchesRule(lower, rule));
}

class SenderAcl {
  /**
   * @param {import('./acl-config').AclConfig} aclConfig
   */
  constructor(aclConfig) {
    this._cfg = aclConfig;
  }

  get denyAction()  { return this._cfg.denyAction; }
  get denyMessage() { return this._cfg.denyMessage; }

  /**
   * True when no ACL rules at all — avoids overhead in open-access deployments.
   * @returns {boolean}
   */
  isOpenAccess() { return this._cfg.isOpenAccess(); }

  /**
   * Check global ACL (merged static + dynamic rules).
   * @param {string} senderEmail
   * @returns {'allow'|'deny'}
   */
  checkGlobal(senderEmail) {
    const lower = (senderEmail || '').toLowerCase();
    if (matchesAny(lower, this._cfg.deniedSenders))  return 'deny';
    if (this._cfg.allowedSenders.length > 0 &&
        !matchesAny(lower, this._cfg.allowedSenders)) return 'deny';
    return 'allow';
  }

  /**
   * Check per-profile ACL (from email-acl.yaml profile_acl section).
   * Call only after checkGlobal() returns 'allow'.
   *
   * @param {string} profileName
   * @param {string} senderEmail
   * @returns {'allow'|'deny'}
   */
  checkProfile(profileName, senderEmail) {
    const profileAllowed = (this._cfg.profileAcl[profileName] || {}).allowed_senders || [];
    if (profileAllowed.length === 0) return 'allow';
    const lower = (senderEmail || '').toLowerCase();
    return matchesAny(lower, profileAllowed) ? 'allow' : 'deny';
  }

  /**
   * True if the sender is an admin (may issue management commands).
   * @param {string} senderEmail
   * @returns {boolean}
   */
  isAdmin(senderEmail) {
    return matchesAny((senderEmail || '').toLowerCase(), this._cfg.adminSenders);
  }
}

module.exports = { SenderAcl, matchesRule, matchesAny };
