'use strict';
/**
 * Tests for SenderAcl — sender access control module.
 *
 * Run: node tests/test-sender-acl.js
 */

const { SenderAcl, matchesRule, matchesAny } = require('../src/sender-acl');

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
// matchesRule
// ---------------------------------------------------------------------------
console.log('\nmatchesRule:');
assert(matchesRule('user@example.com', 'user@example.com'), 'exact match');
assert(matchesRule('USER@EXAMPLE.COM', 'user@example.com'), 'case-insensitive exact');
assert(!matchesRule('other@example.com', 'user@example.com'), 'exact mismatch');

assert(matchesRule('user@example.com', '@example.com'), 'domain match');
assert(matchesRule('any@example.com', '@example.com'), 'domain match any user');
assert(!matchesRule('user@other.com', '@example.com'), 'domain mismatch');
assert(!matchesRule('user@notexample.com', '@example.com'), 'domain suffix not fooled');

assert(matchesRule('user@sub.example.com', '@*.example.com'), 'wildcard subdomain');
assert(matchesRule('user@deep.sub.example.com', '@*.example.com'), 'wildcard deep subdomain');
assert(matchesRule('user@example.com', '@*.example.com'), 'wildcard matches base domain too');
assert(!matchesRule('user@notexample.com', '@*.example.com'), 'wildcard mismatch');

// ---------------------------------------------------------------------------
// Open access (no rules configured)
// ---------------------------------------------------------------------------
console.log('\nOpen access (no rules):');
const openAcl = new SenderAcl({ profiles: {} });
assert(openAcl.isOpenAccess(), 'isOpenAccess() true when no rules');
assert(openAcl.checkGlobal('anyone@anywhere.com') === 'allow', 'allows anyone');
assert(openAcl.checkProfile('claude-code', 'anyone@anywhere.com') === 'allow', 'profile allows anyone');

// ---------------------------------------------------------------------------
// Global allowlist
// ---------------------------------------------------------------------------
console.log('\nGlobal allowlist:');
const allowAcl = new SenderAcl({
  allowed_senders: ['alice@example.com', '@trusted.org'],
  profiles: {},
});
assert(!allowAcl.isOpenAccess(), 'isOpenAccess() false when allowlist configured');
assert(allowAcl.checkGlobal('alice@example.com') === 'allow', 'exact match allowed');
assert(allowAcl.checkGlobal('ALICE@EXAMPLE.COM') === 'allow', 'case-insensitive allowed');
assert(allowAcl.checkGlobal('bob@trusted.org') === 'allow', 'domain match allowed');
assert(allowAcl.checkGlobal('eve@evil.com') === 'deny', 'unlisted denied');
assert(allowAcl.checkGlobal('eve@notexample.com') === 'deny', 'similar domain denied');

// ---------------------------------------------------------------------------
// Global denylist
// ---------------------------------------------------------------------------
console.log('\nGlobal denylist:');
const denyAcl = new SenderAcl({
  denied_senders: ['spam@example.com', '@blocked.com'],
  profiles: {},
});
assert(denyAcl.checkGlobal('spam@example.com') === 'deny', 'exact deny');
assert(denyAcl.checkGlobal('anyone@blocked.com') === 'deny', 'domain deny');
assert(denyAcl.checkGlobal('legit@other.com') === 'allow', 'non-listed allowed');

// ---------------------------------------------------------------------------
// Denylist takes priority over allowlist
// ---------------------------------------------------------------------------
console.log('\nDenylist priority over allowlist:');
const bothAcl = new SenderAcl({
  allowed_senders: ['@example.com'],
  denied_senders:  ['bad@example.com'],
  profiles: {},
});
assert(bothAcl.checkGlobal('good@example.com') === 'allow', 'allowed by domain');
assert(bothAcl.checkGlobal('bad@example.com') === 'deny', 'deny overrides allow');

// ---------------------------------------------------------------------------
// Per-profile ACL
// ---------------------------------------------------------------------------
console.log('\nPer-profile ACL:');
const profileAcl = new SenderAcl({
  profiles: {
    'echo': {
      allowed_senders: ['admin@example.com'],
    },
    'claude-code': {
      // no per-profile list → inherit global
    },
  },
});
assert(profileAcl.checkProfile('echo', 'admin@example.com') === 'allow', 'profile exact allow');
assert(profileAcl.checkProfile('echo', 'other@example.com') === 'deny', 'profile deny unlisted');
assert(profileAcl.checkProfile('claude-code', 'anyone@anywhere.com') === 'allow', 'no list = inherit global = allow');
assert(profileAcl.checkProfile('unknown-profile', 'anyone@anywhere.com') === 'allow', 'unknown profile = allow');

// ---------------------------------------------------------------------------
// deny_action and deny_message defaults
// ---------------------------------------------------------------------------
console.log('\ndeny_action / deny_message:');
const defaultAcl = new SenderAcl({ profiles: {} });
assert(defaultAcl.denyAction === 'silent', 'default deny_action is silent');
assert(defaultAcl.denyMessage === null, 'default deny_message is null');

const notifyAcl = new SenderAcl({
  deny_action: 'notify',
  deny_message: 'Custom message.',
  profiles: {},
});
assert(notifyAcl.denyAction === 'notify', 'notify action set');
assert(notifyAcl.denyMessage === 'Custom message.', 'custom deny message set');

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------
console.log(`\n${passed + failed} tests: ${passed} passed, ${failed} failed\n`);
if (failed > 0) process.exit(1);
