'use strict';
/**
 * ProfileDispatcher — Email Bridge 核心路由层
 *
 * 职责：
 *  1. 解析邮件主题前缀 [profile-name]，映射到 Profile 配置
 *  2. 清理邮件正文（剥离 HTML、移除 quoted 引用行、截断超长内容）
 *  3. 维护每个（邮件线程 × Profile）的会话历史
 *  4. 用 P0 协议（ILINK_* env vars）spawn Profile 子进程
 *  5. 处理会话失效自动降级（无 session ID 重试）
 *
 * 这一层在功能上等价于 iLink Hub 里的 Bridge Manager + Executor，
 * 但面向邮件通道，用配置文件替代动态注册。
 */

const { spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');

// ---------------------------------------------------------------------------
// Session history helpers — optional dependency on ilink-bridge-profile
// ---------------------------------------------------------------------------

let _loadHistory, _appendHistory;
try {
  ({ loadHistory: _loadHistory, appendHistory: _appendHistory } =
    require('ilink-bridge-profile'));
} catch {
  _loadHistory = () => [];
  _appendHistory = () => {};
}

// ---------------------------------------------------------------------------
// YAML loader
// ---------------------------------------------------------------------------

/**
 * Load email-profiles.yaml.
 * Tries js-yaml first; falls back to a minimal inline parser.
 *
 * @param {string} filePath
 * @returns {ProfilesConfig}
 */
function loadProfilesConfig(filePath) {
  const raw = fs.readFileSync(filePath, 'utf8');
  try {
    const yaml = require('js-yaml');
    return yaml.load(raw);
  } catch {
    return _parseSimpleYaml(raw);
  }
}

/**
 * Strip an inline YAML comment from a scalar value string.
 * e.g. './profiles/echo.js   # debug' → './profiles/echo.js'
 * Note: does not handle strings that legitimately contain " #".
 */
function _stripInlineComment(value) {
  // Remove anything from " #" (space + hash) onwards
  return value.replace(/\s+#.*$/, '').trim();
}

function _parseSimpleYaml(text) {
  const result = { default: '', profiles: {} };
  let current = null;
  let inArgs = false;

  for (const line of text.split('\n')) {
    if (line.trim().startsWith('#') || !line.trim()) continue;

    const m0 = line.match(/^default:\s*(.+)/);
    if (m0) { result.default = _stripInlineComment(m0[1]); continue; }
    if (line.match(/^profiles:/)) continue;

    const m1 = line.match(/^  ([\w-]+):/);
    if (m1) {
      current = m1[1];
      result.profiles[current] = { command: '', args: [], trigger: '' };
      inArgs = false;
      continue;
    }

    if (current) {
      const mc = line.match(/^    command:\s*(.+)/);
      if (mc) { result.profiles[current].command = _stripInlineComment(mc[1]); continue; }
      const mt = line.match(/^    trigger:\s*(.+)/);
      if (mt) { result.profiles[current].trigger = _stripInlineComment(mt[1]); continue; }
      const md = line.match(/^    description:\s*(.+)/);
      if (md) { result.profiles[current].description = _stripInlineComment(md[1]); continue; }
      if (line.match(/^    args:/)) { inArgs = true; continue; }
      if (inArgs) {
        const ma = line.match(/^      - (.+)/);
        if (ma) {
          result.profiles[current].args.push(_stripInlineComment(ma[1]));
          continue;
        } else { inArgs = false; }
      }
    }
  }
  return result;
}

// ---------------------------------------------------------------------------
// Email body cleaning
// ---------------------------------------------------------------------------

/**
 * Strip HTML tags to plain text, handling common email patterns.
 *
 * @param {string} html
 * @returns {string}
 */
function stripHtml(html) {
  return html
    .replace(/<style[^>]*>[\s\S]*?<\/style>/gi, '')
    .replace(/<script[^>]*>[\s\S]*?<\/script>/gi, '')
    // Convert block elements to newlines before stripping
    .replace(/<br\s*\/?>/gi, '\n')
    .replace(/<\/p>/gi, '\n')
    .replace(/<\/div>/gi, '\n')
    .replace(/<\/tr>/gi, '\n')
    .replace(/<\/li>/gi, '\n')
    .replace(/<[^>]+>/g, '')
    .replace(/&nbsp;/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/[ \t]{2,}/g, ' ')
    .replace(/\n{3,}/g, '\n\n')
    .trim();
}

/**
 * Remove Agently Mail's automatic footer injected into every sent message.
 * Pattern: "此邮件由{email}通过Agently Mail自动发送。举报退订"
 *
 * @param {string} text
 * @returns {string}
 */
function removeAgentlyFooter(text) {
  return text.replace(/\s*此邮件由[\S]+通过Agently Mail自动发送。举报退订\s*/g, '').trim();
}

/**
 * Remove quoted content from a plain-text email reply.
 *
 * Strips:
 *  1. Lines starting with ">" (standard email quoting)
 *  2. Common "On [date/time], [name] wrote:" dividers followed by quoted lines
 *  3. Common Chinese equivalents ("发件人:", "发送时间:" block headers in reply headers)
 *  4. Trailing signature separators ("-- " on its own line)
 *
 * Only the "new" text written by the sender is kept, reducing token usage
 * and avoiding confusion when passing reply threads to an AI profile.
 *
 * @param {string} text  Plain text email body
 * @returns {string}     Cleaned body with quoted sections removed
 */
function removeQuotedContent(text) {
  const lines = text.split('\n');
  const result = [];
  let inQuote = false;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const trimmed = line.trim();

    // Signature separator
    if (trimmed === '--' || trimmed === '-- ') break;

    // "On [date], [name] wrote:" pattern (English)
    // e.g. "On Thu, Jun 24, 2026 at 9:38 PM John <john@example.com> wrote:"
    if (/^On .{10,200} wrote:$/i.test(trimmed)) {
      inQuote = true;
      continue;
    }

    // Chinese "发件人:" / "From:" header blocks that precede a quoted block
    if (/^(发件人|From|发送时间|Sent|收件人|To|主题|Subject)\s*[:：]/.test(trimmed)) {
      // Look ahead — if multiple such header lines follow, it's a quoted header block
      const nextFew = lines.slice(i, i + 5).map((l) => l.trim());
      const headerCount = nextFew.filter((l) =>
        /^(发件人|From|发送时间|Sent|收件人|To|主题|Subject)\s*[:：]/.test(l),
      ).length;
      if (headerCount >= 2) {
        inQuote = true;
        continue;
      }
    }

    // Lines starting with ">" are quoted
    if (trimmed.startsWith('>')) {
      inQuote = true;
      continue;
    }

    // A non-empty, non-quoted line resets inQuote only if it comes before
    // any quoted block (don't resume after a quote section)
    if (inQuote && trimmed !== '') {
      // Allow text that clearly isn't a quote continuation to pass through,
      // but only if it appears before we've processed any real quoted lines.
      // Simple heuristic: once we've seen a ">" line, stop accepting more.
      continue;
    }

    if (!inQuote) {
      result.push(line);
    }
  }

  // Remove trailing blank lines
  while (result.length && !result[result.length - 1].trim()) result.pop();
  return result.join('\n');
}

/**
 * Truncate text to maxLength characters, appending a note when cut.
 *
 * @param {string} text
 * @param {number} maxLength
 * @returns {string}
 */
function truncate(text, maxLength) {
  if (!maxLength || text.length <= maxLength) return text;
  return text.slice(0, maxLength) + `\n\n[... 内容已截断，原始长度 ${text.length} 字符]`;
}

/**
 * Clean and normalise an email body for passing to a Profile.
 *
 * @param {object} fullMsg   Full message from AgentlyMailClient.read()
 * @param {object} [opts]
 * @param {boolean} [opts.stripQuotes=true]   Remove quoted reply content
 * @param {number}  [opts.maxLength=8000]     Truncate at this many chars (0 = no limit)
 * @returns {string}
 */
function cleanBody(fullMsg, opts = {}) {
  const { stripQuotes = true, maxLength = 8000 } = opts;

  let text = fullMsg.body_format === 'HTML'
    ? stripHtml(fullMsg.body || '')
    : (fullMsg.body || '');

  if (stripQuotes) {
    text = removeQuotedContent(text);
  }

  // Remove Agently Mail's auto-injected footer before sending to Profile
  text = removeAgentlyFooter(text);

  return truncate(text.trim(), maxLength);
}

// ---------------------------------------------------------------------------
// ProfileDispatcher
// ---------------------------------------------------------------------------

class ProfileDispatcher {
  /**
   * @param {string} configPath  Path to email-profiles.yaml
   * @param {object} [opts]
   * @param {boolean} [opts.stripQuotes=true]  Remove quoted text from replies
   * @param {number}  [opts.maxBodyLength=8000] Truncate body at N chars (0=off)
   */
  constructor(configPath, opts = {}) {
    this.configPath = configPath;
    this.config = loadProfilesConfig(configPath);
    this.configDir = path.dirname(path.resolve(configPath));
    this.stripQuotes = opts.stripQuotes !== false;
    this.maxBodyLength = opts.maxBodyLength ?? 8000;

    // Inject configDir into each profile so _spawnProfile can resolve relative paths
    for (const cfg of Object.values(this.config.profiles)) {
      cfg._configDir = this.configDir;
    }
  }

  /**
   * List configured profile names.
   * @returns {string[]}
   */
  profileNames() {
    return Object.keys(this.config.profiles);
  }

  /**
   * Resolve which Profile handles a given email subject.
   *
   * Rules (checked in order):
   *  1. Subject starts with "[tag]" → match by trigger or profile name
   *  2. Fall back to the configured default profile
   *
   * @param {string} subject
   * @returns {{ profileName: string, profileConfig: object, cleanSubject: string }}
   */
  resolveProfile(subject) {
    const m = (subject || '').match(/^\[([^\]]+)\]\s*/);
    if (m) {
      const tag = m[1].toLowerCase();
      const clean = subject.slice(m[0].length);
      for (const [name, cfg] of Object.entries(this.config.profiles)) {
        if ((cfg.trigger && cfg.trigger.toLowerCase() === tag) || name === tag) {
          return { profileName: name, profileConfig: cfg, cleanSubject: clean };
        }
      }
    }

    const defaultName = this.config.default;
    const defaultCfg = this.config.profiles[defaultName];
    if (!defaultCfg) throw new Error(`Default profile "${defaultName}" not found in config`);
    return { profileName: defaultName, profileConfig: defaultCfg, cleanSubject: subject };
  }

  /**
   * Dispatch a full email message to the appropriate Profile.
   *
   * @param {object}  fullMsg  Full message from AgentlyMailClient.read()
   * @param {boolean} dryRun   Skip Profile spawn, return placeholder
   * @returns {{ response: string, profileName: string }}
   */
  dispatch(fullMsg, dryRun = false) {
    const { subject, from } = fullMsg;
    const senderEmail = from?.email || 'unknown';
    const senderName = from?.name || senderEmail;

    // 1. Resolve profile
    const { profileName, profileConfig, cleanSubject } = this.resolveProfile(subject || '');

    // 2. Build message string (P0 ILINK_MESSAGE), cleaning up quoted content
    const body = cleanBody(fullMsg, {
      stripQuotes: this.stripQuotes,
      maxLength: this.maxBodyLength,
    });

    const message = [
      `发件人: ${senderName} <${senderEmail}>`,
      `主题: ${cleanSubject}`,
      '',
      body,
    ].join('\n');

    // 3. Load thread × profile session history
    const sid = this._sessionId(fullMsg, profileName);
    const history = _loadHistory(sid);
    const prevEntry = history.filter((e) => e.ilink_session_id).pop();
    const prevSessionId = prevEntry?.ilink_session_id || '';

    // 4. Spawn Profile with P0 protocol
    let response, newSessionId;
    try {
      ({ response, newSessionId } = this._spawnProfile(
        profileConfig, message, prevSessionId,
        `email-${senderEmail}`, senderEmail, dryRun,
      ));
    } catch (err) {
      if (prevSessionId) {
        // Session may have expired — retry with a fresh session
        process.stderr.write(
          `[dispatcher] Session ${prevSessionId} may be expired, retrying fresh\n`,
        );
        ({ response, newSessionId } = this._spawnProfile(
          profileConfig, message, '',
          `email-${senderEmail}`, senderEmail, dryRun,
        ));
      } else {
        throw err;
      }
    }

    // 5. Persist history
    _appendHistory(sid, [
      { role: 'user', content: message },
      { role: 'assistant', content: response, ilink_session_id: newSessionId },
    ]);

    return { response, profileName };
  }

  /**
   * Compute a stable session ID for a (thread × profile) pair.
   *
   * Thread grouping strategy:
   *  - If the message has a References header, use the FIRST entry (the thread root)
   *  - If only In-Reply-To is set, use that (direct parent = thread root for 1-level threads)
   *  - Otherwise use the message's own RFC Message-ID (start of a new thread)
   *
   * All replies in the same email chain therefore share one session, so the
   * AI Profile maintains conversation context across the full thread.
   *
   * @private
   */
  _sessionId(fullMsg, profileName) {
    // references[0] is the oldest (root) message in the thread per RFC 2822
    const threadRoot =
      (Array.isArray(fullMsg.references) && fullMsg.references.length > 0
        ? fullMsg.references[0]
        : null) ||
      fullMsg.in_reply_to ||
      fullMsg.rfc_message_id ||
      fullMsg.message_id ||
      'unknown';

    return `email_${profileName}_${threadRoot.replace(/[^a-zA-Z0-9_-]/g, '_').slice(0, 80)}`;
  }

  /** @private */
  _spawnProfile(cfg, message, sessionId, sessionName, fromUser, dryRun) {
    if (dryRun) {
      return {
        response: `[DRY_RUN] Profile would handle: "${message.slice(0, 80)}..."`,
        newSessionId: sessionId || 'dry-run-' + Date.now(),
      };
    }

    const configDir = cfg._configDir || __dirname;
    const args = (cfg.args || []).map((a) =>
      a.startsWith('.') ? path.resolve(configDir, a) : a,
    );

    const child = spawnSync(cfg.command, args, {
      encoding: 'utf8',
      maxBuffer: 20 * 1024 * 1024,
      timeout: 300_000,
      env: {
        ...process.env,
        ILINK_MESSAGE: message,
        ILINK_SESSION_ID: sessionId || '',
        ILINK_SESSION_NAME: sessionName || 'email',
        ILINK_FROM_USER: fromUser || '',
        ILINK_CONTEXT_TOKEN: '',
      },
    });

    if (child.error) {
      throw new Error(`Failed to spawn profile "${cfg.command}": ${child.error.message}`);
    }
    if (child.status !== 0) {
      const stderr = (child.stderr || '').trim();
      throw new Error(
        `Profile exited with code ${child.status}${stderr ? `: ${stderr}` : ''}`,
      );
    }

    // Parse P0 stdout: extract ILINK_SESSION and ILINK_PARTIAL lines
    let newSessionId = sessionId || '';
    const parts = [];
    for (const line of (child.stdout || '').split('\n')) {
      if (line.startsWith('ILINK_SESSION:')) {
        newSessionId = line.slice(14).trim();
      } else if (line.startsWith('ILINK_PARTIAL:')) {
        try { parts.push(JSON.parse(line.slice(14))); }
        catch { parts.push(line.slice(14)); }
      } else {
        parts.push(line);
      }
    }

    return { response: parts.join('\n').trim(), newSessionId };
  }
}

module.exports = { ProfileDispatcher, loadProfilesConfig, cleanBody, stripHtml, removeQuotedContent };
