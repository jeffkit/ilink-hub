'use strict';
/**
 * Claude Code Bridge Profile (Node.js)
 *
 * 通过 ilink-bridge-profile SDK 接入 Claude Code CLI，
 * 支持多轮对话（--resume）和自动降级（session 失效时新建会话）。
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

/** claude CLI 单次调用的最大等待时间（毫秒） */
const TIMEOUT_MS = 300_000;

/**
 * 执行 claude CLI，stdin 立即关闭（避免 claude 等待管道输入）。
 * 返回 stdout 字符串。
 *
 * 注：claude 在 API 错误（如模型不可用）时也会以非零退出码退出，
 * 但仍然会在 stdout 输出 JSON（其中包含错误信息），所以不以退出码判定成败，
 * 而是由调用方解析 JSON 内容来判断。
 * 仅当 stdout 为空（表示 spawn 本身失败）时才以 stderr 内容抛出。
 *
 * @param {string[]} args
 * @returns {Promise<string>}
 */
function spawnClaude(args) {
  return new Promise((resolve, reject) => {
    const child = spawn('claude', args, { stdio: ['pipe', 'pipe', 'pipe'] });
    const stdoutChunks = [];
    const stderrChunks = [];

    child.stdout.on('data', (d) => stdoutChunks.push(d));
    child.stderr.on('data', (d) => stderrChunks.push(d));

    // 关闭 stdin，避免 claude 等待管道输入
    child.stdin.end();

    const timer = setTimeout(() => {
      child.kill('SIGTERM');
      reject(new Error(`claude timed out after ${TIMEOUT_MS / 1000}s`));
    }, TIMEOUT_MS);

    child.on('close', () => {
      clearTimeout(timer);
      const stdout = Buffer.concat(stdoutChunks).toString('utf8');
      const stderr = Buffer.concat(stderrChunks).toString('utf8');
      if (!stdout.trim()) {
        // 空 stdout 说明 claude 根本没有启动或立即崩溃
        reject(new Error(`claude produced no output\nstderr: ${stderr}`));
      } else {
        resolve(stdout);
      }
    });

    child.on('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

/**
 * 调用 Claude Code CLI（--print --output-format json），
 * 返回回复文本和新 session_id。
 *
 * @param {string} message   用户消息
 * @param {string} sessionId Hub 存储的 Claude session UUID（空字符串 = 新会话）
 * @returns {Promise<{ result: string, sessionId: string }>}
 */
async function callClaude(message, sessionId) {
  const args = ['--print', '--output-format', 'json'];

  // 支持通过环境变量指定模型（在 profiles.yaml 的 env 段设置 CLAUDE_MODEL）
  const model = process.env.CLAUDE_MODEL;
  if (model) {
    args.push('--model', model);
  }

  if (sessionId) {
    args.push('--resume', sessionId);
  }
  args.push(message);

  let stdout;
  try {
    stdout = await spawnClaude(args);
  } catch (err) {
    if (sessionId) {
      // session 失效（如 --resume 的 UUID 过期），降级为新会话
      process.stderr.write(
        `[claude-code] session ${sessionId} resume failed (${err.message}), retrying as new session\n`,
      );
      const freshArgs = ['--print', '--output-format', 'json'];
      if (model) freshArgs.push('--model', model);
      freshArgs.push(message);
      stdout = await spawnClaude(freshArgs);
    } else {
      throw err;
    }
  }

  // Claude 输出 JSON 数组；最后一个 type=result 的事件包含回复正文和新 session_id
  const events = JSON.parse(stdout.trim());
  const resultEvent = [...events].reverse().find((e) => e.type === 'result');
  if (!resultEvent) {
    throw new Error(`no result event in claude output: ${stdout.slice(0, 500)}`);
  }

  // is_error=true 通常表示 API 错误（如模型不可用），直接返回错误信息作为回复
  const resultText = resultEvent.result || '';
  return {
    result: resultText,
    sessionId: resultEvent.is_error ? '' : (resultEvent.session_id || ''),
  };
}

createProfile(async ({ message, sessionId }) => {
  const { result, sessionId: newSessionId } = await callClaude(message, sessionId);
  return { response: result, sessionId: newSessionId || undefined };
});
