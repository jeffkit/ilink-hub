'use strict';
/**
 * Email Bridge — iLink Hub Email Channel Adapter
 *
 * 用邮件主题前缀 [profile-name] 路由到不同的 Profile，
 * 模拟 Hub + Bridge Manager 的行为，但触发方式是定时轮询邮箱。
 *
 * 工作流程：
 *   1. 每 POLL_INTERVAL_MS 检查未读邮件（一次 CLI 调用）
 *   2. 解析主题前缀 [profile-name]，匹配 email-profiles.yaml 配置
 *   3. 加载该 (邮件线程 × Profile) 的会话历史
 *   4. 用 P0 协议 (ILINK_* env vars) spawn Profile 子进程
 *   5. 读取 Profile stdout 中的会话 ID 和响应文本
 *   6. agently-cli reply 回复邮件
 *   7. 追加本轮对话到历史文件
 *
 * P0 协议（与 Hub → Bridge 相同）：
 *   Input  env: ILINK_MESSAGE, ILINK_SESSION_ID, ILINK_SESSION_NAME,
 *               ILINK_FROM_USER, ILINK_CONTEXT_TOKEN
 *   Output stdout: 可选 "ILINK_SESSION:<uuid>" 行，然后是回复文本
 *                  或流式 "ILINK_PARTIAL:<json>" 行
 *   Exit   0 = 成功
 *
 * 环境变量：
 *   POLL_INTERVAL_MS   轮询间隔（毫秒），默认 300000（5 分钟）
 *   PROFILES_CONFIG    profiles yaml 路径，默认 ./email-profiles.yaml
 *   DRY_RUN=1          不实际发送邮件（调试用）
 *
 * 启动：
 *   node handler.js
 *
 * 调试（30 秒轮询一次，不发送邮件）：
 *   POLL_INTERVAL_MS=30000 DRY_RUN=1 node handler.js
 */

const { spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');
const readline = require('readline');

const { AgentlyMailClient, AgentlyMailError } = require('../../sdk/node/src/agently-mail');
const { loadHistory, appendHistory } = require('../../sdk/node/src/index');

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const POLL_INTERVAL_MS = parseInt(process.env.POLL_INTERVAL_MS || '300000', 10);
const DRY_RUN = process.env.DRY_RUN === '1';
const PROFILES_CONFIG = process.env.PROFILES_CONFIG
  || path.join(__dirname, 'email-profiles.yaml');

// ---------------------------------------------------------------------------
// YAML loader (minimal, only handles the structure we need)
// ---------------------------------------------------------------------------

/**
 * Parse the email-profiles.yaml without requiring an external yaml package.
 * Supports our specific schema: top-level "default" string and "profiles" object.
 *
 * @param {string} filePath
 * @returns {{ default: string, profiles: Record<string, { command: string, args: string[], trigger?: string, description?: string }> }}
 */
function loadProfilesConfig(filePath) {
  // Try to use js-yaml if available; otherwise fall back to a tiny inline parser
  try {
    // eslint-disable-next-line import/no-extraneous-dependencies
    const yaml = require('js-yaml');
    return yaml.load(fs.readFileSync(filePath, 'utf8'));
  } catch (_e) {
    // Inline parser — handles simple indented blocks, not a full YAML parser
    return parseSimpleYaml(fs.readFileSync(filePath, 'utf8'));
  }
}

function parseSimpleYaml(text) {
  const result = { default: '', profiles: {} };
  const lines = text.split('\n');
  let currentProfile = null;
  let inArgs = false;

  for (const line of lines) {
    if (line.trim().startsWith('#') || !line.trim()) continue;

    const defaultMatch = line.match(/^default:\s*(.+)/);
    if (defaultMatch) { result.default = defaultMatch[1].trim(); continue; }

    const profileSection = line.match(/^profiles:/);
    if (profileSection) continue;

    const profileName = line.match(/^  (\w+):/);
    if (profileName) {
      currentProfile = profileName[1];
      result.profiles[currentProfile] = { command: '', args: [], trigger: '' };
      inArgs = false;
      continue;
    }

    if (currentProfile) {
      const cmd = line.match(/^    command:\s*(.+)/);
      if (cmd) { result.profiles[currentProfile].command = cmd[1].trim(); continue; }

      const trigger = line.match(/^    trigger:\s*(.+)/);
      if (trigger) { result.profiles[currentProfile].trigger = trigger[1].trim(); continue; }

      const desc = line.match(/^    description:\s*(.+)/);
      if (desc) { result.profiles[currentProfile].description = desc[1].trim(); continue; }

      if (line.match(/^    args:/)) { inArgs = true; continue; }

      if (inArgs) {
        const argItem = line.match(/^      - (.+)/);
        if (argItem) { result.profiles[currentProfile].args.push(argItem[1].trim()); continue; }
        else { inArgs = false; }
      }
    }
  }
  return result;
}

// ---------------------------------------------------------------------------
// Profile routing — parse subject prefix [profile-name]
// ---------------------------------------------------------------------------

/**
 * @param {string} subject
 * @param {object} profilesConfig
 * @returns {{ profileName: string, profileConfig: object, cleanSubject: string }}
 */
function resolveProfile(subject, profilesConfig) {
  const prefixMatch = subject.match(/^\[([^\]]+)\]\s*/);
  if (prefixMatch) {
    const tag = prefixMatch[1].toLowerCase();
    const cleanSubject = subject.slice(prefixMatch[0].length);

    // Match by trigger or by profile name
    for (const [name, cfg] of Object.entries(profilesConfig.profiles)) {
      if ((cfg.trigger && cfg.trigger.toLowerCase() === tag) || name === tag) {
        return { profileName: name, profileConfig: cfg, cleanSubject };
      }
    }
  }

  // Fall back to default
  const defaultName = profilesConfig.default;
  const defaultCfg = profilesConfig.profiles[defaultName];
  if (!defaultCfg) {
    throw new Error(`Default profile "${defaultName}" not found in config`);
  }
  return { profileName: defaultName, profileConfig: defaultCfg, cleanSubject: subject };
}

// ---------------------------------------------------------------------------
// P0 protocol — spawn Profile subprocess
// ---------------------------------------------------------------------------

/**
 * Spawn a Profile process using the P0 exec protocol.
 *
 * @param {object} profileConfig   { command, args, _configDir }
 * @param {string} message         ILINK_MESSAGE
 * @param {string} sessionId       ILINK_SESSION_ID (may be empty)
 * @param {string} sessionName     ILINK_SESSION_NAME
 * @param {string} fromUser        ILINK_FROM_USER
 * @returns {{ response: string, newSessionId: string }}
 */
function runProfile(profileConfig, message, sessionId, sessionName, fromUser) {
  if (DRY_RUN) {
    return {
      response: `[DRY_RUN] Profile would handle: "${message.slice(0, 80)}..."`,
      newSessionId: sessionId || 'dry-run-' + Date.now(),
    };
  }

  // Resolve relative paths in args against the config file's directory
  const configDir = profileConfig._configDir || __dirname;
  const cmdArgs = (profileConfig.args || []).map((a) =>
    a.startsWith('.') ? path.resolve(configDir, a) : a,
  );

  const child = spawnSync(profileConfig.command, cmdArgs, {
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
    throw new Error(`Failed to spawn profile "${profileConfig.command}": ${child.error.message}`);
  }
  if (child.status !== 0) {
    const stderr = (child.stderr || '').trim();
    throw new Error(
      `Profile exited with code ${child.status}${stderr ? `: ${stderr}` : ''}`,
    );
  }

  // Parse P0 stdout
  const lines = (child.stdout || '').split('\n');
  let newSessionId = sessionId || '';
  const responseLines = [];

  for (const line of lines) {
    if (line.startsWith('ILINK_SESSION:')) {
      newSessionId = line.slice('ILINK_SESSION:'.length).trim();
    } else if (line.startsWith('ILINK_PARTIAL:')) {
      // Partial chunks — collect them as final response
      try {
        responseLines.push(JSON.parse(line.slice('ILINK_PARTIAL:'.length)));
      } catch {
        responseLines.push(line.slice('ILINK_PARTIAL:'.length));
      }
    } else {
      responseLines.push(line);
    }
  }

  const response = responseLines.join('\n').trim();
  return { response, newSessionId };
}

// ---------------------------------------------------------------------------
// HTML to plain text (minimal)
// ---------------------------------------------------------------------------

function stripHtml(html) {
  return html
    .replace(/<style[^>]*>[\s\S]*?<\/style>/gi, '')
    .replace(/<script[^>]*>[\s\S]*?<\/script>/gi, '')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&nbsp;/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/\s{2,}/g, ' ')
    .trim();
}

// ---------------------------------------------------------------------------
// Session ID helper
// ---------------------------------------------------------------------------

function threadSessionId(fullMsg, profileName) {
  const raw = fullMsg.rfc_message_id || fullMsg.message_id || 'unknown';
  const safe = raw.replace(/[^a-zA-Z0-9_-]/g, '_').slice(0, 60);
  return `email_${profileName}_${safe}`;
}

// ---------------------------------------------------------------------------
// Main message handler
// ---------------------------------------------------------------------------

/**
 * Process one unread email: read → resolve profile → spawn → reply → log.
 *
 * @param {object} msgSummary
 * @param {AgentlyMailClient} mail
 * @param {object} profilesConfig
 */
async function processEmail(msgSummary, mail, profilesConfig) {
  const { message_id, subject, from } = msgSummary;
  const senderEmail = from?.email || 'unknown';
  const senderName = from?.name || senderEmail;

  process.stderr.write(
    `[email-bridge] Processing: "${subject}" from ${senderEmail} (${message_id})\n`,
  );

  // 1. Read full message body
  const fullMsg = mail.read(message_id);
  const rawBody = fullMsg.body_format === 'HTML'
    ? stripHtml(fullMsg.body)
    : fullMsg.body;

  // 2. Resolve which Profile to use
  const { profileName, profileConfig, cleanSubject } = resolveProfile(
    subject || '',
    profilesConfig,
  );
  process.stderr.write(`[email-bridge] → Profile: ${profileName}\n`);

  // 3. Build message for the Profile
  const message = [
    `发件人: ${senderName} <${senderEmail}>`,
    `主题: ${cleanSubject}`,
    '',
    rawBody,
  ].join('\n');

  // 4. Load session history for this (thread × profile) pair
  const sid = threadSessionId(fullMsg, profileName);
  const history = loadHistory(sid);
  const lastEntry = history.filter((e) => e.ilink_session_id).pop();
  const prevSessionId = lastEntry?.ilink_session_id || '';

  // 5. Run the Profile (P0 protocol)
  let response, newSessionId;
  try {
    ({ response, newSessionId } = runProfile(
      profileConfig,
      message,
      prevSessionId,
      `email-${senderEmail}`,
      senderEmail,
    ));
  } catch (err) {
    // If session expired, retry without session ID
    if (prevSessionId) {
      process.stderr.write(
        `[email-bridge] Session ${prevSessionId} may be expired, retrying fresh\n`,
      );
      ({ response, newSessionId } = runProfile(
        profileConfig,
        message,
        '',
        `email-${senderEmail}`,
        senderEmail,
      ));
    } else {
      throw err;
    }
  }

  process.stderr.write(
    `[email-bridge] Profile response (${response.length} chars): ${response.slice(0, 80)}...\n`,
  );

  // 6. Reply via email
  if (!DRY_RUN) {
    mail.reply(message_id, response);
  } else {
    process.stderr.write(`[email-bridge][DRY_RUN] Would reply: ${response.slice(0, 120)}\n`);
  }

  // 7. Update session history
  appendHistory(sid, [
    { role: 'user', content: message },
    {
      role: 'assistant',
      content: response,
      ilink_session_id: newSessionId,
    },
  ]);

  process.stderr.write(`[email-bridge] Done: ${message_id}\n`);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

function main() {
  // Load profiles config
  if (!fs.existsSync(PROFILES_CONFIG)) {
    process.stderr.write(
      `[email-bridge] Profiles config not found: ${PROFILES_CONFIG}\n`,
    );
    process.exit(1);
  }
  const profilesConfig = loadProfilesConfig(PROFILES_CONFIG);
  // Inject config dir so runProfile can resolve relative paths
  const configDir = path.dirname(path.resolve(PROFILES_CONFIG));
  for (const cfg of Object.values(profilesConfig.profiles)) {
    cfg._configDir = configDir;
  }
  const profileNames = Object.keys(profilesConfig.profiles);
  process.stderr.write(
    `[email-bridge] Loaded ${profileNames.length} profile(s): ${profileNames.join(', ')}\n`,
  );

  // Init mail client and verify auth
  const mail = new AgentlyMailClient();
  try {
    const me = mail.me();
    const email = me?.aliases?.[0]?.email || 'unknown';
    process.stderr.write(
      `[email-bridge] Monitoring ${email} every ${POLL_INTERVAL_MS / 1000}s\n` +
      `[email-bridge] Subject prefix routing: [profile-name] prefix, default=${profilesConfig.default}\n`,
    );
  } catch (err) {
    process.stderr.write(
      `[email-bridge] Auth check failed: ${err.message}\n` +
      `  Run: agently-cli auth login\n`,
    );
    process.exit(3);
  }

  // Start polling
  const poller = mail.poll(
    POLL_INTERVAL_MS,
    (msg, client) => processEmail(msg, client, profilesConfig),
    { limit: 20 },
  );

  // Graceful shutdown
  process.on('SIGINT', () => {
    process.stderr.write('\n[email-bridge] Stopping...\n');
    poller.stop();
    process.exit(0);
  });
  process.on('SIGTERM', () => {
    poller.stop();
    process.exit(0);
  });
}

main();
