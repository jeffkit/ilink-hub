'use strict';
/**
 * AclConfig — 加载并合并 ACL 配置
 *
 * 数据来源（优先级从高到低）：
 *   1. acl-dynamic.json  运行时写入，管理员指令或程序自动维护
 *   2. email-acl.yaml    静态配置，人工编辑，可 git 管理
 *
 * email-acl.yaml 与 email-profiles.yaml 严格分离：
 *   - email-profiles.yaml 只描述 profile 路由规范（command/args/trigger）
 *   - email-acl.yaml 描述所有与 agently mail 访问控制相关的配置
 *
 * 合并规则：
 *   - dynamic.allowed 追加到 static.allowed（动态放行不覆盖静态）
 *   - dynamic.denied  追加到 static.denied（动态封禁不覆盖静态）
 *   - dynamic.allowed 中的地址会从合并后的 denied 中移除（放行优先于封禁）
 *   - admin_senders / deny_action / deny_message / report 仅来自静态配置
 */

const fs   = require('fs');
const path = require('path');
const os   = require('os');

const DEFAULT_STORE_DIR    = path.join(os.homedir(), '.ilink-email-bridge');
const DEFAULT_DYNAMIC_FILE = path.join(DEFAULT_STORE_DIR, 'acl-dynamic.json');

// ---------------------------------------------------------------------------
// YAML loader (mirrors dispatcher.js approach — js-yaml with simple fallback)
// ---------------------------------------------------------------------------

function loadYaml(filePath) {
  const raw = fs.readFileSync(filePath, 'utf8');
  try {
    const yaml = require('js-yaml');
    return yaml.load(raw) || {};
  } catch {
    return _parseSimpleYaml(raw);
  }
}

function _parseSimpleYaml(text) {
  const result = {};
  let currentKey = null;
  let inList     = false;

  for (const rawLine of text.split('\n')) {
    const line    = rawLine;
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;

    // Top-level list item
    const listM = line.match(/^  - (.+)/);
    if (listM && inList && currentKey) {
      result[currentKey].push(listM[1].trim().replace(/\s+#.*$/, ''));
      continue;
    }

    // Key: value  or  Key:  (start of list)
    const kvM = line.match(/^([\w_-]+):\s*(.*)/);
    if (kvM) {
      const key = kvM[1];
      const val = kvM[2].replace(/\s+#.*$/, '').trim();
      if (val === '') {
        result[key] = [];
        currentKey  = key;
        inList      = true;
      } else {
        result[key] = val;
        currentKey  = null;
        inList      = false;
      }
    }
  }
  return result;
}

// ---------------------------------------------------------------------------
// AclConfig
// ---------------------------------------------------------------------------

class AclConfig {
  /**
   * @param {object} [opts]
   * @param {string} [opts.aclConfigFile]   Path to email-acl.yaml (or null to skip)
   * @param {string} [opts.dynamicFile]     Path to acl-dynamic.json
   */
  constructor(opts = {}) {
    this._dynamicFile = opts.dynamicFile || DEFAULT_DYNAMIC_FILE;
    this._static      = this._loadStatic(opts.aclConfigFile);
    this._dynamic     = this._loadDynamic();
    this._merged      = this._merge();
  }

  // ── public getters ────────────────────────────────────────────────────────

  get allowedSenders()  { return this._merged.allowed; }
  get deniedSenders()   { return this._merged.denied; }
  get adminSenders()    { return this._static.admin_senders || []; }
  get denyAction()      { return this._static.deny_action || 'silent'; }
  get denyMessage()     { return this._static.deny_message || null; }
  get profileAcl()      { return this._static.profile_acl || {}; }
  get reportConfig()    { return this._static.report || {}; }

  /** True when no ACL rules at all (open access mode). */
  isOpenAccess() {
    return this._merged.allowed.length === 0 &&
           this._merged.denied.length  === 0;
  }

  // ── dynamic mutations (runtime, persisted to JSON) ────────────────────────

  /**
   * Add addresses to the dynamic allowlist.
   * @param {string[]} addresses
   */
  dynamicAllow(addresses) {
    const d = this._loadDynamic();
    for (const addr of addresses) {
      const lower = addr.toLowerCase();
      if (!d.allowed.includes(lower)) d.allowed.push(lower);
      d.denied = d.denied.filter((a) => a !== lower);
    }
    this._saveDynamic(d);
    this._dynamic = d;
    this._merged  = this._merge();
  }

  /**
   * Add addresses to the dynamic denylist.
   * @param {string[]} addresses
   */
  dynamicDeny(addresses) {
    const d = this._loadDynamic();
    for (const addr of addresses) {
      const lower = addr.toLowerCase();
      if (!d.denied.includes(lower)) d.denied.push(lower);
      d.allowed = d.allowed.filter((a) => a !== lower);
    }
    this._saveDynamic(d);
    this._dynamic = d;
    this._merged  = this._merge();
  }

  /**
   * Remove addresses from both dynamic lists (reset to static-only behaviour).
   * @param {string[]} addresses
   */
  dynamicReset(addresses) {
    const d = this._loadDynamic();
    for (const addr of addresses) {
      const lower = addr.toLowerCase();
      d.allowed = d.allowed.filter((a) => a !== lower);
      d.denied  = d.denied.filter((a) => a !== lower);
    }
    this._saveDynamic(d);
    this._dynamic = d;
    this._merged  = this._merge();
  }

  /** Return a snapshot of the dynamic lists (for /status command). */
  dynamicSnapshot() {
    return { allowed: [...this._dynamic.allowed], denied: [...this._dynamic.denied] };
  }

  // ── private ───────────────────────────────────────────────────────────────

  _loadStatic(aclConfigFile) {
    if (!aclConfigFile) return {};
    try {
      return loadYaml(aclConfigFile);
    } catch (err) {
      process.stderr.write(`[acl-config] Cannot load ${aclConfigFile}: ${err.message}\n`);
      return {};
    }
  }

  _loadDynamic() {
    try {
      if (fs.existsSync(this._dynamicFile)) {
        const raw = JSON.parse(fs.readFileSync(this._dynamicFile, 'utf8'));
        return {
          allowed: Array.isArray(raw.allowed) ? raw.allowed : [],
          denied:  Array.isArray(raw.denied)  ? raw.denied  : [],
        };
      }
    } catch { /* ignore */ }
    return { allowed: [], denied: [] };
  }

  _saveDynamic(data) {
    try {
      fs.mkdirSync(path.dirname(this._dynamicFile), { recursive: true });
      fs.writeFileSync(
        this._dynamicFile,
        JSON.stringify({ allowed: data.allowed, denied: data.denied }, null, 2),
        'utf8',
      );
    } catch (err) {
      process.stderr.write(`[acl-config] Cannot save dynamic ACL: ${err.message}\n`);
    }
  }

  _merge() {
    const staticAllowed = this._static.allowed_senders || [];
    const staticDenied  = this._static.denied_senders  || [];
    const dynAllowed    = this._dynamic.allowed;
    const dynDenied     = this._dynamic.denied;

    // Dynamic allow entries override dynamic deny (already enforced in dynamicAllow/Deny)
    // but static deny is never overridden by dynamic allow — static config is authoritative
    const mergedAllowed = [...new Set([...staticAllowed, ...dynAllowed])];
    const mergedDenied  = [...new Set([...staticDenied,  ...dynDenied])];

    return { allowed: mergedAllowed, denied: mergedDenied };
  }
}

module.exports = { AclConfig };
