#!/usr/bin/env node
/**
 * todo-parser.js — 解析 ilink-hub 的 TODO.md → 分组 JSON
 *
 * 这是 ilink-hub 特定的解析/分组逻辑，理解该项目的 Markdown 格式和 Rust 模块结构。
 * 通用编排能力（fanOut / Checkpoint）来自 @force-lab/flowx。
 *
 * 用法：
 *   node .flowx/flows/todo-parser.js --todo docs/TODO.md
 *   node .flowx/flows/todo-parser.js --todo docs/TODO.md --out /tmp/groups.json
 */

import { readFileSync, writeFileSync } from 'fs'
import { parseArgs } from 'util'

// ── 解析单条 TODO 条目 ────────────────────────────────────────────────

/**
 * 从 TODO.md 中解析出所有 open 条目。
 *
 * 期望格式（ilink-hub 的 TODO.md）：
 *   总览表：| ID | **P1** | 分类 | 简述 |
 *   条目块：
 *     ### ID · 标题
 *     - **状态**：open
 *     - **文件**：`src/foo/bar.rs:123`
 */
export function parseTodos(content) {
  // 第一步：从总览表提取 ID → severity 映射（bold 和非 bold 格式均支持）
  const severityMap = {}
  for (const line of content.split('\n')) {
    const tableMatch = line.match(/^\|\s*([A-Z0-9_-]+)\s*\|\s*\*{0,2}([^|*]+)\*{0,2}\s*\|/)
    if (tableMatch) {
      const id = tableMatch[1]
      const raw = tableMatch[2].toUpperCase()
      const pMatch = raw.match(/P([123])/)
      if (pMatch) severityMap[id] = `P${pMatch[1]}`
    }
  }

  const items = []
  const blocks = content.split(/^### /m).slice(1)

  for (const block of blocks) {
    const lines = block.trim().split('\n')
    const header = lines[0]

    const headerMatch = header.match(/^([A-Z0-9_-]+)\s*[·•\-]\s*(.+)$/)
    if (!headerMatch) continue
    const id = headerMatch[1]
    const title = headerMatch[2].trim()

    const statusLine = lines.find(l => l.includes('**状态**'))
    if (!statusLine || !statusLine.includes('open')) continue

    const fileLine = lines.find(l => l.includes('**文件**'))
    const files = fileLine
      ? [...fileLine.matchAll(/`([^`]+)`/g)].map(m => m[1])
      : []

    let severity = severityMap[id] ?? 'P2'
    if (!severityMap[id]) {
      const blockSevMatch = block.match(/P([123])\b/)
      if (blockSevMatch) severity = `P${blockSevMatch[1]}`
    }

    const descMatch = block.match(/\*\*(?:问题|攻击场景)\*\*[：:]\s*([\s\S]*?)(?=\n- \*\*|\n###|\n---|\n```|$)/)
    const description = descMatch
      ? descMatch[1].replace(/\n/g, ' ').trim().slice(0, 200)
      : ''

    const fixMatch = block.match(/\*\*修复方向\*\*[：:]\s*([\s\S]*?)(?=\n- \*\*|\n###|\n---|\n```|$)/)
    const fixHint = fixMatch
      ? fixMatch[1].replace(/\n/g, ' ').trim().slice(0, 300)
      : ''

    items.push({ id, title, severity, files, description, fixHint })
  }

  return items
}

// ── 分组逻辑（按 Rust 模块路径聚合）────────────────────────────────

/**
 * P1 独立成组；P2/P3 按 src/<module> 第一级目录分组，每组上限 5 条。
 */
export function groupTodos(items) {
  const p1sec   = items.filter(i => i.severity === 'P1' && i.id.startsWith('SEC'))
  const p1other = items.filter(i => i.severity === 'P1' && !i.id.startsWith('SEC'))
  const p2plus  = items.filter(i => i.severity !== 'P1')

  const buckets = {}
  for (const item of p2plus) {
    const mod = detectModule(item)
    if (!buckets[mod]) buckets[mod] = []
    buckets[mod].push(item)
  }

  const groups = []

  if (p1sec.length > 0) groups.push({
    name: 'security-p1', priority: 'P1',
    description: `P1 安全修复：${p1sec.map(i => i.id).join(', ')}`,
    items: p1sec,
  })

  if (p1other.length > 0) groups.push({
    name: 'reliability-p1', priority: 'P1',
    description: `P1 可靠性修复：${p1other.map(i => i.id).join(', ')}`,
    items: p1other,
  })

  for (const [mod, modItems] of Object.entries(buckets)) {
    const chunks = chunkArray(modItems, 5)
    chunks.forEach((chunk, i) => {
      const suffix = chunks.length > 1 ? `-${i + 1}` : ''
      groups.push({
        name: `${mod}${suffix}`,
        priority: chunk[0].severity,
        description: `${mod} 模块修复：${chunk.map(c => c.id).join(', ')}`,
        items: chunk,
      })
    })
  }

  return groups
}

function detectModule(item) {
  for (const f of item.files) {
    const m = f.match(/^src\/([^/]+)/)
    if (m) return m[1]
  }
  if (/^SEC/.test(item.id)) return 'security'
  if (/^DB/.test(item.id))  return 'store'
  if (/^LOCK|CHAN|MEM|SYNC|TO/.test(item.id)) return 'hub'
  if (/^POLL/.test(item.id)) return 'ilink'
  if (/^MGR|B-/.test(item.id)) return 'bridge'
  if (/^API|A-/.test(item.id)) return 'server'
  return 'misc'
}

function chunkArray(arr, size) {
  const result = []
  for (let i = 0; i < arr.length; i += size) result.push(arr.slice(i, i + size))
  return result
}

// ── feature prompt 生成 ───────────────────────────────────────────────

/**
 * 把分组转成 force-dev 可用的 prompt.md 内容（含具体修复方向）。
 */
export function groupToFeaturePrompt(group) {
  const itemLines = group.items.map(item => {
    const fileStr = item.files.length > 0 ? `\n     文件：${item.files.join(', ')}` : ''
    return `  - [${item.id}] ${item.title}${fileStr}
     问题：${item.description}
     修复方向：${item.fixHint}`
  }).join('\n\n')

  return `修复 ${group.description}

## 待修复条目

${itemLines}

## 完成标准
${group.items.map(i => `- [ ] ${i.id} 修复已提交，相关测试通过`).join('\n')}
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖`
}

// ── CLI 入口 ──────────────────────────────────────────────────────────

if (process.argv[1]?.endsWith('todo-parser.js')) {
  const { values } = parseArgs({
    options: {
      todo: { type: 'string' },
      out:  { type: 'string' },
    }
  })

  if (!values.todo) {
    console.error('用法: node .flowx/flows/todo-parser.js --todo docs/TODO.md [--out groups.json]')
    process.exit(1)
  }

  const content = readFileSync(values.todo, 'utf8')
  const items = parseTodos(content)
  const groups = groupTodos(items)

  console.log(`\n解析到 ${items.length} 条 open 条目，分为 ${groups.length} 组：\n`)
  for (const g of groups) {
    console.log(`  [${g.priority}] ${g.name}  (${g.items.length} 条)`)
    console.log(`         ${g.items.map(i => i.id).join(', ')}`)
  }

  if (values.out) {
    writeFileSync(values.out, JSON.stringify(groups, null, 2))
    console.log(`\n已写入 ${values.out}`)
  }
}
