/**
 * The app shell.
 *
 * Everything here is presentation. The two things that matter — that the
 * connection needs no DNS, CA or signaling server, and that the returned bytes
 * are verified against their address before use — happen in `fetch-client.js`
 * and inside the WASM module respectively.
 */

import { connect } from './fetch-client.js'
import { retrieve } from './retrieve.js'
import init, * as wasm from './wasm/autonomi_wasm_client.js'

/**
 * The demo proxy, hard-coded so the page works on first load.
 *
 * The certificate hash and peer id are derived from the proxy's persisted
 * identity, so this string is only valid for as long as that host keeps its
 * `state_dir`. Overridable in the UI and via `?proxy=` for anyone running
 * their own — which is the point: the client is safe against any proxy.
 */
const DEFAULT_PROXY = '/ip4/168.119.152.203/udp/4001/webrtc-direct/certhash/uEiA3d0SdhihawXZKcdRIC2kIzsRTL-toghlU6ruqZ4lUxQ/p2p/12D3KooWCebzDQspE5tiSv2XNYvZ363Pw6tmfdpDynCwW8L2yd6p'

/** Documents offered by default. Addresses are plain 64-character hex. */
const CATALOGUE = [
  {
    title: 'BegBlag.mp3',
    address: '00ac7cbe1fe3e49fcd9e490eb313fabc2fe4407e67196292e961c3b34e9b1afa',
    filename: 'BegBlag.mp3',
    type: 'audio/mpeg'
  }
]

const el = (id) => document.getElementById(id)
const state = { client: null, wasmReady: false }

async function ensureWasm () {
  if (state.wasmReady) return

  // The single-file build embeds the module here; the multi-file build fetches
  // it. The path is given explicitly in the latter case because bundling moves
  // the glue code: by default it resolves the binary next to itself, which
  // after bundling is the site root rather than the directory it was published
  // to.
  const embedded = globalThis.__AUTONOMI_WASM_GZ__
  await init({
    module_or_path: embedded
      ? await inflateBase64(embedded)
      : new URL('./wasm/autonomi_wasm_client_bg.wasm', document.baseURI)
  })

  state.wasmReady = true
}

/**
 * Decode a gzipped, Base64-encoded WASM module.
 *
 * Gzipping first recovers most of Base64's ~33% overhead, and `DecompressionStream`
 * means no decompressor has to be shipped alongside it.
 */
async function inflateBase64 (base64) {
  const binary = atob(base64)
  const packed = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) packed[i] = binary.charCodeAt(i)

  const stream = new Blob([packed]).stream().pipeThrough(new DecompressionStream('gzip'))
  return new Uint8Array(await new Response(stream).arrayBuffer())
}

/** Connect, reusing an existing connection to the same proxy. */
async function ensureConnected (multiaddr) {
  if (state.client?.multiaddr === multiaddr) return state.client

  if (state.client) {
    await state.client.close().catch(() => {})
    state.client = null
  }

  setStatus('connecting', `Connecting to ${shorten(multiaddr)} …`)
  const client = await connect(multiaddr)
  client.multiaddr = multiaddr
  state.client = client
  setStatus('ok', `Connected directly to ${client.peerId.slice(0, 16)}… — no DNS, no CA, no signaling server, nothing installed.`)
  return client
}

async function download (address, filename, mimeType) {
  const multiaddr = el('proxy').value.trim()
  if (!multiaddr) {
    setStatus('error', 'Enter a WebRTC-Direct endpoint multiaddr first.')
    return
  }

  setBusy(true)
  el('result').innerHTML = ''
  try {
    await ensureWasm()
    const client = await ensureConnected(multiaddr)

    const addressBytes = wasm.parse_hex_address(address)
    setStatus('working', 'Fetching data map …')

    const started = performance.now()
    const plaintext = await retrieve(wasm, client.getChunk, addressBytes, ({ held, total }) => {
      setStatus('working', `Verified ${held} of ${total} chunks …`)
      el('bar').style.width = `${Math.round((held / total) * 100)}%`
    })
    const seconds = ((performance.now() - started) / 1000).toFixed(1)

    setStatus('ok',
      `${formatBytes(plaintext.length)} retrieved in ${seconds}s. ` +
      'Every chunk was verified against its address and decrypted in this browser.')
    present(plaintext, filename, mimeType)
  } catch (error) {
    console.error(error)
    setStatus('error', error.message ?? String(error))
  } finally {
    setBusy(false)
    el('bar').style.width = '0%'
  }
}

/** Offer the decrypted bytes as a download, and play them if playable. */
function present (bytes, filename, mimeType) {
  const blob = new Blob([bytes], { type: mimeType || 'application/octet-stream' })
  const url = URL.createObjectURL(blob)

  const link = document.createElement('a')
  link.href = url
  link.download = filename
  link.className = 'button primary'
  link.textContent = `Save ${filename} (${formatBytes(bytes.length)})`

  el('result').append(link)

  if ((mimeType || '').startsWith('audio/')) {
    const audio = document.createElement('audio')
    audio.controls = true
    audio.src = url
    el('result').append(audio)
  }

  // The browser holds the blob until the link is used; revoking immediately
  // would break the download.
  link.addEventListener('click', () => setTimeout(() => URL.revokeObjectURL(url), 60_000))
}

function setStatus (kind, message) {
  const status = el('status')
  status.className = `status ${kind}`
  status.textContent = message
}

function setBusy (busy) {
  document.querySelectorAll('button').forEach((b) => { b.disabled = busy })
  el('progress').hidden = !busy
}

function shorten (multiaddr) {
  const parts = multiaddr.split('/')
  const ip = parts[2] ?? '?'
  const port = parts[4] ?? '?'
  return `${ip}:${port}`
}

function formatBytes (n) {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`
  return `${(n / 1024 / 1024).toFixed(1)} MiB`
}

function build () {
  const list = el('catalogue')
  for (const entry of CATALOGUE) {
    const row = document.createElement('div')
    row.className = 'row'

    const label = document.createElement('div')
    label.innerHTML = `<strong>${entry.title}</strong><code>${entry.address}</code>`

    const button = document.createElement('button')
    button.className = 'button primary'
    button.textContent = 'Download'
    button.addEventListener('click', () => download(entry.address, entry.filename, entry.type))

    row.append(label, button)
    list.append(row)
  }

  el('fetch-custom').addEventListener('click', () => {
    const address = el('address').value.trim()
    if (!/^[0-9a-fA-F]{64}$/.test(address)) {
      setStatus('error', 'An address is exactly 64 hex characters.')
      return
    }
    download(address, `${address.slice(0, 12)}.bin`, '')
  })

  el('secure').textContent = window.isSecureContext
    ? `secure context (${location.protocol}) — WebRTC available`
    : `NOT a secure context (${location.protocol}) — WebRTC will be blocked`
  el('secure').className = window.isSecureContext ? 'note ok' : 'note error'
}

// A `?proxy=` parameter wins over the built-in default, so an operator can
// hand out a link pointing at their own proxy:
//   index.html?proxy=/ip4/1.2.3.4/udp/4001/webrtc-direct/certhash/…/p2p/…
const fromUrl = new URLSearchParams(location.search).get('proxy')
el('proxy').value = fromUrl ?? (DEFAULT_PROXY.startsWith('/') ? DEFAULT_PROXY : '')

build()
setStatus('idle', 'Ready — pick a file, or paste any content address.')
