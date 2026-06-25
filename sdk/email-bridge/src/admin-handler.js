'use strict';
/**
 * AdminHandler — 管理员指令处理 + 巡检报告
 *
 * 指令格式（邮件正文，每条独占一行，忽略大小写）：
 *   /allow user@example.com
 *   /allow @example.com
 *   /deny  user@example.com
 *   /deny  @example.com
 *   /reset user@example.com    ← 从动态名单移除（恢复静态配置行为）
 *   /status                    ← 回复当前 ACL 状态快照
 *
 * 巡检报告：
 *   累计未上报的被拒绝邮件达到 min_denied_count 时（或定时触发），
 *   向所有 admin_senders 发送汇总邮件，邮件底部附带可直接回复执行的
 *   /allow /deny 指令说明。
 */

const COMMAND_RE = /^\s*(\/allow|\/deny|\/reset|\/status)\s*([^\s#]*)?/i;

/**
 * Parse admin commands from an email body string.
 * Returns an array of { cmd, arg } objects.
 *
 * @param {string} body  Plain-text email body (quoted content already stripped)
 * @returns {{ cmd: string, arg: string }[]}
 */
function parseCommands(body) {
  const commands = [];
  for (const line of (body || '').split('\n')) {
    const m = line.match(COMMAND_RE);
    if (!m) continue;
    const cmd = m[1].toLowerCase();
    const arg = (m[2] || '').trim();
    // /allow, /deny, /reset require an argument; /status does not
    if (cmd !== '/status' && !arg) continue;
    commands.push({ cmd, arg });
  }
  return commands;
}

class AdminHandler {
  /**
   * @param {import('./acl-config').AclConfig} aclConfig
   * @param {import('./denied-log').DeniedLog}  deniedLog
   * @param {object} mailClient  AgentlyMailClient instance
   * @param {object} [opts]
   * @param {boolean} [opts.dryRun]
   */
  constructor(aclConfig, deniedLog, mailClient, opts = {}) {
    this._acl      = aclConfig;
    this._log      = deniedLog;
    this._mail     = mailClient;
    this._dryRun   = opts.dryRun || false;
    this._reportTimer = null;
  }

  // ── command handling ──────────────────────────────────────────────────────

  /**
   * True if the email body contains at least one recognisable admin command.
   * @param {string} body
   * @returns {boolean}
   */
  hasCommands(body) {
    return parseCommands(body).length > 0;
  }

  /**
   * Execute all commands found in the email body and reply with results.
   *
   * @param {string} messageId
   * @param {string} body         Plain-text body (quoted content stripped)
   * @param {string} fromEmail    Sender (already verified to be admin)
   */
  async executeCommands(messageId, body, fromEmail) {
    const commands = parseCommands(body);
    if (commands.length === 0) return;

    const lines = [`已处理 ${commands.length} 条指令：`, ''];

    for (const { cmd, arg } of commands) {
      switch (cmd) {
        case '/allow':
          if (!arg) { lines.push(`⚠️  /allow 缺少参数`); break; }
          if (!this._dryRun) this._acl.dynamicAllow([arg]);
          lines.push(`✅ /allow ${arg}${this._dryRun ? ' [DRY_RUN]' : ''}`);
          break;

        case '/deny':
          if (!arg) { lines.push(`⚠️  /deny 缺少参数`); break; }
          if (!this._dryRun) this._acl.dynamicDeny([arg]);
          lines.push(`✅ /deny ${arg}${this._dryRun ? ' [DRY_RUN]' : ''}`);
          break;

        case '/reset':
          if (!arg) { lines.push(`⚠️  /reset 缺少参数`); break; }
          if (!this._dryRun) this._acl.dynamicReset([arg]);
          lines.push(`✅ /reset ${arg}${this._dryRun ? ' [DRY_RUN]' : ''}`);
          break;

        case '/status': {
          const snap = this._acl.dynamicSnapshot();
          const staticAllowed = this._acl.allowedSenders.filter(
            (a) => !snap.allowed.includes(a),
          );
          const staticDenied = this._acl.deniedSenders.filter(
            (a) => !snap.denied.includes(a),
          );
          lines.push('📋 当前 ACL 状态：');
          lines.push(`  静态白名单：${staticAllowed.join(', ') || '（无）'}`);
          lines.push(`  静态黑名单：${staticDenied.join(', ')  || '（无）'}`);
          lines.push(`  动态白名单：${snap.allowed.join(', ')  || '（无）'}`);
          lines.push(`  动态黑名单：${snap.denied.join(', ')   || '（无）'}`);
          lines.push(`  开放访问：${this._acl.isOpenAccess() ? '是' : '否'}`);
          lines.push(`  拒绝动作：${this._acl.denyAction}`);
          break;
        }

        default:
          lines.push(`❓ 未知指令：${cmd}`);
      }
    }

    lines.push('', _commandHelpFooter());

    const replyBody = lines.join('\n');
    process.stderr.write(`[admin] Executed ${commands.length} command(s) from ${fromEmail}\n`);

    if (!this._dryRun) {
      try {
        this._mail.reply(messageId, replyBody, { bodyFormat: 'plain' });
      } catch (err) {
        process.stderr.write(`[admin] Reply failed: ${err.message}\n`);
      }
    } else {
      process.stderr.write(`[admin][DRY_RUN] Would reply:\n${replyBody}\n`);
    }
  }

  // ── inspection report ─────────────────────────────────────────────────────

  /**
   * Start the periodic inspection report scheduler.
   * Reports are sent only when there are unreported denied messages.
   */
  startReportScheduler() {
    const cfg = this._acl.reportConfig;
    if (cfg.enabled === false) return;

    const intervalMs = (cfg.interval_hours || 24) * 60 * 60 * 1000;
    const minCount   = cfg.min_denied_count ?? 1;

    this._reportTimer = setInterval(async () => {
      await this._sendReport(minCount);
    }, intervalMs);

    process.stderr.write(
      `[admin] Report scheduler started (interval=${cfg.interval_hours || 24}h, min=${minCount})\n`,
    );
  }

  stopReportScheduler() {
    if (this._reportTimer) {
      clearInterval(this._reportTimer);
      this._reportTimer = null;
    }
  }

  /**
   * Immediately check and send a report if threshold is met.
   * Called by startReportScheduler on each tick; also exposed for testing.
   *
   * @param {number} [minCount=1]
   */
  async _sendReport(minCount = 1) {
    const admins = this._acl.adminSenders;
    if (admins.length === 0) return;

    const unreported = this._log.getUnreported();
    if (unreported.length < minCount) return;

    const reportedIds = unreported.map((e) => e.message_id);
    const body        = this._buildReportBody(unreported);

    process.stderr.write(
      `[admin] Sending inspection report: ${unreported.length} denied message(s) to ${admins.join(', ')}\n`,
    );

    if (this._dryRun) {
      process.stderr.write(`[admin][DRY_RUN] Report body:\n${body}\n`);
      this._log.markReported(reportedIds);
      return;
    }

    for (const adminEmail of admins) {
      try {
        this._mail.send(
          adminEmail,
          `[Email Bridge] 访问控制巡检报告（${unreported.length} 封被拒绝邮件）`,
          body,
          { bodyFormat: 'plain' },
        );
        process.stderr.write(`[admin] Report sent to ${adminEmail}\n`);
      } catch (err) {
        process.stderr.write(`[admin] Report send failed (${adminEmail}): ${err.message}\n`);
      }
    }

    this._log.markReported(reportedIds);
    this._log.cleanup();
  }

  // ── private helpers ───────────────────────────────────────────────────────

  _buildReportBody(entries) {
    const lines = [
      `Email Bridge 访问控制巡检报告`,
      `生成时间：${new Date().toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai' })}`,
      `共 ${entries.length} 封邮件因访问控制被拒绝：`,
      '',
      '─'.repeat(60),
    ];

    // Group by sender
    const bySender = {};
    for (const e of entries) {
      const key = e.from_email || '(unknown)';
      (bySender[key] = bySender[key] || []).push(e);
    }

    for (const [senderEmail, msgs] of Object.entries(bySender)) {
      const name = msgs[0].from_name ? `${msgs[0].from_name} <${senderEmail}>` : senderEmail;
      lines.push(`发件人：${name}`);
      lines.push(`邮件数：${msgs.length}`);
      lines.push('邮件列表：');
      for (const m of msgs) {
        const ts = new Date(m.received_at).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai' });
        lines.push(`  [${ts}] ${m.subject || '(无主题)'}`);
      }
      lines.push('');
    }

    lines.push('─'.repeat(60));
    lines.push('');
    lines.push('如需放行或封禁，请直接回复本邮件，在正文中添加指令（每条单独一行）：');
    lines.push('');
    lines.push(_commandHelpFooter());

    return lines.join('\n');
  }
}

function _commandHelpFooter() {
  return [
    '可用指令：',
    '  /allow user@example.com   — 放行指定邮箱',
    '  /allow @example.com       — 放行整个域名',
    '  /deny  user@example.com   — 封禁指定邮箱',
    '  /deny  @example.com       — 封禁整个域名',
    '  /reset user@example.com   — 从动态名单移除（恢复静态配置）',
    '  /status                   — 查看当前 ACL 状态',
  ].join('\n');
}

module.exports = { AdminHandler, parseCommands };
