'use strict';
/**
 * DeniedLog — 被拒绝邮件的持久化记录
 *
 * 存储格式（~/.ilink-email-bridge/denied-log.json）：
 * [
 *   {
 *     "message_id": "msg_xxx",
 *     "from_email": "unknown@example.com",
 *     "from_name":  "Unknown",
 *     "subject":    "Hello",
 *     "received_at": "2026-06-25T10:00:00Z",
 *     "reason":      "global ACL",
 *     "reported":    false   // true 表示已包含在某次巡检报告中
 *   }
 * ]
 *
 * 巡检报告发出后，相关条目标记 reported=true。
 * 超过 RETENTION_DAYS 天且已上报的条目会被自动清理。
 */

const fs   = require('fs');
const path = require('path');
const os   = require('os');

const DEFAULT_STORE_DIR  = path.join(os.homedir(), '.ilink-email-bridge');
const DEFAULT_LOG_FILE   = path.join(DEFAULT_STORE_DIR, 'denied-log.json');
const RETENTION_DAYS     = 30;

class DeniedLog {
  /**
   * @param {string} [logFile]
   */
  constructor(logFile = DEFAULT_LOG_FILE) {
    this._file = logFile;
    this._data = null;
  }

  // ── public API ────────────────────────────────────────────────────────────

  /**
   * Record a denied message.
   * @param {object} msgSummary   From +list (message_id, from, subject)
   * @param {string} reason
   */
  record(msgSummary, reason) {
    this._load();
    // Deduplicate by message_id
    if (this._data.some((e) => e.message_id === msgSummary.message_id)) return;
    this._data.push({
      message_id:  msgSummary.message_id,
      from_email:  msgSummary.from?.email || '',
      from_name:   msgSummary.from?.name  || '',
      subject:     msgSummary.subject     || '',
      received_at: new Date().toISOString(),
      reason,
      reported:    false,
    });
    this._save();
  }

  /**
   * Return all unreported entries.
   * @returns {object[]}
   */
  getUnreported() {
    this._load();
    return this._data.filter((e) => !e.reported);
  }

  /**
   * Mark a batch of message_ids as reported.
   * @param {string[]} messageIds
   */
  markReported(messageIds) {
    this._load();
    const ids = new Set(messageIds);
    for (const e of this._data) {
      if (ids.has(e.message_id)) e.reported = true;
    }
    this._save();
  }

  /**
   * Remove entries that are reported and older than RETENTION_DAYS.
   */
  cleanup() {
    this._load();
    const cutoff = Date.now() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
    const before = this._data.length;
    this._data = this._data.filter((e) => {
      if (!e.reported) return true;
      return new Date(e.received_at).getTime() > cutoff;
    });
    if (this._data.length !== before) this._save();
  }

  // ── private ───────────────────────────────────────────────────────────────

  _load() {
    if (this._data !== null) return;
    try {
      if (fs.existsSync(this._file)) {
        this._data = JSON.parse(fs.readFileSync(this._file, 'utf8'));
        if (!Array.isArray(this._data)) this._data = [];
        return;
      }
    } catch { /* ignore */ }
    this._data = [];
  }

  _save() {
    try {
      fs.mkdirSync(path.dirname(this._file), { recursive: true });
      fs.writeFileSync(this._file, JSON.stringify(this._data, null, 2), 'utf8');
    } catch (err) {
      process.stderr.write(`[denied-log] Cannot save: ${err.message}\n`);
    }
  }
}

module.exports = { DeniedLog };
