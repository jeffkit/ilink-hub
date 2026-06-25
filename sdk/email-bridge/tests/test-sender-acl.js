'use strict';
/**
 * Tests for matchesRule / matchesAny low-level functions.
 * Higher-level SenderAcl integration tests are in test-acl-system.js.
 *
 * Run: node tests/test-sender-acl.js
 */
const { matchesRule, matchesAny } = require('../src/sender-acl');

let passed = 0, failed = 0;
function assert(condition, label) {
  if (condition) { console.log(`  ✓ ${label}`); passed++; }
  else           { console.error(`  ✗ ${label}`); failed++; }
}

console.log('\nmatchesRule:');
assert(matchesRule('user@example.com', 'user@example.com'),   'exact match');
assert(matchesRule('USER@EXAMPLE.COM', 'user@example.com'),   'case-insensitive exact');
assert(!matchesRule('other@example.com', 'user@example.com'), 'exact mismatch');
assert(matchesRule('user@example.com', '@example.com'),       'domain match');
assert(matchesRule('any@example.com', '@example.com'),        'domain match any user');
assert(!matchesRule('user@other.com', '@example.com'),        'domain mismatch');
assert(!matchesRule('user@notexample.com', '@example.com'),   'domain suffix not fooled');
assert(matchesRule('user@sub.example.com', '@*.example.com'), 'wildcard subdomain');
assert(matchesRule('user@deep.sub.example.com', '@*.example.com'), 'wildcard deep subdomain');
assert(matchesRule('user@example.com', '@*.example.com'),     'wildcard matches base domain');
assert(!matchesRule('user@notexample.com', '@*.example.com'), 'wildcard mismatch');

console.log('\nmatchesAny:');
assert(matchesAny('a@b.com', ['a@b.com', '@c.com']), 'first rule matches');
assert(matchesAny('x@c.com', ['a@b.com', '@c.com']), 'second rule matches');
assert(!matchesAny('z@d.com', ['a@b.com', '@c.com']), 'no match');
assert(!matchesAny('z@d.com', []), 'empty rules = no match');

console.log(`\n${passed + failed} tests: ${passed} passed, ${failed} failed\n`);
if (failed > 0) process.exit(1);
