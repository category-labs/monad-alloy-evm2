import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { copyFileSync, cpSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const crate = fileURLToPath(new URL('..', import.meta.url))
const root = path.resolve(crate, '../..')
const output = path.join(root, 'target/monad-evm2js/package')
const bindingsVersion = '0.2.128'

function run(command, args, capture = false) {
  const result = spawnSync(command, args, { cwd: root, encoding: 'utf8', stdio: capture ? 'pipe' : 'inherit' })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error(`${command} failed (${result.status}): ${result.stderr || ''}`)
  return result.stdout?.trim()
}

if (run('wasm-bindgen', ['--version'], true) !== `wasm-bindgen ${bindingsVersion}`) {
  throw new Error(`Install matching bindings: cargo +1.96.0 install wasm-bindgen-cli --version ${bindingsVersion} --locked`)
}
run('cargo', ['+1.96.0', 'build', '-p', 'monad-evm2js', '--locked', '--profile', 'wasm-release',
  '--target-dir', path.join(root, 'target'), '--target', 'wasm32-unknown-unknown'])
rmSync(output, { recursive: true, force: true })
mkdirSync(path.join(output, 'dist'), { recursive: true })
run('wasm-bindgen', [path.join(root, 'target/wasm32-unknown-unknown/wasm-release/monad_evm2js.wasm'),
  '--target', 'web', '--out-dir', path.join(output, 'dist'), '--out-name', 'monad_evm2js'])
const manifest = JSON.parse(readFileSync(path.join(crate, 'js/package.json'), 'utf8'))
const crateVersion = readFileSync(path.join(crate, 'Cargo.toml'), 'utf8').match(/^version = "([^"]+)"/m)?.[1]
if (manifest.version !== crateVersion) throw new Error('Cargo and npm package versions must match')
writeFileSync(path.join(output, 'package.json'), JSON.stringify(manifest, null, 2) + '\n')
copyFileSync(path.join(crate, 'js/index.d.ts'), path.join(output, 'index.d.ts'))
copyFileSync(path.join(crate, 'README.md'), path.join(output, 'README.md'))
cpSync(path.join(crate, 'docs'), path.join(output, 'docs'), { recursive: true })
copyFileSync(path.join(root, 'LICENSE'), path.join(output, 'LICENSE'))
writeFileSync(path.join(output, 'build-info.json'), JSON.stringify({
  sourceRepository: 'https://github.com/category-labs/monad-alloy-evm2',
  sourceRevision: run('git', ['rev-parse', 'HEAD'], true),
  sourceDirty: run('git', ['status', '--porcelain'], true) !== '',
  cargoLockSha256: createHash('sha256').update(readFileSync(path.join(root, 'Cargo.lock'))).digest('hex'),
  rustToolchain: '1.96.0', wasmBindgen: bindingsVersion, target: 'wasm32-unknown-unknown'
}, null, 2) + '\n')
console.log(`Built ${manifest.name}@${manifest.version}: ${output}`)
