#!/usr/bin/env node
'use strict';
/**
 * cursor Profile — wraps the Cursor `agent` CLI
 *
 * Differences from claude-code:
 *   - Message is written to stdin (not via -p flag)
 *   - Extra flags: --trust --yolo
 *
 * Env vars:
 *   ILINK_MESSAGE        (P0) User message text
 *   ILINK_SESSION_ID     (P0) Session UUID to resume (empty = new session)
 *   CURSOR_MODEL         Override model
 *
 * Local test:
 *   ILINK_MESSAGE="列出当前目录文件" ILINK_SESSION_ID="" node cursor.js
 */

const { createProfile } = require('../src/index');
const { streamJsonCli, withResumeFallback } = require('./_stream_json');

createProfile(async ({ message, sessionId, sendPartial }) => {
  const model = process.env.CURSOR_MODEL || '';

  const invoke = async (sid) => {
    const args = ['--print', '--trust', '--yolo'];
    if (model) args.push('--model', model);
    if (sid) args.push('--resume', sid);
    return streamJsonCli('agent', args, message, 'stdin', sendPartial);
  };

  const { sessionId: newSid } = await withResumeFallback(invoke, sessionId, 'cursor');

  return { response: '', sessionId: newSid || undefined };
});
