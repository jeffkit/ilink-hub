'use strict';
/**
 * PendingStore — 本地持久化的邮件回复状态追踪
 *
 * 解决的问题：
 *   agently-cli +read 会同时标记邮件为「已读」。如果后续的 Profile 执行失败
 *   （模型不可用、超时、非零退出等），邮件已是已读状态，不会再被 poll 捡起，
 *   导致「已读但未回复」的邮件永久丢失。
 *
 * 工作流：
 *   1. 邮件 read() 之后立即 add()        → state: pending
 *   2. reply() 成功后 markReplied()       → state: replied
 *   3. dispatch 或 reply 失败时不调用     → state 保持 pending
 *   4. 每轮 poll 调用 getPending()        → 返回所有未回复的邮件摘要
 *      然后对这些邮件做 dispatchById() 重试
 *
 * 存储格式（~/.ilink-email-bridge/pending.json）：
 *   {
 *     "msg_xxx": {
 *       "message_id": "msg_xxx",
 *       "subject": "...",
 *       "from_email": "...",
 *       "added_at": "2026-06-25T04:00:00Z",
 *       "replied": false,
 *       "retries": 0,
 *       "last_error": "模型不可用",
 *       "replied_at": null
 *     }
 *   }
 */

const fs = require('fs');
const path = require('path');
const os = require('os');

const DEFAULT_STORE_DIR = path.join(os.homedir(), '.ilink-email-bridge');
const DEFAULT_STORE_FILE = path.join(DEFAULT_STORE_DIR, 'pending.json');

const MAX_RETRIES = 5;           // 超过此次数不再重试
const RETRY_COOLDOWN_MS = 60_000; // 两次重试之间最少间隔 1 分钟

class PendingStore {
  /**
   * @param {string} [storeFile]  自定义存储文件路径
   */
  constructor(storeFile = DEFAULT_STORE_FILE) {
    this.storeFile = storeFile;
    this._data = null;
  }

  // ---------------------------------------------------------------------------
  // Private: load / save
  // ---------------------------------------------------------------------------

  _load() {
    if (this._data !== null) return;
    try {
      fs.mkdirSync(path.dirname(this.storeFile), { recursive: true });
      if (fs.existsSync(this.storeFile)) {
        this._data = JSON.parse(fs.readFileSync(this.storeFile, 'utf8'));
      } else {
        this._data = {};
      }
    } catch {
      this._data = {};
    }
  }

  _save() {
    try {
      fs.mkdirSync(path.dirname(this.storeFile), { recursive: true });
      fs.writeFileSync(this.storeFile, JSON.stringify(this._data, null, 2), 'utf8');
    } catch (err) {
      process.stderr.write(`[pending-store] Failed to save: ${err.message}\n`);
    }
  }

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  /**
   * 邮件被 read() 后调用，记录为待回复状态。
   *
   * @param {object} msgSummary  来自 listUnread() 的邮件摘要（含 message_id, subject, from）
   */
  add(msgSummary) {
    this._load();
    const id = msgSummary.message_id;
    if (!id) return;
    if (!this._data[id]) {
      this._data[id] = {
        message_id: id,
        subject: msgSummary.subject || '',
        from_email: msgSummary.from?.email || '',
        from_name: msgSummary.from?.name || '',
        added_at: new Date().toISOString(),
        replied: false,
        retries: 0,
        last_error: null,
        replied_at: null,
      };
      this._save();
    }
  }

  /**
   * reply() 成功后调用，标记为已回复。
   *
   * @param {string} messageId
   */
  markReplied(messageId) {
    this._load();
    if (this._data[messageId]) {
      this._data[messageId].replied = true;
      this._data[messageId].replied_at = new Date().toISOString();
      this._save();
    }
  }

  /**
   * 回复失败时调用，记录错误并增加重试计数。
   *
   * @param {string} messageId
   * @param {string} [errorMessage]
   */
  markFailed(messageId, errorMessage = '') {
    this._load();
    if (this._data[messageId]) {
      this._data[messageId].retries = (this._data[messageId].retries || 0) + 1;
      this._data[messageId].last_error = errorMessage;
      this._data[messageId].last_failed_at = new Date().toISOString();
      this._save();
    }
  }

  /**
   * 返回所有「应该重试」的邮件摘要（未回复 + 未超过重试上限 + 超过冷却期）。
   *
   * 冷却规则：
   *  - 刚 add() 的消息（无 last_failed_at）必须等满 RETRY_COOLDOWN_MS 才进入重试队列，
   *    防止 poll handler 正在处理时 retry sweep 并发重复 dispatch。
   *  - 失败后重试同样需要等 RETRY_COOLDOWN_MS。
   *
   * @returns {object[]}
   */
  getPending() {
    this._load();
    const now = Date.now();
    return Object.values(this._data).filter((entry) => {
      if (entry.replied) return false;
      if ((entry.retries || 0) >= MAX_RETRIES) return false;
      // 新消息初始冷却：距 add() 时间不足 RETRY_COOLDOWN_MS，跳过
      // 这样 poll handler 的 dispatchAndReply 有足够时间完成并调用 markReplied
      const addedAt = entry.added_at ? new Date(entry.added_at).getTime() : 0;
      if (now - addedAt < RETRY_COOLDOWN_MS) return false;
      // 失败冷却：上次失败后至少等 RETRY_COOLDOWN_MS
      if (entry.last_failed_at) {
        const lastFailed = new Date(entry.last_failed_at).getTime();
        if (now - lastFailed < RETRY_COOLDOWN_MS) return false;
      }
      return true;
    });
  }

  /**
   * 清理已回复超过 7 天的条目，避免文件无限增长。
   */
  cleanup() {
    this._load();
    const cutoff = Date.now() - 7 * 24 * 60 * 60 * 1000;
    let changed = false;
    for (const [id, entry] of Object.entries(this._data)) {
      if (entry.replied && entry.replied_at) {
        if (new Date(entry.replied_at).getTime() < cutoff) {
          delete this._data[id];
          changed = true;
        }
      }
    }
    if (changed) this._save();
  }
}

module.exports = { PendingStore };
