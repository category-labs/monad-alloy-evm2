# monad-evm2js

Run Monad's evm2 runtime in browsers and Node.js through WebAssembly. The package
provides a local Engine API with JSON requests, debugger traces and version-1
snapshots for Monad Eight, Nine, Ten and Next.

```js
import init, { Engine } from 'monad-evm2js'

await init()
const engine = new Engine('monad-next', 1337)
engine.fund('0x1111111111111111111111111111111111111111', '0x3635c9adc5dea00000')
console.log(JSON.parse(engine.account('0x1111111111111111111111111111111111111111')))
engine.free()
```

The built package includes WASM, an ES module loader and TypeScript declarations.
Applications supply block context, wallets and persistence. Execution is synchronous;
use a worker in the browser. Monad Next is experimental.

- [API, initialization and traces](docs/api.md)
- [Build and tests](docs/development.md)
- [Local staking](docs/staking.md)
