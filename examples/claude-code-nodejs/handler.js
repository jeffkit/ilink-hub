'use strict';
/**
 * Claude Code Bridge Profile (Node.js)
 *
 * 通过 ilink-bridge-profile SDK 接入 Claude Code CLI，
 * 支持多轮对话（--resume）、流式输出（--output-format stream-json）
 * 和自动降级（session 失效时新建会话）。
 *
 * 依赖：
 *   npm install
 *
 * 本地测试（不需要启动 bridge）：
 *   ILINK_MESSAGE="你好，介绍一下自己" \
 *   ILINK_SESSION_ID="" \
 *   ILINK_SESSION_NAME="default" \
 *   ILINK_FROM_USER="test" \
 *   ILINK_CONTEXT_TOKEN="test-token" \
 *   node handler.js
 *
 * 接入 bridge：
 *   ilink-hub-bridge --config profiles.yaml
 */

const { createProfile } = require('ilink-bridge-profile');
const { spawn } = require('child_process');
const readline = require('readline');

/** claude CLI 单次调用的最大等待时间（毫秒） */
const TIMEOUT_MS = 300_000;

/**
 * 流式调用 claude CLI（--output-format stream-json），逐行解析事件并通过回调推送。
 *
 * - `type == "assistant"` 事件中的 text block → onChunk(text)（ILINK_PARTIAL）
 * - `type == "result"` 事件 → 返回 { result, sessionId }
 *
 * 若 sessionId 非空则认为调用成功；若进程以非零退出且未收到 result 事件则抛出错误。
 *
 * @param {string[]}  args      传给 claude 的参数（不含 --output-format）
 * @param {(text: string) => void} onChunk  每收到一段文本立即回调
 * @returns {Promise<{ result: string, sessionId: string }>}
 */
function streamClaude(args, onChunk) {
  return new Promise((resolve, reject) => {
    const allArgs = ['--output-format', 'stream-json', '--dangerously-skip-permissions', ...args];
    const child = spawn('claude', allArgs, { stdio: ['pipe', 'pipe', 'pipe'] });

    const stderrChunks = [];
    child.stderr.on('data', (d) => stderrChunks.push(d));
    child.stdin.end(); // claude reads prompt from -p arg, not stdin

    let resultData = null;
    let timedOut = false;

    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGTERM');
      reject(new Error(`claude timed out after ${TIMEOUT_MS / 1000}s`));
    }, TIMEOUT_MS);

    const rl = readline.createInterface({ input: child.stdout, crlfDelay: Infinity });

    rl.on('line', (line) => {
      if (!line.trim()) return;
      let event;
      try {
        event = JSON.parse(line);
      } catch {
        return;
      }

      if (event.type === 'assistant') {
        // Stream each text content block immediately via onChunk → ILINK_PARTIAL.
        const blocks = event.message?.content ?? [];
        const text = blocks
          .filter((b) => b.type === 'text')
          .map((b) => b.text ?? '')
          .join('');
        if (text) onChunk(text);
      } else if (event.type === 'result') {
        // Only capture session_id; the response text is already streamed above.
        resultData = { result: event.result ?? '', sessionId: event.session_id ?? '' };
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
        reject(new Error(`claude exited with code ${code}, no result event\nstderr: ${stderr}`));
      }
    });

    child.on('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

createProfile(async (ctx) => {
  const { message, sessionId, sendPartial } = ctx;
  const model = process.env.CLAUDE_MODEL;
  const baseArgs = ['--print'];
  if (model) baseArgs.push('--model', model);
  baseArgs.push('-p', message);

  let newSessionId = '';

  const tryStream = async (sid) => {
    const args = [...baseArgs];
    if (sid) args.push('--resume', sid);
    const data = await streamClaude(args, (chunk) => sendPartial(chunk));
    newSessionId = data.sessionId;
  };

  try {
    await tryStream(sessionId);
  } catch (err) {
    if (sessionId) {
      // session 失效（如 --resume 的 UUID 过期），降级为新会话
      process.stderr.write(
        `[claude-code] session ${sessionId} resume failed (${err.message}), retrying as new session\n`,
      );
      await tryStream('');
    } else {
      throw err;
    }
  }

  // All text was already sent via ILINK_PARTIAL; response="" skips the duplicate final send.
  return { response: '', sessionId: newSessionId || undefined };
});
