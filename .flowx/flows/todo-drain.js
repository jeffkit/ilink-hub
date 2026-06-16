#!/usr/bin/env node
/**
 * todo-drain.js — ilink-hub 技术债批量消化 orchestrator
 *
 * 用法：
 *   node .flowx/flows/todo-drain.js --todo docs/TODO.md --repo .
 *   node .flowx/flows/todo-drain.js --run-id <id>          # 断点续跑
 *   node .flowx/flows/todo-drain.js --dry-run              # 只显示分组，不执行
 *   node .flowx/flows/todo-drain.js --list                 # 列出历史 run
 *
 * 更方便的方式（在项目根目录）：
 *   npm run todo-dry        # 预览分组
 *   npm run todo-p1         # 只跑 P1
 *   npm run todo-drain      # 全量运行
 *
 * 设计：
 *   - 解析/分组/回写（本文件 + todo-parser.js）是 ilink-hub 特定业务逻辑
 *   - fanOut / Checkpoint 等编排原语来自 @force-lab/flowx（零运行时依赖库）
 *   - force-dev flow 也来自 @force-lab/flowx，负责单个 feature 的完整开发闭环
 */

import { parseArgs } from 'util'
import { readFileSync, writeFileSync, existsSync, readdirSync, mkdirSync, copyFileSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import { Checkpoint, fanOut, archiveChildRun } from '@force-lab/flowx'
import { parseTodos, groupTodos, groupToFeaturePrompt } from './todo-parser.js'

const __dirname = dirname(fileURLToPath(import.meta.url))
// force-dev.js 来自已安装的 @force-lab/flowx 包
const FORCE_DEV = new URL('../../flows/force-dev.js', import.meta.url).pathname

// ── CLI 参数解析 ───────────────────────────────────────────────────────
const { values: opts } = parseArgs({
  options: {
    'run-id':     { type: 'string' },
    todo:         { type: 'string' },
    repo:         { type: 'string', default: process.cwd() },
    list:         { type: 'boolean', default: false },
    'dry-run':    { type: 'boolean', default: false },
    model:        { type: 'string' },
    reviewer:     { type: 'string', default: 'claude' },
    skip:         { type: 'string', default: '' },
    priority:     { type: 'string', default: '' },
    inplace:      { type: 'boolean', default: false },
    concurrency:  { type: 'string', default: '1' },
  }
})

const concurrency = Math.max(1, parseInt(opts.concurrency, 10) || 1)

if (opts.list) { listRuns(); process.exit(0) }

const runId        = opts['run-id'] ?? `drain-${Date.now()}`
const repo         = opts.repo
const skipNames    = opts.skip     ? opts.skip.split(',').map(s => s.trim())     : []
const filterPriority = opts.priority ? opts.priority.split(',').map(s => s.trim()) : []

// ── 主流程 ────────────────────────────────────────────────────────────
const cp = new Checkpoint(runId, join(repo, '.flowx/runs'))

const todoPath = opts.todo ?? cp.getPauseContext().todoPath
if (!todoPath) {
  console.error('缺少 --todo 参数（指向 TODO.md 的路径）')
  process.exit(1)
}

console.log(`\n▶ todo-drain  run=${runId}  repo=${repo}  todo=${todoPath}`)
if (opts['dry-run']) console.log('  [dry-run 模式，只显示分组]\n')

await run()

// ── 阶段实现 ──────────────────────────────────────────────────────────

async function run() {
  const groups = await cp.step('parse-todos', async () => {
    const content = readFileSync(todoPath, 'utf8')
    const items = parseTodos(content)
    let groups = groupTodos(items)

    if (filterPriority.length > 0)
      groups = groups.filter(g => filterPriority.includes(g.priority))

    console.log(`\n  解析到 ${items.length} 条 open 条目，分为 ${groups.length} 组`)
    groups.forEach((g, i) => {
      const skipMark = skipNames.includes(g.name) ? ' [SKIP]' : ''
      console.log(`    ${i + 1}. [${g.priority}] ${g.name} (${g.items.length} 条)${skipMark}`)
    })
    return groups
  })

  if (opts['dry-run']) {
    console.log('\n[dry-run] 分组如上，退出。')
    console.log(`\n运行命令：\n  node .flowx/flows/todo-drain.js --todo ${todoPath} --repo ${repo} --run-id ${runId}`)
    process.exit(0)
  }

  let skipped = 0, alreadyDone = 0, completed = 0, failed = 0
  const tasks = []

  for (const group of groups) {
    const stepKey = `group.${group.name}`
    if (skipNames.includes(group.name)) { skipped++;    continue }
    if (cp.has(stepKey))                { alreadyDone++; continue }

    const featureName      = `todo-${group.name}`
    const featurePromptFile = join(repo, '.flowx', `prompt-${group.name}.md`)
    mkdirSync(dirname(featurePromptFile), { recursive: true })
    writeFileSync(featurePromptFile, groupToFeaturePrompt(group))

    const args = ['--feature', featureName, '--reviewer', opts.reviewer, '--prompt-file', featurePromptFile]
    if (opts.model) args.push('--model', opts.model)

    tasks.push({ name: featureName, flow: FORCE_DEV, runId: `${runId}-${group.name}`, args, _group: group, _stepKey: stepKey })
  }

  if (tasks.length > 0) {
    const live   = concurrency === 1
    const logDir = join(repo, '.flowx/runs', runId)
    console.log(`\n  待跑 ${tasks.length} 组，并发度 ${concurrency}${live ? '' : `（日志见 ${logDir}/<组>.log）`}`)

    await fanOut(tasks, {
      repo,
      concurrency,
      isolate: opts.inplace ? 'none' : 'worktree',
      timeout: 7_200_000,
      logDir:  live ? undefined : logDir,
      onData:  live ? (d) => process.stdout.write(d) : undefined,
      // worktree 隔离时把 .flowx/config.json 拷进去，force-dev 才能读到质量门配置
      prepare: (_task, { worktree }) => {
        if (!worktree) return
        const srcCfg = join(repo, '.flowx', 'config.json')
        if (existsSync(srcCfg)) {
          mkdirSync(join(worktree, '.flowx'), { recursive: true })
          copyFileSync(srcCfg, join(worktree, '.flowx', 'config.json'))
        }
      },
      onResult: ({ task, result, worktree }) => {
        const success = result.ok
        const reason  = success ? 'ok' : (result.spawnError ? 'spawn error' : `exit ${result.exitCode}`)
        cp.record(task._stepKey, { success, reason, featureName: task.name, forceDevRunId: task.runId, worktree })
        cp.event('group', { name: task.name, status: success ? 'done' : 'failed', reason, childRunId: task.runId })
        archiveChildRun(repo, worktree, task.runId)
        if (success) {
          completed++
          markTodosAsDone(todoPath, task._group.items.map(it => it.id))
          console.log(`  ✓ ${task.name}`)
        } else {
          failed++
          console.warn(`  ✗ ${task.name} 未完成（${reason}）`)
          const errOut = (result.stderr || result.stdout || '').trim()
          if (errOut) {
            const preview = errOut.split('\n').slice(-20).map(l => `       ${l}`).join('\n')
            console.warn(`     ↳ 输出末尾:\n${preview}`)
          }
        }
      },
    })
  }

  await cp.step('final-report', async () => {
    const total   = groups.length - skipped
    const summary = `${completed + alreadyDone}/${total} 组完成，${failed} 组失败，${skipped} 组跳过`
    console.log(`\n${'═'.repeat(60)}\n✓ todo-drain 完成  ${summary}`)
    return summary
  })

  cp.done({ completed: completed + alreadyDone, failed, skipped, total: groups.length })
}

// ── TODO.md 回写 ──────────────────────────────────────────────────────

function markTodosAsDone(todoPath, ids) {
  if (!existsSync(todoPath)) return
  let content = readFileSync(todoPath, 'utf8')
  for (const id of ids) {
    const pat = new RegExp(
      `(###\\s+${escapeRegex(id)}\\s*[·•\\-][\\s\\S]*?)(- \\*\\*状态\\*\\*：)open`, 'g'
    )
    content = content.replace(pat, '$1$2done')
  }
  writeFileSync(todoPath, content)
  console.log(`  ✓ TODO.md 已更新：${ids.join(', ')} → done`)
}

function escapeRegex(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

// ── 列出历史 run ──────────────────────────────────────────────────────

function listRuns() {
  const dir = join(repo, '.flowx/runs')
  if (!existsSync(dir)) { console.log('无历史 run'); return }
  readdirSync(dir).filter(id => id.startsWith('drain-')).forEach(id => {
    try {
      const s = JSON.parse(readFileSync(join(dir, id, 'state.json'), 'utf8'))
      console.log(`${id}  status=${s.status}  step=${s.currentStep ?? s.status}`)
    } catch { /* 跳过损坏的 state */ }
  })
}
