//! Bitswap 1.2.0 (`/ipfs/bitswap/1.2.0`), wire-compatible with Kubo and Helia.
//!
//! Every message travels on its own outbound stream as an unsigned-varint length-prefixed
//! protobuf; a peer answers wants by opening a stream back. The engine serves any block in the
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
    Block(Cid, Arc<Vec<u8>>),
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
}

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
        let this = Self { control, source, arrivals, wanted: Arc::new(Mutex::new(HashSet::new())) };
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
                match (self.source.get(&cid), e.want_type) {
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
            let _ = self.arrivals.send(Arrival::Block(cid, Arc::new(b.data)));
        }
    }

    async fn send(&self, peer: PeerId, msg: &pb::Message) {
        let mut control = self.control.clone();
        match control.open_stream(peer, PROTOCOL).await {
            Ok(mut s) => {
                if let Err(e) = write_msg(&mut s, msg).await {
                    tracing::debug!(%peer, "bitswap write: {e}");
                }
                let _ = s.close().await;
            }
            Err(e) => tracing::debug!(%peer, "bitswap open stream: {e}"),
        }
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
                Ok(Ok(Arrival::Block(cid, data))) => {
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
            Err(FetchError { missing: todo.into_iter().collect() })
        }
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
