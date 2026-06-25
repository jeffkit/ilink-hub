'use strict';
/**
 * ilink-email-bridge — 主入口
 *
 * 暴露三个核心模块：
 *  - AgentlyMailClient  邮件收发 SDK（agently-cli subprocess 封装）
 *  - ProfileDispatcher  邮件到 Profile 的路由 + 会话管理层
 *  - createEmailBridge  一键启动函数（顶层 API）
 *
 * @example
 * const { createEmailBridge } = require('ilink-email-bridge');
 *
 * createEmailBridge({
 *   profilesConfig: './email-profiles.yaml',
 *   pollIntervalMs: 5 * 60_000,
 * });
 */

const { AgentlyMailClient, AgentlyMailError } = require('./agently-mail');
const { ProfileDispatcher } = require('./dispatcher');

// Re-export createProfile from ilink-bridge-profile (or a minimal fallback)
let _createProfile;
try {
  ({ createProfile: _createProfile } = require('ilink-bridge-profile'));
} catch {
  // Minimal standalone fallback when ilink-bridge-profile is not installed
  _createProfile = function createProfile(handler) {
    const ctx = {
      message: process.env.ILINK_MESSAGE || '',
      sessionId: process.env.ILINK_SESSION_ID || '',
      sessionName: process.env.ILINK_SESSION_NAME || 'email',
      fromUser: process.env.ILINK_FROM_USER || '',
      contextToken: process.env.ILINK_CONTEXT_TOKEN || '',
      sendPartial(text) {
        process.stdout.write(`ILINK_PARTIAL:${JSON.stringify(text)}\n`);
      },
    };
    Promise.resolve()
      .then(() => handler(ctx))
      .then((result) => {
        const response = typeof result === 'string' ? result : (result?.response || '');
        const newSid = typeof result === 'object' ? result?.sessionId : undefined;
        if (newSid) process.stdout.write(`ILINK_SESSION:${newSid}\n`);
        process.stdout.write(response);
        process.exit(0);
      })
      .catch((err) => {
        process.stderr.write(`[ilink-email-bridge/profile] error: ${err?.stack || err}\n`);
        process.exit(1);
      });
  };
}
const createProfile = _createProfile;

// ---------------------------------------------------------------------------
// Self-email filter helpers
// ---------------------------------------------------------------------------

/**
 * Collect all email addresses belonging to the authenticated account.
 * Includes all aliases to guard against edge cases.
 *
 * @param {AgentlyMailClient} mail
 * @returns {Set<string>}  lowercase email addresses
 */
function getOwnAddresses(mail) {
  try {
    const me = mail.me();
    const addresses = new Set();
    for (const alias of (me?.aliases || [])) {
      if (alias.email) addresses.add(alias.email.toLowerCase());
    }
    return addresses;
  } catch {
    return new Set();
  }
}

/**
 * Return true if the message was sent by ourselves.
 * Catches two common self-loop patterns:
 *  1. from.email matches our own address (reply-to-self or Echo-generated)
 *  2. Subject starts with "Re:" AND sender is us (our own reply came back as unread)
 *
 * @param {object}      msgSummary   From +list
 * @param {Set<string>} ownAddresses
 * @returns {boolean}
 */
function isSelfSent(msgSummary, ownAddresses) {
  const senderEmail = (msgSummary.from?.email || '').toLowerCase();
  return ownAddresses.has(senderEmail);
}

// ---------------------------------------------------------------------------
// createEmailBridge
// ---------------------------------------------------------------------------

/**
 * Start the email bridge daemon.
 *
 * @param {object}  [options]
 * @param {string}  [options.profilesConfig]   Path to email-profiles.yaml
 * @param {number}  [options.pollIntervalMs]   Poll interval in ms (default 300_000)
 * @param {boolean} [options.dryRun]           Skip actual replies (default false)
 * @param {number}  [options.limit]            Max unread per poll cycle (default 20)
 * @param {boolean} [options.filterSelfSent]   Skip emails sent by our own address (default true)
 * @returns {{ stop: () => void }}
 */
function createEmailBridge(options = {}) {
  const {
    profilesConfig = require('path').join(process.cwd(), 'email-profiles.yaml'),
    pollIntervalMs = 300_000,
    dryRun = process.env.DRY_RUN === '1',
    limit = 20,
    filterSelfSent = true,
  } = options;

  const mail = new AgentlyMailClient();
  const dispatcher = new ProfileDispatcher(profilesConfig);

  const profileNames = dispatcher.profileNames();
  process.stderr.write(
    `[email-bridge] Loaded ${profileNames.length} profile(s): ${profileNames.join(', ')}\n`,
  );

  // Verify auth and collect own addresses for self-filter
  let ownAddresses = new Set();
  try {
    const me = mail.me();
    const email = me?.aliases?.[0]?.email || 'unknown';
    ownAddresses = getOwnAddresses(mail);
    process.stderr.write(
      `[email-bridge] Monitoring ${email} every ${pollIntervalMs / 1000}s\n` +
      `[email-bridge] Subject prefix routing: [profile-name], default=${dispatcher.config.default}\n` +
      (filterSelfSent ? `[email-bridge] Self-sent filter: ON (${[...ownAddresses].join(', ')})\n` : ''),
    );
  } catch (err) {
    process.stderr.write(
      `[email-bridge] Auth check failed: ${err.message}\n` +
      `  Run: agently-cli auth login\n`,
    );
    process.exit(3);
  }

  const poller = mail.poll(pollIntervalMs, async (msg, client) => {
    const { message_id, subject, from } = msg;

    // Skip emails we sent ourselves (prevents reply loops)
    if (filterSelfSent && isSelfSent(msg, ownAddresses)) {
      process.stderr.write(
        `[email-bridge] Skipping self-sent: "${subject}" (${message_id})\n`,
      );
      return;
    }

    process.stderr.write(
      `[email-bridge] Processing: "${subject}" from ${from?.email || '?'} (${message_id})\n`,
    );

    const fullMsg = client.read(message_id);
    const { response, profileName } = dispatcher.dispatch(fullMsg, dryRun);

    process.stderr.write(
      `[email-bridge] Profile: ${profileName} → ${response.length} chars\n`,
    );

    if (!dryRun) {
      client.reply(message_id, response);
      process.stderr.write(`[email-bridge] Replied: ${message_id}\n`);
    } else {
      process.stderr.write(`[email-bridge][DRY_RUN] Would reply: ${response.slice(0, 120)}\n`);
    }
  }, { limit });

  // Graceful shutdown
  process.on('SIGINT', () => {
    process.stderr.write('\n[email-bridge] Stopping...\n');
    poller.stop();
    process.exit(0);
  });
  process.on('SIGTERM', () => { poller.stop(); process.exit(0); });

  return poller;
}

module.exports = {
  AgentlyMailClient,
  AgentlyMailError,
  ProfileDispatcher,
  createEmailBridge,
  createProfile,
};
