//! Browser-side Autonomi retrieval.
//!
//! Everything that decides whether the returned bytes are trustworthy lives
//! here: address verification, data-map walking, and self-decryption. The
//! network is somebody else's problem — this crate never opens a socket.
//!
//! # Why a state machine rather than a `retrieve(address, getChunk)` callback
//!
//! `self_encryption::get_root_data_map` takes a **synchronous** fetcher
//! (`FnMut(XorName) -> Result<Bytes>`), but fetching a chunk in a browser is
//! inherently asynchronous. Rather than block, [`Retrieval`] inverts control:
//! it tells JavaScript which addresses it needs, JavaScript supplies the bytes
//! whenever they arrive, and the synchronous parts run over an in-memory cache.
//!
//! The resulting loop on the JS side:
//!
//! ```js
//! const r = Retrieval.begin(address, dataMapBytes)   // throws if bytes != address
//! while (!r.is_complete) {
//!   for (const addr of chunks(r.required_addresses())) r.supply(addr, await getChunk(addr))
//!   r.advance()                                      // resolves a shrunk data map
//! }
//! const plaintext = r.finish()
//! ```
//!
//! # The trust boundary
//!
//! [`Retrieval::supply`] verifies every chunk against the address it was
//! requested under before accepting it, and [`Retrieval::begin`] does the same
//! for the data map itself. Since the content address *is* the BLAKE3 hash of
//! the bytes, a proxy that alters or substitutes any byte is rejected here. It
//! can refuse to serve; it cannot lie.

use std::collections::HashMap;

use self_encryption::bytes::Bytes;
use self_encryption::{decrypt, get_root_data_map, verify_chunk, DataMap, EncryptedChunk, XorName};
use wasm_bindgen::prelude::*;

/// Length of a content address, in bytes.
const ADDRESS_LEN: usize = 32;

/// A retrieval failure, reported to JavaScript as a thrown `Error`.
///
/// Deliberately not `JsError`: that type can only be constructed inside a wasm
/// runtime, which would make the logic in this crate untestable outside the
/// browser. The conversion to a JS value happens at the boundary and only ever
/// runs on wasm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<Error> for JsValue {
    fn from(error: Error) -> Self {
        js_sys::Error::new(&error.0).into()
    }
}

/// An in-progress retrieval of one piece of content.
#[wasm_bindgen]
pub struct Retrieval {
    /// The data map currently being worked on: initially the one fetched from
    /// the pasted address, later its resolved root form.
    map: DataMap,
    /// Whether `map` is already the root (flat) map.
    root_resolved: bool,
    /// Verified chunks, keyed by content address.
    cache: HashMap<XorName, EncryptedChunk>,
}

#[wasm_bindgen]
impl Retrieval {
    /// Start from the chunk stored at a public address.
    ///
    /// `bytes` must be the chunk stored at `address`; it is verified against
    /// that address before being parsed. Errors if verification fails or the
    /// bytes are not a MessagePack-encoded data map.
    pub fn begin(address: &[u8], bytes: &[u8]) -> Result<Retrieval, Error> {
        let address = parse_address(address)?;
        verify_chunk(address, bytes)
            .map_err(|e| Error::new(&format!("data map chunk failed verification: {e}")))?;

        let map: DataMap = rmp_serde::from_slice(bytes).map_err(|e| {
            Error::new(&format!(
                "address does not hold a data map (MessagePack decode failed: {e})"
            ))
        })?;

        let root_resolved = !map.is_child();
        Ok(Retrieval {
            map,
            root_resolved,
            cache: HashMap::new(),
        })
    }

    /// Addresses still needed, concatenated as 32-byte records.
    ///
    /// Returned flat rather than as an array of arrays to keep the JS boundary
    /// to a single copy.
    pub fn required_addresses(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for info in self.map.infos() {
            if !self.cache.contains_key(&info.dst_hash) {
                out.extend_from_slice(&info.dst_hash.0);
            }
        }
        out
    }

    /// Accept the bytes for one address, verifying them first.
    ///
    /// Errors if the bytes do not hash to `address` — the check that makes the
    /// proxy untrusted.
    pub fn supply(&mut self, address: &[u8], bytes: &[u8]) -> Result<(), Error> {
        let address = parse_address(address)?;
        let chunk = verify_chunk(address, bytes).map_err(|e| {
            Error::new(&format!(
                "chunk {} failed verification: {e}",
                hex(&address.0)
            ))
        })?;
        self.cache.insert(address, chunk);
        Ok(())
    }

    /// Make progress once every currently-required chunk has been supplied.
    ///
    /// For a large upload the fetched map is *shrunk*: its entries point at
    /// wrapper chunks rather than content chunks. Resolving it yields the root
    /// map, after which [`Self::required_addresses`] reports the real content
    /// chunks. For a flat map this is a no-op.
    pub fn advance(&mut self) -> Result<(), Error> {
        if self.root_resolved || !self.required_addresses().is_empty() {
            return Ok(());
        }

        let cache = &self.cache;
        let mut fetch = |address: XorName| {
            cache
                .get(&address)
                .map(|chunk| chunk.content.clone())
                .ok_or_else(|| {
                    self_encryption::Error::Generic(format!(
                        "missing wrapper chunk {}",
                        hex(&address.0)
                    ))
                })
        };

        let root = get_root_data_map(self.map.clone(), &mut fetch)
            .map_err(|e| Error::new(&format!("failed to resolve the root data map: {e}")))?;

        self.map = root;
        self.root_resolved = true;
        // Wrapper chunks are addressed differently from content chunks, so the
        // cache cannot accidentally satisfy the next round; it is kept only
        // because `decrypt` tolerates extra chunks.
        Ok(())
    }

    /// True once the root map is resolved and every content chunk is present.
    #[wasm_bindgen(getter)]
    pub fn is_complete(&self) -> bool {
        self.root_resolved && self.required_addresses().is_empty()
    }

    /// Number of chunks the current map references.
    #[wasm_bindgen(getter)]
    pub fn chunk_count(&self) -> usize {
        self.map.infos().len()
    }

    /// Number of chunks already verified and held.
    #[wasm_bindgen(getter)]
    pub fn chunks_held(&self) -> usize {
        self.map
            .infos()
            .iter()
            .filter(|info| self.cache.contains_key(&info.dst_hash))
            .count()
    }

    /// Decrypt and reassemble the content.
    ///
    /// Errors unless [`Self::is_complete`] is true.
    pub fn finish(&self) -> Result<Vec<u8>, Error> {
        if !self.is_complete() {
            return Err(Error::new(
                "cannot finish: chunks are still missing or the root map is unresolved",
            ));
        }

        let chunks: Vec<EncryptedChunk> = self
            .map
            .infos()
            .iter()
            .map(|info| {
                self.cache
                    .get(&info.dst_hash)
                    .cloned()
                    .ok_or_else(|| Error::new(&format!("missing chunk {}", hex(&info.dst_hash.0))))
            })
            .collect::<Result<_, _>>()?;

        let plaintext: Bytes = decrypt(&self.map, &chunks)
            .map_err(|e| Error::new(&format!("decryption failed: {e}")))?;

        Ok(plaintext.to_vec())
    }
}

/// The network's content address for a blob: BLAKE3 of the bytes.
///
/// Matches `ant_protocol::compute_address` and the hash `verify_chunk` checks
/// against. Note that `XorName::from_content` is **not** this function — it is
/// SHA3-256, a legacy of the pre-BLAKE3 addressing scheme, and using it here
/// would produce addresses the network has never heard of.
#[wasm_bindgen]
pub fn content_address(bytes: &[u8]) -> Vec<u8> {
    blake3::hash(bytes).as_bytes().to_vec()
}

/// Parse a hex address into its 32 raw bytes.
///
/// Matches the Autonomi CLI's `parse_address`: plain hex, no multibase or
/// prefix (docs/FINDINGS.md §4).
#[wasm_bindgen]
pub fn parse_hex_address(text: &str) -> Result<Vec<u8>, Error> {
    let text = text.trim();
    if text.len() != ADDRESS_LEN * 2 {
        return Err(Error::new(&format!(
            "expected {} hex characters, got {}",
            ADDRESS_LEN * 2,
            text.len()
        )));
    }
    let mut out = Vec::with_capacity(ADDRESS_LEN);
    for pair in text.as_bytes().chunks(2) {
        let hi = hex_digit(pair[0])?;
        let lo = hex_digit(pair[1])?;
        out.push(hi << 4 | lo);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8, Error> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::new("address contains a non-hex character")),
    }
}

fn parse_address(bytes: &[u8]) -> Result<XorName, Error> {
    let array: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| Error::new(&format!("address must be {ADDRESS_LEN} bytes")))?;
    Ok(XorName(array))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A stand-in for the network: address -> bytes, content-addressed.
    struct Store(HashMap<[u8; 32], Vec<u8>>);

    impl Store {
        fn get(&self, address: &[u8]) -> Vec<u8> {
            let key: [u8; 32] = address.try_into().unwrap();
            self.0.get(&key).expect("address present in store").clone()
        }
    }

    fn address_of(bytes: &[u8]) -> [u8; 32] {
        *blake3::hash(bytes).as_bytes()
    }

    fn content(len: usize) -> Bytes {
        Bytes::from((0..len).map(|i| (i % 251) as u8).collect::<Vec<u8>>())
    }

    /// Store content the way the Autonomi client does: chunks by address, plus
    /// the MessagePack-serialized data map as its own chunk.
    fn upload(data: &Bytes, shrink: bool) -> (Store, [u8; 32]) {
        let (map, chunks) = self_encryption::encrypt(data.clone()).unwrap();
        let mut store = HashMap::new();
        for chunk in &chunks {
            store.insert(address_of(&chunk.content), chunk.content.to_vec());
        }

        let map = if shrink {
            // Mirrors what a client producing shrunk maps does: the serialized
            // map is recursively encrypted into wrapper chunks which are stored
            // alongside the content chunks.
            let (child, _wrappers) =
                self_encryption::shrink_data_map(map, |name: XorName, bytes: Bytes| {
                    store.insert(name.0, bytes.to_vec());
                    Ok(())
                })
                .unwrap();
            child
        } else {
            map
        };

        let serialized = rmp_serde::to_vec(&map).unwrap();
        let data_map_address = address_of(&serialized);
        store.insert(data_map_address, serialized);
        (Store(store), data_map_address)
    }

    /// Drive the retrieval the way the browser does, returning the plaintext.
    fn retrieve(store: &Store, address: [u8; 32]) -> Vec<u8> {
        let mut retrieval = Retrieval::begin(&address, &store.get(&address)).unwrap();
        for _ in 0..8 {
            if retrieval.is_complete() {
                break;
            }
            let required = retrieval.required_addresses();
            for chunk_address in required.chunks(32) {
                let bytes = store.get(chunk_address);
                retrieval.supply(chunk_address, &bytes).unwrap();
            }
            retrieval.advance().unwrap();
        }
        assert!(retrieval.is_complete(), "retrieval did not converge");
        retrieval.finish().unwrap()
    }

    #[test]
    fn retrieves_a_flat_data_map() {
        let data = content(90_000);
        let (store, address) = upload(&data, false);
        assert_eq!(retrieve(&store, address), data.to_vec());
    }

    #[test]
    fn retrieves_through_a_shrunk_data_map() {
        // Large enough that shrinking produces a genuine child map whose
        // entries point at wrapper chunks rather than content chunks.
        let data = content(20 * 1024 * 1024);
        let (store, address) = upload(&data, true);

        let map: DataMap = rmp_serde::from_slice(&store.get(&address)).unwrap();
        assert!(map.is_child(), "test fixture did not produce a child map");

        assert_eq!(retrieve(&store, address), data.to_vec());
    }

    /// The security claim of the whole design: a proxy that alters a byte is
    /// caught, because the address is the hash.
    #[test]
    fn rejects_a_tampered_chunk() {
        let data = content(90_000);
        let (store, address) = upload(&data, false);

        let mut retrieval = Retrieval::begin(&address, &store.get(&address)).unwrap();
        let required = retrieval.required_addresses();
        let target = &required[..32];

        let mut tampered = store.get(target);
        tampered[0] ^= 0x01;

        assert!(
            retrieval.supply(target, &tampered).is_err(),
            "a modified chunk was accepted"
        );
    }

    /// Substituting a *different* valid chunk must fail too — the address is
    /// checked, not merely the internal consistency of the bytes.
    #[test]
    fn rejects_a_substituted_chunk() {
        let data = content(90_000);
        let (store, address) = upload(&data, false);

        let mut retrieval = Retrieval::begin(&address, &store.get(&address)).unwrap();
        let required = retrieval.required_addresses();
        let (wanted, other) = (&required[..32], &required[32..64]);

        assert!(
            retrieval.supply(wanted, &store.get(other)).is_err(),
            "a chunk served under the wrong address was accepted"
        );
    }

    #[test]
    fn rejects_a_tampered_data_map() {
        let data = content(90_000);
        let (store, address) = upload(&data, false);

        let mut tampered = store.get(&address);
        tampered[0] ^= 0x01;

        assert!(Retrieval::begin(&address, &tampered).is_err());
    }

    #[test]
    fn parses_hex_addresses() {
        let address = [0xabu8; 32];
        assert_eq!(parse_hex_address(&hex(&address)).unwrap(), address.to_vec());
        assert!(parse_hex_address("abcd").is_err());
        assert!(parse_hex_address(&"zz".repeat(32)).is_err());
    }
}
