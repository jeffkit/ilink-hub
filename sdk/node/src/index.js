'use strict';
/**
 * ilink-bridge-profile — iLink Hub Bridge Profile SDK (Node.js)
 *
 * Implements the P0 exec protocol so you can write a single async handler function
 * instead of manually reading env vars and formatting stdout.
 *
 * P0 contract (read by the bridge):
 *   Input  — env vars: ILINK_MESSAGE, ILINK_SESSION_ID, ILINK_SESSION_NAME,
 *                      ILINK_FROM_USER, ILINK_CONTEXT_TOKEN
 *   Output — stdout: optional first line "ILINK_SESSION:<uuid>", then reply text
 *   Exit   — 0 = success, non-zero = error
 *
 * @example
 * // my-profile.js
 * const { createProfile } = require('ilink-bridge-profile');
 *
 * createProfile(async ({ message, sessionId, sessionName, fromUser }) => {
 *   const reply = await myAI(message);
 *   return { response: reply, sessionId: newSessionId };
 * });
 */

const path = require('path');
const os = require('os');
const fs = require('fs');

// ---------------------------------------------------------------------------
// Context — passed to the handler function
// ---------------------------------------------------------------------------

/**
 * @typedef {Object} ProfileContext
 * @property {string} message       - User message text (ILINK_MESSAGE)
 * @property {string} sessionId     - Hub-persisted backend session UUID (ILINK_SESSION_ID)
 * @property {string} sessionName   - Human-readable session name (ILINK_SESSION_NAME)
 * @property {string} fromUser      - Sender user ID (ILINK_FROM_USER)
 * @property {string} contextToken  - Hub context token (ILINK_CONTEXT_TOKEN)
 */

/**
 * @typedef {Object} ProfileResult
 * @property {string}           response   - Reply text to send back to the WeChat user
 * @property {string|undefined} sessionId  - New backend session ID to persist (optional)
 */

// ---------------------------------------------------------------------------
// Session history helpers (optional — for SDK users calling LLM APIs directly)
// ---------------------------------------------------------------------------

/**
 * Default directory for session history files.
 * @returns {string}
 */
function defaultSessionDir() {
  return path.join(os.homedir(), '.ilink-hub', 'sessions');
}

/**
 * Path for a session JSONL file, keyed by the stable session UUID.
 * @param {string} sessionId
 * @param {string} [sessionDir]
 * @returns {string}
 */
function sessionFilePath(sessionId, sessionDir) {
  const dir = sessionDir || defaultSessionDir();
  return path.join(dir, `${sessionId}.jsonl`);
}

/**
 * Load conversation history for a session from its JSONL file.
 * Returns an empty array if the file does not exist.
 *
 * @param {string} sessionId
 * @param {string} [sessionDir]
 * @returns {{ role: string, content: string, ts: string }[]}
 */
function loadHistory(sessionId, sessionDir) {
  if (!sessionId) return [];
  const file = sessionFilePath(sessionId, sessionDir);
  if (!fs.existsSync(file)) return [];
  return fs
    .readFileSync(file, 'utf8')
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      try { return JSON.parse(line); }
      catch { return null; }
    })
    .filter(Boolean);
}

/**
 * Append one or more entries to a session's JSONL history file.
 * Creates the file (and parent directory) if it does not exist.
 *
 * @param {string} sessionId
 * @param {{ role: string, content: string, ts?: string }[]} entries
 * @param {string} [sessionDir]
 */
function appendHistory(sessionId, entries, sessionDir) {
  if (!sessionId || !entries.length) return;
  const file = sessionFilePath(sessionId, sessionDir);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const lines = entries.map((e) =>
    JSON.stringify({ role: e.role, content: e.content, ts: e.ts || new Date().toISOString() })
  );
  fs.appendFileSync(file, lines.join('\n') + '\n', 'utf8');
}

// ---------------------------------------------------------------------------
// createProfile — main entry point
// ---------------------------------------------------------------------------

/**
 * Run a profile handler following the P0 exec protocol.
 *
 * Reads ILINK_* env vars, invokes `handler(ctx)`, writes the P0 output to stdout,
 * and exits the process with code 0 (success) or 1 (error).
 *
 * @param {(ctx: ProfileContext) => Promise<ProfileResult | string>} handler
 *   Async function that receives the profile context and returns either:
 *   - A `ProfileResult` object: `{ response, sessionId? }`
 *   - A plain string (treated as the response; no session ID update)
 */
function createProfile(handler) {
  const ctx = {
    message: process.env.ILINK_MESSAGE || '',
    sessionId: process.env.ILINK_SESSION_ID || '',
    sessionName: process.env.ILINK_SESSION_NAME || 'default',
    fromUser: process.env.ILINK_FROM_USER || '',
    contextToken: process.env.ILINK_CONTEXT_TOKEN || '',
    /**
     * Send a partial response chunk to the WeChat user immediately.
     *
     * Writes an `ILINK_PARTIAL:<json>` line to stdout and flushes.
     * The bridge reads this in real-time and forwards the text to the Hub
     * without waiting for the process to exit.
     *
     * @param {string} text
     */
    sendPartial(text) {
      process.stdout.write(`ILINK_PARTIAL:${JSON.stringify(text)}\n`);
    },
  };

  Promise.resolve()
    .then(() => handler(ctx))
    .then((result) => {
      let response, newSessionId;

      if (typeof result === 'string') {
        response = result;
      } else {
        response = result.response || '';
        newSessionId = result.sessionId;
      }

      // P0 output: optional session line first, then reply text
      if (newSessionId) {
        process.stdout.write(`ILINK_SESSION:${newSessionId}\n`);
      }
      process.stdout.write(response);
      process.exit(0);
    })
    .catch((err) => {
      process.stderr.write(`[ilink-hub/profile] handler error: ${err?.stack || err}\n`);
      process.exit(1);
    });
}

module.exports = { createProfile, loadHistory, appendHistory, sessionFilePath };
