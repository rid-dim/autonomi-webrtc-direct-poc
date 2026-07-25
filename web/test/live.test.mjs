/**
 * Live end-to-end test against the production Autonomi network.
 *
 * Skipped unless `LIVE=1`, because it needs outbound UDP, working bootstrap
 * peers and content that is actually still stored on the network — none of
 * which belong in a normal test run.
 *
 *   LIVE=1 npm test
 *
 * `LIVE_ADDRESS` overrides the address; `LIVE_EXPECT_PREFIX` (hex) checks the
 * first decrypted bytes, which is how the run proves it recovered a real file
 * rather than merely something self-consistent.
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
import { retrieve } from '../src/retrieve.js'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.resolve(here, '../..')
const PROXY_BIN = path.join(root, 'crates/proxy/target/debug/autonomi-webrtc-proxy')

// A public mp3 on the production network.
const ADDRESS = process.env.LIVE_ADDRESS ??
  '00ac7cbe1fe3e49fcd9e490eb313fabc2fe4407e67196292e961c3b34e9b1afa'

// "ID3" — the tag that opens an mp3 file.
const EXPECT_PREFIX = process.env.LIVE_EXPECT_PREFIX ?? '494433'

const CONNECT_TIMEOUT_MS = 120_000

/** Start the proxy against the live network and wait for its multiaddr. */
async function startLiveProxy () {
  const stateDir = await fs.mkdtemp(path.join(root, '.proxy-state-live-'))
  const proxy = spawn(PROXY_BIN, [
    '--bind', '127.0.0.1',
    '--port', '0',
    '--state-dir', stateDir
  ], { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, RUST_LOG: 'warn' } })

  let stderr = ''
  proxy.stderr.on('data', (d) => { stderr += d })

  const timeout = setTimeout(() => proxy.kill('SIGKILL'), CONNECT_TIMEOUT_MS)
  let multiaddr
  try {
    for await (const line of createInterface({ input: proxy.stdout })) {
      const match = line.match(/multiaddr\s*:\s*(\S+)/)
      if (match) { multiaddr = match[1]; break }
    }
  } finally {
    clearTimeout(timeout)
  }

  if (!multiaddr) {
    proxy.kill('SIGKILL')
    throw new Error(`proxy never reached the network.\n${stderr}`)
  }

  return {
    multiaddr,
    stop: async () => {
      proxy.kill('SIGTERM')
      await once(proxy, 'exit').catch(() => {})
      await fs.rm(stateDir, { recursive: true, force: true })
    }
  }
}

test('retrieves a real file from the production network', {
  skip: process.env.LIVE === '1' ? false : 'set LIVE=1 to run against the live network'
}, async (t) => {
  const wasm = createRequire(import.meta.url)('../src/wasm-node/autonomi_wasm_client.js')

  const proxy = await startLiveProxy()
  t.after(() => proxy.stop())

  const client = await connect(proxy.multiaddr)
  t.after(() => client.close())

  const address = wasm.parse_hex_address(ADDRESS)

  let last = { held: 0, total: 0 }
  const started = Date.now()
  const plaintext = await retrieve(wasm, client.getChunk, address, (p) => { last = p })
  const seconds = ((Date.now() - started) / 1000).toFixed(1)

  const prefix = Buffer.from(plaintext.subarray(0, EXPECT_PREFIX.length / 2)).toString('hex')
  console.log(
    `retrieved ${plaintext.length} bytes in ${last.total} chunks ` +
    `(${seconds}s), starts with 0x${prefix}`
  )

  assert.ok(plaintext.length > 0, 'expected a non-empty file')
  assert.equal(prefix, EXPECT_PREFIX, 'decrypted content should start with the expected magic bytes')
  assert.equal(last.held, last.total, 'every chunk should be verified')

  // Kept so the result can be played by hand.
  const out = path.join(root, 'fixtures', 'live-download.bin')
  await fs.mkdir(path.dirname(out), { recursive: true })
  await fs.writeFile(out, plaintext)
})
