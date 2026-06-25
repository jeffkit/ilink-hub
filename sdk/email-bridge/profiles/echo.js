#!/usr/bin/env node
'use strict';
/**
 * echo Profile — 原样回显收到的消息，用于调试 Email Bridge 路由
 *
 * 发送主题为 [echo] xxx 的邮件，会收到带完整上下文信息的回复。
 *
 * Local test:
 *   ILINK_MESSAGE="hello" node echo.js
 */

const { createProfile } = require('../src/index');

createProfile(async ({ message, sessionId, sessionName, fromUser }) => {
  const info = [
    `Session: ${sessionId || '(new)'}`,
    `Name: ${sessionName}`,
    `From: ${fromUser}`,
    '',
    '── Message ──',
    message,
  ].join('\n');

  return { response: `[Echo]\n${info}`, sessionId: sessionId || undefined };
});
