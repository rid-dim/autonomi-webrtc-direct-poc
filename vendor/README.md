# Vendored dependencies

Two crates are vendored, each with a single change. Both fix upstream bugs that
make the browser path impossible rather than merely slow, and both are small
enough to read in a minute.

Each is otherwise a verbatim copy of its published release, with one omission:
the crates' `examples/` directories are left out, because they ship throwaway
test certificates and private keys that are not needed to build the library and
only serve to trip secret scanners. Diff accordingly:

```sh
diff -ru --exclude=examples --exclude=.cargo-ok \
  ~/.cargo/registry/src/*/webrtc-dtls-0.11.0 vendor/webrtc-dtls
```

---

## `webrtc-dtls` 0.11.0 — Chrome cannot complete a handshake

The more serious of the two: **as published, `webrtc-rs` cannot accept a DTLS
connection from any current version of Chrome.**

### The bug

Server-side curve selection took the client's first offer without checking
whether it was supported:

```rust
// webrtc-dtls-0.11.0/src/flight/flight0.rs
Extension::SupportedEllipticCurves(e) => {
    if e.elliptic_curves.is_empty() { /* error */ }
    state.named_curve = e.elliptic_curves[0];   // <- no negotiation
}
```

`NamedCurve` knows only P-256, P-384 and X25519; anything else parses as
`NamedCurve::Unsupported`. Chrome enables post-quantum key agreement by default
and lists **X25519MLKEM768 (0x11ec)** first, so `named_curve` becomes
`Unsupported` and the handshake fails a few steps later:

```
WARN webrtc::peer_connection: peer connection state changed: failed
WARN webrtc::peer_connection::peer_connection_internal:
     Failed to start manager dtls: invalid named curve
WARN webrtc::peer_connection::peer_connection_internal:
     Failed to start SCTP: DTLS not established
```

ICE connects first, so from the browser this looks like a plain connection
timeout with nothing useful in the console — the reason is only visible in the
server log.

This is also why Node worked while Chrome did not: `node-datachannel`
(libdatachannel/OpenSSL) offers a classical curve first.

### The change

`src/flight/flight0.rs`: choose the client's most-preferred curve that this
implementation can actually perform, and fail only if there is no overlap —
which is what negotiating a curve is supposed to mean.

```rust
let selected = e.elliptic_curves.iter().copied()
    .find(|curve| !matches!(curve, NamedCurve::Unsupported));
```

Chrome lists X25519 and P-256 after the hybrid, so the handshake now settles on
X25519.

### Upstream

Worth reporting to `webrtc-rs` regardless of this PoC: every Rust WebRTC server
built on this crate is currently unreachable from Chrome. The proper fix is the
same shape as above, ideally alongside real support for the hybrid group.

---

## `libp2p-webrtc` 0.9.0-alpha.1

A verbatim copy of the published crate with **one change**, applied because
without it every inbound browser connection kills a worker thread in the proxy.

### The bug

`webrtc-srtp`'s `CipherAeadAesGcm::new` derives an SRTP session key the length
of the negotiated master key, but then always constructs an `Aes128Gcm`:

```rust
// webrtc-srtp-0.14.0/src/cipher/cipher_aead_aes_gcm.rs
let srtp_session_key = aes_cm_key_derivation(
    LABEL_SRTP_ENCRYPTION, master_key, master_salt, 0, master_key.len(),
)?;
let srtp_block = GenericArray::from_slice(&srtp_session_key);
let srtp_cipher = Aes128Gcm::new(srtp_block);   // <- 16-byte key type
```

With `SRTP_AEAD_AES_256_GCM` the derived key is 32 bytes, so `from_slice`
panics:

```
thread 'tokio-rt-worker' panicked at generic-array-0.14.7/src/lib.rs:572:9:
assertion `left == right` failed
  left: 32
 right: 16
    at webrtc_srtp::key_derivation::aes_cm_key_derivation
    at webrtc_srtp::cipher::cipher_aead_aes_gcm::CipherAeadAesGcm::new
    at webrtc_srtp::session::Session::new
```

Which profile gets used is **the remote peer's choice**: `webrtc-dtls`'s
`find_matching_srtp_profile(remote, local)` iterates the remote list first and
returns the first entry the local side also offers. `webrtc`'s defaults include
`Srtp_Aead_Aes_256_Gcm`, so any client preferring it triggers the panic.
`libdatachannel` (used by `@libp2p/webrtc` under Node) does exactly that.

### The change

`src/tokio/upgrade.rs`, in `setting_engine()`:

```rust
se.set_srtp_protection_profiles(vec![
    SrtpProtectionProfile::Srtp_Aead_Aes_128_Gcm,
    SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80,
]);
```

Not offering the profile means it can never be selected. Nothing is given up:
this transport carries only SCTP data channels, so no SRTP session ever protects
media here — the code path exists only because `webrtc-rs` sets up SRTP
unconditionally after the DTLS handshake.

### Reproducing and reverting

`git diff` against the pristine crate:

```sh
diff -ru ~/.cargo/registry/src/*/libp2p-webrtc-0.9.0-alpha.1 vendor/libp2p-webrtc
```

The patch is wired in through `crates/proxy/Cargo.toml`:

```toml
[patch.crates-io]
libp2p-webrtc = { path = "../../vendor/libp2p-webrtc" }
```

Delete that section and the `vendor/` directory to go back to the published
crate — and to reproduce the panic.

### Upstream

Two candidate fixes, neither filed yet:

- **`webrtc-rs`** — make `CipherAeadAesGcm` honour the profile and use
  `Aes256Gcm` for 256-bit keys. `webrtc-srtp` is at 0.17.2 upstream, far ahead
  of the 0.14.0 that `libp2p-webrtc` 0.9.0-alpha.1 pins transitively via
  `webrtc ^0.12`; worth checking whether this is already fixed there.
- **`rust-libp2p`** — apply this change, or something like it, so a
  data-channel-only transport does not negotiate SRTP profiles it will never
  use. This is the smaller and safer of the two.
