import init, { Engine, initSync, type MonadSpec, type ExecuteRequest, type ExecutionResult, type StakingRequest, type StakingResult } from 'monad-evm2js'

async function execute(bytes: Uint8Array, spec: MonadSpec, request: ExecuteRequest, staking: StakingRequest) {
  await init({ module_or_path: bytes })
  initSync({ module: bytes })
  const engine = new Engine(spec, 1337)
  engine.fund(request.from, '0x1')
  engine.set_block_hash(1, '0x' + '00'.repeat(32))
  const result: ExecutionResult = JSON.parse(engine.execute(JSON.stringify(request)))
  const transition: StakingResult = JSON.parse(engine.staking_transition(JSON.stringify(staking)))
  engine.restore(engine.snapshot())
  const account: string = engine.account(request.from)
  engine.free()
  return { result, transition, account }
}
void execute
