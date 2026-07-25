/**
 * Transport half of the browser client: a WebRTC-Direct connection to a proxy,
 * and the one RPC spoken over it.
 *
 * There is no DNS lookup, no certificate authority and no signaling server in
 * this path. The multiaddr carries the proxy's IP, port and the hash of its
 * self-signed DTLS certificate; the browser pins that hash instead of
 * validating a chain. That is what makes the connection possible from an
 * ordinary https:// page — or from a local file — without any infrastructure.
 *
 * This module deliberately knows nothing about data maps, chunks or
 * decryption. It moves opaque bytes; the WASM client decides whether to
 * believe them.
 */

import { createLibp2p } from 'libp2p'
import { webRTCDirect } from '@libp2p/webrtc'
import { noise } from '@chainsafe/libp2p-noise'
import { multiaddr } from '@multiformats/multiaddr'

/** Must match `protocol::PROTOCOL` in the proxy. */
export const PROTOCOL = '/autonomi-fetch/0.1.0'

const ADDRESS_LEN = 32
const STATUS_OK = 0x00
const STATUS_NOT_FOUND = 0x01
const STATUS_REFUSED = 0x02

/** Largest response we will accept, mirroring the proxy's own limit. */
const MAX_RESPONSE_LEN = 8 * 1024 * 1024

/**
 * Dial a proxy and return a handle for fetching chunks from it.
 *
 * @param {string} address - a `/ip4/../udp/../webrtc-direct/certhash/../p2p/..` multiaddr
 * @returns {Promise<{getChunk: (addr: Uint8Array) => Promise<Uint8Array|null>, close: () => Promise<void>, peerId: string}>}
 */
export async function connect (address) {
  const node = await createLibp2p({
    transports: [
      webRTCDirect({
        dataChannel: {
          // A chunk is up to MAX_CHUNK_SIZE (~4 MiB) and arrives as a single
          // response, but libp2p's default read buffer is exactly 4 MiB and a
          // stream that exceeds it is reset. The margin is a few kilobytes, so
          // any delay in draining kills the transfer. Raise it well clear.
          //
          // Undocumented on `DataChannelOptions`, but the muxer spreads these
          // options straight into the stream constructor, which reads it.
          maxReadBufferLength: 16 * 1024 * 1024
        }
      })
    ],
    connectionEncrypters: [noise()],
    // The default gater rejects addresses it considers unroutable, which
    // includes the loopback addresses used for local testing.
    connectionGater: { denyDialMultiaddr: () => false }
  })

  const target = multiaddr(address)
  const connection = await node.dial(target)

  /**
   * Fetch the bytes stored at one content address.
   *
   * Returns `null` when the proxy reports the address as absent. The returned
   * bytes are *unverified* — the caller must check them against `addr`.
   */
  async function getChunk (addr) {
    if (addr.length !== ADDRESS_LEN) {
      throw new Error(`address must be ${ADDRESS_LEN} bytes, got ${addr.length}`)
    }

    // One request per stream keeps framing trivial and lets requests run
    // concurrently over the same (expensive to establish) connection.
    const stream = await connection.newStream(PROTOCOL)
    const read = exactReader(stream)

    try {
      await send(stream, addr)

      const status = (await read(1))[0]
      if (status === STATUS_NOT_FOUND) return null
      if (status === STATUS_REFUSED) {
        throw new Error('proxy refused the address (not on its allowlist)')
      }
      if (status !== STATUS_OK) {
        throw new Error(`proxy returned an unknown status byte: 0x${status.toString(16)}`)
      }

      const header = await read(4)
      const length = new DataView(header.buffer, header.byteOffset, 4).getUint32(0, false)
      if (length > MAX_RESPONSE_LEN) {
        throw new Error(`proxy announced an oversized chunk: ${length} bytes`)
      }

      return await read(length)
    } finally {
      // Close the write side once the response is in, so the proxy sees EOF and
      // can release its end gracefully. A stream the proxy drops *without* a
      // clean close makes `libp2p-webrtc`'s drop listener emit a RESET, which
      // surfaces on this side as "the stream has been reset".
      //
      // Two details, both learned the hard way:
      //
      // - Closing *before* reading looks tidier but breaks large responses: the
      //   FIN lands while the proxy is still writing, and the Rust stream state
      //   machine tears the transfer down mid-flight.
      // - The promise is not awaited, because it only settles on a FIN_ACK that
      //   `libp2p-webrtc-utils` 0.4 never sends. Awaiting would add the full 10s
      //   `DEFAULT_FIN_ACK_TIMEOUT` to every single chunk.
      stream.close().catch(() => {})
    }
  }

  return {
    getChunk,
    peerId: connection.remotePeer.toString(),
    close: async () => { await node.stop() }
  }
}

/**
 * Write bytes, respecting backpressure.
 *
 * `send` returns false when the transport's buffer is full; writing anyway
 * risks the stream being reset, so wait for the drain event first.
 */
async function send (stream, bytes) {
  if (stream.send(bytes)) return
  await new Promise((resolve, reject) => {
    const onDrain = () => { cleanup(); resolve() }
    const onClose = () => { cleanup(); reject(new Error('stream closed while sending')) }
    const cleanup = () => {
      stream.removeEventListener('drain', onDrain)
      stream.removeEventListener('close', onClose)
    }
    stream.addEventListener('drain', onDrain)
    stream.addEventListener('close', onClose)
  })
}

/**
 * Read exactly N bytes at a time from a libp2p stream.
 *
 * libp2p streams deliver arbitrarily-sized pieces — a DataChannel message
 * boundary has nothing to do with our framing — so the reader buffers across
 * deliveries. Returned arrays are copies, so callers may hold them while the
 * buffer moves on.
 */
function exactReader (stream) {
  const iterator = stream[Symbol.asyncIterator]()

  // Pieces are kept as a queue and joined once, when a read is satisfied.
  // Concatenating on every delivery instead would be quadratic: a 4 MiB chunk
  // arrives as ~256 messages of 16 KiB, so growing one array per message copies
  // hundreds of megabytes and stalls the event loop long enough for libp2p's
  // 4 MiB read buffer to overflow, which resets the stream.
  const queue = []
  let queued = 0

  return async function read (wanted) {
    while (queued < wanted) {
      const { value, done } = await iterator.next()
      if (done) {
        throw new Error(`stream ended after ${queued} of ${wanted} bytes`)
      }
      // Deliveries may be a Uint8Array or a Uint8ArrayList.
      const piece = typeof value.subarray === 'function' ? value.subarray() : value
      queue.push(piece)
      queued += piece.length
    }

    const out = new Uint8Array(wanted)
    let filled = 0
    while (filled < wanted) {
      const piece = queue[0]
      const take = Math.min(piece.length, wanted - filled)
      out.set(piece.subarray(0, take), filled)
      filled += take
      if (take === piece.length) {
        queue.shift()
      } else {
        queue[0] = piece.subarray(take)
      }
    }
    queued -= wanted
    return out
  }
}
