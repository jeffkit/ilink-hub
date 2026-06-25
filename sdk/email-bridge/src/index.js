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

const fs = require('fs');
const os = require('os');
const path = require('path');

const { AgentlyMailClient, AgentlyMailError } = require('./agently-mail');
const { ProfileDispatcher, convertMarkdownToHtml } = require('./dispatcher');
const { PendingStore } = require('./pending-store');
const { AclConfig } = require('./acl-config');
const { SenderAcl } = require('./sender-acl');
const { DeniedLog } = require('./denied-log');
const { AdminHandler } = require('./admin-handler');

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
 * @param {string}  [options.aclConfig]        Path to email-acl.yaml (optional)
 * @param {number}  [options.pollIntervalMs]   Poll interval in ms (default 300_000)
 * @param {boolean} [options.dryRun]           Skip actual replies (default false)
 * @param {number}  [options.limit]            Max messages per poll cycle (default 20)
 * @param {boolean} [options.filterSelfSent]   Skip emails sent by our own address (default true)
 * @param {string}  [options.pendingStoreFile] Custom path for pending state JSON file
 * @returns {{ stop: () => void }}
 */
function createEmailBridge(options = {}) {
  const {
    profilesConfig = path.join(process.cwd(), 'email-profiles.yaml'),
    aclConfig: aclConfigFile = (() => {
      const candidate = path.join(process.cwd(), 'email-acl.yaml');
      return fs.existsSync(candidate) ? candidate : null;
    })(),
    pollIntervalMs = 300_000,
    dryRun = process.env.DRY_RUN === '1',
    limit = 20,
    filterSelfSent = true,
    pendingStoreFile,
  } = options;

  // Cursor file lives alongside the pending store for persistence across restarts
  const storeDir = pendingStoreFile
    ? path.dirname(pendingStoreFile)
    : path.join(os.homedir(), '.ilink-email-bridge');
  const cursorFile = path.join(storeDir, 'poll-cursor.json');

  function loadCursor() {
    try {
      if (fs.existsSync(cursorFile)) {
        const { afterTimestamp } = JSON.parse(fs.readFileSync(cursorFile, 'utf8'));
        if (afterTimestamp) return afterTimestamp;
      }
    } catch { /* ignore */ }
    return null;
  }

  function saveCursor(ts) {
    try {
      fs.mkdirSync(storeDir, { recursive: true });
      fs.writeFileSync(cursorFile, JSON.stringify({ afterTimestamp: ts }, null, 2), 'utf8');
    } catch { /* ignore */ }
  }

  const mail       = new AgentlyMailClient();
  const dispatcher = new ProfileDispatcher(profilesConfig);
  const pending    = new PendingStore(pendingStoreFile);
  const aclCfg     = new AclConfig({
    aclConfigFile,
    dynamicFile: pendingStoreFile
      ? path.join(path.dirname(pendingStoreFile), 'acl-dynamic.json')
      : undefined,
  });
  const acl        = new SenderAcl(aclCfg);
  const deniedLog  = new DeniedLog(
    pendingStoreFile
      ? path.join(path.dirname(pendingStoreFile), 'denied-log.json')
      : undefined,
  );
  const admin      = new AdminHandler(aclCfg, deniedLog, mail, { dryRun });

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
    const adminList = aclCfg.adminSenders;
    process.stderr.write(
      `[email-bridge] Monitoring ${email} every ${pollIntervalMs / 1000}s\n` +
      `[email-bridge] Subject prefix routing: [profile-name], default=${dispatcher.config.default}\n` +
      (filterSelfSent ? `[email-bridge] Self-sent filter: ON (${[...ownAddresses].join(', ')})\n` : '') +
      (acl.isOpenAccess() ? '' : `[email-bridge] Sender ACL: ON (deny_action=${acl.denyAction})\n`) +
      (adminList.length > 0 ? `[email-bridge] Admin senders: ${adminList.join(', ')}\n` : '') +
      (aclConfigFile ? `[email-bridge] ACL config: ${aclConfigFile}\n` : '[email-bridge] ACL config: (none — open access)\n'),
    );
  } catch (err) {
    process.stderr.write(
      `[email-bridge] Auth check failed: ${err.message}\n` +
      `  Run: agently-cli auth login\n`,
    );
    process.exit(3);
  }

  /**
   * Handle a sender that failed ACL checks: log it and optionally notify.
   */
  async function handleDenied(client, msg, reason) {
    const { message_id, subject, from } = msg;
    process.stderr.write(
      `[email-bridge] ACL denied: "${subject}" from ${from?.email} — ${reason}\n`,
    );

    deniedLog.record(msg, reason);

    // Mark as done in pending store to prevent retry sweep re-processing
    pending.add(msg);
    pending.markReplied(message_id);

    if (acl.denyAction === 'notify' && !dryRun) {
      const body = acl.denyMessage ||
        '感谢您的来信。您的邮件无法被自动处理，请联系管理员。\n\nThank you for your message. Your email could not be processed automatically. Please contact the administrator.';
      try {
        client.reply(message_id, body, { bodyFormat: 'plain' });
        process.stderr.write(`[email-bridge] ACL deny notification sent: ${message_id}\n`);
      } catch (err) {
        process.stderr.write(`[email-bridge] ACL notify reply failed: ${err.message}\n`);
      }
    }
  }

  // In-memory guard: tracks message IDs currently being dispatched.
  // Prevents retry sweep from launching a second dispatch while the poll
  // handler's dispatchAndReply is still running (e.g. waiting for Claude).
  const processingSet = new Set();

  /**
   * Core dispatch-and-reply logic, shared between new mail handler and retry handler.
   * Returns true on success, false on failure.
   */
  async function dispatchAndReply(message_id, subject, fromEmail, client, isRetry = false) {
    if (processingSet.has(message_id)) {
      process.stderr.write(
        `[email-bridge] Skipping duplicate dispatch for ${message_id} (already in progress)\n`,
      );
      return true;
    }
    processingSet.add(message_id);
    try {
      const tag = isRetry ? '[RETRY]' : '';
      process.stderr.write(
        `[email-bridge]${tag} Processing: "${subject}" from ${fromEmail} (${message_id})\n`,
      );

      let fullMsg;
      try {
        fullMsg = client.read(message_id);
      } catch (err) {
        process.stderr.write(`[email-bridge]${tag} Failed to read ${message_id}: ${err.message}\n`);
        pending.markFailed(message_id, `read failed: ${err.message}`);
        return false;
      }

      // Resolve profile first so we can run per-profile ACL check
      let resolvedProfile;
      try {
        resolvedProfile = dispatcher.resolveProfile(fullMsg.subject || '');
      } catch (err) {
        process.stderr.write(`[email-bridge]${tag} Profile resolution failed: ${err.message}\n`);
        pending.markFailed(message_id, `profile resolve failed: ${err.message}`);
        return false;
      }

      // Per-profile ACL check (global ACL already passed in poll handler)
      if (acl.checkProfile(resolvedProfile.profileName, fromEmail) === 'deny') {
        process.stderr.write(
          `[email-bridge]${tag} ACL denied profile "${resolvedProfile.profileName}" for ${fromEmail}\n`,
        );
        // Build a minimal msg-like object for handleDenied (retry path may not have full msg)
        const msgSummary = { message_id, subject, from: { email: fromEmail } };
        await handleDenied(client, msgSummary, `profile "${resolvedProfile.profileName}" not allowed`);
        return true; // treated as "handled", not a retriable failure
      }

      let response, profileName;
      try {
        ({ response, profileName } = dispatcher.dispatch(fullMsg, dryRun));
      } catch (err) {
        process.stderr.write(`[email-bridge]${tag} Dispatch failed for ${message_id}: ${err.message}\n`);
        pending.markFailed(message_id, `dispatch failed: ${err.message}`);
        return false;
      }

      process.stderr.write(
        `[email-bridge]${tag} Profile: ${profileName} → ${response.length} chars\n`,
      );

      if (!dryRun) {
        try {
          const htmlResponse = convertMarkdownToHtml(response);
          client.reply(message_id, htmlResponse, { bodyFormat: 'html' });
          pending.markReplied(message_id);
          process.stderr.write(`[email-bridge]${tag} Replied (HTML): ${message_id}\n`);
        } catch (err) {
          process.stderr.write(`[email-bridge]${tag} Reply failed for ${message_id}: ${err.message}\n`);
          pending.markFailed(message_id, `reply failed: ${err.message}`);
          return false;
        }
      } else {
        pending.markReplied(message_id);
        process.stderr.write(`[email-bridge][DRY_RUN] Would reply: ${response.slice(0, 120)}\n`);
      }
      return true;
    } finally {
      processingSet.delete(message_id);
    }
  }

  const savedCursor = loadCursor();
  if (savedCursor) {
    process.stderr.write(`[email-bridge] Resuming from cursor: ${savedCursor}\n`);
  }

  const poller = mail.poll(pollIntervalMs, async (msg, client) => {
    const { message_id, subject, from } = msg;
    const senderEmail = from?.email || '';

    // Skip emails we sent ourselves (prevents reply loops)
    if (filterSelfSent && isSelfSent(msg, ownAddresses)) {
      process.stderr.write(
        `[email-bridge] Skipping self-sent: "${subject}" (${message_id})\n`,
      );
      return;
    }

    // Admin path: read message, check for commands, bypass normal ACL + dispatch
    if (acl.isAdmin(senderEmail)) {
      process.stderr.write(`[email-bridge] Admin message from ${senderEmail}: "${subject}"\n`);
      let fullMsg;
      try { fullMsg = client.read(message_id); } catch { fullMsg = null; }
      const body = fullMsg ? _plainBody(fullMsg) : '';
      if (admin.hasCommands(body)) {
        await admin.executeCommands(message_id, body, senderEmail);
        return;
      }
      // Admin with no commands → fall through to normal dispatch
    }

    // Global ACL check (non-admin senders)
    if (!acl.isAdmin(senderEmail) && acl.checkGlobal(senderEmail) === 'deny') {
      await handleDenied(client, msg, 'global ACL');
      return;
    }

    // Register in pending store BEFORE reading (read() marks as read on server side)
    pending.add(msg);

    await dispatchAndReply(message_id, subject, senderEmail, client, false);

  }, { limit, afterTimestamp: savedCursor || undefined, saveCursor });

  // Retry sweep: runs on every poll interval even when inbox is empty.
  // Delayed by half an interval so it doesn't fire simultaneously with the
  // main poll, spreading API calls evenly and reducing burst rate.
  let retryTimer = null;
  const runRetrySweep = async () => {
    const retryQueue = pending.getPending();
    if (retryQueue.length === 0) {
      pending.cleanup();
      return;
    }
    process.stderr.write(`[email-bridge] Retry sweep: ${retryQueue.length} pending message(s)\n`);
    const client = mail;
    for (const entry of retryQueue) {
      await dispatchAndReply(entry.message_id, entry.subject, entry.from_email, client, true);
    }
    pending.cleanup();
  };
  // Start retry sweep after half-interval offset to avoid simultaneous poll+retry bursts
  setTimeout(() => {
    runRetrySweep();
    retryTimer = setInterval(runRetrySweep, pollIntervalMs);
  }, Math.floor(pollIntervalMs / 2));

  // Start inspection report scheduler
  admin.startReportScheduler();

  // Graceful shutdown
  const stop = () => {
    poller.stop();
    admin.stopReportScheduler();
    if (retryTimer) clearInterval(retryTimer);
  };
  process.on('SIGINT', () => {
    process.stderr.write('\n[email-bridge] Stopping...\n');
    stop();
    process.exit(0);
  });
  process.on('SIGTERM', () => { stop(); process.exit(0); });

  return { stop };
}

/** Extract plain text from a full message (strips HTML + quoted content). */
function _plainBody(fullMsg) {
  const { cleanBody } = require('./dispatcher');
  try { return cleanBody(fullMsg, { stripQuotes: true }); } catch { return ''; }
}

module.exports = {
  AgentlyMailClient,
  AgentlyMailError,
  ProfileDispatcher,
  PendingStore,
  AclConfig,
  SenderAcl,
  DeniedLog,
  AdminHandler,
  createEmailBridge,
  createProfile,
  convertMarkdownToHtml,
};
