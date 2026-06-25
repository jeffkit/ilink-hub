#!/usr/bin/env node
'use strict';
/**
 * agy Profile — wraps the Antigravity (Google DeepMind) `agy` CLI
 *
 * Unlike the stream-json CLIs, agy:
 *  - Outputs plain text to stdout
 *  - Writes a conversation ID to its log file
 *  - Requires stdin to be closed immediately (non-interactive)
 *
 * Session management: parse "Created conversation <uuid>" from the log file.
 *
 * Env vars:
 *   ILINK_MESSAGE        (P0) User message text
 *   ILINK_SESSION_ID     (P0) Conversation UUID to resume
 *   AGY_MODEL            Override model
 *
 * Local test:
 *   ILINK_MESSAGE="hello" ILINK_SESSION_ID="" node agy.js
 */

const { createProfile } = require('../src/index');
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const TIMEOUT_MS = 300_000;

/**
 * @param {string} message
 * @param {string} sessionId  conversation UUID or empty
 * @returns {Promise<{ response: string, sessionId: string }>}
 */
function runAgy(message, sessionId) {
  return new Promise((resolve, reject) => {
    const logPath = path.join(os.tmpdir(), `agy-ilink-${process.pid}-${Date.now()}.log`);
    const model = process.env.AGY_MODEL || '';

    const args = ['--dangerously-skip-permissions', '--log-file', logPath];
    if (model) args.push('--model', model);
    if (sessionId) args.push('--conversation', sessionId);
    args.push('-p', message);

    const child = spawn('agy', args, { stdio: ['pipe', 'pipe', 'pipe'] });
    child.stdin.end(); // agy blocks if stdin is a tty; close immediately

    const stdoutChunks = [];
    const stderrChunks = [];
    child.stdout.on('data', (d) => stdoutChunks.push(d));
    child.stderr.on('data', (d) => stderrChunks.push(d));

    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGTERM');
      reject(new Error(`agy timed out after ${TIMEOUT_MS / 1000}s`));
    }, TIMEOUT_MS);

    child.on('close', (code) => {
      clearTimeout(timer);
      if (timedOut) return;

      if (code !== 0) {
        const stderr = Buffer.concat(stderrChunks).toString('utf8').trim();
        return reject(new Error(`agy exited with code ${code}${stderr ? `\n${stderr}` : ''}`));
      }

      const response = Buffer.concat(stdoutChunks).toString('utf8').trim();

      // Extract conversation ID from log file
      let newSessionId = sessionId || '';
      try {
        const log = fs.readFileSync(logPath, 'utf8');
        const m = log.match(/Created conversation ([a-f0-9-]{36})/i);
        if (m) newSessionId = m[1];
      } catch { /* log may not exist if agy skipped it */ }
      // Clean up temp log
      try { fs.unlinkSync(logPath); } catch { /* ignore */ }

      resolve({ response, sessionId: newSessionId });
    });

    child.on('error', (err) => { clearTimeout(timer); reject(err); });
  });
}

createProfile(async ({ message, sessionId }) => {
  let result;
  try {
    result = await runAgy(message, sessionId);
  } catch (err) {
    if (sessionId) {
      process.stderr.write(`[agy] session ${sessionId} resume failed: ${err.message}\n`);
      result = await runAgy(message, '');
    } else {
      throw err;
    }
  }

  return { response: result.response, sessionId: result.sessionId || undefined };
});
