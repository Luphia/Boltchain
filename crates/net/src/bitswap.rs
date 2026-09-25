//! Bitswap 1.2.0 (`/ipfs/bitswap/1.2.0`), wire-compatible with Kubo and Helia.
//!
//! Messages are unsigned-varint length-prefixed protobufs on outbound streams; a peer answers
//! wants on its own outbound stream back. Like Kubo, the engine keeps one long-lived outbound
//! stream per peer and writes every message to it (opening a stream per message exhausted
//! peers' stream limits over high-latency links on the public testnet). The engine serves any block in the
//! local blockstore (which only ever holds blocks of validated chain data) and fetches blocks by
//! sending `want-block` entries, first to the announcer and then to other connected peers.

use bolt_ipld::{Cid, verify};
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{PeerId, StreamProtocol};
use parking_lot::Mutex;
use prost::Message as _;
use sha2::Digest as _;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::broadcast;

/// Protocol id.
pub const PROTOCOL: StreamProtocol = StreamProtocol::new("/ipfs/bitswap/1.2.0");

/// Largest message accepted (bitswap convention: 4 MiB).
pub const MAX_MESSAGE: usize = 4 << 20;

/// Protobuf schema of bitswap messages (`message.proto` from the bitswap spec).
pub mod pb {
    /// Top-level message.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Message {
        /// Wants.
        #[prost(message, optional, tag = "1")]
        pub wantlist: Option<Wantlist>,
        /// Bitswap 1.0.0 blocks (unused here, kept for decoding).
        #[prost(bytes = "vec", repeated, tag = "2")]
        pub blocks: Vec<Vec<u8>>,
        /// Blocks with CID prefix (1.1.0+).
        #[prost(message, repeated, tag = "3")]
        pub payload: Vec<Block>,
        /// HAVE / DONT_HAVE answers (1.2.0).
        #[prost(message, repeated, tag = "4")]
        pub block_presences: Vec<BlockPresence>,
        /// Bytes queued for the peer.
        #[prost(int32, tag = "5")]
        pub pending_bytes: i32,
    }

    /// A wantlist.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Wantlist {
        /// Entries.
        #[prost(message, repeated, tag = "1")]
        pub entries: Vec<Entry>,
        /// Whether this replaces the whole list.
        #[prost(bool, tag = "2")]
        pub full: bool,
    }

    /// A want entry.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Entry {
        /// CID bytes.
        #[prost(bytes = "vec", tag = "1")]
        pub block: Vec<u8>,
        /// Priority.
        #[prost(int32, tag = "2")]
        pub priority: i32,
        /// Cancel a previous want.
        #[prost(bool, tag = "3")]
        pub cancel: bool,
        /// 0 = Block, 1 = Have.
        #[prost(int32, tag = "4")]
        pub want_type: i32,
        /// Ask for DONT_HAVE if missing.
        #[prost(bool, tag = "5")]
        pub send_dont_have: bool,
    }

    /// A block with its CID prefix.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Block {
        /// CID prefix: version, codec, multihash code, digest length (varints).
        #[prost(bytes = "vec", tag = "1")]
        pub prefix: Vec<u8>,
        /// Block data.
        #[prost(bytes = "vec", tag = "2")]
        pub data: Vec<u8>,
    }

    /// HAVE / DONT_HAVE.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct BlockPresence {
        /// CID bytes.
        #[prost(bytes = "vec", tag = "1")]
        pub cid: Vec<u8>,
        /// 0 = Have, 1 = DontHave.
        #[prost(int32, tag = "2")]
        pub r#type: i32,
    }
}

const WANT_BLOCK: i32 = 0;
const WANT_HAVE: i32 = 1;
const HAVE: i32 = 0;
const DONT_HAVE: i32 = 1;

/// Where served blocks come from.
pub trait BlockSource: Send + Sync + 'static {
    /// Returns the block if held locally.
    fn get(&self, cid: &Cid) -> Option<Vec<u8>>;
}

/// Encodes a CID prefix (everything but the digest).
pub fn cid_prefix(cid: &Cid) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    let mut b = unsigned_varint::encode::u64_buffer();
    for n in
        [u64::from(cid.version()), cid.codec(), cid.hash().code(), u64::from(cid.hash().size())]
    {
        out.extend_from_slice(unsigned_varint::encode::u64(n, &mut b));
    }
    out
}

/// Rebuilds a CID from a prefix and the block data, hashing with the prefix's function.
fn cid_from_prefix(prefix: &[u8], data: &[u8]) -> Option<Cid> {
    let mut rest = prefix;
    let mut next = || {
        let (n, r) = unsigned_varint::decode::u64(rest).ok()?;
        rest = r;
        Some(n)
    };
    let (version, codec, code, _len) = (next()?, next()?, next()?, next()?);
    if version != 1 {
        return None;
    }
    let digest: Vec<u8> = match code {
        bolt_ipld::KECCAK_256 => alloy_primitives::keccak256(data).to_vec(),
        bolt_ipld::SHA2_256 => sha2::Sha256::digest(data).to_vec(),
        _ => return None,
    };
    let mh = multihash::Multihash::<64>::wrap(code, &digest).ok()?;
    Some(Cid::new_v1(codec, mh))
}

async fn write_msg(stream: &mut libp2p::Stream, msg: &pb::Message) -> std::io::Result<()> {
    let body = msg.encode_to_vec();
    let mut len = unsigned_varint::encode::usize_buffer();
    stream.write_all(unsigned_varint::encode::usize(body.len(), &mut len)).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

async fn read_msg(stream: &mut libp2p::Stream) -> std::io::Result<Option<pb::Message>> {
    let len = match unsigned_varint::aio::read_usize(&mut *stream).await {
        Ok(n) => n,
        Err(unsigned_varint::io::ReadError::Io(e))
            if e.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            return Ok(None);
        }
        Err(e) => return Err(std::io::Error::other(e.to_string())),
    };
    if len > MAX_MESSAGE {
        return Err(std::io::Error::other("bitswap message too large"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    pb::Message::decode(buf.as_slice()).map(Some).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Fetch failure.
#[derive(Debug, thiserror::Error)]
#[error("timed out fetching {} block(s)", missing.len())]
pub struct FetchError {
    /// CIDs that did not arrive.
    pub missing: Vec<Cid>,
}

/// Something a waiting fetch may care about.
#[derive(Debug, Clone)]
enum Arrival {
    Block(PeerId, Cid, Arc<Vec<u8>>),
    DontHave(PeerId, Cid),
}

/// The bitswap engine.
#[derive(Clone)]
pub struct Bitswap {
    control: libp2p_stream::Control,
    source: Arc<dyn BlockSource>,
    /// Blocks received and verified, and DONT_HAVE answers, announced to waiting fetches.
    arrivals: broadcast::Sender<Arrival>,
    /// CIDs some local fetch is waiting for (unsolicited blocks are dropped).
    wanted: Arc<Mutex<HashSet<Cid>>>,
    /// One outbound stream per peer, reused for every message.
    outbound: Arc<Mutex<HashMap<PeerId, OutboundSlot>>>,
}

/// A peer's reusable outbound stream (`None` until opened, or after it failed).
type OutboundSlot = Arc<tokio::sync::Mutex<Option<libp2p::Stream>>>;

/// Outbound streams kept at most (the least recently opened are dropped beyond this).
const MAX_OUTBOUND_STREAMS: usize = 512;

impl std::fmt::Debug for Bitswap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bitswap").field("wanted", &self.wanted.lock().len()).finish()
    }
}

impl Bitswap {
    /// Creates the engine and starts serving inbound streams.
    pub fn new(mut control: libp2p_stream::Control, source: Arc<dyn BlockSource>) -> Self {
        let incoming = control.accept(PROTOCOL).expect("bitswap protocol registered once");
        let (arrivals, _) = broadcast::channel(1024);
        let this = Self {
            control,
            source,
            arrivals,
            wanted: Arc::new(Mutex::new(HashSet::new())),
            outbound: Arc::new(Mutex::new(HashMap::new())),
        };
        let server = this.clone();
        tokio::spawn(async move { server.serve(incoming).await });
        this
    }

    async fn serve(self, mut incoming: libp2p_stream::IncomingStreams) {
        while let Some((peer, mut stream)) = incoming.next().await {
            tracing::trace!(%peer, "bitswap inbound stream");
            let this = self.clone();
            tokio::spawn(async move {
                loop {
                    match read_msg(&mut stream).await {
                        Ok(Some(msg)) => this.handle(peer, msg).await,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!(%peer, "bitswap read: {e}");
                            break;
                        }
                    }
                }
                // Close our side too: a peer that opened this stream for one message (Helia,
                // Kubo) waits for it before its stream counts as closed, and stops sending once
                // too many are still open.
                let _ = stream.close().await;
            });
        }
    }

    async fn handle(&self, peer: PeerId, msg: pb::Message) {
        tracing::trace!(
            %peer,
            wants = msg.wantlist.as_ref().map(|w| w.entries.len()).unwrap_or(0),
            blocks = msg.payload.len(),
            presences = msg.block_presences.len(),
            "bitswap message"
        );
        // Answer wants.
        if let Some(wl) = msg.wantlist {
            let mut reply = pb::Message::default();
            for e in wl.entries.iter().filter(|e| !e.cancel) {
                let Ok(cid) = Cid::try_from(e.block.as_slice()) else { continue };
                let found = self.source.get(&cid);
                tracing::trace!(%peer, %cid, have = found.is_some(), want_type = e.want_type, "bitswap want");
                match (found, e.want_type) {
                    (Some(data), WANT_BLOCK) => {
                        reply.payload.push(pb::Block { prefix: cid_prefix(&cid), data })
                    }
                    (Some(_), WANT_HAVE) => reply
                        .block_presences
                        .push(pb::BlockPresence { cid: e.block.clone(), r#type: HAVE }),
                    (None, _) if e.send_dont_have => reply
                        .block_presences
                        .push(pb::BlockPresence { cid: e.block.clone(), r#type: DONT_HAVE }),
                    _ => {}
                }
            }
            bolt_primitives::metrics::BITSWAP_SERVED.add(reply.payload.len() as u64);
            tracing::trace!(
                %peer,
                blocks = reply.payload.len(),
                presences = reply.block_presences.len(),
                "bitswap reply"
            );
            if !reply.payload.is_empty() || !reply.block_presences.is_empty() {
                // Split so no single message exceeds the limit.
                for part in split(reply) {
                    self.send(peer, &part).await;
                }
            }
        }
        // A peer that does not have a block yet: let the fetch move on to other peers now.
        for p in msg.block_presences {
            if p.r#type == DONT_HAVE
                && let Ok(cid) = Cid::try_from(p.cid.as_slice())
                && self.wanted.lock().contains(&cid)
            {
                let _ = self.arrivals.send(Arrival::DontHave(peer, cid));
            }
        }
        // Accept blocks we asked for.
        for b in msg.payload {
            let Some(cid) = cid_from_prefix(&b.prefix, &b.data) else { continue };
            if !self.wanted.lock().contains(&cid) || verify(&cid, &b.data).is_err() {
                continue;
            }
            let _ = self.arrivals.send(Arrival::Block(peer, cid, Arc::new(b.data)));
        }
    }

    async fn send(&self, peer: PeerId, msg: &pb::Message) {
        let slot = {
            let mut out = self.outbound.lock();
            if out.len() >= MAX_OUTBOUND_STREAMS && !out.contains_key(&peer) {
                // Drop idle streams (nobody holds their lock) to make room.
                out.retain(|_, s| Arc::strong_count(s) > 1);
            }
            out.entry(peer).or_default().clone()
        };
        let mut stream = slot.lock().await;
        // Write on the existing stream; on failure (peer closed it, connection replaced) open a
        // fresh one and retry once.
        for attempt in 0..2 {
            if stream.is_none() {
                let mut control = self.control.clone();
                match control.open_stream(peer, PROTOCOL).await {
                    Ok(s) => *stream = Some(s),
                    Err(e) => {
                        tracing::debug!(%peer, "bitswap open stream: {e}");
                        break;
                    }
                }
            }
            let s = stream.as_mut().expect("just opened");
            match write_msg(s, msg).await {
                Ok(()) => {
                    tracing::trace!(%peer, attempt, "bitswap sent");
                    return;
                }
                Err(e) => {
                    tracing::debug!(%peer, attempt, "bitswap write: {e}");
                    *stream = None;
                }
            }
        }
        drop(stream);
        self.outbound.lock().remove(&peer);
    }

    fn want_message(cids: &[Cid]) -> pb::Message {
        pb::Message {
            wantlist: Some(pb::Wantlist {
                entries: cids
                    .iter()
                    .map(|c| pb::Entry {
                        block: c.to_bytes(),
                        priority: 1,
                        cancel: false,
                        want_type: WANT_BLOCK,
                        send_dont_have: true,
                    })
                    .collect(),
                full: false,
            }),
            ..Default::default()
        }
    }

    /// Fetches `cids`. `peers[0]` (usually whoever relayed the announcement) is asked first; the
    /// others are asked as soon as it answers DONT_HAVE or after `fan_out_after`. Wants are
    /// repeated every `fan_out_after` until `timeout`, since peers that are still importing the
    /// block will have it moments later.
    pub async fn fetch(
        &self,
        cids: &[Cid],
        peers: &[PeerId],
        fan_out_after: Duration,
        timeout: Duration,
    ) -> Result<HashMap<Cid, Vec<u8>>, FetchError> {
        let mut got: HashMap<Cid, Vec<u8>> = HashMap::new();
        let mut todo: HashSet<Cid> = HashSet::new();
        for c in cids {
            match self.source.get(c) {
                Some(d) => {
                    got.insert(*c, d);
                }
                None => {
                    todo.insert(*c);
                }
            }
        }
        if todo.is_empty() {
            return Ok(got);
        }
        let mut rx = self.arrivals.subscribe();
        self.wanted.lock().extend(todo.iter().copied());
        const MAX_PEERS: usize = 5;
        let ask = |this: &Self, todo: &HashSet<Cid>, targets: Vec<PeerId>| {
            let msg = Self::want_message(&todo.iter().copied().collect::<Vec<_>>());
            let this = this.clone();
            tokio::spawn(async move {
                for p in targets {
                    this.send(p, &msg).await;
                }
            });
        };
        if let Some(first) = peers.first() {
            ask(self, &todo, vec![*first]);
        }
        let mut fanned = peers.len() <= 1;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut next_round = tokio::time::Instant::now() + fan_out_after;
        while !todo.is_empty() {
            match tokio::time::timeout_at(next_round.min(deadline), rx.recv()).await {
                Ok(Ok(Arrival::Block(_, cid, data))) => {
                    if todo.remove(&cid) {
                        got.insert(cid, data.as_ref().clone());
                    }
                }
                Ok(Ok(Arrival::DontHave(peer, cid))) => {
                    if !fanned && todo.contains(&cid) && Some(&peer) == peers.first() {
                        fanned = true;
                        ask(
                            self,
                            &todo,
                            peers.iter().skip(1).take(MAX_PEERS - 1).copied().collect(),
                        );
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    // Periodic round: ask everyone again.
                    fanned = true;
                    ask(self, &todo, peers.iter().take(MAX_PEERS).copied().collect());
                    next_round = tokio::time::Instant::now() + fan_out_after;
                }
                Err(_) => break,
            }
        }
        {
            let mut w = self.wanted.lock();
            for c in cids {
                w.remove(c);
            }
        }
        if todo.is_empty() {
            Ok(got)
        } else {
            bolt_primitives::metrics::BITSWAP_FETCH_FAILURES.inc();
            Err(FetchError { missing: todo.into_iter().collect() })
        }
    }
}

impl Bitswap {
    /// Asks `peer` alone for `cid`, ignoring the local blockstore (storage audits: did this
    /// provider serve the data?). Returns the verified block, or an error on DONT_HAVE or
    /// timeout.
    pub async fn probe(
        &self,
        cid: Cid,
        peer: PeerId,
        timeout: Duration,
    ) -> Result<Vec<u8>, FetchError> {
        let mut rx = self.arrivals.subscribe();
        self.wanted.lock().insert(cid);
        let msg = Self::want_message(&[cid]);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut next = tokio::time::Instant::now();
        let mut result = None;
        loop {
            if tokio::time::Instant::now() >= next {
                let this = self.clone();
                let msg = msg.clone();
                tokio::spawn(async move { this.send(peer, &msg).await });
                next = tokio::time::Instant::now() + Duration::from_secs(2);
            }
            match tokio::time::timeout_at(next.min(deadline), rx.recv()).await {
                Ok(Ok(Arrival::Block(p, c, data))) if p == peer && c == cid => {
                    result = Some(data.as_ref().clone());
                    break;
                }
                Ok(Ok(Arrival::DontHave(p, c))) if p == peer && c == cid => break,
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {}
                Err(_) => break,
            }
        }
        self.wanted.lock().remove(&cid);
        result.ok_or(FetchError { missing: vec![cid] })
    }
}

/// Splits a reply so each part stays under [`MAX_MESSAGE`].
fn split(msg: pb::Message) -> Vec<pb::Message> {
    let mut parts = Vec::new();
    let mut cur = pb::Message { block_presences: msg.block_presences, ..Default::default() };
    let mut size = 0usize;
    for b in msg.payload {
        let len = b.data.len() + b.prefix.len() + 16;
        if size + len > MAX_MESSAGE - 1024 && !cur.payload.is_empty() {
            parts.push(std::mem::take(&mut cur));
            size = 0;
        }
        size += len;
        cur.payload.push(b);
    }
    parts.push(cur);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_roundtrip_for_boltchain_codecs() {
        let data = b"hello boltchain".to_vec();
        for cid in [
            bolt_ipld::sha256_cid(bolt_ipld::RAW, &data),
            bolt_ipld::sha256_cid(bolt_ipld::DAG_CBOR, &data),
            bolt_ipld::header_cid(alloy_primitives::keccak256(&data)),
        ] {
            assert_eq!(cid_from_prefix(&cid_prefix(&cid), &data), Some(cid));
        }
    }

    #[test]
    fn message_encoding_matches_protobuf_field_numbers() {
        // wantlist{entries[{block:[1,2], wantType: Block, sendDontHave: true}]}
        let m = Bitswap::want_message(&[]);
        assert!(m.encode_to_vec().starts_with(&[0x0a])); // field 1, length-delimited
        let b = pb::Message {
            payload: vec![pb::Block { prefix: vec![1], data: vec![2] }],
            ..Default::default()
        };
        assert_eq!(b.encode_to_vec(), vec![0x1a, 0x06, 0x0a, 0x01, 0x01, 0x12, 0x01, 0x02]);
    }
}
