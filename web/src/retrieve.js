/**
 * The retrieval loop: drives the WASM state machine, feeding it bytes that
 * somebody else fetched.
 *
 * The split matters. This file decides *when* to fetch and how many requests to
 * run at once — scheduling, nothing more. Whether the returned bytes are
 * genuine is decided inside WASM, by hashing them against the address they were
 * requested under. A hostile proxy therefore cannot influence anything here.
 *
 * The WASM module is injected rather than imported so the same code runs under
 * the `nodejs` build (integration tests) and the `web` build (the app).
 */

const ADDRESS_LEN = 32

/**
 * Concurrent chunk requests.
 *
 * Kept at one deliberately. Each request opens its own stream, and a stream is
 * a separate `RTCDataChannel`; several multi-megabyte responses arriving in
 * parallel make Chrome's SCTP layer lose track of a channel, which the proxy
 * reports as `stream N not found` and the browser sees as "the stream has been
 * reset" partway through a download. Node (libdatachannel) tolerates it, so
 * this only shows up in a real browser.
 *
 * The cost is that fetches serialise behind DHT lookups (~5-8s each), which is
 * fine for the demo's file sizes. Raising it needs the protocol to stop opening
 * a channel per chunk — multiplexing requests over one long-lived stream — not
 * just a bigger number here.
 */
const DEFAULT_CONCURRENCY = 1

/** Guards against a malformed data map producing an unbounded resolve loop. */
const MAX_ROUNDS = 16

/**
 * Fetch, verify and decrypt the content stored at a public address.
 *
 * @param {object} wasm - the instantiated `autonomi-wasm-client` module
 * @param {(addr: Uint8Array) => Promise<Uint8Array|null>} getChunk
 * @param {Uint8Array} address - 32-byte content address of the data map
 * @param {(progress: {held: number, total: number}) => void} [onProgress]
 * @returns {Promise<Uint8Array>} the decrypted content
 */
export async function retrieve (wasm, getChunk, address, onProgress = () => {}) {
  const dataMapBytes = await getChunk(address)
  if (dataMapBytes == null) {
    throw new Error('no data stored at that address')
  }

  // Throws if the bytes do not hash to `address`, or if they are not a data map.
  const retrieval = wasm.Retrieval.begin(address, dataMapBytes)

  try {
    for (let round = 0; round < MAX_ROUNDS; round++) {
      if (retrieval.is_complete) break

      const required = splitAddresses(retrieval.required_addresses())
      if (required.length > 0) {
        await forEachConcurrently(required, DEFAULT_CONCURRENCY, async (addr) => {
          const bytes = await getChunk(addr)
          if (bytes == null) {
            throw new Error(`chunk ${toHex(addr)} is missing from the network`)
          }
          // Verifies before accepting; throws on any mismatch.
          retrieval.supply(addr, bytes)
          onProgress({ held: retrieval.chunks_held, total: retrieval.chunk_count })
        })
      }

      // Resolves a shrunk data map into its root form. For a flat map this is a
      // no-op and the loop exits on the next `is_complete` check.
      retrieval.advance()

      if (!retrieval.is_complete && retrieval.required_addresses().length === 0) {
        throw new Error('retrieval stalled: nothing left to fetch but not complete')
      }
    }

    if (!retrieval.is_complete) {
      throw new Error(`data map did not resolve within ${MAX_ROUNDS} rounds`)
    }

    return retrieval.finish()
  } finally {
    retrieval.free?.()
  }
}

/** Split the flat 32-byte-record encoding used across the WASM boundary. */
function splitAddresses (flat) {
  const out = []
  for (let offset = 0; offset < flat.length; offset += ADDRESS_LEN) {
    out.push(flat.subarray(offset, offset + ADDRESS_LEN))
  }
  return out
}

/**
 * Run `task` over `items` with a bounded number in flight.
 *
 * Rejects as soon as any task rejects; a failed verification should stop the
 * download rather than quietly produce partial output.
 */
async function forEachConcurrently (items, limit, task) {
  let next = 0
  const workers = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (true) {
      const index = next++
      if (index >= items.length) return
      await task(items[index])
    }
  })
  await Promise.all(workers)
}

export function toHex (bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')
}
