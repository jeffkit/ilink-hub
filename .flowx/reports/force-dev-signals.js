// .flowx/reports/force-dev-signals.js
//
// ilink-hub 项目的 force-dev 业务信号聚合。
// 由 `flowcast dashboard --signal-extractor <本文件>` 加载。
//
// 设计原则：
//   - 只声明"如何聚合"（计数 / 比率 / 分布），不写渲染逻辑
//   - 每个 extractor 是 (events, ctx) => value，value 是数字/对象/null
//   - 抛错时 flowcast 会吞掉，不影响主流程
//   - extractor 文件改了重跑 dashboard 即可生效（无状态）

export default {
  // ── milestone 计划 ────────────────────────────────────────────
  // 每个 force-dev 跑会先 emit 一组 milestone.start，画出"计划里程碑"分布
  'milestone.start': (events) => {
    const e2eCount = events.filter(e => e.e2e).length
    return {
      planned: events.length,
      withE2e: e2eCount,
    }
  },

  // ── review 评级分布 ───────────────────────────────────────────
  // 一行 verdict (PASS / NEEDS_FIX / FAIL / UNKNOWN) 的分布
  // 关键观察：FAIL 表示 agent 主动判断不可修复；UNKNOWN 表示解析失败（看 quality）
  review: (events) => {
    const by = { PASS: 0, NEEDS_FIX: 0, FAIL: 0, UNKNOWN: 0 }
    for (const e of events) by[e.verdict ?? 'UNKNOWN']++
    const total = events.length
    return {
      total,
      byVerdict: by,
      passRate: total ? by.PASS / total : null,
      escalated: by.NEEDS_FIX + by.FAIL,
    }
  },

  // ── fix 轮数分布 ─────────────────────────────────────────────
  // 哪一轮 PASS 占比最高？理想是 1 轮过（fix-1 PASS）
  fix: (events) => {
    const byRound = {}  // {1: {pass: 2, fail: 1}, 2: {...}}
    for (const e of events) {
      if (!byRound[e.round]) byRound[e.round] = { pass: 0, fail: 0, other: 0 }
      if (e.verdict === 'PASS') byRound[e.round].pass++
      else if (e.verdict === 'NEEDS_FIX' || e.verdict === 'FAIL') byRound[e.round].fail++
      else byRound[e.round].other++
    }
    const passEvents = events.filter(e => e.verdict === 'PASS')
    return {
      total: events.length,
      byRound,
      // 1 轮过 = fix-1 是 PASS；2 轮过 = fix-2 是 PASS（fix-1 是 fail）；3 轮过 = fix-3 PASS
      oneShotRate: events.length ? (byRound[1]?.pass ?? 0) / events.length : null,
    }
  },

  // ── escalate 升级率 ──────────────────────────────────────────
  // 修复 3 轮没过的次数（最严重的信号）
  escalate: (events) => ({
    total: events.length,
    milestones: events.map(e => e.milestone),
  }),

  // ── e2e 验证 ─────────────────────────────────────────────────
  // 与 review passRate 对比：review PASS 但 e2e FAIL 的比例 = 误判
  e2e: (events) => {
    const pass = events.filter(e => e.pass).length
    return {
      total: events.length,
      pass,
      passRate: events.length ? pass / events.length : null,
    }
  },

  // ── 提交量 ───────────────────────────────────────────────────
  // 每个 run 一次 commit 事件，count 是 commits ahead
  commit: (events) => {
    const total = events.reduce((s, e) => s + (e.count ?? 0), 0)
    return { runs: events.length, totalCommits: total, avgPerRun: events.length ? total / events.length : 0 }
  },

  // ── PR 关联 ──────────────────────────────────────────────────
  // 列出每个 run 的 PR 编号，跨 run 汇总成 list
  pr: (events) => ({
    total: events.length,
    numbers: events.map(e => e.number).filter(Boolean),
    urls: events.map(e => e.url).filter(Boolean),
  }),
}
