# Autonomi in the browser, without anything installed

**Live demo: [webrtc-demo.autonomi.space](https://webrtc-demo.autonomi.space)**

An ordinary web page fetches a 15 MB file off the Autonomi network, verifies every chunk
against its content address and decrypts it — all in the browser. No extension, no local
daemon or relay, no installed software, no process running on the visitor's machine.

## The question this answers

Browsers speak QUIC perfectly well — HTTP/3 rides on it. The barrier is not the protocol
family, it is three things stacked together:

1. **No raw socket API.** A page cannot open an arbitrary connection to an arbitrary host and
   speak an arbitrary protocol. Browser access to QUIC exists only through HTTP/3 and
   WebTransport.
2. **Autonomi's own wire protocol.** Nodes run a QUIC-based protocol with their own handshake
   and identity model (raw public keys, trust-on-first-use, post-quantum). A browser's TLS
   stack cannot perform that handshake, and no API exposes the layer where it happens.
3. **No names and no CA certificates.** Nodes are bare IP addresses. Nothing a browser
   normally validates against exists.

Today that gap is bridged by asking users to install something — a browser extension, a local
gateway, a daemon on localhost — which is a hard sell for anyone who just wants to look at a
file. WebRTC-Direct sidesteps all three at once: the browser pins the hash of the node's
self-signed certificate instead of validating a chain, and the DataChannel carries whatever
protocol the two ends agree on. So:

> **If Autonomi nodes offered a WebRTC-Direct listener alongside their QUIC one, could any
> website embed network content directly?**

This proof of concept answers yes, against the live production network.

The endpoint the demo talks to (`crates/proxy`) is a **stand-in for such a node**. It is not
a gateway and not part of the design being proposed: it exists only because no node speaks
WebRTC-Direct yet. It answers exactly one question — *"what bytes are stored at this
address?"* — and were nodes to answer that themselves, it would disappear, with **no change
to the browser side at all**.

## What runs where

```
┌──────────────────────────────┐         ┌────────────────────────────┐        ┌──────────────────┐
│ THIS BROWSER   [new]         │ WebRTC- │ WebRTC-DIRECT ENDPOINT     │  QUIC  │ AUTONOMI NETWORK │
│ any https:// page,           │ Direct  │ [stands in for a node]     │        │ [unmodified]     │
│ or a local file              │◄───────►│                            │◄──────►│                  │
│                              │ no DNS  │ GET <address> → raw bytes  │        │ DHT lookup by    │
│ • parses the content address │ no CA   │                            │        │ content address  │
│ • walks the data map         │ no      │ • no cryptography          │        │                  │
│ • VERIFIES every chunk       │ signal- │ • no data maps             │        │ returns content- │
│   against its BLAKE3 address │ ing     │ • never sees plaintext     │        │ addressed chunks │
│ • DECRYPTS and reassembles   │ server  │ • can only refuse to serve │        │                  │
└──────────────────────────────┘         └────────────────────────────┘        └──────────────────┘
        ▲                                                                                          
        └── everything that decides whether bytes are trustworthy lives here
```

Because a content address *is* the BLAKE3 hash of its content, and that check happens in the
browser, the endpoint **cannot tamper and cannot read**. It can refuse to serve; that is the
whole of its power. Point the page at anyone's endpoint and the guarantees are identical —
which is what makes this deployable by strangers.

## Where this could go

A WebRTC-Direct listener only works on a publicly reachable node, which looks like it limits
how much of the network a browser can draw on. It need not: a public node does not have to
*serve* the data, it could just **broker** — accept the browser, help it establish a connection
to another node, and step out of the path.

Crucially, that second node does not need to hold the chunk either. It would do exactly what
the endpoint in this demo does today: an ordinary DHT lookup over Kademlia XOR routing, then
hand the bytes back. **Any node can answer any address**, so serving browsers is decoupled from
who stores what — no content-aware routing to a specific holder is required.

That inverts the load picture. Browser traffic spreads across the whole network rather than
concentrating on the publicly-reachable minority, and a broker's cost stays near zero no
matter how large the file. libp2p already has machinery of this shape — browser-to-private-peer
WebRTC negotiated over a relayed hop. The open questions are which NATs it traverses without a
fallback relay, and how it meshes with Autonomi's own QUIC-based hole punching, which is a
different mechanism from the one browsers use.

### WebRTC-Direct is not the only candidate

WebTransport reaches a host the same way — `serverCertificateHashes` lets the browser pin a
hash of a self-signed certificate instead of validating a chain — and it became available in
every current browser in March 2026, when Safari 26.4 shipped it. In several respects it is the
better-behaved option: real QUIC congestion control, no SRTP or SDP machinery, and none of the
deprecation risk hanging over the SDP munging WebRTC-Direct relies on.

The catch is operational. A pinned WebTransport certificate must be **ECDSA P-256 and valid for
at most two weeks**, so the hash baked into a page expires and has to be re-published
continuously. WebRTC's certificate hash is stable for as long as the node keeps its key, which
is what makes the multiaddr on the demo page something you can write down and share. That
trade-off deserves a proper evaluation rather than an assumption either way — and it is a good
argument for keeping the transport behind an abstraction, as `LinkTransport` already is.

**Further out, and more speculative:** if nodes spoke this channel to *each other*, a node
might get by with fewer open ports. That one deserves scrutiny rather than enthusiasm. SCTP
over DTLS carries less well than QUIC, the handshake costs several more round trips, and the
DTLS stacks here are behind on crypto agility — this PoC hit exactly that, when current
Chrome offered a post-quantum key exchange the Rust DTLS implementation cannot perform at all
(`docs/FINDINGS.md` §9, bug #2). That sits awkwardly beside Autonomi's pure-PQC direction. As
a browser-facing edge, WebRTC earns its place; as a replacement for node-to-node QUIC, the
case looks much weaker.

## What it removes

- **No browser extension** — plain page JavaScript and WASM.
- **No local daemon, relay or gateway** — nothing listening on localhost.
- **No install and no process rights** on the visitor's machine.
- **No DNS name and no CA certificate** for the node: a bare IP suffices, because the browser
  pins the certificate's hash instead of validating a chain.
- **No trusted intermediary**, per the argument above.

Any website could embed Autonomi content the way it embeds an image today.

## Layout

| Path | What it is |
|---|---|
| `crates/wasm-client` | The browser client: address parsing, chunk verification, data-map walking, decryption. Compiles to `wasm32-unknown-unknown`. **This is the piece that would be reused unchanged against a real node.** |
| `crates/proxy` | The WebRTC-Direct endpoint standing in for a node. Bridges browser ↔ network; performs no cryptography. |
| `web/` | The demo page and its integration tests. |
| `vendor/` | Two upstream crates with one-line fixes each — see `vendor/README.md`. |
| `docs/FINDINGS.md` | What was verified in code, what the work order got wrong, and the transport bugs found along the way. |

## Status

Verified end to end against the production network in Chrome: 15.0 MiB in 38.6 s, from an
`https://` page, with the file playing back in the browser.

Also available as **one self-contained HTML file** (744 KiB, WASM inlined, nothing fetched from
anywhere): `npm run build` writes it to `dist/autonomi-webrtc.html`, and the demo page links it.

Not done, and deliberately not claimed:

- **No abuse protection.** The endpoint serves any address to anyone, with no rate limiting.
- **Requests are serialised**, because parallel data channels carrying megabytes trip Chrome's
  SCTP layer. Fixing it properly means multiplexing over one long-lived stream.
- **Occasional stream resets.** Roughly one run in several still fails partway with
  `the stream has been reset` and succeeds on retry. Serialising made it rare, not impossible;
  the underlying data-channel lifecycle issue is not fully understood.
- **Only Chrome tested.** Firefox and Safari are unverified, and bug #2 above shows how much
  DTLS behaviour varies between clients.
- **`file://` untested.** The single-file build is self-contained and should work from disk,
  but that has not been verified — only a bare static server with no other files present.

## Building

Needs a Rust toolchain with the `wasm32-unknown-unknown` target, `wasm-bindgen-cli`, and
Node 20+.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli
cd web && npm install
```

### Run the tests

They spawn a real proxy process, open a real WebRTC-Direct connection and drive the real WASM
client, so both binaries have to exist first. Fixtures are generated, nothing to download.

```sh
cargo build --manifest-path crates/proxy/Cargo.toml   # debug build — the tests look for it
cd web
npm run build:wasm-node                               # WASM for the nodejs target
npm test
LIVE=1 npm test                                       # also hits the production network
```

### Build the demo page

```sh
cd web && npm run build
```

Produces `dist/` — `index.html` plus `app.js` and `wasm/` for a static host, and
`dist/autonomi-webrtc.html`, the single self-contained file.

### Run the endpoint

```sh
cargo build --release --manifest-path crates/proxy/Cargo.toml

# Against the live network (default bootstrap peers):
./crates/proxy/target/release/autonomi-webrtc-proxy --port 4001 --state-dir ./state

# Or against a local file, no network involved:
./crates/proxy/target/release/autonomi-webrtc-proxy --fixture ./some-file --state-dir ./state
```

It prints the multiaddr to paste into the page. **Keep `--state-dir`**: it holds the identity
keypair and DTLS certificate, and regenerating either changes the multiaddr, breaking every
client that has it hard-coded.

Both crates pin `MAX_CHUNK_SIZE` in `.cargo/config.toml`; see `docs/FINDINGS.md` §7 for why
that matters.
