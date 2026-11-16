import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import path from 'node:path'
import { test } from 'node:test'
import { pathToFileURL } from 'node:url'

const require = createRequire(path.join(process.env.MONAD_EVM2JS_TEST_PROJECT, 'package.json'))
const { default: init, Engine } = await import(pathToFileURL(require.resolve('monad-evm2js')))
await init({ module_or_path: readFileSync(require.resolve('monad-evm2js/wasm')) })
const cases = JSON.parse(readFileSync(process.env.MONAD_EVM2JS_NATIVE_DEBUGGING, 'utf8'))
for (const [label, { initial, request, result, state, followup }] of Object.entries(cases)) {
  test(`native/WASM child debugging parity: ${label}`, () => {
    const vm = new Engine(label.split('/')[0], 1337)
    try {
      vm.restore(JSON.stringify(initial))
      const before = vm.snapshot()
      assert.deepEqual(JSON.parse(vm.execute(JSON.stringify({ ...request, commit: false }))), result)
      assert.equal(vm.snapshot(), before)
      assert.deepEqual(JSON.parse(vm.execute(JSON.stringify(request))), result)
      assert.deepEqual(JSON.parse(vm.snapshot()), state)
      vm.restore(JSON.stringify(state))
      if (followup) assert.deepEqual(JSON.parse(vm.execute(JSON.stringify(followup.request))), followup.result)
    } finally { vm.free() }
  })
}
