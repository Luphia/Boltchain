//! Block propagation over IPFS.
//!
//! * The producer stores each block's IPLD bundle in its blockstore and gossips a small
//!   [`Announce`] (envelope, header and, for bodies up to 64 KiB, the body itself).
//! * A follower verifies every part against its CID, fetches missing body chunks over Bitswap
//!   (from the relaying peer first, then from others that already have them), re-executes the
//!   block and requires the resulting header and envelope to match.
//! * A follower that is behind walks `parent` links from the announced envelope back to its own
//!   head, fetching envelopes over Bitswap, then imports the missing blocks in order.

use bolt_chain::{BuiltBlock, Chain, ChainError};
use bolt_ipld::{BlockBundle, Cid, Envelope, IpldError, decode_block, verify};
use bolt_net::{Announce, InlineBlock, NetError, NetHandle};
use bolt_primitives::params::INLINE_BODY_BYTES;
use libp2p::PeerId;
use std::{collections::HashMap, sync::Arc, time::Instant};

/// Sync errors.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// A block part failed verification.
    #[error("ipld: {0}")]
    Ipld(#[from] IpldError),
    /// Fetching failed.
    #[error("network: {0}")]
    Net(#[from] NetError),
    /// Import failed.
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// The announced chain does not connect to ours.
    #[error("fork: announced chain does not extend our head {0}")]
    Fork(u64),
    /// Background task failed.
    #[error("task: {0}")]
    Task(String),
}

/// Result alias.
pub type Result<T, E = SyncError> = std::result::Result<T, E>;

/// Builds the announcement for a block this node produced.
pub fn announce_for(bundle: &BlockBundle) -> Announce {
    let find = |c: &Cid| {
        bundle.blocks.iter().find(|(x, _)| x == c).map(|(_, d)| d.clone()).unwrap_or_default()
    };
    let inline = if bundle.body_len() <= INLINE_BODY_BYTES {
        bundle.envelope.chunks.iter().map(|c| InlineBlock { cid: *c, data: find(c) }).collect()
    } else {
        Vec::new()
    };
    Announce {
        height: bundle.envelope.height,
        root: bundle.root,
        envelope: find(&bundle.root),
        header: find(&bundle.envelope.header),
        inline,
    }
}

/// Publishes a freshly produced block.
pub async fn publish(net: &NetHandle, built: &BuiltBlock) -> Result<()> {
    Ok(net.publish(announce_for(&built.bundle)).await?)
}

/// What happened to an announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Imported `count` block(s), ending at `height`.
    Imported {
        /// New head.
        height: u64,
        /// Blocks imported (more than one after a backfill).
        count: u64,
        /// Milliseconds from receiving the announcement until all data was in hand.
        fetch_ms: u64,
    },
    /// Already had it.
    Known,
}

/// Follows announcements and keeps the local chain in sync.
#[derive(Debug, Clone)]
pub struct Follower {
    chain: Arc<Chain>,
    net: NetHandle,
}

impl Follower {
    /// Creates a follower.
    pub fn new(chain: Arc<Chain>, net: NetHandle) -> Self {
        Self { chain, net }
    }

    async fn head(&self) -> Result<u64> {
        let chain = self.chain.clone();
        tokio::task::spawn_blocking(move || chain.head().map(|h| h.number))
            .await
            .map_err(|e| SyncError::Task(e.to_string()))?
            .map_err(Into::into)
    }

    fn local(&self, cid: &Cid) -> Option<Vec<u8>> {
        self.chain.store().reader().ok()?.ipld(cid).ok().flatten()
    }

    async fn sources(&self, via: PeerId) -> Vec<PeerId> {
        let mut peers = vec![via];
        peers.extend(self.net.peers().await.into_iter().filter(|p| *p != via));
        peers
    }

    /// Handles one announcement.
    pub async fn on_announce(&self, via: PeerId, a: Announce) -> Result<Outcome> {
        let started = Instant::now();
        let head = self.head().await?;
        if a.height <= head {
            return Ok(Outcome::Known);
        }
        verify(&a.root, &a.envelope)?;
        let envelope = Envelope::decode(&a.envelope)?;
        if envelope.height != a.height {
            return Err(IpldError::Malformed("announce height").into());
        }
        let mut have: HashMap<Cid, Vec<u8>> = HashMap::new();
        have.insert(envelope.header, a.header.clone());
        for b in a.inline {
            have.insert(b.cid, b.data);
        }
        let peers = self.sources(via).await;

        // Behind by more than one block: collect the missing envelopes by walking parents.
        let mut chain_of: Vec<(Cid, Envelope)> = vec![(a.root, envelope)];
        while chain_of.last().map(|(_, e)| e.height).unwrap_or(0) > head + 1 {
            let parent =
                chain_of.last().and_then(|(_, e)| e.parent).ok_or(SyncError::Fork(head))?;
            let bytes = match self.local(&parent) {
                Some(b) => b,
                None => self
                    .net
                    .fetch(&[parent], &peers)
                    .await?
                    .remove(&parent)
                    .ok_or(SyncError::Fork(head))?,
            };
            let env = Envelope::decode(&bytes)?;
            chain_of.push((parent, env));
        }
        let ours = self
            .chain
            .store()
            .reader()
            .map_err(ChainError::from)?
            .envelope_root(head)
            .map_err(ChainError::from)?;
        if chain_of.last().and_then(|(_, e)| e.parent) != ours {
            return Err(SyncError::Fork(head));
        }

        // Fetch every missing header and chunk in one bitswap round.
        let need: Vec<Cid> = chain_of
            .iter()
            .flat_map(|(_, e)| std::iter::once(e.header).chain(e.chunks.iter().copied()))
            .filter(|c| !have.contains_key(c))
            .collect();
        if !need.is_empty() {
            have.extend(self.net.fetch(&need, &peers).await?);
        }
        let fetch_ms = started.elapsed().as_millis() as u64;

        // Import oldest first.
        let count = chain_of.len() as u64;
        let mut height = head;
        for (root, env) in chain_of.into_iter().rev() {
            let block = decode_block(env, |c| have.get(c).cloned())?;
            let chain = self.chain.clone();
            height = block.header.number;
            tokio::task::spawn_blocking(move || {
                chain.import_block_with_root(&block.header, block.transactions, Some(root))
            })
            .await
            .map_err(|e| SyncError::Task(e.to_string()))??;
        }
        Ok(Outcome::Imported { height, count, fetch_ms })
    }
}

/// Serves the chain's blockstore over Bitswap.
#[derive(Debug, Clone)]
pub struct ChainBlocks(pub Arc<Chain>);

impl bolt_net::bitswap::BlockSource for ChainBlocks {
    fn get(&self, cid: &Cid) -> Option<Vec<u8>> {
        self.0.store().reader().ok()?.ipld(cid).ok().flatten()
    }
}

/// Processes network events for a follower until the channel closes. Announcements are handled
/// one at a time so imports never race.
pub async fn run_follower(
    follower: Follower,
    mut events: tokio::sync::mpsc::Receiver<bolt_net::NetEvent>,
) {
    use bolt_net::NetEvent;
    while let Some(ev) = events.recv().await {
        match ev {
            NetEvent::Announce { via, announce } => {
                let height = announce.height;
                match follower.on_announce(via, announce).await {
                    Ok(Outcome::Imported { height, count, fetch_ms }) => {
                        tracing::info!(height, count, fetch_ms, "imported block(s) from IPFS")
                    }
                    Ok(Outcome::Known) => {}
                    Err(e) => tracing::warn!(height, "announcement not imported: {e}"),
                }
            }
            NetEvent::Tx { reply, .. } => {
                let _ = reply.send(Err("this node is not a block producer".into()));
            }
            NetEvent::Connected(peer) => tracing::debug!(%peer, "peer connected"),
        }
    }
}
