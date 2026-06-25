#!/usr/bin/env node
'use strict';
/**
 * codex Profile — wraps the OpenAI Codex CLI (`codex`)
 *
 * Event format (JSONL, one per line):
 *   {"type":"thread.started","thread_id":"<uuid>"}
 *   {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
 *   {"type":"turn.completed","usage":{...}}
 *
 * Env vars:
 *   ILINK_MESSAGE        (P0) User message text
 *   ILINK_SESSION_ID     (P0) thread_id to resume (empty = new thread)
 *
 * Local test:
 *   ILINK_MESSAGE="hello" ILINK_SESSION_ID="" node codex.js
 */

const { createProfile } = require('../src/index');
const { spawn } = require('child_process');
const readline = require('readline');

const TIMEOUT_MS = 300_000;

/**
 * @param {string} message
 * @param {string} sessionId  thread_id or empty
 * @param {(chunk: string) => void} onChunk
 * @returns {Promise<{ sessionId: string, responseText: string }>}
 */
function runCodex(message, sessionId, onChunk) {
  return new Promise((resolve, reject) => {
    const args = ['exec'];
    if (sessionId) {
      args.push('resume', sessionId);
    }
    args.push(message, '--dangerously-bypass-approvals-and-sandbox', '--json');

    const child = spawn('codex', args, { stdio: ['pipe', 'pipe', 'pipe'] });
    child.stdin.end();

    const stderrChunks = [];
    child.stderr.on('data', (d) => stderrChunks.push(d));

    let threadId = sessionId || '';
    let timedOut = false;
    const chunks = [];
    let completed = false;

    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGTERM');
      reject(new Error(`codex timed out after ${TIMEOUT_MS / 1000}s`));
    }, TIMEOUT_MS);

    const rl = readline.createInterface({ input: child.stdout, crlfDelay: Infinity });

    rl.on('line', (line) => {
      if (!line.trim()) return;
      let event;
      try { event = JSON.parse(line); } catch { return; }

      if (event.type === 'thread.started' && event.thread_id) {
        threadId = event.thread_id;
      } else if (event.type === 'item.completed' && event.item?.type === 'agent_message') {
        const text = event.item.text || '';
        if (text) {
          chunks.push(text);
          onChunk(text);
        }
      } else if (event.type === 'turn.completed') {
        completed = true;
      }
    });

    child.on('close', (code) => {
      clearTimeout(timer);
      if (timedOut) return;
      rl.close();

      if (completed || chunks.length > 0) {
        resolve({ sessionId: threadId, responseText: chunks.join('') });
      } else {
        const stderr = Buffer.concat(stderrChunks).toString('utf8').trim();
        reject(new Error(
          `codex exited with code ${code}, no agent_message${stderr ? `\n${stderr}` : ''}`,
        ));
      }
    });

    child.on('error', (err) => { clearTimeout(timer); reject(err); });
  });
}

createProfile(async ({ message, sessionId, sendPartial }) => {
  let result;
  try {
    result = await runCodex(message, sessionId, sendPartial);
  } catch (err) {
    if (sessionId) {
      process.stderr.write(`[codex] session ${sessionId} resume failed: ${err.message}\n`);
      result = await runCodex(message, '', sendPartial);
    } else {
      throw err;
    }
  }

  return { response: '', sessionId: result.sessionId || undefined };
});
