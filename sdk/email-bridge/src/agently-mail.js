'use strict';
/**
 * AgentlyMailClient — Node.js wrapper around the `agently-cli` binary.
 *
 * All operations spawn `agently-cli` as a child process and parse the
 * structured JSON output.  Write operations (send / reply / forward / trash)
 * implement the two-phase confirmation protocol automatically: the first call
 * returns a confirmation token; the client then re-runs with that token and
 * resolves only after the server confirms success.
 *
 * @example
 * const { AgentlyMailClient } = require('ilink-bridge-profile');
 * const mail = new AgentlyMailClient();
 *
 * // Poll every 5 minutes, process each unread message
 * mail.poll(5 * 60_000, async (msg, client) => {
 *   const full = await client.read(msg.message_id);
 *   const reply = await myAI(full.body);
 *   await client.reply(msg.message_id, reply);
 * });
 */

const { spawnSync } = require('child_process');

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

class AgentlyMailError extends Error {
  /**
   * @param {string} message
   * @param {number} exitCode
   * @param {string} [serverMessage]
   */
  constructor(message, exitCode, serverMessage) {
    super(message);
    this.name = 'AgentlyMailError';
    this.exitCode = exitCode;
    this.serverMessage = serverMessage;
  }
}

// ---------------------------------------------------------------------------
// Low-level CLI runner
// ---------------------------------------------------------------------------

/**
 * Run `agently-cli <args>` synchronously and return parsed JSON data.
 * Throws AgentlyMailError on non-zero exit codes.
 *
 * @param {string[]} args
 * @returns {unknown} data field from the JSON envelope
 */
function runCli(args) {
  const result = spawnSync('agently-cli', args, {
    encoding: 'utf8',
    maxBuffer: 10 * 1024 * 1024,
  });

  if (result.error) {
    throw new AgentlyMailError(
      `Failed to spawn agently-cli: ${result.error.message}`,
      -1,
    );
  }

  const exitCode = result.status ?? -1;
  let envelope;
  try {
    envelope = JSON.parse(result.stdout || '{}');
  } catch {
    throw new AgentlyMailError(
      `agently-cli returned non-JSON output (exit ${exitCode}): ${result.stdout}`,
      exitCode,
    );
  }

  if (exitCode !== 0) {
    const msg =
      envelope?.error?.message || envelope?.message || `exit code ${exitCode}`;
    throw new AgentlyMailError(
      `agently-cli error (exit ${exitCode}): ${msg}`,
      exitCode,
      msg,
    );
  }

  return envelope.data;
}

// ---------------------------------------------------------------------------
// Two-phase confirmation helper
// ---------------------------------------------------------------------------

/**
 * Execute a write command that requires two-phase confirmation.
 * First call returns a confirmation_token; we automatically re-run with it.
 *
 * @param {string[]} args  CLI args WITHOUT --confirmation-token
 * @returns {unknown} final data from the confirmed call
 */
function runConfirmed(args) {
  // Phase 1 — get confirmation token
  const phase1 = runCli(args);

  // Some commands may succeed without confirmation (e.g. dry-run)
  if (!phase1?.confirmation_token) {
    return phase1;
  }

  // Phase 2 — confirm with token
  return runCli([...args, '--confirmation-token', phase1.confirmation_token]);
}

// ---------------------------------------------------------------------------
// AgentlyMailClient
// ---------------------------------------------------------------------------

class AgentlyMailClient {
  // -------------------------------------------------------------------------
  // Read operations
  // -------------------------------------------------------------------------

  /**
   * List messages in a folder.
   *
   * @param {object} [options]
   * @param {'inbox'|'sent'|'trash'|'spam'} [options.dir='inbox']
   * @param {number} [options.limit=10]
   * @param {string} [options.cursor]
   * @param {string} [options.after]   ISO date string
   * @param {string} [options.before]  ISO date string
   * @param {boolean} [options.hasAttachments]
   * @param {boolean} [options.isUnread]
   * @returns {{ messages: object[], pagination: object }}
   */
  list(options = {}) {
    const args = ['message', '+list'];
    if (options.dir) args.push('--dir', options.dir);
    if (options.limit != null) args.push('--limit', String(options.limit));
    if (options.cursor) args.push('--cursor', options.cursor);
    if (options.after) args.push('--after', options.after);
    if (options.before) args.push('--before', options.before);
    if (options.hasAttachments) args.push('--has-attachments');
    if (options.isUnread) args.push('--is-unread');
    const data = runCli(args);
    return { messages: data?.data ?? [], pagination: data?.pagination ?? {} };
  }

  /**
   * List only unread messages in the inbox.
   *
   * @param {number} [limit=20]
   * @returns {object[]}
   */
  listUnread(limit = 20) {
    return this.list({ isUnread: true, limit }).messages;
  }

  /**
   * Read a single message in full (body + attachments).
   *
   * @param {string} messageId  msg_xxx
   * @returns {object}
   */
  read(messageId) {
    return runCli(['message', '+read', '--id', messageId]);
  }

  /**
   * Search messages with keyword and optional filters.
   *
   * @param {string} query
   * @param {object} [options]
   * @param {'SEARCH_IN_ALL'|'SEARCH_IN_SUBJECT'|'SEARCH_IN_CONTENT'} [options.searchIn]
   * @param {string} [options.from]
   * @param {string} [options.to]
   * @param {'inbox'|'sent'|'trash'|'spam'} [options.dir]
   * @param {string} [options.after]
   * @param {string} [options.before]
   * @param {boolean} [options.hasAttachments]
   * @param {boolean} [options.isUnread]
   * @param {number} [options.limit]
   * @param {string} [options.cursor]
   * @returns {{ messages: object[], pagination: object }}
   */
  search(query, options = {}) {
    const args = ['message', '+search', '--q', query];
    if (options.searchIn) args.push('--search-in', options.searchIn);
    if (options.from) args.push('--from', options.from);
    if (options.to) args.push('--to', options.to);
    if (options.dir) args.push('--dir', options.dir);
    if (options.after) args.push('--after', options.after);
    if (options.before) args.push('--before', options.before);
    if (options.hasAttachments) args.push('--has-attachments');
    if (options.isUnread) args.push('--is-unread');
    if (options.limit != null) args.push('--limit', String(options.limit));
    if (options.cursor) args.push('--cursor', options.cursor);
    const data = runCli(args);
    return { messages: data?.data ?? [], pagination: data?.pagination ?? {} };
  }

  /**
   * Get current user info and alias list.
   *
   * @returns {object}
   */
  me() {
    return runCli(['+me']);
  }

  // -------------------------------------------------------------------------
  // Write operations (two-phase confirmation handled automatically)
  // -------------------------------------------------------------------------

  /**
   * Send a new email.
   *
   * @param {string|string[]} to         Recipient(s)
   * @param {string}          subject
   * @param {string}          body
   * @param {object}          [options]
   * @param {string|string[]} [options.cc]
   * @param {string|string[]} [options.bcc]
   * @param {'plain'|'html'}  [options.bodyFormat='plain']
   * @param {string[]}        [options.attachments]  Relative file paths
   * @returns {object}
   */
  send(to, subject, body, options = {}) {
    const args = ['message', '+send', '--subject', subject, '--body', body];
    const recipients = Array.isArray(to) ? to : [to];
    for (const r of recipients) args.push('--to', r);
    if (options.cc) {
      const ccs = Array.isArray(options.cc) ? options.cc : [options.cc];
      for (const c of ccs) args.push('--cc', c);
    }
    if (options.bcc) {
      const bccs = Array.isArray(options.bcc) ? options.bcc : [options.bcc];
      for (const b of bccs) args.push('--bcc', b);
    }
    if (options.bodyFormat === 'html') args.push('--body-format', 'html');
    if (options.attachments) {
      for (const a of options.attachments) args.push('--attachment', a);
    }
    return runConfirmed(args);
  }

  /**
   * Reply to a message.
   *
   * @param {string} messageId  msg_xxx
   * @param {string} body
   * @param {object} [options]
   * @param {boolean}         [options.replyAll]
   * @param {string|string[]} [options.cc]
   * @param {string|string[]} [options.bcc]
   * @param {'plain'|'html'}  [options.bodyFormat='plain']
   * @param {string[]}        [options.attachments]
   * @returns {object}
   */
  reply(messageId, body, options = {}) {
    const args = ['message', '+reply', '--id', messageId, '--body', body];
    if (options.replyAll) args.push('--reply-all');
    if (options.cc) {
      const ccs = Array.isArray(options.cc) ? options.cc : [options.cc];
      for (const c of ccs) args.push('--cc', c);
    }
    if (options.bcc) {
      const bccs = Array.isArray(options.bcc) ? options.bcc : [options.bcc];
      for (const b of bccs) args.push('--bcc', b);
    }
    if (options.bodyFormat === 'html') args.push('--body-format', 'html');
    if (options.attachments) {
      for (const a of options.attachments) args.push('--attachment', a);
    }
    return runConfirmed(args);
  }

  /**
   * Forward a message to new recipients.
   *
   * @param {string}          messageId  msg_xxx
   * @param {string|string[]} to
   * @param {string}          [body]
   * @param {object}          [options]
   * @param {string|string[]} [options.cc]
   * @param {string|string[]} [options.bcc]
   * @param {'plain'|'html'}  [options.bodyFormat='plain']
   * @param {boolean}         [options.includeAttachments]
   * @param {string[]}        [options.attachments]
   * @returns {object}
   */
  forward(messageId, to, body, options = {}) {
    const recipients = Array.isArray(to) ? to : [to];
    const args = ['message', '+forward', '--id', messageId];
    for (const r of recipients) args.push('--to', r);
    if (body) args.push('--body', body);
    if (options.cc) {
      const ccs = Array.isArray(options.cc) ? options.cc : [options.cc];
      for (const c of ccs) args.push('--cc', c);
    }
    if (options.bcc) {
      const bccs = Array.isArray(options.bcc) ? options.bcc : [options.bcc];
      for (const b of bccs) args.push('--bcc', b);
    }
    if (options.bodyFormat === 'html') args.push('--body-format', 'html');
    if (options.includeAttachments) args.push('--include-attachments');
    if (options.attachments) {
      for (const a of options.attachments) args.push('--attachment', a);
    }
    return runConfirmed(args);
  }

  /**
   * Move a message to trash (soft delete, 30-day retention).
   *
   * @param {string} messageId  msg_xxx
   * @returns {object}
   */
  trash(messageId) {
    return runConfirmed(['message', '+trash', '--id', messageId]);
  }

  // -------------------------------------------------------------------------
  // Polling
  // -------------------------------------------------------------------------

  /**
   * Poll for new inbox messages at a fixed interval using a time-cursor strategy.
   *
   * Unlike the previous unread-flag approach, this method uses `--after <timestamp>`
   * so that messages are never missed due to external read operations (e.g. manual
   * `agently-cli message +read`, reading in another client, or a crash mid-processing).
   *
   * The cursor (`afterTimestamp`) advances only after all messages in a batch have
   * been handed to the handler. It is persisted via `options.saveCursor` so restarts
   * resume from where they left off.
   *
   * De-duplication: a Set of seen message_ids prevents the same message from being
   * dispatched twice within one process lifetime (the server may return messages whose
   * created_at equals the cursor boundary).
   *
   * The handler is called once per message.  If the handler throws, the error is
   * logged to stderr and polling continues.  Call `stop()` on the returned controller
   * to stop polling.
   *
   * @param {number} intervalMs
   * @param {(msg: object, client: AgentlyMailClient) => Promise<void>} handler
   * @param {object} [options]
   * @param {number} [options.limit=20]          Max messages per poll cycle
   * @param {string} [options.afterTimestamp]    Initial cursor (ISO 8601); defaults to now
   * @param {(ts: string) => void} [options.saveCursor]  Called after each batch to persist cursor
   * @returns {{ stop: () => void }}
   */
  poll(intervalMs, handler, options = {}) {
    const limit = options.limit ?? 20;
    // Start from now if no saved cursor: don't reprocess the entire inbox on first run
    let afterTimestamp = options.afterTimestamp || new Date().toISOString();
    const saveCursor = options.saveCursor || null;
    const seenIds = new Set();
    let stopped = false;
    let timer = null;

    const tick = async () => {
      if (stopped) return;
      try {
        const { messages } = this.list({ after: afterTimestamp, limit, dir: 'inbox' });
        // Sort ascending by created_at so we process oldest-first and advance cursor correctly
        messages.sort((a, b) => new Date(a.created_at) - new Date(b.created_at));

        let latestTimestamp = afterTimestamp;
        for (const msg of messages) {
          if (stopped) break;
          if (seenIds.has(msg.message_id)) continue;
          seenIds.add(msg.message_id);

          try {
            await handler(msg, this);
          } catch (err) {
            process.stderr.write(
              `[agently-mail] handler error for ${msg.message_id}: ${err?.message || err}\n`,
            );
          }

          // Advance cursor past this message
          if (msg.created_at && msg.created_at > latestTimestamp) {
            latestTimestamp = msg.created_at;
          }
        }

        // Move cursor forward so next poll only fetches newer messages.
        // We add 1ms to avoid re-fetching the last message on the boundary.
        if (messages.length > 0 && latestTimestamp >= afterTimestamp) {
          afterTimestamp = new Date(new Date(latestTimestamp).getTime() + 1).toISOString();
          if (saveCursor) saveCursor(afterTimestamp);
        }
      } catch (err) {
        process.stderr.write(
          `[agently-mail] poll error: ${err?.message || err}\n`,
        );
      }
      if (!stopped) {
        timer = setTimeout(tick, intervalMs);
      }
    };

    // Start immediately
    tick();

    return {
      stop() {
        stopped = true;
        if (timer) clearTimeout(timer);
      },
    };
  }
}

module.exports = { AgentlyMailClient, AgentlyMailError };
