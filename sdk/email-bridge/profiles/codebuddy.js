#!/usr/bin/env node
'use strict';
/**
 * codebuddy Profile — wraps the CodeBuddy Code CLI (`codebuddy`)
 *
 * CodeBuddy uses the same stream-json protocol as Claude Code.
 *
 * Env vars:
 *   ILINK_MESSAGE              (P0) User message text
 *   ILINK_SESSION_ID           (P0) Session UUID to resume
 *   ILINK_CODEBUDDY_MODEL      Override model (e.g. claude-sonnet-4.6)
 *
 * Local test:
 *   ILINK_MESSAGE="你好" ILINK_SESSION_ID="" node codebuddy.js
 */

const { createProfile } = require('../src/index');
const { streamJsonCli, withResumeFallback } = require('./_stream_json');

createProfile(async ({ message, sessionId, sendPartial }) => {
  const model = process.env.ILINK_CODEBUDDY_MODEL || '';

  const invoke = async (sid) => {
    const args = [
      '--print',
      '--dangerously-skip-permissions',
      '--disallowedTools', 'AskUserQuestion',
    ];
    if (model) args.push('--model', model);
    if (sid) args.push('--resume', sid);
    return streamJsonCli('codebuddy', args, message, 'arg', sendPartial);
  };

  const { sessionId: newSid } = await withResumeFallback(invoke, sessionId, 'codebuddy');

  return { response: '', sessionId: newSid || undefined };
});
