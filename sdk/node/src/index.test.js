'use strict';
const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const os = require('os');
const path = require('path');
const fs = require('fs');

const { loadHistory, appendHistory, sessionFilePath } = require('./index.js');

describe('sessionFilePath', () => {
  it('uses default dir when sessionDir omitted', () => {
    const p = sessionFilePath('abc-123');
    assert.ok(p.includes(path.join('.ilink-hub', 'sessions', 'abc-123.jsonl')));
  });

  it('uses custom dir when provided', () => {
    const p = sessionFilePath('abc-123', '/tmp/test-sessions');
    assert.equal(p, '/tmp/test-sessions/abc-123.jsonl');
  });
});

describe('loadHistory / appendHistory', () => {
  it('returns empty array for missing file', () => {
    const result = loadHistory('nonexistent-uuid-xyz', '/tmp/no-such-dir');
    assert.deepEqual(result, []);
  });

  it('round-trips history entries via JSONL', () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ilink-test-'));
    const sid = 'test-session-1';

    appendHistory(sid, [
      { role: 'user', content: 'hello', ts: '2026-01-01T00:00:00Z' },
      { role: 'assistant', content: 'hi there', ts: '2026-01-01T00:00:01Z' },
    ], tmpDir);

    const entries = loadHistory(sid, tmpDir);
    assert.equal(entries.length, 2);
    assert.equal(entries[0].role, 'user');
    assert.equal(entries[0].content, 'hello');
    assert.equal(entries[1].role, 'assistant');
    assert.equal(entries[1].content, 'hi there');
  });

  it('appends to existing file across multiple calls', () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ilink-test-'));
    const sid = 'test-session-2';

    appendHistory(sid, [{ role: 'user', content: 'msg1' }], tmpDir);
    appendHistory(sid, [{ role: 'assistant', content: 'reply1' }], tmpDir);

    const entries = loadHistory(sid, tmpDir);
    assert.equal(entries.length, 2);
    assert.equal(entries[0].content, 'msg1');
    assert.equal(entries[1].content, 'reply1');
  });
});
