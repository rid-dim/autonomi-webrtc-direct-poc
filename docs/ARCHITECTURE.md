# The layer model: WebRTC is just the tunnel

This page explains the single most important idea in this repository. Everything else —
the proof of concept, the transport bugs, the certificate discussion — follows from it.

## One idea

**WebRTC (or WebTransport) is used purely as a UDP replacement so that browsers can
connect. Every security property lives one level above it, on the communication layer —
which is where the current network implements it already.**

Autonomi nodes today speak a QUIC-based protocol over raw UDP. The UDP datagrams themselves
are, from the network's security model's point of view, just carriers: what makes the network
trustworthy is what rides *inside* them — self-encrypted chunks, BLAKE3 content addressing,
and a post-quantum identity layer, all implemented in the application. In other words, the
quantum-safe part is **not something that needs to be invented for the browser** — it is how
the network works right now, without any browser involved.

A browser cannot send raw UDP. But it can open a WebRTC DataChannel to a bare IP address,
with no DNS name, no CA-signed certificate, and no signaling server. That channel is our
UDP replacement — nothing more. The browser path is therefore maximally similar to the
existing setup: same chunks, same crypto, same verification, one swapped carrier:

![Same data, same result — only the transport wrapper differs](webrtc-tunnel.png)

Both paths in the diagram do exactly the same thing:

1. A client (CLI or browser) asks for a content address.
2. Chunks arrive from nodes — as raw UDP packets on the left, inside a WebRTC channel on
   the right.
3. The client verifies every chunk against its BLAKE3 content address and decrypts it.
4. The client has the file.

Same chunks, same order, same file, same post-quantum encrypted content. **Only the wrapper
around the bytes in flight differs.**

## Why the tunnel is allowed to be "weak"

WebRTC's own encryption (DTLS 1.2, classical elliptic curves) is not quantum-safe, and we
do not care — for the same reason nobody worries that raw UDP has no encryption at all.

Let that also settle the argument before it starts: **the design is end-to-end
post-quantum secure.** Claiming otherwise because "WebRTC isn't PQ" is the same category
error as calling the current network insecure because UDP isn't PQ. The tunnel sits at
the UDP transport level; all communicated content is post-quantum protected at the
application layer on both ends, so what crosses the tunnel is ciphertext that a quantum
adversary recording the wire gains nothing from. The layers stack like this:

```
┌───────────────────────────────────────────────────────────────┐
│  APPLICATION LAYER — where all trust lives                    │
│  • self-encryption (chunks are encrypted before they exist    │
│    on the network)                                            │
│  • BLAKE3 content addressing — the address IS the hash,       │
│    so tampering is detectable by the client alone             │
│  • post-quantum cryptography, implemented in our own code     │
│    (Rust natively, or compiled to WASM in the browser)        │
├───────────────────────────────────────────────────────────────┤
│  TRANSPORT LAYER — untrusted, interchangeable                 │
│  • native nodes:  QUIC over raw UDP                           │
│  • browsers:      WebRTC DataChannel (or WebTransport)        │
│  Treated exactly like UDP: assumed hostile, carries opaque    │
│  ciphertext, contributes nothing to the security model.       │
└───────────────────────────────────────────────────────────────┘
```

Consequences of drawing the line there:

- **No dependency on browser capabilities.** Browser TLS/DTLS stacks cannot do Autonomi's
  raw-public-key, post-quantum handshake, and no browser API exposes that layer. It does
  not matter: quantum-safe crypto runs in WASM, above the transport, in code shipped with
  the page. When browsers gain or lose crypto features, nothing in the security model moves.
- **Nothing sensitive is ever in the tunnel in plaintext.** Chunks are self-encrypted
  before upload and verified after download. A transport that is broken, downgraded, or
  actively malicious can refuse to carry bytes — that is the whole of its power.
- **Transports are swappable.** WebRTC-Direct today, WebTransport tomorrow if ever needed
  (see [TRANSPORT-CHOICE.md](TRANSPORT-CHOICE.md)), raw UDP for native nodes — the
  application layer neither knows nor cares. This is why the code keeps the transport
  behind an abstraction (`LinkTransport`).

## The invariants, stated once

Anything built on this design must preserve these. They are what the proof of concept
verified against the live network (details in [FINDINGS.md](FINDINGS.md)):

1. **The client verifies everything.** A content address is the BLAKE3 hash of its
   content; the client recomputes it for every chunk. Verified identical between
   `ant-protocol` and `self_encryption` (FINDINGS §5).
2. **The transport endpoint performs no cryptography.** It maps `address → bytes` and can
   only refuse to serve. It never sees plaintext and cannot tamper undetected.
3. **Post-quantum protection is application-level.** It must never be delegated to the
   transport's handshake, because the transport is the one layer we do not control on the
   browser side.
4. **The endpoint is replaceable by a stranger's.** Because of 1–3, pointing the client
   at anyone's endpoint yields identical guarantees.

## Building blocks: libraries that are known to work

For anyone (human or LLM) implementing against this design, these are the crates the
proof of concept used or verified, and where they fit:

**Application layer (compiles to `wasm32-unknown-unknown`, runs in the browser):**

- [`self_encryption`](https://github.com/WithAutonomi/self_encryption) — the heart of the
  client: chunk encryption/decryption and verification. **Compiles and runs in WASM
  unmodified**, including its `rayon` paths (empirically verified, FINDINGS §6). Needs
  `getrandom` with the `js` feature.
- `blake3` — content addressing. `compute_address` and `self_encryption`'s
  `content_hash` are the identical function (FINDINGS §5).
- `rmp_serde` — DataMaps on the network are **MessagePack**, not bincode (FINDINGS §3).
- [`saorsa-pqc`](https://github.com/saorsa-labs/saorsa-pqc) — the post-quantum primitives
  (ML-KEM / ML-DSA) the Autonomi stack builds on, for anything beyond content retrieval
  that needs quantum-safe channels at application level.
- `wasm-bindgen` + `web-sys` — the JS boundary; the browser's own `RTCPeerConnection`
  does the tunneling, so no WebRTC stack is needed inside WASM at all.

**Node / endpoint side (native Rust):**

- [`ant-client`](https://github.com/WithAutonomi/ant-client) (`ant-core`) — the whole
  DHT-GET primitive in two calls: `Client::connect(&bootstrap_peers, …)` then
  `client.chunk_get(&address)`. No wallet needed for reads (FINDINGS §2).
- `libp2p-webrtc` / `webrtc-rs` — the WebRTC-Direct listener. Works, but needs the two
  one-line fixes in `vendor/` until upstreamed (FINDINGS §9: SRTP profile, DTLS curve
  selection against post-quantum-first Chrome).
- `str0m` — a sans-io WebRTC alternative worth evaluating for a production listener.
- `quinn` — the QUIC side; accepts custom `AsyncUdpSocket` implementations, which is one
  half of the RFC 9443 single-port demultiplexer (the other half being `UDPMux` in
  `libp2p-webrtc`).

## What this means for Autonomi nodes

The proposal is small: nodes offer a WebRTC-Direct listener *alongside* their existing
QUIC one — on the **same UDP port**, demultiplexed by first byte per RFC 9443 (FINDINGS
§10). The native protocol is untouched, no fork, no protocol change. Any web page can then
embed network content the way it embeds an image today, with nothing installed on the
visitor's machine.
