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

/**
 * Start the email bridge daemon.
 *
 * @param {object} [options]
 * @param {string}  [options.profilesConfig]  Path to email-profiles.yaml
 * @param {number}  [options.pollIntervalMs]  Poll interval in ms (default 300_000)
 * @param {boolean} [options.dryRun]          Skip actual replies (default false)
 * @param {number}  [options.limit]           Max unread per poll cycle (default 20)
 * @returns {{ stop: () => void }}
 */
function createEmailBridge(options = {}) {
  const {
    profilesConfig = require('path').join(process.cwd(), 'email-profiles.yaml'),
    pollIntervalMs = 300_000,
    dryRun = process.env.DRY_RUN === '1',
    limit = 20,
  } = options;

  const mail = new AgentlyMailClient();
  const dispatcher = new ProfileDispatcher(profilesConfig);

  const profileNames = dispatcher.profileNames();
  process.stderr.write(
    `[email-bridge] Loaded ${profileNames.length} profile(s): ${profileNames.join(', ')}\n`,
  );

  // Verify auth
  try {
    const me = mail.me();
    const email = me?.aliases?.[0]?.email || 'unknown';
    process.stderr.write(
      `[email-bridge] Monitoring ${email} every ${pollIntervalMs / 1000}s\n` +
      `[email-bridge] Subject prefix routing: [profile-name], default=${dispatcher.config.default}\n`,
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
  process.on('SIGINT', () => { process.stderr.write('\n[email-bridge] Stopping...\n'); poller.stop(); process.exit(0); });
  process.on('SIGTERM', () => { poller.stop(); process.exit(0); });

  return poller;
}

module.exports = { AgentlyMailClient, AgentlyMailError, ProfileDispatcher, createEmailBridge };
