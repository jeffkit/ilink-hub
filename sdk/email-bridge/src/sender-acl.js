'use strict';
/**
 * SenderAcl — 发件人访问控制
 *
 * 支持：
 *   - 精确匹配   user@example.com
 *   - 域名匹配   @example.com（匹配该域所有地址）
 *   - 子域通配   @*.example.com（匹配所有子域名）
 *
 * 优先级（高到低）：
 *   1. 全局 denied_senders 命中 → 拒绝
 *   2. 全局 allowed_senders 未命中（且已配置白名单）→ 拒绝
 *   3. 全局放行 → 检查 per-profile allowed_senders
 *   4. Per-profile allowed_senders 未命中（且已配置）→ 拒绝
 */

/**
 * Test whether an email address matches a rule string.
 *
 * @param {string} email  lowercase sender address
 * @param {string} rule   one of: "user@domain", "@domain", "@*.domain"
 * @returns {boolean}
 */
function matchesRule(email, rule) {
  const r = rule.trim().toLowerCase();

  // Wildcard subdomain: @*.example.com
  if (r.startsWith('@*.')) {
    const baseDomain = r.slice(3); // "example.com"
    const atIdx = email.indexOf('@');
    if (atIdx === -1) return false;
    const domain = email.slice(atIdx + 1); // "sub.example.com"
    return domain === baseDomain || domain.endsWith('.' + baseDomain);
  }

  // Domain match: @example.com
  if (r.startsWith('@')) {
    return email.endsWith(r);
  }

  // Exact match (both sides already lowercased)
  return email.toLowerCase() === r;
}

/**
 * Test whether an email matches any rule in a list.
 *
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
   * @param {object} config  Parsed email-profiles.yaml root object
   */
  constructor(config) {
    this._globalAllowed = config.allowed_senders || [];
    this._globalDenied  = config.denied_senders  || [];
    this._denyAction    = config.deny_action || 'silent';
    this._denyMessage   = config.deny_message || null;
    this._profiles      = config.profiles || {};
  }

  /** @returns {'silent'|'notify'} */
  get denyAction() { return this._denyAction; }

  /** @returns {string|null} Custom deny reply body, or null to use default */
  get denyMessage() { return this._denyMessage; }

  /**
   * Check global ACL for a sender.
   *
   * @param {string} senderEmail
   * @returns {'allow'|'deny'}
   */
  checkGlobal(senderEmail) {
    const lower = (senderEmail || '').toLowerCase();

    // Denylist has highest priority
    if (matchesAny(lower, this._globalDenied)) return 'deny';

    // Allowlist: only enforce when explicitly configured
    if (this._globalAllowed.length > 0 && !matchesAny(lower, this._globalAllowed)) {
      return 'deny';
    }

    return 'allow';
  }

  /**
   * Check per-profile ACL for a sender.
   * Call only after checkGlobal() returns 'allow'.
   *
   * @param {string} profileName
   * @param {string} senderEmail
   * @returns {'allow'|'deny'}
   */
  checkProfile(profileName, senderEmail) {
    const profileCfg = this._profiles[profileName] || {};
    const profileAllowed = profileCfg.allowed_senders || [];

    // No per-profile list configured → inherit global (already passed)
    if (profileAllowed.length === 0) return 'allow';

    const lower = (senderEmail || '').toLowerCase();
    return matchesAny(lower, profileAllowed) ? 'allow' : 'deny';
  }

  /**
   * Convenience: returns true if no ACL rules are configured at all
   * (avoids any overhead when running in fully-open mode).
   *
   * @returns {boolean}
   */
  isOpenAccess() {
    return this._globalAllowed.length === 0 && this._globalDenied.length === 0;
  }
}

module.exports = { SenderAcl, matchesRule, matchesAny };
