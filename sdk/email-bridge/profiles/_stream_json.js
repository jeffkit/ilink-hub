'use strict';
/**
 * _stream_json.js — shared helper for CLIs that emit `--output-format stream-json`
 *
 * Handles: claude / codebuddy / cursor (all use the same event schema)
 *
 * Event schema:
 *   {"type":"system","session_id":"<uuid>",...}
 *   {"type":"assistant","message":{"content":[{"type":"text","text":"..."}],...},...}
 *   {"type":"result","subtype":"success","result":"...","session_id":"<uuid>"}
 *
 * Returns { sessionId, responseText } where:
 *   - responseText: full response (for callers that do NOT stream)
 *   - All text is also emitted via onChunk during streaming
 */

const { spawn } = require('child_process');
const readline = require('readline');

const TIMEOUT_MS = 300_000;

/**
 * Invoke a stream-json CLI.
 *
 * @param {string}   command     e.g. 'claude', 'agent', 'codebuddy'
 * @param {string[]} args        Additional CLI args (NOT including --output-format)
 * @param {string}   message     Text to pass via -p or stdin
 * @param {'arg'|'stdin'} inputMode  'arg' = pass message via -p flag, 'stdin' = write to stdin
 * @param {(chunk: string) => void} onChunk  Called for each streamed text fragment
 * @returns {Promise<{ sessionId: string, responseText: string }>}
 */
function streamJsonCli(command, args, message, inputMode, onChunk) {
  return new Promise((resolve, reject) => {
    const finalArgs = [
      '--output-format', 'stream-json',
      ...args,
      ...(inputMode === 'arg' ? ['-p', message] : []),
    ];

    const child = spawn(command, finalArgs, {
      stdio: ['pipe', 'pipe', 'pipe'],
    });

    if (inputMode === 'stdin') {
      child.stdin.write(message);
      child.stdin.end();
    } else {
      child.stdin.end();
    }

    const stderrChunks = [];
    child.stderr.on('data', (d) => stderrChunks.push(d));

    let resultData = null;
    let timedOut = false;
    const chunks = [];

    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGTERM');
      reject(new Error(`${command} timed out after ${TIMEOUT_MS / 1000}s`));
    }, TIMEOUT_MS);

    const rl = readline.createInterface({ input: child.stdout, crlfDelay: Infinity });

    rl.on('line', (line) => {
      if (!line.trim()) return;
      let event;
      try { event = JSON.parse(line); } catch { return; }

      if (event.type === 'assistant') {
        const blocks = event.message?.content ?? [];
        const text = blocks
          .filter((b) => b.type === 'text')
          .map((b) => b.text ?? '')
          .join('');
        if (text) {
          chunks.push(text);
          onChunk(text);
        }
      } else if (event.type === 'result') {
        const sessionId = event.session_id ?? '';
        // result.result may contain text when no assistant event preceded it
        const fallbackText = (event.result || '').trim();
        if (fallbackText && chunks.length === 0) {
          chunks.push(fallbackText);
          onChunk(fallbackText);
        }
        resultData = { sessionId, responseText: chunks.join('') };
      }
    });

    child.on('close', (code) => {
      clearTimeout(timer);
      if (timedOut) return;
      rl.close();

      if (resultData) {
        resolve(resultData);
      } else {
        const stderr = Buffer.concat(stderrChunks).toString('utf8').trim();
        reject(new Error(
          `${command} exited with code ${code}, no result event${stderr ? `\n${stderr}` : ''}`,
        ));
      }
    });

    child.on('error', (err) => { clearTimeout(timer); reject(err); });
  });
}

/**
 * Run with session-resume fallback.
 * If invocation with sessionId fails, retry with empty session.
 *
 * @param {(sid: string) => Promise<{ sessionId: string, responseText: string }>} invoke
 * @param {string} sessionId
 * @param {string} label  For error messages
 * @returns {Promise<{ sessionId: string, responseText: string }>}
 */
async function withResumeFallback(invoke, sessionId, label) {
  try {
    return await invoke(sessionId);
  } catch (err) {
    if (sessionId) {
      process.stderr.write(`[${label}] session ${sessionId} resume failed: ${err.message}\n`);
      return await invoke('');
    }
    throw err;
  }
}

module.exports = { streamJsonCli, withResumeFallback };
