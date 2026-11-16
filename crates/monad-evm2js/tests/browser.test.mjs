import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { readFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'
import { chromium } from 'playwright'

const crate = fileURLToPath(new URL('..', import.meta.url))
const packageRoot = process.env.MONAD_EVM2JS_BROWSER_PACKAGE || path.resolve(crate, '../../target/monad-evm2js/package')

test('browser and module worker load WASM and deploy, debug and restore every fork', { timeout: 120_000 }, async () => {
  const server = createServer(async (request, response) => {
    try {
      const url = new URL(request.url, 'http://localhost')
      if (url.pathname === '/') {
        response.setHeader('Content-Type', 'text/html')
        response.end('<!doctype html><title>Monad WASM test</title>')
      } else if (url.pathname === '/worker.mjs') {
        response.setHeader('Content-Type', 'text/javascript')
        response.end("import { exercise } from '/test.mjs'; exercise().then(result => postMessage({ result }), error => postMessage({ error: String(error) }));")
      } else if (url.pathname === '/test.mjs') {
        response.setHeader('Content-Type', 'text/javascript')
        response.end(await readFile(path.join(crate, 'tests/browser.mjs')))
      } else if (url.pathname.startsWith('/package/')) {
        const file = path.resolve(packageRoot, url.pathname.slice('/package/'.length))
        if (!file.startsWith(packageRoot + path.sep)) throw new Error('Invalid path')
        response.setHeader('Content-Type', file.endsWith('.wasm') ? 'application/wasm' : 'text/javascript')
        response.end(await readFile(file))
      } else { response.writeHead(404).end() }
    } catch { response.writeHead(500).end() }
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  let browser
  try {
    browser = await chromium.launch({ headless: true })
    const page = await browser.newPage()
    await page.goto(`http://127.0.0.1:${server.address().port}`)
    const expected = ['monad-eight', 'monad-nine', 'monad-ten', 'monad-next'].map(fork => ({
      fork, success: true, value: '42', opcode: fork === 'monad-next' ? 'CALLSTACKDEPTH' : 'PUSH1',
      depth: 1, deployTraced: true
    }))
    assert.deepEqual(await page.evaluate(async () => (await import('/test.mjs')).exercise()), expected)
    assert.deepEqual(await page.evaluate(() => new Promise((resolve, reject) => {
      const worker = new Worker('/worker.mjs', { type: 'module' })
      worker.onmessage = ({ data }) => { worker.terminate(); data.error ? reject(new Error(data.error)) : resolve(data.result) }
      worker.onerror = event => { worker.terminate(); reject(new Error(event.message)) }
    })), expected)
    // A fresh page exercises the synchronous initializer before any async initialization.
    await page.reload()
    assert.deepEqual(await page.evaluate(async () => (await import('/test.mjs')).exercise(true)), expected)
  } finally {
    await browser?.close()
    await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()))
  }
})
