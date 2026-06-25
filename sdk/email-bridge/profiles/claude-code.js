#!/usr/bin/env node
'use strict';
/**
 * claude-code Profile — wraps the `claude` CLI (Claude Code)
 *
 * Env vars:
 *   ILINK_MESSAGE        (P0) User message text
 *   ILINK_SESSION_ID     (P0) Session UUID to resume (empty = new session)
 *   CLAUDE_MODEL         Override model (e.g. claude-opus-4-5)
 *
 * Local test:
 *   ILINK_MESSAGE="你好" ILINK_SESSION_ID="" node claude-code.js
 */

const { createProfile } = require('../src/index');
const { streamJsonCli, withResumeFallback } = require('./_stream_json');

createProfile(async ({ message, sessionId, sendPartial }) => {
  const model = process.env.CLAUDE_MODEL || 'claude-sonnet-4-5';

  const invoke = async (sid) => {
    const args = ['--print', '--dangerously-skip-permissions'];
    if (model) args.push('--model', model);
    if (sid) args.push('--resume', sid);
    return streamJsonCli('claude', args, message, 'arg', sendPartial);
  };

  const { sessionId: newSid } = await withResumeFallback(invoke, sessionId, 'claude-code');

  // Text already streamed via sendPartial; return empty response to avoid duplicate
  return { response: '', sessionId: newSid || undefined };
});
