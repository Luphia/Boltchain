//! Boltchain networking on libp2p.
//!
//! * Transports: QUIC (preferred) and TCP + Noise + Yamux, with DNS (including `/dnsaddr`).
//! * Discovery: Kademlia on the chain's own protocol id (`/bolt/<chain id>/kad/1.0.0`), so the
//!   Boltchain swarm stays separate from the public IPFS DHT.
//! * Announcements: gossipsub topic `/bolt/<chain id>/announce`, signed by the author.
//! * Block exchange: Bitswap 1.2.0 (see [`bitswap`]).
//! * Transaction forwarding: request-response `/bolt/<chain id>/tx/1.0.0` to the block producer.

pub mod bitswap;

use alloy_primitives::B256;
use bitswap::{Bitswap, BlockSource, FetchError};
use bolt_ipld::Cid;
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, gossipsub, identify, identity, kad,
    multiaddr::Protocol,
    noise, ping, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

/// Default P2P port (TCP and UDP/QUIC).
pub const DEFAULT_PORT: u16 = 8017;

/// Maximum gossip message (announcements may inline up to 64 KiB of body).
pub const MAX_GOSSIP_BYTES: usize = 512 * 1024;

/// Mainnet bootstrap nodes. Each name publishes `_dnsaddr` TXT records with its current
/// addresses and peer id, so hosts can move without a client release.
pub fn mainnet_bootnodes() -> Vec<Multiaddr> {
    (1..=9)
        .map(|i| format!("/dnsaddr/node{i:03}.cafeca.io").parse().expect("valid multiaddr"))
        .collect()
}

/// Default listen addresses on `port`.
pub fn default_listen(port: u16) -> Vec<Multiaddr> {
    vec![
        format!("/ip4/0.0.0.0/udp/{port}/quic-v1").parse().expect("valid multiaddr"),
        format!("/ip4/0.0.0.0/tcp/{port}").parse().expect("valid multiaddr"),
    ]
}

/// A body chunk carried inside an announcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineBlock {
    /// CID.
    pub cid: Cid,
    /// Content.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// New-block announcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announce {
    /// Block number.
    pub height: u64,
    /// Envelope CID.
    pub root: Cid,
    /// Envelope bytes (small; saves one round trip).
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
    /// Header RLP (about 600 bytes).
    #[serde(with = "serde_bytes")]
    pub header: Vec<u8>,
    /// Body chunks, when the body is small enough to inline.
    pub inline: Vec<InlineBlock>,
    /// Finality proof (consensus networks): encoded `CommitProof` for this block.
    #[serde(default, with = "serde_bytes")]
    pub proof: Vec<u8>,
}

impl Announce {
    /// Encodes as dag-cbor.
    pub fn encode(&self) -> Vec<u8> {
        serde_ipld_dagcbor::to_vec(self).expect("announce serializes")
    }

    /// Decodes dag-cbor.
    pub fn decode(b: &[u8]) -> Option<Self> {
        serde_ipld_dagcbor::from_slice(b).ok()
    }
}

/// Transaction forwarding request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRequest(#[serde(with = "serde_bytes")] pub Vec<u8>);

/// Transaction forwarding response: the hash, or the pool's rejection reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxResponse(pub Result<B256, String>);

/// Network configuration.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Chain id (namespaces every protocol).
    pub chain_id: u64,
    /// Node identity.
    pub keypair: identity::Keypair,
    /// Listen addresses.
    pub listen: Vec<Multiaddr>,
    /// Peers to dial at start (with or without `/p2p/<id>`; `/dnsaddr` supported).
    pub bootnodes: Vec<Multiaddr>,
    /// Only announcements authored by this peer are accepted and relayed (single-producer
    /// devnet). `None` accepts announcements from anyone (validation moves to consensus in M3).
    pub producer: Option<PeerId>,
    /// This node's fork id (ADR 0008 §2), advertised in the identify agent string.
    pub fork_id: String,
    /// Judges a peer's fork id; peers on incompatible rules are disconnected.
    pub fork_check: Option<ForkCheck>,
}

/// Verdict on a peer's fork id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRules {
    /// Same rules.
    Compatible,
    /// The peer runs software with a fork this node does not know: the operator should upgrade.
    Newer,
    /// Different rules.
    Incompatible,
}

/// Fork id checker: peer fork id string -> verdict.
#[derive(Clone)]
pub struct ForkCheck(pub std::sync::Arc<dyn Fn(&str) -> PeerRules + Send + Sync>);

impl std::fmt::Debug for ForkCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForkCheck")
    }
}

/// Events delivered to the node.
#[derive(Debug)]
pub enum NetEvent {
    /// A valid-looking announcement arrived.
    Announce {
        /// Peer that relayed it to us (best first source for its blocks).
        via: PeerId,
        /// The announcement.
        announce: Announce,
    },
    /// A peer forwarded a transaction; answer through `reply`.
    Tx {
        /// Raw EIP-2718 bytes.
        raw: Vec<u8>,
        /// Reply channel.
        reply: oneshot::Sender<Result<B256, String>>,
    },
    /// A consensus message (proposal, vote or timeout), still encoded.
    Consensus {
        /// Peer that relayed it.
        via: PeerId,
        /// dag-cbor bytes.
        data: Vec<u8>,
    },
    /// A storage-layer message (snapshot announcement or audit vote), still encoded.
    Storage {
        /// Peer that relayed it.
        via: PeerId,
        /// Encoded message.
        data: Vec<u8>,
    },
    /// A connection was established.
    Connected(PeerId),
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    stream: libp2p_stream::Behaviour,
    tx: request_response::cbor::Behaviour<TxRequest, TxResponse>,
}

enum Command {
    Publish(Announce),
    PublishConsensus(Vec<u8>),
    PublishStorage(Vec<u8>),
    ForwardTx(PeerId, Vec<u8>, oneshot::Sender<Result<B256, String>>),
    RespondTx(u64, Result<B256, String>),
    Peers(oneshot::Sender<Vec<PeerId>>),
    ListenAddrs(oneshot::Sender<Vec<Multiaddr>>),
    Dial(Multiaddr),
    Shutdown,
}

/// Network errors.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// Transport or behaviour setup failed.
    #[error("setup: {0}")]
    Setup(String),
    /// The network task stopped.
    #[error("network task stopped")]
    Stopped,
    /// Blocks did not arrive in time.
    #[error(transparent)]
    Fetch(#[from] FetchError),
}

/// Handle to the running network.
#[derive(Clone, Debug)]
pub struct NetHandle {
    cmd: mpsc::Sender<Command>,
    bitswap: Bitswap,
    peer_id: PeerId,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::Publish(_) => "Publish",
            Command::PublishConsensus(_) => "PublishConsensus",
            Command::PublishStorage(_) => "PublishStorage",
            Command::ForwardTx(..) => "ForwardTx",
            Command::RespondTx(..) => "RespondTx",
            Command::Peers(_) => "Peers",
            Command::ListenAddrs(_) => "ListenAddrs",
            Command::Dial(_) => "Dial",
            Command::Shutdown => "Shutdown",
        })
    }
}

impl NetHandle {
    /// This node's peer id.
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Publishes an announcement.
    pub async fn publish(&self, a: Announce) -> Result<(), NetError> {
        self.cmd.send(Command::Publish(a)).await.map_err(|_| NetError::Stopped)
    }

    /// Publishes an encoded consensus message.
    pub async fn publish_consensus(&self, data: Vec<u8>) -> Result<(), NetError> {
        self.cmd.send(Command::PublishConsensus(data)).await.map_err(|_| NetError::Stopped)
    }

    /// Publishes an encoded storage-layer message (ADR 0009).
    pub async fn publish_storage(&self, data: Vec<u8>) -> Result<(), NetError> {
        self.cmd.send(Command::PublishStorage(data)).await.map_err(|_| NetError::Stopped)
    }

    /// Asks `peer` alone for one block, bypassing the local blockstore (storage audits).
    pub async fn probe(
        &self,
        cid: Cid,
        peer: PeerId,
        timeout: Duration,
    ) -> Result<Vec<u8>, NetError> {
        Ok(self.bitswap.probe(cid, peer, timeout).await?)
    }

    /// Fetches blocks over bitswap: `peers[0]` first, the others on its DONT_HAVE or after
    /// 150 ms, repeating every 150 ms for up to 5 s.
    pub async fn fetch(
        &self,
        cids: &[Cid],
        peers: &[PeerId],
    ) -> Result<HashMap<Cid, Vec<u8>>, NetError> {
        Ok(self
            .bitswap
            .fetch(cids, peers, Duration::from_millis(150), Duration::from_secs(5))
            .await?)
    }

    /// Forwards a raw transaction to `peer` (the producer) and returns its answer.
    pub async fn forward_tx(&self, peer: PeerId, raw: Vec<u8>) -> Result<B256, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::ForwardTx(peer, raw, tx))
            .await
            .map_err(|_| "network stopped".to_string())?;
        rx.await.map_err(|_| "network stopped".to_string())?
    }

    /// Currently connected peers.
    pub async fn peers(&self) -> Vec<PeerId> {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(Command::Peers(tx)).await.is_err() {
            return vec![];
        }
        rx.await.unwrap_or_default()
    }

    /// Addresses we listen on (with `/p2p/<id>` appended).
    pub async fn listen_addrs(&self) -> Vec<Multiaddr> {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(Command::ListenAddrs(tx)).await.is_err() {
            return vec![];
        }
        rx.await.unwrap_or_default()
    }

    /// Stops the network (closes all connections).
    pub async fn shutdown(&self) {
        let _ = self.cmd.send(Command::Shutdown).await;
    }

    /// Dials an address.
    pub async fn dial(&self, addr: Multiaddr) -> Result<(), NetError> {
        self.cmd.send(Command::Dial(addr)).await.map_err(|_| NetError::Stopped)
    }
}

fn topic(chain_id: u64) -> gossipsub::IdentTopic {
    gossipsub::IdentTopic::new(format!("/bolt/{chain_id}/announce"))
}

fn consensus_topic(chain_id: u64) -> gossipsub::IdentTopic {
    gossipsub::IdentTopic::new(format!("/bolt/{chain_id}/consensus"))
}

fn storage_topic(chain_id: u64) -> gossipsub::IdentTopic {
    gossipsub::IdentTopic::new(format!("/bolt/{chain_id}/storage"))
}

fn peer_of(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|p| if let Protocol::P2p(id) = p { Some(id) } else { None })
}

/// Starts the network.
pub async fn start(
    cfg: NetConfig,
    source: Arc<dyn BlockSource>,
) -> Result<(NetHandle, mpsc::Receiver<NetEvent>), NetError> {
    let setup = |e: &dyn std::fmt::Display| NetError::Setup(e.to_string());
    let chain_id = cfg.chain_id;
    let fork_id = cfg.fork_id.clone();
    let kad_protocol = StreamProtocol::try_from_owned(format!("/bolt/{chain_id}/kad/1.0.0"))
        .map_err(|e| setup(&e))?;
    let tx_protocol = StreamProtocol::try_from_owned(format!("/bolt/{chain_id}/tx/1.0.0"))
        .map_err(|e| setup(&e))?;

    let mut swarm: Swarm<Behaviour> =
        libp2p::SwarmBuilder::with_existing_identity(cfg.keypair.clone())
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|e| setup(&e))?
            .with_quic()
            .with_dns()
            .map_err(|e| setup(&e))?
            .with_behaviour(|key| {
                let peer_id = key.public().to_peer_id();
                let gossip_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_millis(700))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .validate_messages()
                    .max_transmit_size(MAX_GOSSIP_BYTES)
                    .message_id_fn(|m: &gossipsub::Message| {
                        gossipsub::MessageId::from(alloy_primitives::keccak256(&m.data).to_vec())
                    })
                    .build()
                    .map_err(|e| e.to_string())?;
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossip_cfg,
                )?;
                let mut kad_cfg = kad::Config::new(kad_protocol.clone());
                kad_cfg.set_query_timeout(Duration::from_secs(30));
                let mut kad = kad::Behaviour::with_config(
                    peer_id,
                    kad::store::MemoryStore::new(peer_id),
                    kad_cfg,
                );
                kad.set_mode(Some(kad::Mode::Server));
                Ok(Behaviour {
                    identify: identify::Behaviour::new(
                        identify::Config::new(format!("/bolt/{chain_id}/1.0.0"), key.public())
                            .with_agent_version(format!(
                                "boltchain/{} fork/{}",
                                env!("CARGO_PKG_VERSION"),
                                fork_id
                            )),
                    ),
                    ping: ping::Behaviour::default(),
                    kad,
                    gossipsub,
                    stream: libp2p_stream::Behaviour::new(),
                    tx: request_response::cbor::Behaviour::new(
                        [(tx_protocol, request_response::ProtocolSupport::Full)],
                        request_response::Config::default()
                            .with_request_timeout(Duration::from_secs(10)),
                    ),
                })
            })
            .map_err(|e| setup(&e))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(300)))
            .build();

    for addr in &cfg.listen {
        swarm.listen_on(addr.clone()).map_err(|e| setup(&e))?;
    }
    swarm.behaviour_mut().gossipsub.subscribe(&topic(chain_id)).map_err(|e| setup(&e))?;
    swarm.behaviour_mut().gossipsub.subscribe(&consensus_topic(chain_id)).map_err(|e| setup(&e))?;
    swarm.behaviour_mut().gossipsub.subscribe(&storage_topic(chain_id)).map_err(|e| setup(&e))?;
    for addr in &cfg.bootnodes {
        if let Some(peer) = peer_of(addr) {
            swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
        }
        if let Err(e) = swarm.dial(addr.clone()) {
            tracing::warn!(%addr, "dial bootnode: {e}");
        }
    }

    let bitswap = Bitswap::new(swarm.behaviour().stream.new_control(), source);
    let peer_id = *swarm.local_peer_id();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (ev_tx, ev_rx) = mpsc::channel(EVENT_BUFFER);
    tokio::spawn(run(swarm, cfg, kad_protocol, cmd_rx, cmd_tx.clone(), ev_tx));
    Ok((NetHandle { cmd: cmd_tx, bitswap, peer_id }, ev_rx))
}

async fn run(
    mut swarm: Swarm<Behaviour>,
    cfg: NetConfig,
    kad_protocol: StreamProtocol,
    mut cmd_rx: mpsc::Receiver<Command>,
    cmd_tx: mpsc::Sender<Command>,
    ev_tx: mpsc::Sender<NetEvent>,
) {
    let topic = topic(cfg.chain_id);
    let ctopic = consensus_topic(cfg.chain_id);
    let stopic = storage_topic(cfg.chain_id);
    let mut pending_fwd: HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<Result<B256, String>>,
    > = HashMap::new();
    let mut inbound_tx: HashMap<u64, request_response::ResponseChannel<TxResponse>> =
        HashMap::new();
    let mut next_id = 0u64;
    let mut bootstrap_tick = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            _ = bootstrap_tick.tick() => {
                let _ = swarm.behaviour_mut().kad.bootstrap();
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Command::Publish(a) => {
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), a.encode()) {
                            tracing::debug!("publish: {e}");
                        }
                    }
                    Command::PublishConsensus(data) => {
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(ctopic.clone(), data) {
                            tracing::debug!("publish consensus: {e}");
                        }
                    }
                    Command::PublishStorage(data) => {
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(stopic.clone(), data) {
                            tracing::debug!("publish storage: {e}");
                        }
                    }
                    Command::ForwardTx(peer, raw, reply) => {
                        let id = swarm.behaviour_mut().tx.send_request(&peer, TxRequest(raw));
                        pending_fwd.insert(id, reply);
                    }
                    Command::RespondTx(id, res) => {
                        if let Some(ch) = inbound_tx.remove(&id) {
                            let _ = swarm.behaviour_mut().tx.send_response(ch, TxResponse(res));
                        }
                    }
                    Command::Peers(reply) => {
                        let _ = reply.send(swarm.connected_peers().copied().collect());
                    }
                    Command::ListenAddrs(reply) => {
                        let me = *swarm.local_peer_id();
                        let _ = reply.send(swarm.listeners().map(|a| a.clone().with(Protocol::P2p(me))).collect());
                    }
                    Command::Shutdown => break,
                    Command::Dial(addr) => {
                        if let Some(peer) = peer_of(&addr) {
                            swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
                        }
                        let _ = swarm.dial(addr);
                    }
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::NewListenAddr { address, .. } => tracing::info!(%address, "p2p listening"),
                SwarmEvent::ConnectionEstablished { peer_id, num_established, .. }
                    if num_established.get() == 1 =>
                {
                    deliver(&ev_tx, NetEvent::Connected(peer_id));
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                    let remote = info.agent_version.split_whitespace().find_map(|t| t.strip_prefix("fork/"));
                    let verdict = match (&cfg.fork_check, remote) {
                        (None, _) => PeerRules::Compatible,
                        (Some(check), Some(r)) => (check.0)(r),
                        // Not a Boltchain node (e.g. a stock IPFS client fetching blocks over
                        // Bitswap): keep the connection, but it is not a chain peer.
                        (Some(_), None) => {
                            tracing::debug!(%peer_id, agent = %info.agent_version, "non-Boltchain peer (IPFS client)");
                            continue;
                        }
                    };
                    match verdict {
                        PeerRules::Incompatible => {
                            tracing::debug!(%peer_id, agent = %info.agent_version, "peer runs incompatible rules; disconnecting");
                            swarm.behaviour_mut().kad.remove_peer(&peer_id);
                            let _ = swarm.disconnect_peer_id(peer_id);
                            continue;
                        }
                        PeerRules::Newer => tracing::warn!(
                            %peer_id,
                            agent = %info.agent_version,
                            "a peer announces a hard fork this software does not know: upgrade the node"
                        ),
                        PeerRules::Compatible => {}
                    }
                    if info.protocols.contains(&kad_protocol) {
                        for addr in info.listen_addrs {
                            swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source, message_id, message,
                })) if message.topic == ctopic.hash() => {
                    // Signatures are checked by the consensus engine; relay promptly.
                    let _ = swarm.behaviour_mut().gossipsub.report_message_validation_result(
                        &message_id, &propagation_source, gossipsub::MessageAcceptance::Accept,
                    );
                    deliver(&ev_tx, NetEvent::Consensus { via: propagation_source, data: message.data });
                }
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source, message_id, message,
                })) if message.topic == stopic.hash() => {
                    // Checked by the node (signatures, snapshot roots against local headers).
                    let _ = swarm.behaviour_mut().gossipsub.report_message_validation_result(
                        &message_id, &propagation_source, gossipsub::MessageAcceptance::Accept,
                    );
                    deliver(&ev_tx, NetEvent::Storage { via: propagation_source, data: message.data });
                }
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source, message_id, message,
                })) => {
                    let author = message.source;
                    let allowed = match cfg.producer {
                        Some(p) => author == Some(p),
                        None => true,
                    };
                    let decoded = if allowed { Announce::decode(&message.data) } else { None };
                    let acceptance = if decoded.is_some() {
                        gossipsub::MessageAcceptance::Accept
                    } else {
                        gossipsub::MessageAcceptance::Reject
                    };
                    let _ = swarm.behaviour_mut().gossipsub.report_message_validation_result(
                        &message_id, &propagation_source, acceptance,
                    );
                    if let Some(announce) = decoded {
                        deliver(&ev_tx, NetEvent::Announce { via: propagation_source, announce });
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Tx(request_response::Event::Message { message, .. })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let id = next_id;
                        next_id += 1;
                        inbound_tx.insert(id, channel);
                        let (reply, rx) = oneshot::channel();
                        // If the node is saturated the reply is dropped and the peer gets an error.
                        deliver(&ev_tx, NetEvent::Tx { raw: request.0, reply });
                        let cmd_tx = cmd_tx.clone();
                        tokio::spawn(async move {
                            let res = rx.await.unwrap_or_else(|_| Err("node dropped the request".into()));
                            let _ = cmd_tx.send(Command::RespondTx(id, res)).await;
                        });
                    }
                    request_response::Message::Response { request_id, response } => {
                        if let Some(reply) = pending_fwd.remove(&request_id) {
                            let _ = reply.send(response.0);
                        }
                    }
                },
                SwarmEvent::Behaviour(BehaviourEvent::Tx(request_response::Event::OutboundFailure { request_id, error, .. })) => {
                    if let Some(reply) = pending_fwd.remove(&request_id) {
                        let _ = reply.send(Err(format!("forwarding failed: {error}")));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Events buffered between the network task and the node.
const EVENT_BUFFER: usize = 1024;

/// Hands an event to the node without ever blocking the swarm: a node that falls behind loses
/// gossip (which is lossy anyway, and recovered through announcements and certificates) instead
/// of stalling Bitswap and every other protocol for all its peers.
fn deliver(tx: &mpsc::Sender<NetEvent>, ev: NetEvent) {
    if let Err(mpsc::error::TrySendError::Full(ev)) = tx.try_send(ev) {
        tracing::warn!(event = ev.kind(), "node is not keeping up; dropped a network event");
    }
}

impl NetEvent {
    fn kind(&self) -> &'static str {
        match self {
            NetEvent::Announce { .. } => "announce",
            NetEvent::Tx { .. } => "tx",
            NetEvent::Consensus { .. } => "consensus",
            NetEvent::Storage { .. } => "storage",
            NetEvent::Connected(_) => "connected",
        }
    }
}

/// Loads an ed25519 node key from `path`, creating it on first use.
pub fn load_or_create_key(path: &std::path::Path) -> std::io::Result<identity::Keypair> {
    if let Ok(bytes) = std::fs::read(path) {
        return identity::Keypair::from_protobuf_encoding(&bytes).map_err(std::io::Error::other);
    }
    let key = identity::Keypair::generate_ed25519();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = key.to_protobuf_encoding().map_err(std::io::Error::other)?;
    std::fs::write(path, bytes)?;
    Ok(key)
}

#[cfg(test)]
mod tests;
