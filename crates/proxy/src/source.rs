//! Where the proxy gets chunk bytes from.
//!
//! The proxy's entire job is `address -> bytes`. Everything cryptographically
//! meaningful (verification, data-map walking, decryption) happens in the
//! browser, so a source is allowed to be completely dumb — and, importantly,
//! is allowed to be *wrong* without compromising the client.
//!
//! Two implementations:
//!
//! - [`FixtureSource`] self-encrypts a local file at startup and serves the
//!   resulting chunks from memory. It exercises the full browser-side flow
//!   (data map -> chunks -> verify -> decrypt) with no network involved, which
//!   is what makes the client testable before the live-network path exists.
//! - `NetworkSource` (Stage 1) will wrap `ant_core::data::Client::chunk_get`.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use bytes::Bytes;

/// A content-addressed byte store.
pub trait ChunkSource: Send + Sync + 'static {
    /// Fetch the bytes stored at `address`, or `None` if nothing is there.
    fn get(
        &self,
        address: [u8; 32],
    ) -> impl std::future::Future<Output = Result<Option<Bytes>>> + Send;
}

/// An in-memory source built by self-encrypting a local file.
///
/// Serves both the serialized `DataMap` chunk and every content chunk through
/// the same address space, exactly as the real network does.
pub struct FixtureSource {
    chunks: HashMap<[u8; 32], Bytes>,
    /// Address of the chunk holding the serialized `DataMap` — the address a
    /// user would paste into the browser app.
    pub data_map_address: [u8; 32],
    pub plaintext_len: usize,
    pub chunk_count: usize,
}

impl FixtureSource {
    /// Self-encrypt `content` and index every resulting chunk by its address.
    pub fn new(content: Bytes) -> Result<Self> {
        let plaintext_len = content.len();
        let (data_map, encrypted) = self_encryption::encrypt(content)
            .map_err(|e| anyhow::anyhow!("self-encryption failed: {e}"))?;

        let mut chunks = HashMap::new();
        for chunk in &encrypted {
            chunks.insert(address_of(&chunk.content), chunk.content.clone());
        }

        // Stored exactly the way the Autonomi client stores it: a MessagePack
        // encoding of the DataMap, put as an ordinary content-addressed chunk.
        // See docs/FINDINGS.md §3 — this is rmp_serde, not bincode.
        let serialized = Bytes::from(
            rmp_serde::to_vec(&data_map).context("failed to serialize the data map")?,
        );
        let data_map_address = address_of(&serialized);
        let chunk_count = data_map.infos().len();
        chunks.insert(data_map_address, serialized);

        Ok(Self {
            chunks,
            data_map_address,
            plaintext_len,
            chunk_count,
        })
    }

    /// Every address this source can serve — the natural allowlist for a demo.
    pub fn addresses(&self) -> Vec<[u8; 32]> {
        self.chunks.keys().copied().collect()
    }
}

impl ChunkSource for FixtureSource {
    async fn get(&self, address: [u8; 32]) -> Result<Option<Bytes>> {
        Ok(self.chunks.get(&address).cloned())
    }
}

/// The live Autonomi network, reached over QUIC.
///
/// This is an ordinary network *client*, not a node: it joins nothing, stores
/// nothing and needs no wallet, because reads are free. All it does is ask the
/// closest peers for a chunk by address.
pub struct NetworkSource {
    client: ant_core::data::Client,
}

impl NetworkSource {
    /// Connect to the network via the given bootstrap peers.
    pub async fn connect(bootstrap: &[SocketAddr]) -> Result<Self> {
        let client = ant_core::data::Client::connect(bootstrap, Default::default())
            .await
            .context("failed to connect to the Autonomi network")?;
        Ok(Self { client })
    }
}

impl ChunkSource for NetworkSource {
    async fn get(&self, address: [u8; 32]) -> Result<Option<Bytes>> {
        let chunk = self
            .client
            .chunk_get(&address)
            .await
            .with_context(|| format!("DHT lookup failed for {}", hex::encode(address)))?;
        Ok(chunk.map(|chunk| chunk.content))
    }
}

/// The two things a proxy can serve from.
///
/// An enum rather than a trait object because [`ChunkSource::get`] returns an
/// opaque future, which cannot be made into a `dyn` trait without boxing.
pub enum Source {
    Fixture(FixtureSource),
    Network(NetworkSource),
}

impl ChunkSource for Source {
    async fn get(&self, address: [u8; 32]) -> Result<Option<Bytes>> {
        match self {
            Source::Fixture(source) => source.get(address).await,
            Source::Network(source) => source.get(address).await,
        }
    }
}

/// The network's content address for a blob: BLAKE3 of the bytes.
///
/// Matches `ant_protocol::compute_address` and `self_encryption`'s
/// `hash::content_hash` — verified identical in docs/FINDINGS.md §5.
pub fn address_of(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}
