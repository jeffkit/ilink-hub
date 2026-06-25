#!/usr/bin/env node
'use strict';
/**
 * 临时测试运行器 — 关闭自发邮件过滤，允许自发自收验证路由
 * 使用: node test-run.js [--dry-run]
 */
const path = require('path');
const { createEmailBridge } = require('./src/index');

const dryRun = process.argv.includes('--dry-run');

createEmailBridge({
  profilesConfig: path.join(__dirname, 'email-profiles.example.yaml'),
  pollIntervalMs: 15_000,   // 15 秒轮询，快速看到结果
  dryRun,
  filterSelfSent: false,    // 关键：允许处理自发邮件
  limit: 10,
});
