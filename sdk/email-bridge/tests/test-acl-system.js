'use strict';
/**
 * Tests for the full ACL system:
 *   AclConfig, SenderAcl (refactored), DeniedLog, AdminHandler.parseCommands
 *
 * Run: node tests/test-acl-system.js
 */

const os   = require('os');
const fs   = require('fs');
const path = require('path');

const { AclConfig }     = require('../src/acl-config');
const { SenderAcl }     = require('../src/sender-acl');
const { DeniedLog }     = require('../src/denied-log');
const { parseCommands } = require('../src/admin-handler');

let passed = 0;
let failed = 0;

function assert(condition, label) {
  if (condition) {
    console.log(`  ✓ ${label}`);
    passed++;
  } else {
    console.error(`  ✗ ${label}`);
    failed++;
  }
}

// ---------------------------------------------------------------------------
// Temp dir for file-based tests
// ---------------------------------------------------------------------------
const tmpDir      = fs.mkdtempSync(path.join(os.tmpdir(), 'acl-test-'));
const dynamicFile = path.join(tmpDir, 'acl-dynamic.json');
const deniedFile  = path.join(tmpDir, 'denied-log.json');

function cleanup() {
  try { fs.rmSync(tmpDir, { recursive: true }); } catch {}
}

// ---------------------------------------------------------------------------
// AclConfig — static only
// ---------------------------------------------------------------------------
console.log('\nAclConfig (static only):');

const cfgOpen = new AclConfig({ dynamicFile });
assert(cfgOpen.isOpenAccess(), 'no config = open access');
assert(cfgOpen.allowedSenders.length === 0, 'empty allowed');
assert(cfgOpen.deniedSenders.length  === 0, 'empty denied');
assert(cfgOpen.denyAction === 'silent', 'default deny_action=silent');
assert(cfgOpen.adminSenders.length === 0, 'no admins by default');

// ---------------------------------------------------------------------------
// AclConfig — dynamic mutations
// ---------------------------------------------------------------------------
console.log('\nAclConfig (dynamic mutations):');

const cfgDyn = new AclConfig({ dynamicFile });
cfgDyn.dynamicAllow(['alice@example.com', '@trusted.org']);
assert(cfgDyn.allowedSenders.includes('alice@example.com'), 'dynamic allow persisted');
assert(cfgDyn.allowedSenders.includes('@trusted.org'), 'domain dynamic allow');
assert(!cfgDyn.isOpenAccess(), 'no longer open after allow added');

cfgDyn.dynamicDeny(['eve@evil.com']);
assert(cfgDyn.deniedSenders.includes('eve@evil.com'), 'dynamic deny persisted');

// allow overrides deny in dynamic lists
cfgDyn.dynamicAllow(['eve@evil.com']);
assert(!cfgDyn.deniedSenders.includes('eve@evil.com'), 'dynamicAllow removes from denied');

// reset
cfgDyn.dynamicReset(['alice@example.com']);
assert(!cfgDyn.allowedSenders.includes('alice@example.com'), 'dynamic reset removes from allowed');

// snapshot
const snap = cfgDyn.dynamicSnapshot();
assert(Array.isArray(snap.allowed), 'snapshot has allowed array');
assert(Array.isArray(snap.denied),  'snapshot has denied array');

// Reload from disk — another AclConfig instance should see same data
const cfgDyn2 = new AclConfig({ dynamicFile });
assert(cfgDyn2.allowedSenders.includes('@trusted.org'), 'dynamic data persists across instances');

// ---------------------------------------------------------------------------
// SenderAcl — open access
// ---------------------------------------------------------------------------
console.log('\nSenderAcl (open access):');

const aclOpen = new SenderAcl(new AclConfig({ dynamicFile: path.join(tmpDir, 'open.json') }));
assert(aclOpen.isOpenAccess(), 'open access');
assert(aclOpen.checkGlobal('anyone@anywhere.com') === 'allow', 'open: allows anyone');
assert(aclOpen.isAdmin('admin@co.com') === false, 'no admin configured');

// ---------------------------------------------------------------------------
// SenderAcl — with static config object (simulated)
// ---------------------------------------------------------------------------
console.log('\nSenderAcl (with rules):');

// Simulate AclConfig with known values via a plain object satisfying the interface
class FakeAclConfig {
  constructor(data) { this._d = data; }
  get allowedSenders() { return this._d.allowed || []; }
  get deniedSenders()  { return this._d.denied  || []; }
  get adminSenders()   { return this._d.admins  || []; }
  get denyAction()     { return this._d.denyAction || 'silent'; }
  get denyMessage()    { return this._d.denyMessage || null; }
  get profileAcl()     { return this._d.profileAcl || {}; }
  get reportConfig()   { return {}; }
  isOpenAccess()       { return this.allowedSenders.length === 0 && this.deniedSenders.length === 0; }
}

const aclRules = new SenderAcl(new FakeAclConfig({
  allowed:    ['alice@example.com', '@trusted.org'],
  denied:     ['eve@evil.com'],
  admins:     ['admin@example.com'],
  profileAcl: { echo: { allowed_senders: ['admin@example.com'] } },
}));

assert(aclRules.checkGlobal('alice@example.com') === 'allow',  'exact allow');
assert(aclRules.checkGlobal('any@trusted.org')   === 'allow',  'domain allow');
assert(aclRules.checkGlobal('eve@evil.com')       === 'deny',   'exact deny');
assert(aclRules.checkGlobal('stranger@other.com') === 'deny',   'not in allowlist = deny');
assert(aclRules.isAdmin('admin@example.com')      === true,     'admin recognised');
assert(aclRules.isAdmin('nobody@example.com')     === false,    'non-admin');

// per-profile
assert(aclRules.checkProfile('echo', 'admin@example.com')    === 'allow', 'profile allow');
assert(aclRules.checkProfile('echo', 'alice@example.com')    === 'deny',  'profile deny');
assert(aclRules.checkProfile('claude-code', 'alice@example.com') === 'allow', 'no profile rule = allow');

// ---------------------------------------------------------------------------
// DeniedLog
// ---------------------------------------------------------------------------
console.log('\nDeniedLog:');

const log = new DeniedLog(deniedFile);

const msg1 = { message_id: 'msg_001', from: { email: 'a@b.com', name: 'A' }, subject: 'Hi' };
const msg2 = { message_id: 'msg_002', from: { email: 'c@d.com', name: 'C' }, subject: 'Hello' };

log.record(msg1, 'global ACL');
log.record(msg2, 'global ACL');
log.record(msg1, 'global ACL');  // duplicate — should be ignored

assert(log.getUnreported().length === 2, 'records 2 unique messages');

log.markReported(['msg_001']);
assert(log.getUnreported().length === 1, 'one unreported after markReported');

log.cleanup();
assert(log.getUnreported().length === 1, 'cleanup keeps recent unreported entries');

// Persist across instances
const log2 = new DeniedLog(deniedFile);
assert(log2.getUnreported().length === 1, 'data persists across instances');

// ---------------------------------------------------------------------------
// parseCommands
// ---------------------------------------------------------------------------
console.log('\nparseCommands:');

const cmds = parseCommands(`
Hello admin,

/allow alice@example.com
/deny  @spam.com
/ALLOW @trusted.org
/reset bob@example.com
/status
some random text
/allow
`);

assert(cmds.length === 5, 'parses 5 commands');
assert(cmds[0].cmd === '/allow' && cmds[0].arg === 'alice@example.com', '/allow exact');
assert(cmds[1].cmd === '/deny'  && cmds[1].arg === '@spam.com',         '/deny domain');
assert(cmds[2].cmd === '/allow' && cmds[2].arg === '@trusted.org',      '/allow case-insensitive');
assert(cmds[3].cmd === '/reset' && cmds[3].arg === 'bob@example.com',   '/reset');
assert(cmds[4].cmd === '/status' && cmds[4].arg === '',                  '/status no arg');

const noCmd = parseCommands('just a regular email reply, no commands');
assert(noCmd.length === 0, 'no commands in plain text');

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------
cleanup();
console.log(`\n${passed + failed} tests: ${passed} passed, ${failed} failed\n`);
if (failed > 0) process.exit(1);
