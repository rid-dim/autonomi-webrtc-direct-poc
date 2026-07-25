# Work Order: Proof of Concept — Browser Client for Autonomi over WebRTC-Direct

**Goal in one sentence:** A browser, loaded from an ordinary HTTPS page *or* from a locally opened HTML file, connects **directly to a public Autonomi node with no DNS, no CA certificate, and no signaling server**, retrieves a file from the network, verifies and decrypts it entirely client-side, and lets the user download (and optionally view) it.

This document is a work order for Claude Code, and doubles as design documentation for the Autonomi developer community. Read **Section 0** first — it fixes the architecture and prevents the most likely misstep.

---

## 0. Core architecture: dumb proxy + fat WASM client

**The node is NOT forked and QUIC is NOT replaced.** The working title ("WebRTC-direct instead of QUIC") is misleading about what actually gets built.

We build two pieces with a deliberately lopsided split:

### The sidecar is a pure content-addressed fetch proxy

It does **exactly one thing**: given a content address, return the raw bytes stored at that address in the Autonomi network. One RPC:

```
GET <32-byte XorName>  ->  raw bytes  |  NOT_FOUND
```

It performs **no cryptography**, parses **no data maps**, sees **no plaintext**, and makes **no trust decisions**. It is a QUIC↔WebRTC-Direct bridge: browser asks over a WebRTC DataChannel, the sidecar resolves the address via a normal DHT GET over QUIC, and hands the opaque bytes back. That's it.

### The browser (WASM) owns everything cryptographically meaningful

Address handling, retrieval orchestration (walking the data map), per-chunk verification against its content address, decryption, and reassembly all run in the browser via a WASM build of `self_encryption`.

### Why this split is the right one

- **Complete trustless property.** Because the browser derives/knows every chunk address and verifies every returned chunk against it (BLAKE3 content-addressing), a malicious or buggy sidecar **cannot** compromise confidentiality (it never sees plaintext) or integrity (tampering fails the hash check). It can only refuse to serve. You could point the app at *any* sidecar run by *anyone* and be safe.
- **Low-trust to operate → good for adoption.** A proxy that provably can't read user data is something people are far more willing to run. This matters for the community pitch (Section 11).
- **No fork, no protocol change.** The sidecar embeds the existing network client as a library; the wire protocol, DHT, and consensus are untouched. Zero merge risk against fast-moving upstream repos.
- **The one primitive covers everything.** Both the data map *and* every data chunk are content-addressed chunks, so the single `GET <XorName>` RPC is used for all of it. No special cases.

> **A boundary worth stating:** the sidecar *does* perform the actual network I/O (the DHT GET), because a browser cannot join the DHT — it has no raw sockets, cannot be dialed, and cannot run Kademlia. So this is a *content-addressed fetch proxy*, not a transparent byte relay. Porting the full network/routing protocol into the browser (a true light client speaking the native wire format end-to-end) is a much larger effort and is **out of scope**; see Section 8's "Upstream direction."

**The node software itself does not need to be modified.** The sidecar is a new, separate project that uses the existing crates as libraries.

---

## 1. Relevant repositories

All URLs below were verified during preparation (existence checked; several cloned and inspected).

### Autonomi core (org `WithAutonomi`, successor to `maidsafe`)

| Repo | URL | Role in the PoC |
|---|---|---|
| `ant-node` | https://github.com/WithAutonomi/ant-node | Reference node of the 2.0 line. Shows how a node configures `saorsa-core` and joins the network. **Do not fork** — read as a template. |
| `ant-protocol` | https://github.com/WithAutonomi/ant-protocol | Protocol / data types (version `2.3.0`). Defines chunk/record types and addresses. Needed to confirm the exact address/serialization conventions (see §4). |
| `self_encryption` | https://github.com/WithAutonomi/self_encryption | **The heart of the browser client.** Must be built for WASM (see §4). |

> The old codebase at https://github.com/maidsafe/autonomi is **archived** (since May 2026, libp2p-based, 1.0). Use only as historical reference, never as a basis.

### Networking & crypto layer (org `saorsa-labs`, David Irvine's team)

| Repo | URL | Role in the PoC |
|---|---|---|
| `saorsa-core` | https://github.com/saorsa-labs/saorsa-core | Networking + DHT primitives. The **sidecar** uses this to join the network and perform the DHT GET. Contains `P2PNode` / `P2PNetworkNode<T: LinkTransport>`, `DhtNetworkManager`. Node pin: `saorsa-core = "0.26.2"`. |
| `ant-quic` | https://github.com/saorsa-labs/ant-quic | The QUIC transport. Contains the **`LinkTransport` trait** (`src/link_transport.rs`) and — notably — already-**reserved WebRTC stream types** (`StreamType::webrtc_types()`, id range `0x20–0x2F`). Home of the authoritative **ADRs** (see §2). |
| `saorsa-pqc` | https://github.com/saorsa-labs/saorsa-pqc | Post-quantum crypto: **ML-KEM-768** (key exchange) and **ML-DSA-65** (signatures). Node pin: `saorsa-pqc = "0.5"`. For the Stage-3 handshake. Pure Rust → WASM candidate. |
| `x0x` | https://github.com/saorsa-labs/x0x | **Important precedent.** Its "apps are single HTML files calling a local API" model (`docs/local-apps.md`) is exactly our target UX — only against a remote node instead of a local daemon. Also contains `saorsa-webrtc` trait surfaces (`src/voice/`) as an API-shape reference (deliberately no real stack: "trait surfaces only — no webrtc-rs, no codecs. quic-native matches x0x's transport philosophy"). |
| `saorsa-gossip` | https://github.com/saorsa-labs/saorsa-gossip | Only if needed — gossip / stream-multiplexing ADRs. Not required for the PoC. |

### WebRTC libraries (external)

| Repo / package | URL | Role in the PoC |
|---|---|---|
| `libp2p/rust-libp2p` → crate `libp2p-webrtc` | https://github.com/libp2p/rust-libp2p (path `transports/webrtc`) | **Recommended server side for Stages 0–2.** Implements the WebRTC-Direct spec (browser→server) including SDP synthesis and the Noise handshake. Interops with `@libp2p/webrtc`. |
| `@libp2p/webrtc` (in `libp2p/js-libp2p`) | https://github.com/libp2p/js-libp2p (package `@libp2p/webrtc`) | **Recommended browser side.** Counterpart to `libp2p-webrtc`. Provides a **stream abstraction over the DataChannel** (handles SCTP fragmentation + backpressure — saves us the DataChannel message-size handwork). |
| `libp2p/specs` → `webrtc/webrtc-direct.md` | https://github.com/libp2p/specs/blob/master/webrtc/webrtc-direct.md | **Required reading.** Defines exactly: certhash-in-multiaddr, ICE-Lite, the Noise prologue `libp2p-webrtc-noise:` + fingerprint binding. Our Stage-3 PQC handshake only swaps the Noise part. |
| `libp2p/universal-connectivity` | https://github.com/libp2p/universal-connectivity | Reference app demonstrating browser↔Rust/Go node over WebRTC-Direct for real. Best wiring example. |
| `algesten/str0m` | https://github.com/algesten/str0m | **Alternative server side** (sans-IO WebRTC, very lean, DataChannels only, ICE-Lite). Choose only if the `libp2p` dependency weight is a problem, or for the native PQC transport in §8. More manual work (SDP synthesis, prologue by hand). |
| `webrtc-rs/webrtc` | https://github.com/webrtc-rs/webrtc | Full WebRTC stack in Rust. Heavyweight. Fallback only if both `libp2p-webrtc` and `str0m` are ruled out. |

---

## 2. Required reading: the governing ADRs (in `saorsa-labs/ant-quic/docs/adr/`)

These justify **why** the approach is architecturally clean, and where the limits are. Read before implementing:

- **ADR-001 "Link Transport Abstraction"** — introduces the `LinkTransport` trait and names *"WebRTC for browsers, TCP fallback"* verbatim as motivation. This is the seam a future native transport would attach to.
- **ADR-003 "Pure Post-Quantum Cryptography"** — mandates **raw public keys** (RFC-7250 style), **trust-on-first-use**, **no X.509/CA**, and lists *"Cannot connect to non-PQC peers"* as an **accepted** trade-off. Important: our certhash pinning **is** exactly this TOFU model in browser form — so it is **not** a departure from the philosophy.
- **ADR-005 (NAT traversal / hole-punching)** — explains the native QUIC-extension approach instead of STUN/ICE, and thereby why the browser path only reaches **publicly reachable** nodes.
- **ADR-006 "MASQUE Relay Fallback"** — the existing relay topology; useful for comparison if NATed nodes should later become browser-reachable.
- **ADR-008 "Universal Connectivity Architecture"** — the overall transport-layering picture.

**Limits that follow from the ADRs and hold for this PoC:**
1. Only **publicly addressable nodes** are reachable (ICE-Lite responders). NATed home nodes still require signaling — deliberately **out of scope** here.
2. **An HTTPS origin for the app shell remains.** Not middleware in the data path, but whoever serves the HTML/JS/WASM controls the client. This is the weakest link; the single-file HTML (§6) makes it *auditable*, not eliminated.
3. Browsers stay **pure consumers** (dialers). They never become providers/nodes.

---

## 3. Target architecture & data flow

```
┌──────────────────────────────────────┐        ┌───────────────────────────────────────────┐
│ BROWSER (HTTPS page / local HTML file) │        │ SIDECAR (public IP, open UDP port)          │
│                                        │        │  = pure content-addressed fetch proxy       │
│  ┌──────────────────────────────────┐  │        │                                             │
│  │ WebRTC-Direct client (@libp2p/webrtc)│ WebRTC- │  ┌───────────────────────────────────────┐  │
│  │                                  │◄─┼─Direct──┼─►│ WebRTC-Direct listener (ICE-Lite)     │  │
│  └───────────────┬──────────────────┘  │DataChan.│  │ (rust-libp2p `libp2p-webrtc`)         │  │
│                  │ GET <XorName> / bytes│(DTLS+SCTP)  └──────────────────┬────────────────────┘  │
│  ┌───────────────▼──────────────────┐  │        │                     │ GET <XorName>          │
│  │ WASM: self_encryption + logic    │  │        │                     ▼                        │
│  │  1. fetch data map (GET)         │  │        │  ┌───────────────────────────────────────┐  │
│  │  2. get_root_data_map (if child) │  │        │  │ saorsa-core client (P2PNode, QUIC)    │  │
│  │  3. for each ChunkInfo: GET      │  │        │  │  DHT GET(address) -> raw chunk bytes  │──┼──► Autonomi
│  │  4. verify_chunk (BLAKE3)        │  │        │  └───────────────────────────────────────┘  │    network
│  │  5. decrypt(root_map, chunks)    │  │        │       (NO crypto, NO plaintext, NO maps)     │    (QUIC)
│  └───────────────┬──────────────────┘  │        └───────────────────────────────────────────┘
│                  ▼                       │
│         Blob → download / render         │
└──────────────────────────────────────┘
```

**Retrieval flow (this is the demo's payoff).** Everything except the raw network GET happens in WASM:

1. Obtain the file's **address** — either selected from the curated Reading Room list or pasted by the user (a `XorName`).
2. `bytes = GET(address)` via the proxy → deserialize into a `DataMap`.
3. If `data_map.is_child()` (a shrunken data map — this is what large files produce): call `get_root_data_map(data_map, &mut |addr| GET(addr))`, which fetches the child chunks to reconstruct the full root `DataMap`. **This step is what makes larger files work.**
4. For each `ChunkInfo` in `root_data_map.infos()`: `raw = GET(info.dst_hash)`, then `chunk = verify_chunk(info.dst_hash, raw)` — hashes with BLAKE3 and fails on mismatch, returning the `EncryptedChunk`.
5. `plaintext = decrypt(&root_data_map, &chunks)`.
6. Wrap `plaintext` in a `Blob` → trigger a download, and render inline if it's a viewable type.

The sidecar **never** sees plaintext (only opaque chunk bytes flow) and **cannot** tamper (content-addressing + `verify_chunk`). That is what separates this PoC from "a gateway over WebRTC."

---

## 4. Component A — the browser WASM client (`self_encryption` → WASM)

This is the **hardest single item**. The crate currently has **no** WASM support.

### Verified public API (from `self_encryption/src/lib.rs` and `src/data_map.rs`, inspected)

```rust
// One primitive used for both the data map and every chunk:
pub fn verify_chunk(name: XorName, bytes: &[u8]) -> Result<EncryptedChunk>;   // BLAKE3 addr check + build chunk
pub fn decrypt(data_map: &DataMap, chunks: &[EncryptedChunk]) -> Result<Bytes>;
pub fn get_root_data_map<F>(data_map: DataMap, get_chunk: &mut F) -> Result<DataMap>
where F: FnMut(XorName) -> Result<Bytes>;                                     // resolves shrunken data maps
pub fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T>;           // bytes -> DataMap
pub fn serialize<T: Serialize>(data: &T) -> Result<Vec<u8>>;

pub struct DataMap { pub chunk_identifiers: Vec<ChunkInfo>, /* child: Option<usize> */ }
impl DataMap {
    pub fn infos(&self) -> &[ChunkInfo];   // each ChunkInfo has the XorName (dst_hash) to fetch
    pub fn child(&self) -> Option<usize>;
    pub fn is_child(&self) -> bool;
    pub fn len(&self) -> usize;
}
pub struct ChunkInfo { pub index: usize, /* dst_hash: XorName (post-encryption), src_hash, src_size */ }
pub struct EncryptedChunk;
pub use xor_name::XorName;   // 32-byte content address
```

`verify_chunk` is the key call: it combines the integrity check (BLAKE3 hash == address) **and** construction of the `EncryptedChunk` in one step. That is exactly the browser's "trust but verify" gate.

### WASM blockers (from the verified dependency set) and how to resolve them

Current deps include: `blake3`, `chacha20poly1305`, `brotli (std)`, `bincode`, `xor_name`, `bytes`, `serde`, **`rayon`**, **`tempfile`**, **`tokio (rt)`**, `rand`/`rand_chacha`.

WASM-ready out of the box: `blake3`, `chacha20poly1305`, `brotli`, `bincode`, `xor_name`, `bytes`, `serde`.

To resolve:

1. **`rayon`** (needs threads). For the PoC build **single-threaded** — use the non-parallel functions (`get_root_data_map`, not `get_root_data_map_parallel`), feature-gate out any `rayon` code paths. Browser threads are possible (`wasm-bindgen-rayon` + `SharedArrayBuffer`) but force COOP/COEP headers (see §7) → **avoid** for the PoC.
2. **`tempfile`** (filesystem). Only used in the streaming/file path. Use the **in-memory API** (`decrypt(&DataMap, &[EncryptedChunk])` is fully in-memory, no FS).
3. **`tokio (rt)`** — not needed in the pure crypto core. Feature-gate it out of the wasm32 build; if an async surface is required, use `wasm-bindgen-futures`.
4. **`rand` → `getrandom`** — on `wasm32-unknown-unknown` enable **`getrandom` with the `js` feature**, or you get a runtime "no randomness" panic. Classic pitfall. Enforce transitively in `Cargo.toml`: `getrandom = { version = "…", features = ["js"] }`.

### CRITICAL build-time gotcha: `MAX_CHUNK_SIZE` must match the network

`self_encryption` has a build-time constant `MAX_CHUNK_SIZE` (env-overridable via `option_env!("MAX_CHUNK_SIZE")`). The code's own `stream_decrypt` comments warn about "different MAX_CHUNK_SIZE schemes." **The WASM client must be compiled with the same `MAX_CHUNK_SIZE` the live network uses**, or chunk boundaries and addresses won't line up and decryption fails. Confirm the network's value against `ant-node` / the release build and pin it in the WASM build.

### Approach

- Create a new crate `autonomi-wasm-client` that depends on `self_encryption` with a WASM-friendly feature selection and exposes a minimal API to JS via `wasm-bindgen`. (Cleaner than editing `self_encryption` in place; keeps the port reviewable.)
- Toolchain: `wasm-bindgen` + `wasm-pack` (or `trunk`), target `wasm32-unknown-unknown`; run **`wasm-opt -Oz`** afterward.
- JS-facing API (at minimum): a single `retrieve(address: Uint8Array, getChunk: (addr: Uint8Array) => Promise<Uint8Array>) => Promise<Uint8Array>` that runs the whole flow of §3 and calls back into JS (`getChunk`) for each network fetch, so the WASM never needs sockets. JS wires `getChunk` to the WebRTC proxy.

### Address / serialization conventions — VERIFY IN CODE

The **exact** meaning of the pasted address and the on-the-wire serialization of a stored `DataMap` are Autonomi-client conventions, not pure `self_encryption`. Claude Code must confirm against `ant-node` / `ant-protocol` / the 2.0 client:
- What a shared "data address" resolves to (most likely: the `XorName` of a chunk holding the serialized root/shrunken `DataMap`).
- The exact serialization used to store/deserialize a `DataMap` (self_encryption exposes `serialize`/`deserialize`, but confirm the client uses that and not a wrapper type).
- Whether there is a wrapping type (e.g. a `DataAddress`/pointer) around the raw `XorName`.

The likely flow is: pasted `XorName` → `GET` → `deserialize` → `DataMap` → `get_root_data_map` if child → `decrypt`. Confirm, don't assume.

### Practical file-size ceiling (fine for the PoC)

Everything resident in RAM at once means peak memory ≈ 2–3× file size (encrypted chunks + plaintext + data map). `wasm32` linear memory maxes at 4 GiB and browsers often cap lower, so the practical ceiling is roughly **low hundreds of MB**. That comfortably covers Reading Room papers and reasonably large documents. Arbitrary/GB-scale files would need chunk-by-chunk streaming with incremental decryption (`self_encryption` has streaming APIs) or `memory64` — **explicitly out of scope** per the requirement that all-in-RAM is acceptable.

---

## 5. Component B — the sidecar (Rust)

A standalone binary crate. Two halves plus the one RPC.

### B1. Network client (fetch chunks)

- Embed `saorsa-core` as a library. Relevant public types (inspected): `P2PNode` / `P2PNetworkNode<T: LinkTransport = P2pLinkTransport>`, `DhtNetworkManager`, `NodeConfigBuilder`. Keep the default transport `P2pLinkTransport` (QUIC via ant-quic) — the sidecar speaks ordinary QUIC to the network.
- **Task for Claude Code:** identify the exact entry point for a **chunk-level DHT GET** in the current `saorsa-core` / `ant-node` code. Candidates: `DhtNetworkManager` and the record/chunk GET paths under `src/dht/`. The README notes "user-facing … examples live in saorsa-node" — if a separate `saorsa-node` repo with client examples exists, copy its usage patterns. Also check whether `ant-node` bundles a higher-level client facade.
- The sidecar needs the network's **bootstrap addresses** (take from `ant-node` config/docs).

### B2. WebRTC-Direct listener (serve browsers)

- **Recommended for Stages 0–2:** `libp2p-webrtc` (from `rust-libp2p`) in **ICE-Lite / server mode**. It implements the webrtc-direct spec and pairs with `@libp2p/webrtc` in the browser.
- The node generates/**persists** its **DTLS certificate** and publishes a **multiaddr** of the form
  `/ip4/<IP>/udp/<PORT>/webrtc-direct/certhash/<HASH>/p2p/<PeerId>`.
  **Persist the certificate to disk** so the multiaddr survives restarts (otherwise the hard-coded certhash in the client breaks).
- Print the full multiaddr (incl. certhash) at startup so it can be dropped into the app.

### B3. The one RPC over the DataChannel

Keep it trivial. Use the **libp2p stream abstraction** (not raw DataChannel messages — then libp2p handles SCTP fragmentation):

- **Browser → sidecar:** `GET` + 32-byte `XorName`.
- **sidecar → browser:** `OK` + length-prefixed raw bytes, or `NOT_FOUND`.
- One request per stream, or simple framing over one stream — pick the simplest that works cleanly with `@libp2p/webrtc`.
- The same RPC serves the data map **and** every chunk; the sidecar does not distinguish them (it doesn't know or care which is which).

> Without libp2p (str0m/raw): mind the SCTP message size — fragment chunks > ~64 KiB into 16–64 KiB frames for cross-browser safety. This is the main reason to stay on libp2p for the PoC.

### B4. Abuse protection (even in the PoC)

An open UDP endpoint is a bandwidth-abuse vector and, by design, has **no** TURN fallback. For v1:
- Serve **only a fixed demo set** (an allowlist of chunk addresses derived from the curated documents + any pasted address the operator opts to allow), **not** arbitrary addresses — or at minimum gate arbitrary lookups behind a flag.
- Simple per-peer/IP rate limiting.

---

## 6. Component C — the downloadable single-file app

Target UX (the primary deliverable): **download one HTML file, open it, pick or paste an address, get the file.**

### UI

- **A curated list of Reading Room documents** — a hard-coded array of `{ title, xorAddress }`. Selecting one runs the retrieval flow. (The addresses come from the actual Reading Room / a manual upload step via indelibletool.com — record them once and embed them.)
- **A "paste your own address" field** — a text input accepting a `XorName` (hex/multibase; match whatever the network uses). Lets the user fetch any data map they know the address of, including larger files.
- **A result area** — a **Download** button (Blob → `<a download>`), plus inline rendering when the type is viewable (PDF via `<embed>`/`pdf.js`, or text/image directly). Show progress (chunks fetched / total) since larger files take several round trips.

### Packaging

- JS **inline**. WASM **inline as Base64**, loaded via `WebAssembly.instantiate(bytes, imports)` (not `instantiateStreaming` — that needs a MIME type).
- **Shrink it:** gzip the WASM → Base64 the gzip → decompress in-browser with the native `DecompressionStream('gzip')`. Recovers most of the ~33% Base64 overhead. Result: a single HTML file in the low-single-digit-MB range.
- **Node multiaddr(s) hard-coded** in the file (incl. certhash) — no DHT lookup, no signaling.
- **Auditability as a feature:** the file loads **nothing** externally (no CDN, no subresources, no dynamic imports). It has a stable hash you can publish and anyone can check. This mitigates (does not remove) the app-shell trust issue from §2.
- **Optional flourish:** store the HTML file itself on Autonomi. The chunk is content-addressed — its address **is** its hash — so the app lives permanently on the network it makes accessible. (Chicken-and-egg only for the very first fetch.)

---

## 7. Browser gotchas (concrete, read before building)

- **No mixed-content problem.** WebRTC is **not** subject to mixed-content blocking (DTLS is mandatory there) — unlike `ws://` or `http://` fetch. That's why direct contact from an `https://` page works. **No CORS** (a DataChannel isn't a fetch → no preflight, no headers on the node). **No certificate warning** (fingerprint validation; the self-signed cert never appears in any UI).
- **Secure context required.** `https://` or `localhost`. For local files: `file://` is spec-classified "potentially trustworthy" (so `isSecureContext` should be `true`), but **test WebRTC + WASM-from-buffer at origin `null` in every target browser** — don't assume. If it holds, "double-click the file" is the whole flow.
- **`getrandom` `js` feature** on wasm32 — otherwise a runtime "no randomness" panic. (§4)
- **CSP:** with a strict CSP, WASM instantiation from a buffer needs `'wasm-unsafe-eval'` in `script-src`. Irrelevant on a static host with no CSP header.
- **COOP/COEP only if threading.** We build single-threaded → **not needed**. (If you ever add `wasm-bindgen-rayon`: `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy: require-corp`; GitHub Pages can't set custom headers, Cloudflare Pages/Netlify can, or use `coi-serviceworker`.)
- **Local testing: use `localhost`, not the LAN IP.** Chrome obfuscates local ICE candidates via mDNS and Private Network Access interferes — otherwise you lose an evening debugging the wrong layer. Note this does **not** affect the real use case: nodes are on **public IPs**, so Private/Local Network Access restrictions don't apply.
- **UDP blocking.** Some corporate/mobile networks block outbound UDP → with no TURN fallback it fails hard there. Acceptable for the PoC; document it.
- **Firefox `RTCCertificate#getFingerprints`** is disallowed — per the libp2p spec this is practically harmless because the fingerprint is in the local SDP, but test in Firefox.
- **Browser support (verified):** WebRTC-Direct works in **Chrome, Firefox, Safari** (incl. iOS/WebKit); server side **rust-libp2p** and **go-libp2p**. Per-connection setup is ~5–6 RTT (STUN + DTLS + handshake) — **once** per node, then the connection stays open, so **don't** reconnect per chunk.

---

## 8. Known risk: SDP munging (and how the design insulates against it)

WebRTC-Direct works because the browser **synthesizes an SDP answer** from the node's multiaddr (a form of "SDP munging"). The libp2p spec is candid: this is *disallowed by the specification but not enforced by any major browser due to real-world use cases*. Separately, Chrome/libWebRTC has a long-running project to deprecate SDP munging for **certain SDP fields**.

Why this is a real but slow and well-hedged risk:
- The active deprecation targets primarily **local media/codec** munging (for which standard alternatives like the transceiver API exist). A **DataChannel-only** connection has no media SDP to munge, and remote-description synthesis is a different operation.
- It is explicitly slow and telegraphed — Chrome has "tracked" this since ~2020 and it still hasn't landed; there would be years of console warnings before any removal.
- It's a **shared ecosystem dependency** (libp2p, IPFS/Helia browser connectivity), so there's organized pressure on vendors and organized work on standards-track alternatives.
- **Rock-solid part:** the self-signed **DTLS + fingerprint** model (the thing that removes DNS/CA) is *how WebRTC has always worked* — it never used WebPKI for DTLS. Removing it would break all of WebRTC. That building block is as stable as WebRTC itself.

**Design implication — already baked in:** keep the WebRTC-Direct transport behind the `LinkTransport`-style abstraction (which the codebase already has, incl. reserved WebRTC stream types). If the mechanism is ever restricted, swapping to **WebTransport with `serverCertificateHashes`** (same "no CA" property; caveats: no Safari, ECDSA-P256 only, ≤14-day certs) or a future standardized path is a **transport swap, not a rewrite**. For the PoC specifically, this changes nothing — it works across all major browsers today.

### Upstream direction (explicitly NOT part of this work order)

The "proper" merge path: implement WebRTC-Direct as a **native `LinkTransport`** in `saorsa-core` (the trait and the `0x20–0x2F` WebRTC stream types are already reserved in `ant-quic`), with the **PQC handshake in place of Noise** directly in the transport layer. That would be the PR to the project. And a true browser **light client** (native wire protocol in WASM, sidecar as a plain relay) is a separate, larger effort. Both are left out of the PoC on purpose — prove it first, then propose.

---

## 9. Milestones (incremental; each stage is demoable)

**Stage 0 — Transport smoke test.** Browser (HTTPS page) establishes a WebRTC-Direct connection to the sidecar and echoes a message over the libp2p stream abstraction. Proves: connectivity with no DNS/CA/signaling, from an `https://` origin.

**Stage 1 — Single chunk + verification.** Browser requests one chunk by `XorName`; sidecar returns raw bytes via DHT GET; browser checks with `verify_chunk(address, bytes)`. Proves the **trustless integrity** primitive and the full proxy round trip.

**Stage 2 — Full retrieval + self-decrypt in WASM.** Browser resolves an address → data map → (root data map if child) → all chunks → `decrypt` in WASM → reassembles and **downloads/renders a real Reading Room paper**. Wire up the curated list and the paste field. **This is the moment people get it.** This is the core deliverable.

**Stage 3 — PQC handshake over the DataChannel.** On top of the (Noise-secured) libp2p transport, add an **application-layer** handshake with **ML-KEM-768 + ML-DSA-65** (`saorsa-pqc`, also built for WASM), bound to the DTLS fingerprints (prologue pattern from the webrtc-direct spec). This authenticates the sidecar's **ML-DSA identity** (so a MITM can't impersonate the proxy or selectively lie about "not found"), aligning with ADR-003. If capacity allows only **one** thing beyond Stage 2: **this one** — it's what turns the PoC from a hack into something aligned with Autonomi's "pure PQC" story.

**Stage 4 — Multi-node + single-file packaging.** Point the app at **2–3 independent sidecars** ("not just your server") and package the whole client as **one self-contained downloadable HTML file** (§6). Optional: store that file on Autonomi.

---

## 10. Deliverables

1. **`autonomi-wasm-client`** — Rust crate exposing a WASM-friendly `self_encryption` via `wasm-bindgen`; `wasm-pack`/`trunk` build, `wasm-opt -Oz`, single-threaded/in-memory, `getrandom` `js`, `MAX_CHUNK_SIZE` pinned to the network. Public API: a `retrieve(address, getChunk)` entry point plus `verify_chunk` exposed for Stage 1.
2. **`autonomi-webrtc-proxy`** — Rust binary: `saorsa-core` client (QUIC to the network) + `libp2p-webrtc` listener (ICE-Lite) + the single `GET <XorName>` RPC + certificate persistence + address allowlist + rate limiting. Prints its multiaddr (incl. certhash) at startup. Performs **no** crypto and holds **no** plaintext.
3. **`index.html`** — self-contained downloadable app (inline JS + Base64/gzip WASM + `@libp2p/webrtc`), node multiaddr(s) hard-coded; curated Reading Room list + paste-address field; download + optional render; progress indicator.
4. **`README.md`** — build & deploy: a VPS with a public IP + open UDP port for the proxy (**no domain** for the node); any static HTTPS host **or** `python -m http.server` (localhost) for the shell; the §7 gotcha checklist; the §4 "verify in code" list.
5. **Short demo script** — which document/address, which browser, expected result per stage.

### Definition of Done (for the demoable core)

On a clean machine: open `index.html` from an HTTPS URL (or locally) → the browser connects to the proxy with **no** DNS/CA/signaling → select a Reading Room paper (or paste a data-map address) → the browser fetches **raw encrypted chunks**, **verifies** each against its address, **decrypts in-browser**, and **downloads/renders** the file. The proxy never saw plaintext and could not have tampered. (That is Stage 2; Stage 3 adds the PQC identity handshake.)

