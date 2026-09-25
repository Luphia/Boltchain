//! Block propagation over IPFS.
//!
//! * The producer stores each block's IPLD bundle in its blockstore and gossips a small
//!   [`Announce`] (envelope, header and, for bodies up to 64 KiB, the body itself).
//! * A follower verifies every part against its CID, fetches missing body chunks over Bitswap
//!   (from the relaying peer first, then from others that already have them), re-executes the
//!   block and requires the resulting header and envelope to match.
//! * A follower that is behind walks `parent` links from the announced envelope back to its own
//!   head, fetching envelopes over Bitswap, then imports the missing blocks in order.
//! * On consensus networks every import is backed by a finality proof. When the follower is more
//!   than an epoch behind it cannot check the tip's proof yet (it does not know that committee),
//!   so it imports epoch by epoch: the first block of each epoch carries the proof that the
//!   previous epoch's last block is final, checkable with a committee already in local state
//!   (ADR 0006 §4). Blocks after the last boundary wait for the tip's own proof.
//! * Mined blocks (before PoS, ADR 0007) carry no proof: the follower walks back to a block it
//!   knows (possibly on another branch), checks each seal and lets the chain's fork choice decide
//!   (total difficulty, reorganisations of at most 128 blocks).

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
    /// The announcement carried no valid finality proof.
    #[error("announcement lacks a valid finality proof")]
    NotFinal,
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
        proof: Vec::new(),
    }
}

/// The announcement for committed block `number`, rebuilt from the blockstore (inline body up to
/// 64 KiB), with `proof` attached.
pub fn announce_stored(chain: &Chain, number: u64, proof: Vec<u8>) -> Option<Announce> {
    let r = chain.store().reader().ok()?;
    let root = r.envelope_root(number).ok()??;
    let envelope = r.ipld(&root).ok()??;
    let env = Envelope::decode(&envelope).ok()?;
    let header = r.ipld(&env.header).ok()??;
    let mut inline = Vec::new();
    let mut size = 0;
    for c in &env.chunks {
        let data = r.ipld(c).ok()??;
        size += data.len();
        inline.push(InlineBlock { cid: *c, data });
    }
    if size > INLINE_BODY_BYTES {
        inline.clear();
    }
    Some(Announce { height: number, root, envelope, header, inline, proof })
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

/// Result of checking a finality proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The proof is valid.
    Final,
    /// The proof is invalid.
    NotFinal,
    /// The committee needed to check it is not known locally yet (the node is behind).
    Unknown,
}

/// Checks finality proofs (consensus networks).
pub trait FinalityCheck: Send + Sync + std::fmt::Debug + 'static {
    /// Whether `proof` shows that block `hash` at `height` is final.
    fn check(&self, height: u64, hash: &alloy_primitives::B256, proof: &[u8]) -> Verdict;
    /// The finality proof of a block's parent carried in the block's envelope certificate (the
    /// first block of an epoch carries one for the previous epoch's last block).
    fn parent_proof(&self, cert: &[u8]) -> Option<Vec<u8>>;
    /// Whether `height` is the first block of an epoch.
    fn is_epoch_start(&self, height: u64) -> bool;
}

/// Follows announcements and keeps the local chain in sync.
#[derive(Debug, Clone)]
pub struct Follower {
    chain: Arc<Chain>,
    net: NetHandle,
    finality: Option<Arc<dyn FinalityCheck>>,
}

impl Follower {
    /// Creates a follower that trusts announcements from the configured producer (single-producer
    /// devnet; the network layer drops announcements from anyone else).
    pub fn new(chain: Arc<Chain>, net: NetHandle) -> Self {
        Self { chain, net, finality: None }
    }

    /// Creates a follower that only imports blocks with a valid finality proof.
    pub fn with_finality(chain: Arc<Chain>, net: NetHandle, check: Arc<dyn FinalityCheck>) -> Self {
        Self { chain, net, finality: Some(check) }
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
        verify(&a.root, &a.envelope)?;
        let envelope = Envelope::decode(&a.envelope)?;
        if envelope.height != a.height {
            return Err(IpldError::Malformed("announce height").into());
        }
        verify(&envelope.header, &a.header)?;
        let tip_hash = envelope.block_hash().ok_or(IpldError::Malformed("header cid"))?;
        let mined =
            <alloy_consensus::Header as alloy_rlp::Decodable>::decode(&mut a.header.as_slice())
                .map_err(|_| IpldError::Malformed("header"))?
                .difficulty
                > alloy_primitives::U256::ZERO;
        if mined && self.finality.is_some() {
            return self.on_mined(via, a, envelope, started).await;
        }
        let head = self.head().await?;
        if a.height <= head {
            return Ok(Outcome::Known);
        }
        let tip_verdict = match &self.finality {
            Some(check) => match check.check(a.height, &tip_hash, &a.proof) {
                Verdict::NotFinal => return Err(SyncError::NotFinal),
                v => v,
            },
            None => Verdict::Final,
        };
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
                None => {
                    let b = self
                        .net
                        .fetch(&[parent], &peers)
                        .await?
                        .remove(&parent)
                        .ok_or(SyncError::Fork(head))?;
                    // Content-addressed (verified against its CID by the fetch): keep it, so a
                    // retry after a timeout resumes the walk instead of starting over.
                    if let Ok(w) = self.chain.store().writer()
                        && w.put_ipld(&parent, &b).is_ok()
                    {
                        let _ = w.commit();
                    }
                    b
                }
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

        // Import oldest first. With the tip proven, every ancestor is final. Otherwise each block
        // is imported only once a later epoch-boundary proof (or finally the tip's) covers it.
        let count = chain_of.len() as u64;
        let ordered: Vec<(Cid, Envelope)> = chain_of.into_iter().rev().collect();
        // (height, hash, certificate) of each block, for the boundary proofs.
        let meta: Vec<(u64, Option<alloy_primitives::B256>, Vec<u8>)> =
            ordered.iter().map(|(_, e)| (e.height, e.block_hash(), e.qc.clone())).collect();
        let mut height = head;
        let mut proven_upto = if tip_verdict == Verdict::Final { a.height } else { head };
        for (i, (root, env)) in ordered.into_iter().enumerate() {
            let number = meta[i].0;
            let header_mined = have
                .get(&env.header)
                .and_then(|h| {
                    <alloy_consensus::Header as alloy_rlp::Decodable>::decode(&mut h.as_slice())
                        .ok()
                })
                .is_some_and(|h| !h.difficulty.is_zero());
            if header_mined && self.finality.is_some() {
                // Mined ancestors of a PoS block: checked by their seals.
                let block = decode_block(env, |c| have.get(c).cloned())?;
                let chain = self.chain.clone();
                height = block.header.number;
                tokio::task::spawn_blocking(move || {
                    chain.import_mined(&block.header, block.transactions, Some(root))
                })
                .await
                .map_err(|e| SyncError::Task(e.to_string()))??;
                continue;
            }
            if number > proven_upto {
                let check =
                    self.finality.as_ref().expect("unproven blocks only with a finality check");
                // Next epoch start in the batch: its envelope proves its parent final.
                let boundary = (i + 1..meta.len()).find(|j| check.is_epoch_start(meta[*j].0));
                let mut proved = false;
                if let Some(j) = boundary
                    && let (Some(proof), Some(parent_hash)) =
                        (check.parent_proof(&meta[j].2), meta[j - 1].1)
                {
                    match check.check(meta[j - 1].0, &parent_hash, &proof) {
                        Verdict::Final => {
                            proven_upto = meta[j - 1].0;
                            proved = true;
                        }
                        Verdict::NotFinal => return Err(SyncError::NotFinal),
                        Verdict::Unknown => {}
                    }
                }
                if !proved {
                    // Past the last boundary: the tip's proof must be checkable by now.
                    match check.check(a.height, &tip_hash, &a.proof) {
                        Verdict::Final => proven_upto = a.height,
                        _ => return Err(SyncError::NotFinal),
                    }
                }
            }
            let qc = env.qc.clone();
            let block = decode_block(env, |c| have.get(c).cloned())?;
            let chain = self.chain.clone();
            height = block.header.number;
            tokio::task::spawn_blocking(move || {
                chain.import_final(&block.header, block.transactions, Some(root), qc)
            })
            .await
            .map_err(|e| SyncError::Task(e.to_string()))??;
        }
        Ok(Outcome::Imported { height, count, fetch_ms })
    }
}

impl Follower {
    /// A mined block: walk back to a known block, fetch the branch, import it oldest first.
    async fn on_mined(
        &self,
        via: PeerId,
        a: Announce,
        envelope: Envelope,
        started: Instant,
    ) -> Result<Outcome> {
        let tip_hash = envelope.block_hash().ok_or(IpldError::Malformed("header cid"))?;
        let chain = self.chain.clone();
        let known = move |h: alloy_primitives::B256| chain.knows(&h).unwrap_or(false);
        if known(tip_hash) {
            return Ok(Outcome::Known);
        }
        let head = self.head().await?;
        let max_walk = a.height.saturating_sub(head) + bolt_primitives::params::MAX_REORG_DEPTH + 1;
        let mut have: HashMap<Cid, Vec<u8>> = HashMap::new();
        have.insert(envelope.header, a.header.clone());
        for b in a.inline {
            have.insert(b.cid, b.data);
        }
        let peers = self.sources(via).await;
        let mut branch: Vec<(Cid, Envelope)> = vec![(a.root, envelope)];
        loop {
            let (_, last) = branch.last().expect("non-empty");
            let parent = last.parent.ok_or(SyncError::Fork(head))?;
            let bytes = match self.local(&parent) {
                Some(b) => b,
                None => {
                    let b = self
                        .net
                        .fetch(&[parent], &peers)
                        .await?
                        .remove(&parent)
                        .ok_or(SyncError::Fork(head))?;
                    if let Ok(w) = self.chain.store().writer()
                        && w.put_ipld(&parent, &b).is_ok()
                    {
                        let _ = w.commit();
                    }
                    b
                }
            };
            let env = Envelope::decode(&bytes)?;
            let hash = env.block_hash().ok_or(IpldError::Malformed("header cid"))?;
            if known(hash) {
                break;
            }
            branch.push((parent, env));
            if branch.len() as u64 > max_walk {
                return Err(SyncError::Fork(head));
            }
        }
        let need: Vec<Cid> = branch
            .iter()
            .flat_map(|(_, e)| std::iter::once(e.header).chain(e.chunks.iter().copied()))
            .filter(|c| !have.contains_key(c))
            .collect();
        if !need.is_empty() {
            have.extend(self.net.fetch(&need, &peers).await?);
        }
        let fetch_ms = started.elapsed().as_millis() as u64;
        let count = branch.len() as u64;
        let mut height = head;
        for (root, env) in branch.into_iter().rev() {
            let block = decode_block(env, |c| have.get(c).cloned())?;
            let chain = self.chain.clone();
            height = block.header.number;
            tokio::task::spawn_blocking(move || {
                chain.import_mined(&block.header, block.transactions, Some(root))
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
            NetEvent::Consensus { .. } => {}
        }
    }
}
