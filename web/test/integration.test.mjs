/**
 * End-to-end test: a real proxy process, a real WebRTC-Direct connection, and
 * the real WASM client.
 *
 * This is the test that answers the question the whole PoC hangs on — whether
 * Rust `libp2p-webrtc` and JavaScript `@libp2p/webrtc` actually interoperate —
 * and it does so without a browser, so it can run in CI.
 *
 * Run with:  npm test
 * Requires:  cargo build (proxy) and npm run build:wasm-node (client)
 */

import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { createInterface } from 'node:readline'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import assert from 'node:assert/strict'
import path from 'node:path'
import fs from 'node:fs/promises'
import test from 'node:test'

import { connect } from '../src/fetch-client.js'
import { retrieve, toHex } from '../src/retrieve.js'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.resolve(here, '../..')
const PROXY_BIN = path.join(root, 'crates/proxy/target/debug/autonomi-webrtc-proxy')

/** Scratch directory for generated fixtures; not checked in. */
const WORK = path.join(root, 'fixtures')

/**
 * Write a fixture and return its path.
 *
 * Fixtures are generated rather than committed so a fresh clone can run the
 * suite with nothing but the two build steps.
 */
async function fixture (name, contents) {
  await fs.mkdir(WORK, { recursive: true })
  const file = path.join(WORK, name)
  await fs.writeFile(file, contents)
  return file
}

/** Compressible text, small enough that each chunk fits in one transport message. */
function textFixture () {
  const sentence =
    'The Autonomi network stores data by content address. This file exists only to be ' +
    'self-encrypted, fetched through a WebRTC DataChannel, verified against BLAKE3 ' +
    'addresses in the browser, and decrypted there. '
  return Buffer.from(sentence.repeat(400))
}

/**
 * Incompressible bytes, large enough that chunks span hundreds of 16 KiB
 * transport messages.
 *
 * Compressible content would collapse to a single message and silently skip
 * what the multi-message test exists to cover.
 */
function binaryFixture (bytes) {
  const payload = Buffer.alloc(bytes)
  for (let i = 0; i < payload.length; i += 4) {
    payload.writeUInt32LE((i * 2654435761) >>> 0, i)
  }
  return payload
}

/** Start the proxy on an ephemeral port and wait until it reports its multiaddr. */
async function startProxy (fixturePath) {
  const stateDir = await fs.mkdtemp(path.join(root, '.proxy-state-test-'))
  const proxy = spawn(PROXY_BIN, [
    '--bind', '127.0.0.1',
    '--port', '0',
    '--fixture', fixturePath,
    '--state-dir', stateDir
  ], { stdio: ['ignore', 'pipe', 'pipe'] })

  let stderr = ''
  proxy.stderr.on('data', (d) => { stderr += d })

  const lines = createInterface({ input: proxy.stdout })
  let multiaddr, dataMapAddress

  const timeout = setTimeout(() => proxy.kill('SIGKILL'), 20_000)
  try {
    for await (const line of lines) {
      const dial = line.match(/multiaddr\s*:\s*(\S+)/)
      if (dial) multiaddr = dial[1]
      const address = line.match(/fixture\s*:\s*([0-9a-f]{64})/)
      if (address) dataMapAddress = address[1]
      if (multiaddr && dataMapAddress) break
    }
  } finally {
    clearTimeout(timeout)
  }

  if (!multiaddr || !dataMapAddress) {
    proxy.kill('SIGKILL')
    throw new Error(`proxy did not report a multiaddr.\n${stderr}`)
  }

  return {
    multiaddr,
    dataMapAddress,
    stop: async () => {
      proxy.kill('SIGTERM')
      await once(proxy, 'exit').catch(() => {})
      await fs.rm(stateDir, { recursive: true, force: true })
    }
  }
}

/**
 * The WASM client, built for the nodejs target.
 *
 * wasm-bindgen's `nodejs` target emits CommonJS, which this package's
 * `"type": "module"` would otherwise reject, so load it through `require`.
 */
async function loadWasm () {
  const require = createRequire(import.meta.url)
  try {
    return require('../src/wasm-node/autonomi_wasm_client.js')
  } catch (error) {
    throw new Error(
      'WASM client not built. Run `npm run build:wasm-node` first.\n' + error.message
    )
  }
}

test('browser client retrieves and decrypts a file over WebRTC-Direct', async (t) => {
  const wasm = await loadWasm()
  const expected = textFixture()
  const proxy = await startProxy(await fixture('hello.txt', expected))
  t.after(() => proxy.stop())

  const client = await connect(proxy.multiaddr)
  t.after(() => client.close())

  const address = wasm.parse_hex_address(proxy.dataMapAddress)

  const progress = []
  const plaintext = await retrieve(wasm, client.getChunk, address, (p) => progress.push(p))

  assert.deepEqual(Buffer.from(plaintext), expected, 'decrypted content must match the original')
  assert.ok(progress.length > 0, 'progress should be reported')
  assert.equal(progress.at(-1).held, progress.at(-1).total, 'all chunks accounted for')
})

test('a chunk served under the wrong address is rejected', async (t) => {
  const wasm = await loadWasm()
  const proxy = await startProxy(await fixture('hello.txt', textFixture()))
  t.after(() => proxy.stop())

  const client = await connect(proxy.multiaddr)
  t.after(() => client.close())

  const address = wasm.parse_hex_address(proxy.dataMapAddress)

  // Deliberately single-stepped rather than run through `retrieve`: with
  // several fetches in flight, a torn-down sibling stream can reject first and
  // the test would pass without ever exercising the verification path.
  const dataMapBytes = await client.getChunk(address)
  const retrieval = wasm.Retrieval.begin(address, dataMapBytes)

  const target = retrieval.required_addresses().subarray(0, 32)
  const honest = await client.getChunk(target)

  const tampered = honest.slice()
  tampered[0] ^= 0x01

  assert.throws(
    () => retrieval.supply(target, tampered),
    /failed verification/,
    'a modified chunk must be rejected'
  )

  // The honest bytes for the same address are still accepted, proving the
  // rejection was about the content and not the request.
  retrieval.supply(target, honest)
})

test('transfers a response spanning many transport messages', async (t) => {
  const wasm = await loadWasm()

  // Responses larger than one transport message used to stall forever: a chunk
  // of 15,020 bytes arrived in 7 ms while one of 20,000 bytes never arrived.
  const payload = binaryFixture(6 * 1024 * 1024)
  const proxy = await startProxy(await fixture('multi-message.bin', payload))
  t.after(() => proxy.stop())

  const client = await connect(proxy.multiaddr)
  t.after(() => client.close())

  const address = wasm.parse_hex_address(proxy.dataMapAddress)
  const plaintext = await retrieve(wasm, client.getChunk, address)

  assert.deepEqual(Buffer.from(plaintext), payload, 'large content must survive the transfer')
})

test('an unknown address reports not-found rather than hanging', async (t) => {
  const proxy = await startProxy(await fixture('hello.txt', textFixture()))
  t.after(() => proxy.stop())

  const client = await connect(proxy.multiaddr)
  t.after(() => client.close())

  const unknown = new Uint8Array(32).fill(0xaa)
  assert.equal(await client.getChunk(unknown), null, `expected null for ${toHex(unknown)}`)
})
