# Recon findings — corrections and confirmations to the work order

Everything below was verified against the actual source (repos cloned and inspected, code
executed) rather than assumed. It supersedes the corresponding parts of
`work-order.md`. Each entry states how it was verified so it can be re-checked.

> **What is being proved.** That a browser can reach the Autonomi network with *nothing
> installed* — no extension, no local daemon or relay, no process on the user's machine. The
> component in `crates/proxy` is a **stand-in for an Autonomi node that speaks WebRTC-Direct**,
> not a gateway and not a proposed piece of architecture. It exists only because no node offers
> that listener today. Everything in `crates/wasm-client` is written to be reused unchanged the
> day one does. See `README.md`.

---

## 1. Repository moves

| Work order says | Reality |
|---|---|
| `saorsa-labs/saorsa-core` | Redirects to **`WithAutonomi/saorsa-core`** (HTTP 301). |
| — (not mentioned) | **`WithAutonomi/ant-client`** exists and is the piece the work order was missing. |

`saorsa-labs/ant-quic` and `saorsa-labs/saorsa-pqc` still resolve directly.

Current published versions (crates.io, checked): `ant-protocol` 2.3.0, `self_encryption` 0.36.0,
`saorsa-core` 0.26.3, `libp2p` 0.56.0, `libp2p-webrtc` 0.9.0-alpha.1, `str0m` 0.21.0.
`ant-client` is **not** on crates.io — it must be a git dependency.

---

## 2. The sidecar is far simpler than §5 assumed

§5/B1 asks us to "identify the exact entry point for a chunk-level DHT GET in `saorsa-core`".
That work is already done and packaged: **`ant-client`'s `ant-core` crate** exposes exactly the
primitive the sidecar needs, and nothing else is required.

```rust
// ant_core::data::Client — verified in ant-client/ant-core/src/data/client/{mod,chunk}.rs
let client = Client::connect(&bootstrap_peers, ClientConfig::default()).await?;   // no wallet
let chunk: Option<DataChunk> = client.chunk_get(&address).await?;                 // address: [u8; 32]
```

Two consequences:

- **No wallet, no EVM, no payment for reads.** `connect()` sets `wallet: None`; `with_wallet()`
  is a separate opt-in builder step needed only for uploads. Reads are free.
- The sidecar does **not** need to touch `saorsa-core` directly. It depends on `ant-client`
  (git) and gets QUIC transport, DHT routing and close-group retry logic for free.

**Bootstrap peers for the production network** (`ant-node/config/bootstrap_peers.toml`, plain
`ip:port`, no multiaddrs):

```
207.148.94.42:10000   45.77.50.10:10000    66.135.23.83:10000   149.248.9.2:10000
49.12.119.240:10000   5.161.25.133:10000   18.228.202.183:10000
```

---

## 3. The DataMap is MessagePack, not bincode — §4 is wrong here

§4 lists `self_encryption::deserialize` (bincode) as the way to turn the fetched bytes into a
`DataMap`. The Autonomi client does **not** use it. Verified in
`ant-client/ant-core/src/data/client/data.rs`:

```rust
// store:
let serialized = rmp_serde::to_vec(data_map)?;   // MessagePack
self.chunk_put(Bytes::from(serialized)).await

// fetch:
fn decode_data_map_chunk(content: &[u8]) -> Result<DataMap> {
    rmp_serde::from_slice(content)               // MessagePack
}
```

**The WASM client must use `rmp_serde`.** Using `self_encryption::deserialize` would fail on
every real address.

There is **no wrapper type** around the address. §4's open question "is there a `DataAddress`
pointer type?" is answered: no.

---

## 4. The full address convention, confirmed end to end

Traced from the CLI (`ant file download <address>`) down to the wire:

1. **Address format** — 64 hex chars → 32 raw bytes. `ant-cli/src/commands/data/chunk.rs`:
   `parse_address()` is a plain `hex::decode` with a length check. No multibase, no prefix.
2. `chunk_get(address)` → the chunk holding the **MessagePack-serialized `DataMap`**.
3. `rmp_serde::from_slice` → `DataMap`.
4. If `data_map.is_child()` → `get_root_data_map(map, &mut get_chunk)` to resolve the shrunk map.
5. For each `info` in `root.infos()` → `chunk_get(info.dst_hash)` → `verify_chunk` → `decrypt`.

So the flow §3 sketches is correct; only the serialization format was wrong.

## 5. The trustless property holds exactly as claimed

The browser's integrity check and the network's addressing are provably the same function:

```rust
// ant-protocol/src/data_types.rs
pub type XorName = [u8; 32];
pub fn compute_address(content: &[u8]) -> XorName { *blake3::hash(content).as_bytes() }

// self_encryption/src/hash.rs  (what verify_chunk uses)
pub fn content_hash(content: &[u8]) -> XorName { XorName(*blake3::hash(content).as_bytes()) }
```

Identical. `verify_chunk(addr, bytes)` in the browser therefore rejects any byte the sidecar
alters or substitutes. The core claim of the PoC is sound.

---

## 6. WASM: the blockers in §4 are mostly not real

Empirically tested, not reasoned about: a crate depending on `self_encryption = "0.36"` was
built for `wasm32-unknown-unknown` and **executed** under Node via `wasm-bindgen`.

| §4 blocker | Reality |
|---|---|
| `rayon` needs threads → must be feature-gated out | **Compiles and runs unmodified.** `decrypt` (whose `decrypt_sorted_set` uses `par_iter`) completed correctly in wasm. No fork, no patch. |
| `tempfile` (filesystem) | Only used in tests and doc comments. Never on the in-memory path. |
| `tokio (rt)` | Only in doc comments. Compiles for wasm; never invoked. |
| `getrandom` needs the `js` feature | **Real.** Confirmed necessary — keep the explicit dep. |

Round-trip probe result (encrypt → `rmp_serde` → `verify_chunk` → `decrypt`, all inside wasm):

```
        3 B -> OK chunks=3  (34ms)
      1 KiB -> OK chunks=3  (17ms)
      5 MiB -> OK chunks=3  (355ms)
```

**No fork of `self_encryption` is required for the PoC.** §4's "hardest single item" is largely
already solved; the remaining work is the `wasm-bindgen` surface, not a port.

---

## 7. `MAX_CHUNK_SIZE`: a real trap, but not where §4 puts it

§4 calls this the "CRITICAL build-time gotcha" and claims a mismatch breaks decryption.
Both halves need correcting.

**(a) It does not affect download.** Traced the in-memory path
`decrypt` → `decrypt_full_set` → `decrypt_sorted_set` → `decrypt_chunk`. It derives everything
from the `DataMap`'s `src_hashes` and `child_level`; `MAX_CHUNK_SIZE` never appears. The constant
is read only by `utils::get_num_chunks` / `get_chunk_size` (the **encrypt** path) and by
`stream_decrypt`. A download-only browser client is therefore immune.

> If a later stage switches to `stream_decrypt` for large files, the constant starts to matter
> again. Chunk-by-chunk streaming is out of scope for now — the in-memory path is the safe one.

**(b) The environment here silently poisons it anyway.** `~/.zshrc:13` exports
`MAX_CHUNK_SIZE=4194304`, and `self_encryption` reads it via `option_env!` **at compile time**.
The published crate's default is `4_190_208` (4 MiB − 4 KiB). Our first wasm build reported
`4194304` purely because of that shell export — a build whose behaviour depends on whose shell
ran it.

Fix applied: each crate pins the value in `.cargo/config.toml` with `force = true`, so the
shell environment can no longer influence the build:

```toml
[env]
MAX_CHUNK_SIZE = { value = "4190208", force = true }
```

---

## 8. Ordering trap in `encrypt()` (worth knowing, cost an hour)

`encrypt()` returns `(DataMap, Vec<EncryptedChunk>)` where the chunk vector is **not** in
`DataMap::infos()` order: chunks 0 and 1 are deferred and appended last, so 5 chunks come back as
`[2, 3, 4, 0, 1]`. Zipping `infos()` with the returned vector therefore fails hash verification
for any file above 3 chunks.

Irrelevant to the retrieval flow (which fetches *by* `dst_hash`), but it will bite anyone writing
an encrypt-side test fixture.

---

## 9. Transport: five bugs between the Rust and browser stacks

The interop risk was real, but not where §8 of the work order expected it. SDP munging was never
a problem. What did break, in the order encountered:

| # | Symptom | Cause | Fix |
|---|---|---|---|
| 1 | Every inbound connection panics a worker thread | `webrtc-srtp` derives a session key the length of the master key but always builds an `Aes128Gcm`; a client preferring `SRTP_AEAD_AES_256_GCM` triggers a 32-vs-16 assert | Offer only 128-bit SRTP profiles (`vendor/libp2p-webrtc`) |
| 2 | Chrome connects at ICE level, then fails: `invalid named curve` | `webrtc-dtls` takes the client's *first* offered curve without checking support. Chrome now lists post-quantum X25519MLKEM768 first | Pick the first *supported* curve (`vendor/webrtc-dtls`) |
| 3 | Every request takes exactly 10.0 s | `close()` waits for a FIN_ACK that `libp2p-webrtc-utils` 0.4 never sends (`DEFAULT_FIN_ACK_TIMEOUT`) | Don't await the close |
| 4 | Any response over one 16 KiB message stalls forever | `PollDataChannel::poll_write` stores the write future without polling it, leaving it for a later poll that never comes | Flush after each message |
| 5 | Multi-megabyte responses reset mid-transfer, in Chrome only | Several parallel data channels carrying MBs each make Chrome's SCTP lose a channel (`stream N not found`) | Serialise requests |

Two are worth reporting upstream regardless of this PoC:

- **#2 means no `webrtc-rs` server can accept a connection from any current Chrome.** That is not
  specific to libp2p or to this project.
- **#4 means `webrtc-rs` cannot reliably send more than 16 KiB on a data channel** through the
  `AsyncWrite` path, which is how `libp2p-webrtc` uses it.

Measurements that pinned #3 and #4 down, both on loopback:

```
request → first byte      1 ms          close()                   10 002 ms
15 020 byte response      7 ms          20 000 byte response      never arrives
```

`vendor/README.md` carries the diffs and the upstream story.

### What this cost in performance

With #3 and #4 fixed, a 12 MB local transfer takes 5.4 s; before, it did not complete at all.
Against the live network the limit is DHT lookup latency (~5–8 s per chunk), not the transport:
15.7 MB in 38.6 s from the browser, of which the transport is a small fraction.

---

## 10. Verified end to end

The Definition of Done from the work order is met, on the production network:

- `https://webrtc-demo.autonomi.space` → browser connects straight to a proxy on a bare IP, with
  no DNS name, no CA and no signaling server in the data path.
- A 15 MB mp3 (`00ac7cbe…1afa`) is fetched as encrypted chunks, each verified against its BLAKE3
  address in WASM, then decrypted and reassembled in the browser: 15.0 MiB in 38.6 s.
- The address turned out to be a **shrunk (child) data map**, so the `get_root_data_map` path is
  exercised against real network data, not just fixtures.
- `file` on the result: `Audio file with ID3 version 2.2.0, MPEG ADTS, layer III, v1, 320 kbps`.

### A correction to §8 of the work order — and to how this was first described

The work order lists WebTransport as the fallback if SDP munging is ever restricted, with the
caveat *"no Safari"*. That is out of date: **WebTransport reached Baseline in March 2026, when
Safari 26.4 shipped it.** It now works in every current browser, so it is not a fallback — it is
a live alternative that deserves evaluating on the merits.

Worth stating plainly, because an early draft of this repo's own README got it wrong: **browsers
speak QUIC.** HTTP/3 rides on it. What a browser cannot do is open a *raw* connection to an
arbitrary host and speak an arbitrary protocol — there is no socket API, only HTTP/3 and
WebTransport. Autonomi is out of reach because of three things together: that missing API, its
own QUIC-based handshake and identity model (which no browser TLS stack can perform and no API
exposes), and nodes having neither DNS names nor CA-issued certificates.

Comparing the two browser-reachable transports on the property that matters here:

| | WebRTC-Direct | WebTransport |
|---|---|---|
| Reaches a bare IP with no CA | yes, via `certhash` | yes, via `serverCertificateHashes` |
| Certificate | self-signed, untrusted; stable indefinitely | self-signed, untrusted; **ECDSA P-256, valid ≤ 2 weeks** |
| Congestion control | SCTP over DTLS | QUIC |
| Standards risk | depends on SDP munging, formally disallowed | standards-track |
| Browser support | Chrome, Firefox, Safari | Baseline since March 2026 |

Both genuinely accept a **self-signed certificate that no CA vouches for**: the pinned hash
*replaces* chain validation rather than adding to it. Firefox originally implemented
`serverCertificateHashes` as an extra check on top of PKI validation, which rejected self-signed
certificates outright; that was fixed in Firefox 125
([bug 1873263](https://bugzilla.mozilla.org/show_bug.cgi?id=1873263)). Pinning also disables 0-RTT.

The two-week certificate ceiling is the substantive difference for this design: a pinned
WebTransport hash expires and must be continuously re-published, whereas a WebRTC certhash is
stable for as long as the node keeps its key — which is what makes the multiaddr on the demo
page something you can write down and hand to someone. Keeping the transport behind the
`LinkTransport` abstraction, as the work order already recommends, remains the right call.

**Does that mean a node could stay on QUIC?** Partly — and it is worth being precise, because
this is exactly the confusion the wrong sentence invited.

WebTransport *is* QUIC. A browser connects to `https://<ip>:<port>/…` over QUIC, pinning the
certificate hash, with no CA and no DNS name — the shape being asked about: IP, port and
certificate info in the contact description. What it is **not** is raw QUIC carrying Autonomi's
existing protocol. WebTransport is QUIC → HTTP/3 → WebTransport sessions, so a node would need
an HTTP/3 listener with WebTransport session handling, and Autonomi's protocol would ride
*inside* WebTransport streams — structurally the same as riding inside a DataChannel here. A
browser still cannot perform Autonomi's native handshake: there is no API for raw QUIC with a
custom ALPN, and the raw-public-key/PQC identity model is not something a browser TLS stack
does.

One consequence is interesting for the "fewer open ports" question: because both are QUIC,
a single UDP port could in principle serve the native protocol and HTTP/3, selected by ALPN.
Whether that is practical is untested here and not obvious — the two need different TLS
configurations (raw public keys and PQC on one side, an X.509 ECDSA P-256 certificate on the
other), so a server would have to vary its TLS setup per ClientHello. Plausible, not free.

### One port for both: the browser listener need not cost a second port

The obvious objection to putting a WebRTC listener on a node is operational: another open UDP
port, another firewall rule, another thing for every operator to change. That objection turns
out to be avoidable — **QUIC and WebRTC can share one UDP port, and the scheme is standardised**
in [RFC 9443](https://www.rfc-editor.org/rfc/rfc9443.html), which extends RFC 7983 to cover QUIC.

Demultiplex on the first byte of each datagram:

| First byte | Protocol |
|---|---|
| 0–3 | STUN → ICE (WebRTC) |
| 16–19 | ZRTP |
| 20–63 | DTLS → WebRTC |
| 64–79 | TURN channel if from a known TURN server, otherwise QUIC |
| 80–127 | QUIC (short header) |
| 128–191 | RTP/RTCP |
| 192–255 | QUIC (long header) |

A node needs only a subset: data channels involve no RTP and no TURN, so STUN and DTLS go to the
WebRTC stack and everything from 64 up goes to the existing QUIC stack.

It works because QUIC sets the fixed bit — which yields exactly one hard constraint, stated
explicitly in the RFC:

> Endpoints that wish to demultiplex QUIC MUST NOT send the `grease_quic_bit` transport parameter.

Greasing that bit ([RFC 9287](https://www.rfc-editor.org/rfc/rfc9287.html)) destroys the very
property the demultiplexer relies on. Worth knowing before rather than after.

**This is where WebRTC beats WebTransport, contrary to what the comparison above might suggest.**
DTLS 1.2 and QUIC are separate stacks with separate TLS configurations, so the router moves
packets and the two handshakes never meet: a node keeps raw public keys and PQC on the QUIC side
while presenting an ordinary self-signed certificate to browsers. WebTransport would instead put
two irreconcilable TLS configurations on the *same* QUIC endpoint — the PQC identity model on one
hand, X.509 ECDSA P-256 with ≤2-week validity on the other. For the single-port goal, the
awkward-looking transport is the tractable one.

The seams already exist in both implementations. `libp2p-webrtc` abstracts over
`Arc<dyn UDPMux + Send + Sync>`, so an implementation fed by a shared demultiplexer fits its
existing API — though today `UDPMuxNewAddr::listen_on()` binds its own socket and would need to
accept one instead. On the other side, quinn (and therefore ant-quic) accepts custom
`AsyncUdpSocket` implementations. Both sides can already bring their own socket; what is missing
is the router between them.

What this buys: one firewall rule instead of two, existing `ip:port` bootstrap lists valid
unchanged, and the same address serving both worlds with only the multiaddr suffix differing.
Operators would not have to reconfigure anything to start serving browsers — likely worth more
for adoption than any detail of the transport.

What it costs: the open port now answers two protocols, so the attack surface grows. STUN in
particular is a classic amplification vector and needs rate limiting, and a node that already
listens publicly is more exposed than the demo endpoint here.

### Still open

- **Abuse protection.** The proxy serves any address to anyone with no rate limiting — §5/B4 of
  the work order is not implemented.
- **Concurrency.** Serialised because of bug #5. Fixing it properly means multiplexing requests
  over one long-lived stream instead of a data channel per chunk.
- **Browser coverage.** Verified in Chrome and Safari. Firefox is untested, and bug #2 shows how
  much the DTLS stack differs between clients.
