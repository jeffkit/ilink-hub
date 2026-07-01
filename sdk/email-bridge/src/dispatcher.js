'use strict';
/**
 * ProfileDispatcher — Email Bridge 核心路由层
 *
 * 职责：
 *  1. 解析邮件主题前缀 [profile-name]，映射到 Profile 配置
 *  2. 维护每个（邮件线程 × Profile）的会话历史
 *  3. 用 P0 协议（ILINK_* env vars）spawn Profile 子进程
 *  4. 处理会话失效自动降级（无 session ID 重试）
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
  // Graceful fallback — session history just won't persist
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

function _parseSimpleYaml(text) {
  const result = { default: '', profiles: {} };
  let current = null;
  let inArgs = false;

  for (const line of text.split('\n')) {
    if (line.trim().startsWith('#') || !line.trim()) continue;

    const m0 = line.match(/^default:\s*(.+)/);
    if (m0) { result.default = m0[1].trim(); continue; }
    if (line.match(/^profiles:/)) continue;

    const m1 = line.match(/^  (\w+):/);
    if (m1) { current = m1[1]; result.profiles[current] = { command: '', args: [], trigger: '' }; inArgs = false; continue; }

    if (current) {
      const mc = line.match(/^    command:\s*(.+)/); if (mc) { result.profiles[current].command = mc[1].trim(); continue; }
      const mt = line.match(/^    trigger:\s*(.+)/); if (mt) { result.profiles[current].trigger = mt[1].trim(); continue; }
      const md = line.match(/^    description:\s*(.+)/); if (md) { result.profiles[current].description = md[1].trim(); continue; }
      if (line.match(/^    args:/)) { inArgs = true; continue; }
      if (inArgs) { const ma = line.match(/^      - (.+)/); if (ma) { result.profiles[current].args.push(ma[1].trim()); continue; } else { inArgs = false; } }
    }
  }
  return result;
}

// ---------------------------------------------------------------------------
// HTML → plain text
// ---------------------------------------------------------------------------

function stripHtml(html) {
  return html
    .replace(/<style[^>]*>[\s\S]*?<\/style>/gi, '')
    .replace(/<script[^>]*>[\s\S]*?<\/script>/gi, '')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&nbsp;/g, ' ').replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<').replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"').replace(/\s{2,}/g, ' ')
    .trim();
}

// ---------------------------------------------------------------------------
// ProfileDispatcher
// ---------------------------------------------------------------------------

class ProfileDispatcher {
  /**
   * @param {string} configPath  Path to email-profiles.yaml
   */
  constructor(configPath) {
    this.configPath = configPath;
    this.config = loadProfilesConfig(configPath);
    this.configDir = path.dirname(path.resolve(configPath));

    // Inject configDir into each profile so runProfile can resolve relative paths
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
   * Reads message body, resolves profile, maintains session, spawns Profile,
   * and returns the response text (caller is responsible for replying).
   *
   * @param {object} fullMsg   Full message from AgentlyMailClient.read()
   * @param {boolean} dryRun   Skip Profile spawn, return placeholder
   * @returns {{ response: string, profileName: string }}
   */
  dispatch(fullMsg, dryRun = false) {
    const { subject, from } = fullMsg;
    const senderEmail = from?.email || 'unknown';
    const senderName = from?.name || senderEmail;

    // 1. Resolve profile
    const { profileName, profileConfig, cleanSubject } = this.resolveProfile(subject || '');

    // 2. Build message string (P0 ILINK_MESSAGE)
    const body = fullMsg.body_format === 'HTML' ? stripHtml(fullMsg.body) : (fullMsg.body || '');
    const message = [
      `发件人: ${senderName} <${senderEmail}>`,
      `主题: ${cleanSubject}`,
      '',
      body,
    ].join('\n');

    // 3. Load thread × profile session
    const sid = this._sessionId(fullMsg, profileName);
    const history = _loadHistory(sid);
    const prevEntry = history.filter((e) => e.ilink_session_id).pop();
    const prevSessionId = prevEntry?.ilink_session_id || '';

    // 4. Spawn Profile
    let response, newSessionId;
    try {
      ({ response, newSessionId } = this._spawnProfile(
        profileConfig, message, prevSessionId,
        `email-${senderEmail}`, senderEmail, dryRun,
      ));
    } catch (err) {
      if (prevSessionId) {
        // Retry with fresh session
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

  /** @private */
  _sessionId(fullMsg, profileName) {
    const raw = fullMsg.rfc_message_id || fullMsg.message_id || 'unknown';
    return `email_${profileName}_${raw.replace(/[^a-zA-Z0-9_-]/g, '_').slice(0, 60)}`;
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

    if (child.error) throw new Error(`Failed to spawn profile "${cfg.command}": ${child.error.message}`);
    if (child.status !== 0) {
      const stderr = (child.stderr || '').trim();
      throw new Error(`Profile exited with code ${child.status}${stderr ? `: ${stderr}` : ''}`);
    }

    // Parse P0 stdout
    let newSessionId = sessionId || '';
    const parts = [];
    for (const line of (child.stdout || '').split('\n')) {
      if (line.startsWith('ILINK_SESSION:')) { newSessionId = line.slice(14).trim(); }
      else if (line.startsWith('ILINK_PARTIAL:')) {
        try { parts.push(JSON.parse(line.slice(14))); } catch { parts.push(line.slice(14)); }
      } else { parts.push(line); }
    }

    return { response: parts.join('\n').trim(), newSessionId };
  }
}

module.exports = { ProfileDispatcher, loadProfilesConfig };
