//! `autonomi-webrtc-proxy` — a content-addressed fetch proxy for browsers.
//!
//! Listens for WebRTC-Direct connections (no DNS, no CA, no signaling server)
//! and answers exactly one question: "what bytes are stored at this address?".
//!
//! It performs no cryptography, parses no data maps, and never sees plaintext.
//! A browser client verifies every returned byte against the address it asked
//! for, so a malicious proxy can withhold data but cannot forge it.

mod protocol;
mod source;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::Transport;
use libp2p::{Multiaddr, PeerId, Swarm};

use crate::source::{ChunkSource, FixtureSource, NetworkSource, Source};

#[derive(Parser, Debug)]
#[command(name = "autonomi-webrtc-proxy", version, about)]
struct Args {
    /// UDP port to listen on for WebRTC-Direct.
    #[arg(long, default_value_t = 4001)]
    port: u16,

    /// Address to bind. Use 0.0.0.0 on a public host; 127.0.0.1 for local
    /// testing (see docs/FINDINGS.md and the work order §7 on why LAN IPs
    /// are the wrong thing to test against).
    #[arg(long, default_value = "0.0.0.0")]
    bind: std::net::IpAddr,

    /// Public IP to advertise in the printed multiaddr, when it differs from
    /// the bind address (the usual case behind a cloud NAT).
    #[arg(long)]
    advertise: Option<std::net::IpAddr>,

    /// Where the DTLS certificate and identity keypair are persisted.
    ///
    /// Both must survive restarts: the certhash and PeerId are baked into the
    /// client, and regenerating either breaks every deployed copy of the app.
    #[arg(long, default_value = "./proxy-identity")]
    state_dir: PathBuf,

    /// Serve a self-encrypted local file instead of the live network.
    ///
    /// Exercises the complete browser-side flow (data map, chunk fetch,
    /// verification, decryption) with no network dependency.
    #[arg(long, conflicts_with = "bootstrap")]
    fixture: Option<PathBuf>,

    /// Bootstrap peers (`ip:port`) for the live network. Defaults to the
    /// production set when neither this nor `--fixture` is given.
    #[arg(long, value_delimiter = ',')]
    bootstrap: Vec<SocketAddr>,

    /// Write the fixture's chunks to a directory (named by hex address) and
    /// exit, instead of listening. Lets the client be tested without a
    /// transport in the way.
    #[arg(long, requires = "fixture")]
    dump_to: Option<PathBuf>,
}

/// Production bootstrap peers, from `ant-node/config/bootstrap_peers.toml`.
const DEFAULT_BOOTSTRAP: &[&str] = &[
    "207.148.94.42:10000",
    "45.77.50.10:10000",
    "66.135.23.83:10000",
    "149.248.9.2:10000",
    "49.12.119.240:10000",
    "5.161.25.133:10000",
    "18.228.202.183:10000",
];

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,libp2p_webrtc=warn".into()),
        )
        .init();

    install_crypto_provider();

    let args = Args::parse();

    tokio::fs::create_dir_all(&args.state_dir)
        .await
        .with_context(|| format!("failed to create state dir {}", args.state_dir.display()))?;

    let keypair = load_or_create_keypair(&args.state_dir.join("identity.key")).await?;
    let certificate = load_or_create_certificate(&args.state_dir.join("cert.pem")).await?;
    let local_peer_id = PeerId::from(keypair.public());

    let (source, data_map_address) = match &args.fixture {
        Some(path) => {
            let content = tokio::fs::read(path)
                .await
                .with_context(|| format!("failed to read fixture {}", path.display()))?;
            let fixture = FixtureSource::new(bytes::Bytes::from(content))?;
            tracing::info!(
                file = %path.display(),
                bytes = fixture.plaintext_len,
                chunks = fixture.chunk_count,
                "fixture self-encrypted"
            );

            if let Some(dir) = &args.dump_to {
                return dump(&fixture, dir).await;
            }

            let address = fixture.data_map_address;
            (Source::Fixture(fixture), Some(address))
        }
        None => {
            let bootstrap = if args.bootstrap.is_empty() {
                DEFAULT_BOOTSTRAP
                    .iter()
                    .map(|peer| peer.parse().expect("built-in bootstrap peers are valid"))
                    .collect()
            } else {
                args.bootstrap.clone()
            };

            tracing::info!(peers = bootstrap.len(), "connecting to the Autonomi network");
            let network = NetworkSource::connect(&bootstrap).await?;
            tracing::info!("connected");
            (Source::Network(network), None)
        }
    };

    let source = Arc::new(source);

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_other_transport(|key| {
            libp2p_webrtc::tokio::Transport::new(key.clone(), certificate)
                .map(|(peer_id, conn), _| (peer_id, StreamMuxerBox::new(conn)))
        })?
        .with_behaviour(|key| Behaviour {
            stream: libp2p_stream::Behaviour::new(),
            identify: libp2p::identify::Behaviour::new(libp2p::identify::Config::new(
                "/autonomi-webrtc-proxy/0.1.0".into(),
                key.public(),
            )),
        })?
        // WebRTC-Direct setup costs ~5-6 RTT, so connections are expensive to
        // establish and cheap to hold. Keep them open across a whole download.
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(600)))
        .build();

    let mut control = swarm.behaviour().stream.new_control();
    let incoming = control
        .accept(protocol::PROTOCOL)
        .context("failed to register the fetch protocol")?;

    tokio::spawn(serve(incoming, Arc::clone(&source)));

    let listen_addr: Multiaddr = format!("/ip4/{}/udp/{}/webrtc-direct", args.bind, args.port)
        .parse()
        .context("failed to build the listen multiaddr")?;
    swarm
        .listen_on(listen_addr)
        .context("failed to listen for WebRTC-Direct connections")?;

    run(&mut swarm, local_peer_id, args.advertise, data_map_address).await
}

/// What the proxy speaks.
///
/// `identify` is not optional in practice: js-libp2p asks a new peer for its
/// protocol list and, until that answer arrives, holds back every stream it
/// opens. Without it each request paid a fixed ~10 second negotiation stall
/// before any bytes moved — the single largest source of latency in the demo.
#[derive(libp2p::swarm::NetworkBehaviour)]
struct Behaviour {
    stream: libp2p_stream::Behaviour,
    identify: libp2p::identify::Behaviour,
}

/// Pick a rustls crypto provider before anything can ask for one.
///
/// The network client pulls rustls in twice over (QUIC via `saorsa-transport`,
/// HTTP via `alloy`/`reqwest`), and the graph ends up containing both `ring`
/// and `aws-lc-rs`. rustls refuses to guess between them and panics inside
/// whichever background task touches TLS first — which, being a task, takes
/// down the connection rather than the process, and looks like a mysterious
/// timeout from the browser side.
fn install_crypto_provider() {
    // Errs only if a provider is already installed, which is equally fine.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Write every fixture chunk to `dir`, named by its hex address.
async fn dump(fixture: &FixtureSource, dir: &std::path::Path) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;

    for address in fixture.addresses() {
        let bytes = fixture
            .get(address)
            .await?
            .expect("addresses() only yields addresses the source holds");
        tokio::fs::write(dir.join(hex::encode(address)), &bytes).await?;
    }

    tokio::fs::write(
        dir.join("data-map-address"),
        hex::encode(fixture.data_map_address),
    )
    .await?;

    println!("{}", hex::encode(fixture.data_map_address));
    Ok(())
}

/// Handle inbound fetch streams, one request per stream.
async fn serve<S: ChunkSource>(mut incoming: libp2p_stream::IncomingStreams, source: Arc<S>) {
    while let Some((peer, stream)) = incoming.next().await {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            if let Err(error) = handle_request(stream, source).await {
                tracing::debug!(%peer, %error, "fetch stream failed");
            }
        });
    }
}

async fn handle_request<S: ChunkSource>(
    mut stream: libp2p::Stream,
    source: Arc<S>,
) -> Result<()> {
    let address = protocol::read_request(&mut stream).await?;

    match source.get(address).await? {
        Some(bytes) => {
            tracing::info!(address = %hex::encode(address), len = bytes.len(), "serving chunk");
            protocol::write_found(&mut stream, &bytes).await?;
        }
        None => {
            tracing::info!(address = %hex::encode(address), "chunk not found");
            protocol::write_status(&mut stream, protocol::STATUS_NOT_FOUND).await?;
        }
    }

    // Wait for the client's EOF before letting the stream drop.
    //
    // `libp2p-webrtc`'s drop listener sends a RESET for any stream that is
    // dropped without a graceful close, and that RESET can reach the client
    // while it is still reading the response — which is exactly what the
    // browser reported as "the stream has been reset". Reading to EOF lets the
    // close complete in both directions first.
    protocol::drain_to_eof(&mut stream).await;
    Ok(())
}

/// Drive the swarm, reporting each listen address in the form a browser needs.
async fn run(
    swarm: &mut Swarm<Behaviour>,
    local_peer_id: PeerId,
    advertise: Option<std::net::IpAddr>,
    data_map_address: Option<[u8; 32]>,
) -> Result<()> {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                return Ok(());
            }
            event = swarm.select_next_some() => match event {
                libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                    let dial = advertised(address, advertise).with_p2p(local_peer_id)
                        .expect("peer id is well-formed");
                    println!();
                    println!("  multiaddr : {dial}");
                    if let Some(address) = data_map_address {
                        println!("  fixture   : {}", hex::encode(address));
                    }
                    println!();
                }
                libp2p::swarm::SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    tracing::info!(%peer_id, "browser connected");
                }
                libp2p::swarm::SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                    tracing::info!(%peer_id, ?cause, "browser disconnected");
                }
                _ => {}
            }
        }
    }
}

/// Replace the bound IP with the public one, keeping port and certhash intact.
fn advertised(address: Multiaddr, advertise: Option<std::net::IpAddr>) -> Multiaddr {
    let Some(public) = advertise else {
        return address;
    };
    address
        .into_iter()
        .map(|segment| match (segment, public) {
            (libp2p::multiaddr::Protocol::Ip4(_), std::net::IpAddr::V4(ip)) => {
                libp2p::multiaddr::Protocol::Ip4(ip)
            }
            (libp2p::multiaddr::Protocol::Ip6(_), std::net::IpAddr::V6(ip)) => {
                libp2p::multiaddr::Protocol::Ip6(ip)
            }
            (other, _) => other,
        })
        .collect()
}

/// Load the node identity, creating it on first run.
///
/// Stable across restarts by design: the PeerId is part of the multiaddr that
/// clients hard-code.
async fn load_or_create_keypair(path: &std::path::Path) -> Result<libp2p::identity::Keypair> {
    if let Ok(encoded) = tokio::fs::read(path).await {
        return libp2p::identity::Keypair::from_protobuf_encoding(&encoded)
            .with_context(|| format!("failed to decode identity at {}", path.display()));
    }

    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let encoded = keypair
        .to_protobuf_encoding()
        .context("failed to encode the generated identity")?;
    tokio::fs::write(path, &encoded)
        .await
        .with_context(|| format!("failed to persist identity to {}", path.display()))?;
    tracing::info!(path = %path.display(), "generated a new node identity");
    Ok(keypair)
}

/// Load the DTLS certificate, creating it on first run.
///
/// Its fingerprint is the `certhash` in the multiaddr, which is what replaces
/// the CA: the browser pins this hash instead of validating a chain.
async fn load_or_create_certificate(
    path: &std::path::Path,
) -> Result<libp2p_webrtc::tokio::Certificate> {
    if let Ok(pem) = tokio::fs::read_to_string(path).await {
        return libp2p_webrtc::tokio::Certificate::from_pem(&pem)
            .with_context(|| format!("failed to parse certificate at {}", path.display()));
    }

    let certificate = libp2p_webrtc::tokio::Certificate::generate(&mut rand::thread_rng())
        .context("failed to generate a DTLS certificate")?;
    tokio::fs::write(path, certificate.serialize_pem())
        .await
        .with_context(|| format!("failed to persist certificate to {}", path.display()))?;
    tracing::info!(path = %path.display(), "generated a new DTLS certificate");
    Ok(certificate)
}
