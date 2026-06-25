'use strict';
/**
 * Echo Profile — 原样回显收到的消息，用于调试 Email Bridge 路由是否正确。
 *
 * 用法：发送主题为 [echo] 测试 的邮件，会原样收到回复。
 */
const { createProfile } = require('../../sdk/node/src/index');

createProfile(async ({ message, sessionId }) => {
  return {
    response: `[Echo] 收到你的消息:\n\n${message}`,
    sessionId: sessionId || undefined,
  };
});
