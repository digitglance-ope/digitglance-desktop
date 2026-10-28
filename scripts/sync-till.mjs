#!/usr/bin/env node
// Vendors the offline till (apps/till in the web monorepo) into this repo.
//
//   node scripts/sync-till.mjs <path to the digitglance web repo checkout> [--no-build]
//
// Runs `pnpm --filter till build` in that checkout, then replaces till-dist/
// here with apps/till/dist and writes till-dist/SOURCE.json
// { repo, branch, commit, dirty, builtAt }. The built files are committed, so
// the release and MSIX workflows need no access to the web repo: DigitGlance
// Trade's frontendDist (src-tauri/tauri.trade.conf.json) is ../till-dist.
//
// It also copies the Trade launcher page (dist/trade/index.html, the
// connectivity gate that opens the web back office) to
// till-dist/backoffice/index.html, which the `backoffice` window loads.
//
// Commit the result with the commit message naming SOURCE.json's commit.

import { spawnSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.resolve(here, '..')
const args = process.argv.slice(2)
const repoArg = args.find((a) => !a.startsWith('--'))
const noBuild = args.includes('--no-build')

if (!repoArg) {
  console.error('Usage: node scripts/sync-till.mjs <path to the digitglance web repo> [--no-build]')
  process.exit(2)
}
const repo = path.resolve(repoArg)
const tillDir = path.join(repo, 'apps', 'till')
if (!existsSync(path.join(tillDir, 'package.json'))) {
  console.error(`No apps/till in ${repo}. Pass the web monorepo checkout (or a worktree of it).`)
  process.exit(2)
}

function run(cmd, cmdArgs, cwd, capture = false) {
  const r = spawnSync(cmd, cmdArgs, {
    cwd,
    stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit',
    encoding: 'utf8',
    // pnpm is a .cmd shim on Windows.
    shell: process.platform === 'win32' && cmd === 'pnpm',
  })
  if (r.status !== 0) {
    console.error(`${cmd} ${cmdArgs.join(' ')} failed in ${cwd} (exit ${r.status}).`)
    process.exit(1)
  }
  return capture ? r.stdout.trim() : ''
}

if (!noBuild) run('pnpm', ['--filter', 'till', 'build'], repo)

const dist = path.join(tillDir, 'dist')
if (!existsSync(path.join(dist, 'index.html'))) {
  console.error(`${dist}/index.html is missing: the till build did not produce a bundle.`)
  process.exit(1)
}

const commit = run('git', ['rev-parse', 'HEAD'], repo, true)
const branch = run('git', ['rev-parse', '--abbrev-ref', 'HEAD'], repo, true)
const dirty = run('git', ['status', '--porcelain', '--', 'apps/till', 'packages'], repo, true) !== ''
const remote = spawnSync('git', ['config', '--get', 'remote.origin.url'], { cwd: repo, encoding: 'utf8' })
const repoUrl = (remote.stdout || '').trim().replace(/\.git$/, '') || 'digitglance-ope/digitglance'

const out = path.join(root, 'till-dist')
rmSync(out, { recursive: true, force: true })
mkdirSync(out, { recursive: true })
cpSync(dist, out, { recursive: true })

// The back-office window's local launcher (connectivity gate -> the web app).
const launcher = path.join(root, 'dist', 'trade')
mkdirSync(path.join(out, 'backoffice'), { recursive: true })
// Byte for byte as committed: a Windows checkout with core.autocrlf has CRLF in
// the working copy, so take the files from git (LF) rather than the disk.
for (const f of readdirSync(launcher)) {
  const rel = `dist/trade/${f}`
  const r = spawnSync('git', ['show', `HEAD:${rel}`], { cwd: root, encoding: 'buffer', maxBuffer: 64 * 1024 * 1024 })
  if (r.status === 0) writeFileSync(path.join(out, 'backoffice', f), r.stdout)
  else cpSync(path.join(launcher, f), path.join(out, 'backoffice', f), { recursive: true })
}

const source = {
  repo: repoUrl.replace(/^https:\/\/github\.com\//, '').replace(/^git@github\.com:/, ''),
  branch,
  commit,
  dirty,
  builtAt: new Date().toISOString(),
}
writeFileSync(path.join(out, 'SOURCE.json'), JSON.stringify(source, null, 2) + '\n')
console.log(`till-dist/ updated from ${source.repo}@${commit.slice(0, 10)}${dirty ? ' (with uncommitted changes)' : ''}.`)
