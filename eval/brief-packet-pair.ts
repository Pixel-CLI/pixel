// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import { createHash, randomUUID } from 'node:crypto'
import { access, mkdir, readFile, readdir, realpath, rm, writeFile } from 'node:fs/promises'
import path from 'node:path'

const frozenCommit = '5c47874700c23a6c9e976de3f553ffe75bac39d8'
const image = 'sha256:1426219784d53f6e86e4a305a96544a2baf5a648d12e894801ef8461b7f4ebd7'
const model = 'gpt-5.6-terra'
const effort = 'medium'
const tasks = ['g7-lookup-handleerror', 'g8-lookup-custommenu'] as const
const arms = ['native-agent', 'native-packet', 'pixel-packet'] as const
const reps = 2
const maxPacketBytes = 2048
const argument = (name: string, envName?: string) => {
  const equals = process.argv.find((arg) => arg.startsWith(`--${name}=`))
  if (equals) return equals.slice(name.length + 3)
  const index = process.argv.indexOf(`--${name}`)
  if (index >= 0) return process.argv[index + 1]
  return envName ? process.env[envName] : undefined
}
const repoArg = argument('repo', 'PIXEL_PACKET_REPO')
const authArg = argument('auth', 'CODEX_AUTH_FILE')
const graphArg = argument('graph', 'PIXEL_PACKET_GRAPH')
const resultArg = argument('results', 'PIXEL_PACKET_RESULTS')
const repo = repoArg ? path.resolve(repoArg) : undefined
const authFile = authArg ? path.resolve(authArg) : undefined
const preparedGraphFile = graphArg ? path.resolve(graphArg) : undefined
const results = resultArg ? path.resolve(resultArg) : undefined
const preflightOnly = process.argv.includes('--preflight-only')
const exportExisting = argument('export-existing')
const receiptArg = argument('receipt')
const runId = `brief-packet-${new Date().toISOString().replaceAll(/[-:.TZ]/g, '')}-${randomUUID().slice(0, 8)}`
const scenarioDir = path.join(import.meta.dir, 'scenarios')
const defaultResultsRoot = path.join(import.meta.dir, 'arena-results')

const limitations = [
  'The packet parser takes the first identifier inside backticks; multiple identifiers or non-backticked names are not disambiguated.',
  'Declaration discovery uses regular expressions for variable/function declarations and a lightweight delimiter scan; it is not a TypeScript parser and does not safely model comments, regex literals, or complex template literals.',
  'The frozen evaluation covers only the two exact-lookup fixtures g7 and g8. It makes no completeness claim for other languages, question kinds, or repositories.',
]

type CommandResult = {
  argv: string[]
  cwd?: string
  exitCode: number
  stdout: string
  stderr: string
  wallMs: number
}

type Scenario = { id: string; prompt: string; must: Array<{ pattern: string; points: number; why: string }> }
type PacketReceipt = {
  backend: 'native' | 'pixel'
  task: string
  rep: number
  query: CommandResult
  packMs: number
  queryMs: number
  totalMs: number
  packetBytes: number
  packetSha256: string
  packet: string
  complete: boolean
  reason?: string
}

type PacketEvidenceReceipt = {
  kind: 'packet'
  backend: 'native' | 'pixel'
  task: string
  rep: number
  query: CommandResult
  packMs: number
  queryMs: number
  totalMs: number
  packetBytes: number
  packetSha256: string
  packet: string
  complete: boolean
  reason?: string
}

type PathParityReceipt = {
  kind: 'path-parity'
  task: string
  rep: number
  equal: boolean
  nativePaths: string[]
  pixelPaths: string[]
}

const sha256 = (value: string | Uint8Array) => createHash('sha256').update(value).digest('hex')

const run = async (argv: string[], cwd?: string, extraEnv: Record<string, string> = {}): Promise<CommandResult> => {
  const start = performance.now()
  const child = Bun.spawn(argv, {
    cwd,
    env: { ...process.env, ...extraEnv, NO_COLOR: '1', TERM: 'dumb' },
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  return { argv, cwd, exitCode, stdout, stderr, wallMs: performance.now() - start }
}

const assert: (condition: unknown, message: string) => asserts condition = (condition, message) => {
  if (!condition) throw new Error(message)
}

const getScenario = async (task: string): Promise<Scenario> =>
  JSON.parse(await readFile(path.join(scenarioDir, `${task}.json`), 'utf8')) as Scenario

const extractIdentifier = (prompt: string) => prompt.match(/`([A-Za-z_$][\w$]*)`/)?.[1] ?? null

const normalizePaths = (stdout: string, root: string) => [...new Set(stdout
  .split(/\r?\n/)
  .map((line) => line.trim().replace(/^\u001b\[[0-9;]*m/g, ''))
  .filter((line) => line && !line.startsWith('[') && !line.startsWith('{'))
  .map((line) => line.replace(/^\/repo\//, '').replace(`${root}/`, '').replace(/^\.\//, '')))]
  .sort((left, right) => left.localeCompare(right))

const safeSourcePath = async (root: string, relative: string) => {
  if (!relative || path.isAbsolute(relative) || relative.split(/[\\/]/).includes('..')) return null
  const rootReal = await realpath(root)
  const candidate = path.resolve(rootReal, relative)
  if (!candidate.startsWith(`${rootReal}${path.sep}`)) return null
  const resolved = await realpath(candidate).catch(() => null)
  return resolved && resolved.startsWith(`${rootReal}${path.sep}`) ? resolved : null
}

const findDeclaration = (source: string, identifier: string) => {
  const lines = source.split(/\r?\n/)
  const patterns = [
    new RegExp(`^\\s*(?:export\\s+)?(?:declare\\s+)?(?:const|let|var)\\s+${identifier}\\b`),
    new RegExp(`^\\s*(?:export\\s+)?(?:async\\s+)?function\\s+${identifier}\\b`),
  ]
  const lineIndex = lines.findIndex((line) => patterns.some((pattern) => pattern.test(line)))
  if (lineIndex < 0) return null
  const startOffset = lines.slice(0, lineIndex).reduce((sum, line) => sum + line.length + 1, 0)
  const tail = source.slice(startOffset)
  let curly = 0
  let round = 0
  let square = 0
  let quote = ''
  let escaped = false
  let endOffset = 0
  for (const char of tail) {
    endOffset += 1
    if (quote) {
      if (escaped) escaped = false
      else if (char === '\\') escaped = true
      else if (char === quote) quote = ''
      continue
    }
    if (char === '"' || char === "'" || char === '`') quote = char
    else if (char === '{') curly += 1
    else if (char === '}') curly -= 1
    else if (char === '(') round += 1
    else if (char === ')') round -= 1
    else if (char === '[') square += 1
    else if (char === ']') square -= 1
    if (curly < 0 || round < 0 || square < 0) break
    if (curly === 0 && round === 0 && square === 0 && /[;\n]/.test(char)) break
  }
  const code = tail.slice(0, endOffset).trimEnd()
  const line = lineIndex + 1
  return { line, code }
}

const buildPacket = async (root: string, task: string, rep: number, backend: 'native' | 'pixel', query: CommandResult, scenario: Scenario): Promise<PacketReceipt> => {
  const start = performance.now()
  const identifier = extractIdentifier(scenario.prompt)
  if (!identifier) {
    return { backend, task, rep, query, queryMs: query.wallMs, packMs: 0, totalMs: query.wallMs, packetBytes: 0, packetSha256: sha256(''), packet: '', complete: false, reason: 'prompt identifier was ambiguous or absent' }
  }
  if (query.exitCode !== 0) {
    return { backend, task, rep, query, queryMs: query.wallMs, packMs: 0, totalMs: query.wallMs, packetBytes: 0, packetSha256: sha256(''), packet: '', complete: false, reason: `query failed with ${query.exitCode}` }
  }
  const matches = normalizePaths(query.stdout, root)
  if (
    !matches.length ||
    Buffer.byteLength(query.stdout) >= 1900 ||
    (backend === 'pixel' && query.stderr.includes('stdout cap (PIXEL_OUTPUT_CAP_BYTES)'))
  ) {
    return { backend, task, rep, query, queryMs: query.wallMs, packMs: 0, totalMs: query.wallMs, packetBytes: 0, packetSha256: sha256(''), packet: '', complete: false, reason: 'query returned no paths or may have been capped' }
  }

  const definitions: Array<{ file: string; line: number; code: string }> = []
  for (const relative of matches) {
    const filePath = await safeSourcePath(root, relative)
    if (!filePath) continue
    const source = await readFile(filePath, 'utf8').catch(() => '')
    if (!source) continue
    const declaration = findDeclaration(source, identifier)
    if (declaration) definitions.push({ file: relative, ...declaration })
  }

  if (!definitions.length) {
    return { backend, task, rep, query, queryMs: query.wallMs, packMs: performance.now() - start, totalMs: query.wallMs + performance.now() - start, packetBytes: 0, packetSha256: sha256(''), packet: '', complete: false, reason: `no declaration for ${identifier} found in query paths` }
  }

  const sections = [`Identifier: ${identifier}`, `Matching TypeScript files (${matches.length}):`, ...matches.map((file) => `- ${file}`), 'Declaration evidence:']
  for (const definition of definitions) sections.push(`${definition.file}:${definition.line}\n${definition.code}`)

  // Include local const dependencies named by a declaration initializer. This discovers, for example,
  // the variants object passed into CustomMenu without encoding task-specific paths or expected values.
  const dependencyNames = new Set<string>()
  for (const definition of definitions) {
    const identifiers = definition.code.matchAll(/\b([A-Za-z_$][\w$]*)\b/g)
    for (const [, name] of identifiers) {
      if (name !== identifier && new RegExp(`(?:const|let|var)\\s+${name}\\b`).test(definition.code)) continue
      if (name !== identifier) dependencyNames.add(name)
    }
  }
  const emitted = new Set(definitions.map((definition) => `${definition.file}:${definition.line}`))
  for (const relative of matches) {
    const filePath = await safeSourcePath(root, relative)
    if (!filePath) continue
    const source = await readFile(filePath, 'utf8').catch(() => '')
    if (!source) continue
    for (const name of dependencyNames) {
      const declaration = findDeclaration(source, name)
      if (!declaration || emitted.has(`${relative}:${declaration.line}`)) continue
      if (!new RegExp(`(?:const|let|var)\\s+${name}\\s*=`).test(declaration.code)) continue
      sections.push(`Local dependency ${relative}:${declaration.line}\n${declaration.code}`)
      emitted.add(`${relative}:${declaration.line}`)
    }
  }

  const packet = sections.join('\n')
  const packetBytes = new TextEncoder().encode(packet).byteLength
  const packMs = performance.now() - start
  return {
    backend, task, rep, query, queryMs: query.wallMs, packMs, totalMs: query.wallMs + packMs,
    packetBytes, packetSha256: sha256(packet), packet,
    complete: packetBytes <= maxPacketBytes,
    reason: packetBytes > maxPacketBytes ? `complete evidence exceeds ${maxPacketBytes} bytes` : undefined,
  }
}

const prepareCopy = async (to: string) => {
  await mkdir(path.dirname(to), { recursive: true })
  assert(repo, 'pass --repo or set PIXEL_PACKET_REPO')
  const clone = await run(['rtk', 'git', 'clone', '--quiet', repo, to])
  assert(clone.exitCode === 0, `git clone failed: ${clone.stderr}`)
  const checkout = await run(['rtk', 'git', '-C', to, 'checkout', '--quiet', '--detach', frozenCommit])
  assert(checkout.exitCode === 0, `could not check out frozen fixture commit ${frozenCommit}: ${checkout.stderr}`)
  const checkoutIdentity = await run(['rtk', 'git', '-C', to, 'rev-parse', 'HEAD'])
  assert(checkoutIdentity.stdout.trim() === frozenCommit, `snapshot resolved to unexpected commit: ${checkoutIdentity.stdout.trim()}`)
  let graphBytes = 0
  if (preparedGraphFile) {
    const graph = await readFile(preparedGraphFile)
    await mkdir(path.join(to, '.pixel'), { recursive: true })
    await writeFile(path.join(to, '.pixel/graph.v2.db'), graph)
    graphBytes = graph.byteLength
  }
  const foundSkills = await Promise.all(['.agents/skills', '.codex/skills'].map(async (dir) =>
    readdir(path.join(to, dir)).catch(() => [] as string[])))
  const experimental = new Set(['pixel-question-evidence', 'pixel-question-evidence-filtered'])
  for (const [index, dir] of ['.agents/skills', '.codex/skills'].entries()) {
    for (const name of foundSkills[index] ?? []) if (experimental.has(name)) await rm(path.join(to, dir, name), { recursive: true, force: true })
  }
  return { clone, checkout, checkoutIdentity, graphBytes }
}

const queryPaths = async (root: string, task: string, rep: number, backend: 'native' | 'pixel', identifier: string): Promise<CommandResult> => {
  if (backend === 'native') {
    return run(['rtk', 'rg', '-l', '-F', identifier, '--glob', '*.ts', '--glob', '*.tsx', '--glob', '*.mts', '--glob', '*.cts', root])
  }
  return run([
    'rtk', 'docker', 'run', '--rm', '--volume', `${root}:/repo`, '--env', 'PIXEL_OUTPUT_CAP_BYTES=1900',
    '--entrypoint', 'pixel', image, 'search-content', '-F', identifier, '--type', 'ts',
    '--files-with-matches', '--limit', '100', '/repo',
  ])
}

const rankedPacket = (receipt: PacketReceipt, original: Scenario) =>
  `${original.prompt}\n\nRetrieved evidence packet (precomputed; source excerpts, not an answer):\n${receipt.packet}`

const runModel = async (arm: typeof arms[number], task: string, rep: number, prompt: string, root: string, outputDir: string) => {
  const tag = `${arm}-${task}-${rep}`
  const transcriptPath = path.join(outputDir, `${tag}.jsonl`)
  const stderrPath = path.join(outputDir, `${tag}.stderr`)
  const secondsPath = path.join(outputDir, `${tag}.seconds`)
  const started = performance.now()
  assert(authFile, 'pass --auth or set CODEX_AUTH_FILE')
  const invocation = await run([
    'rtk', 'docker', 'run', '--rm', '--volume', `${root}:/repo`, '--volume', `${authFile}:/root/.codex/auth.json:ro`,
    '--workdir', '/repo', '--entrypoint', 'codex', image,
    'exec', '--json', '--sandbox', 'danger-full-access', '--skip-git-repo-check', '-m', model,
    '-c', `model_reasoning_effort=${effort}`, prompt,
  ])
  const elapsed = (performance.now() - started) / 1000
  await writeFile(transcriptPath, invocation.stdout)
  await writeFile(stderrPath, invocation.stderr)
  await writeFile(secondsPath, String(Math.ceil(elapsed)))
  if (invocation.exitCode !== 0) await writeFile(path.join(outputDir, `${tag}.failed`), String(invocation.exitCode))
  return { tag, exitCode: invocation.exitCode, seconds: elapsed, command: invocation.argv, transcriptPath, stderrPath, stdoutBytes: Buffer.byteLength(invocation.stdout), stderrBytes: Buffer.byteLength(invocation.stderr) }
}

const sanitizePathArg = (value: string) => value
  .replace(/^\/Users\/[^:]+:(\/root\/\.codex\/auth\.json:ro)$/, '<auth-file>:$1')
  .replace(/^\/Users\/[^:]+:(\/repo)$/, '<snapshot>:$1')
  .replace(/^\/tmp\/[^:]+:(\/repo)$/, '<workspace>:$1')
  .replace(/^\/Users\/.*$/, '<local-path>')
  .replace(/^\/tmp\/.*$/, '<temporary-path>')

const sanitizedQuery = (receipt: PacketEvidenceReceipt) => {
  const rootArg = receipt.backend === 'native'
    ? receipt.query.argv.at(-1)
    : receipt.query.argv.find((value) => value.endsWith(':/repo'))?.split(':/repo')[0]
  const stdout = rootArg
    ? receipt.query.stdout.replaceAll(`${rootArg}/`, '').replaceAll('/repo/', '')
    : receipt.query.stdout.replaceAll('/repo/', '')
  return {
    command: receipt.query.argv.map(sanitizePathArg),
    exit_code: receipt.query.exitCode,
    wall_ms: receipt.query.wallMs,
    stdout,
    stderr: receipt.query.stderr.replaceAll(/\/(?:Users|tmp)\/[^\s:]+/g, '<local-path>'),
  }
}

const transcriptResult = async (transcriptPath: string) => {
  const transcript = await readFile(transcriptPath, 'utf8')
  const events = transcript.split(/\r?\n/).filter(Boolean).map((line) => JSON.parse(line) as Record<string, unknown>)
  const answers = events
    .filter((event) => event.type === 'item.completed')
    .map((event) => (event.item as Record<string, unknown> | undefined))
    .filter((item) => item?.type === 'agent_message')
    .map((item) => String(item?.text ?? ''))
  const usage = [...events].reverse().find((event) => event.type === 'turn.completed')?.usage as Record<string, number> | undefined
  return {
    answer: answers.at(-1) ?? '',
    transcript_sha256: sha256(transcript),
    usage: usage ? {
      input_tokens: usage.input_tokens ?? null,
      cached_input_tokens: usage.cached_input_tokens ?? null,
      output_tokens: usage.output_tokens ?? null,
    } : null,
  }
}

const median = (values: number[]) => {
  const sorted = [...values].sort((left, right) => left - right)
  if (!sorted.length) return null
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2
}

const buildTimingMetrics = (runMetadata: Record<string, any>, ranking: Record<string, any>) => {
  const result: Record<string, any> = { run_id: runMetadata.run_id, tasks: {}, arms: {} }
  for (const task of runMetadata.tasks as string[]) {
    result.tasks[task] = {}
    for (const arm of runMetadata.arms as string[]) {
      const scoreRows = (ranking.rows as Array<Record<string, any>>)
        .filter((row) => row.task === task && row.arm === arm)
      const modelRows = (runMetadata.model_receipts as Array<Record<string, any>>)
        .filter((row) => row.task === task && row.arm === arm)
      const endToEnd = modelRows.map((row) => row.seconds + row.packet_query_plus_pack_ms / 1000)
      const tokens = scoreRows.map((row) => row.tokens).filter(Number.isFinite)
      const scores = scoreRows.map((row) => row.score_pct).filter(Number.isFinite)
      result.tasks[task][arm] = {
        reps: modelRows.map((row) => Number(row.rep)),
        model_seconds: modelRows.map((row) => row.seconds),
        retrieval_and_pack_seconds: modelRows.map((row) => row.packet_query_plus_pack_ms / 1000),
        end_to_end_seconds: endToEnd,
        median_end_to_end_seconds: median(endToEnd),
        tokens,
        median_tokens: median(tokens),
        scores_pct: scores,
        median_score_pct: median(scores),
      }
    }
  }
  for (const arm of runMetadata.arms as string[]) {
    const taskMedians = (runMetadata.tasks as string[]).map((task) => result.tasks[task][arm])
    result.arms[arm] = {
      mean_task_median_end_to_end_seconds: taskMedians.reduce((sum, item) => sum + (item.median_end_to_end_seconds ?? 0), 0) / taskMedians.length,
      mean_task_median_tokens: taskMedians.reduce((sum, item) => sum + (item.median_tokens ?? 0), 0) / taskMedians.length,
      macro_quality_pct: taskMedians.reduce((sum, item) => sum + (item.median_score_pct ?? 0), 0) / taskMedians.length,
    }
  }
  const baseline = result.arms['native-agent']
  for (const arm of ['native-packet', 'pixel-packet']) {
    result.arms[arm].end_to_end_time_savings_pct_vs_native_agent =
      100 * (baseline.mean_task_median_end_to_end_seconds - result.arms[arm].mean_task_median_end_to_end_seconds)
      / baseline.mean_task_median_end_to_end_seconds
    result.arms[arm].token_savings_pct_vs_native_agent =
      100 * (baseline.mean_task_median_tokens - result.arms[arm].mean_task_median_tokens)
      / baseline.mean_task_median_tokens
  }
  return result
}

const exportSanitizedReceipt = async (existingResults: string, receiptPath: string) => {
  const metadata = JSON.parse(await readFile(path.join(existingResults, 'run.json'), 'utf8')) as Record<string, any>
  const ranking = JSON.parse(await readFile(path.join(existingResults, 'ranking.json'), 'utf8')) as Record<string, any>
  const timing = JSON.parse(await readFile(path.join(existingResults, 'end-to-end-metrics.json'), 'utf8')) as Record<string, any>
  const codexVersionProbe = await run(['rtk', 'docker', 'run', '--rm', '--entrypoint', 'codex', image, '--version'])
  assert(codexVersionProbe.exitCode === 0, `Codex version probe failed: ${codexVersionProbe.stderr}`)
  const archivedRunnerPath = path.join(existingResults, 'brief-packet-pair.ts')
  const archivedRunner = await readFile(archivedRunnerPath).catch(() => null)
  const runnerHashAtRun = metadata.experimental_runner_sha256 ?? (archivedRunner ? sha256(archivedRunner) : null)
  const archivedRunnerHash = archivedRunner ? sha256(archivedRunner) : null
  const oldRunStatusCheck = archivedRunnerHash === '85d5592b95fbbeb6549e632314108821f9eee9119ef47d54fa9861d59a4ef8d9'
  const queryReceipts = (metadata.packet_receipts as Array<Record<string, any>>)
    .filter((receipt) => (receipt.kind === 'packet' || !receipt.kind) && receipt.backend && receipt.query && receipt.packet)
    .map((receipt) => ({
      task: receipt.task,
      rep: receipt.rep,
      backend: receipt.backend,
      packet_bytes: receipt.packetBytes ?? receipt.packet_bytes,
      packet_sha256: receipt.packetSha256 ?? receipt.packet_sha256,
      complete: receipt.complete,
      query_ms: receipt.queryMs ?? receipt.query_ms,
      packing_ms: receipt.packMs ?? receipt.packing_ms,
      workflow_wall_ms: receipt.totalMs ?? receipt.total_ms,
      command: sanitizedQuery(receipt as PacketEvidenceReceipt),
      packet: receipt.packet,
    }))
  const answers = await Promise.all((ranking.rows as Array<Record<string, any>>).map(async (row) => {
    const stem = `${row.arm}-${row.task}-${row.rep}`
    const final = await transcriptResult(path.join(existingResults, `${stem}.jsonl`))
    return {
      task: row.task,
      arm: row.arm,
      rep: Number(row.rep),
      score: row.score,
      max_score: row.max_score,
      score_pct: row.score_pct,
      input_tokens: row.input_tokens,
      cached_input_tokens: row.cache_read_tokens,
      output_tokens: row.generation_tokens,
      total_tokens: row.tokens,
      cost_usd: row.cost_usd ?? null,
      tool_calls: row.tool_calls,
      pixel_calls: row.pixel_calls,
      native_search_calls: row.native_search_calls,
      seconds_model_only: (metadata.model_receipts as Array<Record<string, any>>)
        .find((receipt) => receipt.task === row.task && receipt.arm === row.arm && Number(receipt.rep) === Number(row.rep))?.seconds ?? null,
      transcript_sha256: final.transcript_sha256,
      final_answer: final.answer,
    }
  }))
  const prompts = await Promise.all((metadata.tasks as string[]).map(async (task) => {
    const scenarioPath = path.join(scenarioDir, `${task}.json`)
    const raw = await readFile(scenarioPath, 'utf8')
    const scenario = JSON.parse(raw) as Scenario
    return {
      task,
      scenario_file: `eval/scenarios/${task}.json`,
      scenario_sha256: sha256(raw),
      prompt: scenario.prompt,
      prompt_sha256: sha256(scenario.prompt),
      rubric: scenario.must,
    }
  }))
  const receipt = {
    experiment: 'brief question preturn packet pilot C1',
    run_id: metadata.run_id,
    provenance: {
      fixture_name: metadata.fixture_name ?? path.basename(String(metadata.source_repo ?? 'fixture')),
      source_commit: metadata.source_commit,
      source_worktree_status_sha256: metadata.source_worktree_status_sha256,
      source_status_unchanged_after_run: metadata.source_status_unchanged_after_run ?? (oldRunStatusCheck ? true : null),
      source_status_check_provenance: metadata.source_status_unchanged_after_run !== undefined
        ? 'recorded by the run'
        : oldRunStatusCheck
          ? 'inferred from the archived original runner, which checked source status after each task'
          : 'not recorded',
      pinned_container_image_id: image,
      pixel_cli_version: metadata.pixel_version,
      prepared_graph_sha256: metadata.graph_sha256 ?? metadata.prepared_graph_sha256 ?? null,
      prepared_graph_bytes: metadata.graph_bytes ?? metadata.prepared_graph_bytes ?? null,
      prepared_graph_setup_ms: metadata.graph_prepare_ms ?? null,
      codex_cli_version: codexVersionProbe.stdout.trim(),
      codex_cli_version_probe: {
        command: ['docker', 'run', '--rm', '--entrypoint', 'codex', '<pinned-image>', '--version'],
        exit_code: codexVersionProbe.exitCode,
        wall_ms: codexVersionProbe.wallMs,
        stdout: codexVersionProbe.stdout.trim(),
      },
      model: metadata.model,
      reasoning_effort: metadata.effort,
      experimental_runner_sha256_at_run: runnerHashAtRun,
      archived_runner_sha256: archivedRunnerHash,
      historical_runner_identity: oldRunStatusCheck ? 'archived original C runner hash verified' : null,
      replay_runner_sha256_after_portability_fixes: sha256(await readFile(path.join(import.meta.dir, 'brief-packet-pair.ts'))),
      portability_changes_after_frozen_run: [
        'Added explicit repo/auth/results/optional graph inputs and a fresh run-id default output directory.',
        'Each clone now checks out and verifies the pinned fixture commit.',
        'Captured the actual Codex CLI version separately from Pixel version.',
        'Added safe source-path resolution and explicit parser scope limitations; no model invocation is performed in export mode.',
      ],
      setup_note: 'A prepared graph was supplied for the frozen run; setup duration was not included in per-question timing. The packet query wall time includes the full host-side RTK/Docker invocation for Pixel and RTK/rg invocation for native.',
      model_invocation_shape: [
        'docker', 'run', '--rm', '--volume', '<snapshot>:/repo', '<read-only-auth-mount; path and value omitted>',
        '--workdir', '/repo', '--entrypoint', 'codex',
        '<pinned-image>', 'exec', '--json', '--sandbox', 'danger-full-access',
        '--skip-git-repo-check', '-m', metadata.model, '-c', `model_reasoning_effort=${metadata.effort}`, '<prompt>',
      ],
    },
    design: {
      tasks: metadata.tasks,
      reps: metadata.reps,
      arms: metadata.arms,
      interleaving: metadata.scheduled_order,
      packet_limit_bytes: maxPacketBytes,
      aggregate_rule: 'For each arm, take the median of its two repetitions within each task, then arithmetic-mean the two task medians. No pooled median is reported.',
      timing_interpretation: 'Charged workflow wall time = Codex invocation wall time + measured precompute query and packing wall time. Pixel query wall time includes Docker startup and is not an intrinsic Pixel-versus-rg speed comparison.',
      limitations: limitations,
    },
    prompts,
    packet_queries_and_contents: queryReceipts,
    answers,
    task_metrics: timing.tasks,
    aggregate_metrics: timing.arms,
  }
  await mkdir(path.dirname(receiptPath), { recursive: true })
  await writeFile(receiptPath, JSON.stringify(receipt, null, 2) + '\n')
  console.log(JSON.stringify({ receipt: receiptPath, answer_rows: answers.length, packet_queries: queryReceipts.length, codex_version: codexVersionProbe.stdout.trim() }, null, 2))
}

const main = async () => {
  if (exportExisting) {
    assert(receiptArg, 'pass --receipt when exporting an existing run')
    await exportSanitizedReceipt(path.resolve(exportExisting), path.resolve(receiptArg))
    return
  }
  assert(repo, 'pass --repo or set PIXEL_PACKET_REPO')
  assert(authFile, 'pass --auth or set CODEX_AUTH_FILE')
  await Promise.all([access(repo), access(authFile)])
  const outputDir = results ?? path.join(defaultResultsRoot, runId)
  await mkdir(outputDir, { recursive: true })
  assert((await readdir(outputDir)).length === 0, `refusing non-empty results directory: ${outputDir}`)
  await writeFile(path.join(outputDir, 'brief-packet-pair.ts'), await readFile(path.join(import.meta.dir, 'brief-packet-pair.ts')))
  const sourceIdentity = await run(['rtk', 'git', '-C', repo, 'rev-parse', 'HEAD'])
  const frozenCommitAvailable = await run(['rtk', 'git', '-C', repo, 'cat-file', '-e', `${frozenCommit}^{commit}`])
  assert(frozenCommitAvailable.exitCode === 0, `fixture repo does not contain pinned commit ${frozenCommit}`)
  const version = await run(['rtk', 'docker', 'run', '--rm', '--entrypoint', 'pixel', image, '--version'])
  const codexVersion = await run(['rtk', 'docker', 'run', '--rm', '--entrypoint', 'codex', image, '--version'])
  assert(version.exitCode === 0, `pinned Pixel image version failed: ${version.stderr}`)
  assert(codexVersion.exitCode === 0, `Codex version probe failed: ${codexVersion.stderr}`)
  const sourceHash = await run(['rtk', 'git', '-C', repo, 'status', '--porcelain'])
  const graph = preparedGraphFile ? await readFile(preparedGraphFile) : null
  const metadata = {
    run_id: runId,
    fixture_name: path.basename(repo),
    source_repo_head_at_start: sourceIdentity.stdout.trim(),
    source_commit: frozenCommit,
    source_worktree_status_before: sourceHash.stdout,
    source_worktree_status_after: null as string | null,
    source_worktree_status_sha256: sha256(sourceHash.stdout),
    pixel_image: image,
    pixel_image_id: image,
    pixel_version: version.stdout.trim(),
    codex_version: codexVersion.stdout.trim(),
    prepared_graph_sha256: graph ? sha256(graph) : null,
    prepared_graph_bytes: graph?.byteLength ?? 0,
    prepared_graph_source_supplied: Boolean(preparedGraphFile),
    model,
    effort,
    arms,
    tasks,
    reps,
    max_packet_bytes: maxPacketBytes,
    treatment: 'two exact lookup prompts; native-agent gets original prompt; native-packet and pixel-packet get the same deterministic, source-derived packet format; measured query and packing workflow wall time is charged to packet-arm end-to-end task time. Pixel query timing includes Docker startup; it is not an intrinsic Pixel-versus-rg speed comparison.',
    limitations,
    image_version_probe: version,
    codex_version_probe: codexVersion,
    source_status_unchanged_after_run: null as boolean | null,
    experimental_runner_sha256: sha256(await readFile(path.join(import.meta.dir, 'brief-packet-pair.ts'))),
    historical_frozen_runner_sha256: '85d5592b95fbbeb6549e632314108821f9eee9119ef47d54fa9861d59a4ef8d9',
    scheduled_order: [] as Array<{ task: string; rep: number; order: string[] }>,
    packet_receipts: [] as Array<PacketEvidenceReceipt | PathParityReceipt>,
    model_receipts: [] as Array<Record<string, unknown>>,
  }
  await writeFile(path.join(outputDir, 'run.json'), JSON.stringify(metadata, null, 2) + '\n')

  for (const [taskIndex, task] of tasks.entries()) {
    const scenario = await getScenario(task)
    const identifier = extractIdentifier(scenario.prompt)
    assert(identifier, `no unique backtick identifier in ${task}`)
    for (let rep = 1; rep <= reps; rep += 1) {
      const prepRoot = path.join(outputDir, 'prep', `${task}-${rep}`)
      const prep = await prepareCopy(prepRoot)
      const nativeQuery = await queryPaths(prepRoot, task, rep, 'native', identifier)
      const nativeReceipt = await buildPacket(prepRoot, task, rep, 'native', nativeQuery, scenario)
      const pixelQuery = await queryPaths(prepRoot, task, rep, 'pixel', identifier)
      const pixelReceipt = await buildPacket(prepRoot, task, rep, 'pixel', pixelQuery, scenario)
      const nativePaths = normalizePaths(nativeQuery.stdout, prepRoot)
      const pixelPaths = normalizePaths(pixelQuery.stdout, prepRoot)
      const pathParity = JSON.stringify(nativePaths) === JSON.stringify(pixelPaths)
      for (const receipt of [nativeReceipt, pixelReceipt]) {
        assert(receipt.complete, `${task} ${receipt.backend} packet incomplete: ${receipt.reason ?? 'unknown'}`)
        metadata.packet_receipts.push({
          kind: 'packet', backend: receipt.backend, task, rep, query: receipt.query,
          packMs: receipt.packMs, queryMs: receipt.queryMs, totalMs: receipt.totalMs,
          packetBytes: receipt.packetBytes, packetSha256: receipt.packetSha256,
          packet: receipt.packet, complete: receipt.complete, reason: receipt.reason,
        })
        await writeFile(path.join(outputDir, `${receipt.backend}-packet-${task}-${rep}.txt`), receipt.packet)
      }
      metadata.packet_receipts.push({ kind: 'path-parity', task, rep, equal: pathParity, nativePaths, pixelPaths })
      const facts = task === 'g7-lookup-handleerror'
        ? ['packages/ui/handleError.ts:4', 'export const handleError = (', 'description: string | null | any[]']
        : ['packages/ui/CustomMenu.ts:48', 'defineMultiStyleConfig({variants})', 'sidebar:', 'navbar:']
      for (const fact of facts) assert(nativeReceipt.packet.includes(fact) && pixelReceipt.packet.includes(fact), `${task} packet is missing verified source fact: ${fact}`)
      assert(nativeReceipt.packetSha256 === pixelReceipt.packetSha256, `${task} native and Pixel packets differ despite identical paths`)
      await writeFile(path.join(outputDir, 'run.json'), JSON.stringify(metadata, null, 2) + '\n')
      if (preflightOnly) {
        await rm(prepRoot, { recursive: true, force: true })
        continue
      }
      const order = [...arms]
      const shift = (taskIndex + rep - 1) % order.length
      const shuffled = [...order.slice(shift), ...order.slice(0, shift)]
      metadata.scheduled_order.push({ task, rep, order: shuffled })
      for (const arm of shuffled) {
        const modelRoot = path.join(outputDir, 'snapshots', `${arm}-${task}-${rep}`)
        const modelPrep = await prepareCopy(modelRoot)
        assert(modelPrep.graphBytes === (graph?.byteLength ?? 0), 'graph copy size changed')
        const packet = arm === 'native-packet' ? nativeReceipt : arm === 'pixel-packet' ? pixelReceipt : null
        const prompt = packet ? rankedPacket(packet, scenario) : scenario.prompt
        const promptSha256 = sha256(prompt)
        const answer = await runModel(arm, task, rep, prompt, modelRoot, outputDir)
        metadata.model_receipts.push({ task, rep, arm, original_prompt_sha256: sha256(scenario.prompt), effective_prompt_sha256: promptSha256, packet_sha256: packet?.packetSha256 ?? null, packet_bytes: packet?.packetBytes ?? 0, packet_query_plus_pack_ms: packet?.totalMs ?? 0, charged_end_to_end_seconds: answer.seconds + (packet?.totalMs ?? 0) / 1000, snapshot_commit: frozenCommit, snapshot_graph_bytes: modelPrep.graphBytes, ...answer })
        await writeFile(path.join(outputDir, 'run.json'), JSON.stringify(metadata, null, 2) + '\n')
      }
      const after = await run(['rtk', 'git', '-C', repo, 'status', '--porcelain'])
      assert(after.exitCode === 0 && after.stdout === sourceHash.stdout, 'source worktree changed during C experiment')
      await rm(prepRoot, { recursive: true, force: true })
    }
  }
  await writeFile(path.join(outputDir, 'run.json'), JSON.stringify(metadata, null, 2) + '\n')
  if (preflightOnly) {
    console.log(JSON.stringify({ run_id: runId, preflight_only: true, output_dir: outputDir, receipts: metadata.packet_receipts }, null, 2))
    return
  }
  const finalSourceHash = await run(['rtk', 'git', '-C', repo, 'status', '--porcelain'])
  assert(finalSourceHash.exitCode === 0, `source status check failed: ${finalSourceHash.stderr}`)
  metadata.source_worktree_status_after = finalSourceHash.stdout
  metadata.source_status_unchanged_after_run = finalSourceHash.stdout === sourceHash.stdout
  assert(metadata.source_status_unchanged_after_run, 'source worktree changed during packet experiment')
  const rank = await run([
    'rtk', 'python3', path.join(import.meta.dir, 'arena/rank.py'), '--results', outputDir,
    '--scenarios-dir', scenarioDir, '--arms', ...arms, '--tasks', ...tasks, '--reps', String(reps),
    '--baseline-arm', 'native-agent', '--run-id', runId, '--model', model, '--effort', effort,
    '--repo-snapshot', `${path.basename(repo)}@${frozenCommit}`, '--pixel-image-id', image,
    '--pixel-source-id', 'existing-pinned-image', '--codex-version', codexVersion.stdout.trim(),
  ])
  await writeFile(path.join(outputDir, 'rank.stdout.txt'), rank.stdout)
  await writeFile(path.join(outputDir, 'rank.stderr.txt'), rank.stderr)
  assert(rank.exitCode === 0, `ranking failed: ${rank.stderr}`)
  const ranking = JSON.parse(await readFile(path.join(outputDir, 'ranking.json'), 'utf8')) as Record<string, any>
  const timing = buildTimingMetrics(metadata, ranking)
  await writeFile(path.join(outputDir, 'end-to-end-metrics.json'), JSON.stringify(timing, null, 2) + '\n')
  await writeFile(path.join(outputDir, 'run.json'), JSON.stringify(metadata, null, 2) + '\n')
  console.log(JSON.stringify({ run_id: runId, output_dir: outputDir, rank: rank.stdout }, null, 2))
}

await main()
