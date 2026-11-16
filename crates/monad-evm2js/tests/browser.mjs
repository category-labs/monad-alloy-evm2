import init, { Engine, initSync } from '/package/dist/monad_evm2js.js'

export async function exercise(sync = false) {
  if (sync) initSync({ module: await (await fetch('/package/dist/monad_evm2js_bg.wasm')).arrayBuffer() })
  else await init()
  const sender = '0x' + '11'.repeat(20)
  const zero = '0x' + '00'.repeat(32)
  const results = []
  for (const fork of ['monad-eight', 'monad-nine', 'monad-ten', 'monad-next']) {
    const vm = new Engine(fork, 1337)
    const restored = new Engine(fork, 1337)
    try {
      vm.fund(sender, '0x3635c9adc5dea00000')
      const execute = (to, data, commit, target = vm) => JSON.parse(target.execute(JSON.stringify({
        from: sender, to, data, value: '0x0', gas: 1_000_000, gasPrice: '0x1',
        nonce: JSON.parse(target.account(sender)).nonce, blockNumber: 2, timestamp: 1800000000,
        blockGasLimit: 30_000_000, baseFee: '0x0', beneficiary: '0x' + '00'.repeat(20),
        prevrandao: zero, commit, trace: true
      })))
      const runtime = fork === 'monad-next' ? 'ae005f55602a5f5260205ff3' : '602a5f555f545f5260205ff3'
      const size = (runtime.length / 2).toString(16).padStart(2, '0')
      const deploy = execute(null, `0x60${size}600c60003960${size}6000f3${runtime}`, true)
      if (!deploy.success) throw new Error('Deployment failed')
      const before = vm.snapshot()
      const call = execute(deploy.createdAddress, '0x', false)
      if (before !== vm.snapshot()) throw new Error('Simulation changed state')
      const tx = execute(deploy.createdAddress, '0x', true)
      vm.set_block_hash(1, '0x' + 'ab'.repeat(32))
      restored.restore(vm.snapshot())
      if (vm.snapshot() !== restored.snapshot()) throw new Error('Snapshot changed')
      const afterRestore = execute(deploy.createdAddress, '0x', false, restored)
      results.push({ fork, success: call.success && tx.success && afterRestore.success,
        value: BigInt(tx.returnValue).toString(), opcode: tx.structLogs[0].op,
        depth: tx.structLogs[0].depth, deployTraced: deploy.structLogs.length > 0 })
    } finally { vm.free(); restored.free() }
  }
  return results
}
