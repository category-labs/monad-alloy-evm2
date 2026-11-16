import { spawnSync } from 'node:child_process'
import { closeSync, copyFileSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const crate = fileURLToPath(new URL('..', import.meta.url))
const output = path.resolve(crate, '../../target/monad-evm2js')
const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm'
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { stdio: 'inherit', ...options })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error(`${command} failed (${result.status})`)
  return result
}

const packed = run(npm, ['pack', '--ignore-scripts', '--json', '--pack-destination', output], {
  cwd: path.join(output, 'package'), stdio: 'pipe', encoding: 'utf8'
})
const [archive] = JSON.parse(packed.stdout)
const project = mkdtempSync(path.join(tmpdir(), 'monad-evm2js-package-'))
try {
  writeFileSync(path.join(project, 'package.json'), '{"private":true,"type":"module"}\n')
  run(npm, ['install', '--ignore-scripts', '--no-audit', '--no-fund', path.join(output, archive.filename)], { cwd: project })
  const installed = JSON.parse(readFileSync(path.join(project, 'node_modules/monad-evm2js/package.json'), 'utf8'))
  if (installed.scripts || installed.dependencies) throw new Error('The runtime package must be self-contained with no install scripts')
  const reports = {}
  // Native reports use the normal native crypto backends; WASM uses pure Rust.
  for (const [name, example] of Object.entries({ precompiles: 'precompile_results', transactions: 'transaction_results', staking: 'staking_results', debugging: 'debugging_results' })) {
    const file = path.join(project, `native-${name}.json`)
    const fd = openSync(file, 'w')
    try {
      run('cargo', ['+1.96.0', 'run', '--locked', '-p', 'monad-evm2js', '--example', example, '--features', 'monad-evm2/default'], {
        cwd: path.resolve(crate, '../..'), stdio: ['ignore', fd, 'inherit']
      })
    } finally { closeSync(fd) }
    reports[name] = file
  }
  copyFileSync(path.join(crate, 'tests/types.ts'), path.join(project, 'types.ts'))
  run(process.execPath, [path.join(crate, 'node_modules/typescript/bin/tsc'), '--noEmit', '--strict', '--target', 'ES2022', '--module', 'NodeNext', path.join(project, 'types.ts')])
  run(process.execPath, ['--test', ...['package', 'precompiles', 'transactions', 'staking', 'debugging', 'browser'].map(name => path.join(crate, `tests/${name}.test.mjs`))], {
    env: { ...process.env, MONAD_EVM2JS_TEST_PROJECT: project,
      MONAD_EVM2JS_BROWSER_PACKAGE: path.join(project, 'node_modules/monad-evm2js'),
      MONAD_EVM2JS_NATIVE_RESULTS: reports.precompiles, MONAD_EVM2JS_NATIVE_TRANSACTIONS: reports.transactions,
      MONAD_EVM2JS_NATIVE_STAKING: reports.staking, MONAD_EVM2JS_NATIVE_DEBUGGING: reports.debugging }
  })
  console.log(`Verified installable package: ${path.join(output, archive.filename)}`)
} finally {
  rmSync(project, { recursive: true, force: true })
}
