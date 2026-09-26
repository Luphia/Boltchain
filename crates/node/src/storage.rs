//! Storage duties of a node (ADR 0009): state snapshots and their announcements, checkpoint sync
//! (start from a snapshot instead of genesis), pruning with history shards, and storage audits.
//!
//! Messages travel on the `/bolt/<chain>/storage` gossip topic: a tag byte, then the payload
//! (a dag-cbor [`SnapshotAnnounce`], or an encoded [`AuditVote`]).

use alloy_consensus::Header;
use alloy_primitives::{B256, keccak256};
use anyhow::{Context, Result, bail};
use bolt_chain::Chain;
use bolt_consensus::{AuditCert, AuditVote};
use bolt_ipld::{
    Cid, Envelope,
    history::{EpochIndex, SnapshotRoot, decode_group},
};
use bolt_net::{NetEvent, NetHandle};
use bolt_primitives::bls::{BlsPublicKey, BlsSecretKey};
use bolt_store::StateView;
use bolt_system::{abi::*, addresses::*, queries};
use libp2p::PeerId;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const TAG_SNAPSHOT: u8 = 1;
const TAG_AUDIT: u8 = 2;

/// How often a node re-announces its latest final snapshot.
pub const ANNOUNCE_EVERY: Duration = Duration::from_secs(5);

/// A node has a snapshot of block `number` (`hash`) with root `root`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotAnnounce {
    /// Block number.
    pub number: u64,
    /// Block hash.
    #[serde(with = "serde_bytes")]
    pub hash: Vec<u8>,
    /// Snapshot root.
    pub root: Cid,
    /// Sender's clock (unix ms) when announcing: makes each re-announcement a new gossip message
    /// (gossipsub drops repeats of a recent message), so nodes that join later still hear it.
    pub at: u64,
}

/// A storage-topic message.
#[derive(Debug, Clone)]
pub enum StorageMsg {
    /// Snapshot announcement.
    Snapshot(SnapshotAnnounce),
    /// Audit vote.
    Audit(AuditVote),
}

impl StorageMsg {
    /// Encodes for the gossip topic.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            StorageMsg::Snapshot(a) => {
                let mut v = vec![TAG_SNAPSHOT];
                v.extend(serde_ipld_dagcbor::to_vec(a).expect("encodes"));
                v
            }
            StorageMsg::Audit(vote) => {
                let mut v = vec![TAG_AUDIT];
                v.extend(vote.encode());
                v
            }
        }
    }

    /// Decodes a gossip message.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (tag, rest) = b.split_first()?;
        match *tag {
            TAG_SNAPSHOT => serde_ipld_dagcbor::from_slice(rest).ok().map(StorageMsg::Snapshot),
            TAG_AUDIT => AuditVote::decode(rest).map(StorageMsg::Audit),
            _ => None,
        }
    }
}

/// Splits the network events: storage messages go to the second receiver, everything else to
/// the first (for the validator or follower loop).
pub fn split(
    mut events: mpsc::Receiver<NetEvent>,
) -> (mpsc::Receiver<NetEvent>, mpsc::Receiver<(PeerId, Vec<u8>)>) {
    let (tx, rx) = mpsc::channel(1024);
    let (stx, srx) = mpsc::channel(1024);
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                NetEvent::Storage { via, data } => {
                    let _ = stx.try_send((via, data));
                }
                other => {
                    if tx.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    (rx, srx)
}

/// What the storage service does.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Take and announce a snapshot at every epoch end.
    pub snapshots: bool,
    /// Prune block bodies older than the recent window, except epochs assigned to this node's
    /// validators (which it fetches if missing). Off: archive node.
    pub prune: bool,
    /// Validator keys (audit panel duty, shard assignments).
    pub keys: Vec<BlsSecretKey>,
    /// How long an auditor waits for a provider's block.
    pub audit_timeout: Duration,
    /// Accounts whose storage-provider ids (without stake, ADR 0012) this node serves: it keeps
    /// the epochs assigned to them instead of pruning them.
    pub storage_accounts: Vec<alloy_primitives::Address>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            snapshots: true,
            prune: true,
            keys: vec![],
            audit_timeout: Duration::from_secs(10),
            storage_accounts: vec![],
        }
    }
}

#[derive(Default)]
struct State {
    /// Snapshot announcements seen (newest last).
    seen: Vec<(PeerId, SnapshotAnnounce)>,
    /// Audit votes by (epoch, task, verdict) and panel position.
    votes: HashMap<(u64, u16, bool), BTreeMap<u16, AuditVote>>,
    /// Tasks certified here.
    certified: HashSet<(u64, u16)>,
    /// (epoch, task, member) this node has voted on.
    audited: HashSet<(u64, u16, u16)>,
    /// Epochs pruned since start.
    pruned: HashSet<u64>,
    /// Panel keys by epoch.
    panels: HashMap<u64, Vec<BlsPublicKey>>,
}

/// The storage service.
pub struct Storage {
    chain: Arc<Chain>,
    net: NetHandle,
    cfg: StorageConfig,
    state: Mutex<State>,
    /// Background jobs running: snapshot, audit, prune.
    busy: [AtomicBool; 3],
}

const SNAPSHOT_JOB: usize = 0;
const AUDIT_JOB: usize = 1;
const PRUNE_JOB: usize = 2;

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage").field("cfg", &self.cfg.snapshots).finish()
    }
}

fn view<C: alloy_sol_types::SolCall>(
    chain: &Chain,
    to: alloy_primitives::Address,
    c: C,
) -> Result<C::Return> {
    let r = chain.store().reader()?;
    Ok(queries::call(&StateView::latest(&r), chain.config().chain_id, to, c)?)
}

impl Storage {
    /// Creates the service.
    pub fn new(chain: Arc<Chain>, net: NetHandle, cfg: StorageConfig) -> Arc<Self> {
        Arc::new(Self {
            chain,
            net,
            cfg,
            state: Mutex::new(State::default()),
            busy: Default::default(),
        })
    }

    /// Snapshot announcements seen so far (newest last).
    pub fn seen_snapshots(&self) -> Vec<(PeerId, SnapshotAnnounce)> {
        self.state.lock().seen.clone()
    }

    /// Runs until the message stream ends.
    pub async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<(PeerId, Vec<u8>)>) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut last_announce = Instant::now() - ANNOUNCE_EVERY;
        let mut last_prune = Instant::now();
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    None => break,
                    Some((via, data)) => self.on_message(via, &data),
                },
                _ = tick.tick() => {
                    if self.cfg.snapshots {
                        self.spawn_guarded(SNAPSHOT_JOB, |s| async move {
                            let chain = s.chain.clone();
                            match tokio::task::spawn_blocking(move || chain.take_snapshot()).await {
                                Ok(Err(e)) => tracing::warn!("snapshot: {e}"),
                                Err(e) => tracing::warn!("snapshot task: {e}"),
                                Ok(Ok(_)) => {}
                            }
                        });
                        if last_announce.elapsed() >= ANNOUNCE_EVERY {
                            last_announce = Instant::now();
                            self.announce().await;
                        }
                    }
                    if !self.cfg.keys.is_empty() {
                        self.spawn_guarded(AUDIT_JOB, |s| async move {
                            if let Err(e) = s.audit_round().await {
                                tracing::debug!("audit round: {e:#}");
                            }
                        });
                    }
                    if self.cfg.prune && last_prune.elapsed() >= Duration::from_secs(2) {
                        last_prune = Instant::now();
                        self.spawn_guarded(PRUNE_JOB, |s| async move {
                            if let Err(e) = s.prune_round().await {
                                tracing::debug!("prune round: {e:#}");
                            }
                        });
                    }
                }
            }
        }
    }

    /// Runs job `job` in the background unless it is still running.
    fn spawn_guarded<F, Fut>(self: &Arc<Self>, job: usize, f: F)
    where
        F: FnOnce(Arc<Self>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if self.busy[job].swap(true, Ordering::AcqRel) {
            return;
        }
        let fut = f(self.clone());
        let this = self.clone();
        tokio::spawn(async move {
            fut.await;
            this.busy[job].store(false, Ordering::Release);
        });
    }

    /// Publishes every kept snapshot whose block is final.
    async fn announce(&self) {
        let Ok(list) = self.chain.snapshots() else { return };
        for (number, root) in list.into_iter().rev() {
            if !self.chain.is_final(number).unwrap_or(false) {
                continue;
            }
            let Ok(Some(hash)) = self.chain.store().reader().and_then(|r| r.block_hash(number))
            else {
                continue;
            };
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let a = SnapshotAnnounce { number, hash: hash.to_vec(), root, at };
            let _ = self.net.publish_storage(StorageMsg::Snapshot(a).encode()).await;
        }
    }

    fn on_message(&self, via: PeerId, data: &[u8]) {
        match StorageMsg::decode(data) {
            Some(StorageMsg::Snapshot(a)) => {
                let mut st = self.state.lock();
                if !st.seen.iter().any(|(_, s)| s.root == a.root) {
                    st.seen.push((via, a));
                    if st.seen.len() > 32 {
                        st.seen.remove(0);
                    }
                }
            }
            Some(StorageMsg::Audit(v)) => {
                if let Err(e) = self.add_vote(v) {
                    tracing::debug!("audit vote: {e:#}");
                }
            }
            None => tracing::debug!(%via, "undecodable storage message"),
        }
    }

    fn panel_keys(&self, epoch: u64) -> Result<Vec<BlsPublicKey>> {
        if let Some(k) = self.state.lock().panels.get(&epoch) {
            return Ok(k.clone());
        }
        let a = view(&self.chain, HISTORY, IHistoryRegistry::auditsCall { epoch })?;
        if a.panel.is_empty() {
            return Ok(vec![]);
        }
        let keys = view(&self.chain, STAKING, IStakingManager::keysOfCall { ids: a.panel })?;
        let keys: Vec<BlsPublicKey> =
            keys.pubkeys.chunks_exact(48).map(BlsPublicKey::from_slice).collect();
        let mut st = self.state.lock();
        st.panels.insert(epoch, keys.clone());
        st.panels.retain(|e, _| *e + 4 > epoch);
        Ok(keys)
    }

    /// Checks a vote, counts it, and queues the certificate once more than 2/3 of the panel
    /// agree.
    fn add_vote(&self, vote: AuditVote) -> Result<()> {
        let chain_id = self.chain.config().chain_id;
        let panel = self.panel_keys(vote.epoch)?;
        if panel.is_empty() || !vote.verify(chain_id, &panel) {
            bail!("vote does not verify against the panel of epoch {}", vote.epoch);
        }
        let key = (vote.epoch, vote.task, vote.passed);
        let mut st = self.state.lock();
        if st.certified.contains(&(vote.epoch, vote.task)) {
            return Ok(());
        }
        let votes = st.votes.entry(key).or_default();
        votes.insert(vote.member, vote.clone());
        if votes.len() * 3 > panel.len() * 2 {
            let all: Vec<AuditVote> = votes.values().cloned().collect();
            if let Some(cert) = AuditCert::aggregate(&all, panel.len()) {
                tracing::info!(
                    epoch = vote.epoch,
                    task = vote.task,
                    passed = vote.passed,
                    votes = all.len(),
                    "storage audit certified"
                );
                st.certified.insert((vote.epoch, vote.task));
                bolt_primitives::metrics::AUDITS_CERTIFIED.inc();
                let epoch = vote.epoch;
                st.votes.retain(|(e, _, _), _| *e + 2 > epoch);
                drop(st);
                self.chain.add_audit_cert(cert.encode());
            }
        }
        Ok(())
    }

    /// Provider ids this node serves: its validator keys' ids (mapped to the key index) and the
    /// active storage-provider ids of `storage_accounts` (mapped to `usize::MAX`: no key, they
    /// never sit on a panel).
    fn my_ids(&self) -> Result<HashMap<u32, usize>> {
        let mut out = HashMap::new();
        for (i, k) in self.cfg.keys.iter().enumerate() {
            let h = keccak256(k.public_key().as_slice());
            let id = view(&self.chain, STAKING, IStakingManager::idOfPubkeyCall { h })?;
            if id != 0 {
                out.insert(id, i);
            }
        }
        for account in &self.cfg.storage_accounts {
            let ids = view(
                &self.chain,
                HISTORY,
                IHistoryRegistry::storageIdsOfCall { account: *account },
            )?;
            for id in ids {
                let p = view(&self.chain, HISTORY, IHistoryRegistry::storageProviderCall { id })?;
                if p.active {
                    out.insert(id, usize::MAX);
                }
            }
        }
        Ok(out)
    }

    /// Serves the panel duty for the current epoch: probe every pending task's provider once and
    /// vote.
    async fn audit_round(&self) -> Result<()> {
        let head = self.chain.head()?.number;
        let epoch = self.chain.rules().epoch_of(head + 1);
        let a = view(&self.chain, HISTORY, IHistoryRegistry::auditsCall { epoch })?;
        if a.panel.is_empty() {
            return Ok(());
        }
        let mine = self.my_ids()?;
        let members: Vec<(u16, usize)> = a
            .panel
            .iter()
            .enumerate()
            .filter_map(|(m, id)| {
                mine.get(id).filter(|k| **k != usize::MAX).map(|k| (m as u16, *k))
            })
            .collect();
        if members.is_empty() {
            return Ok(());
        }
        let chain_id = self.chain.config().chain_id;
        for (task, state) in a.states.iter().enumerate() {
            let task16 = task as u16;
            if *state != 0 || self.state.lock().certified.contains(&(epoch, task16)) {
                continue;
            }
            let todo: Vec<(u16, usize)> = {
                let st = self.state.lock();
                members
                    .iter()
                    .copied()
                    .filter(|(m, _)| !st.audited.contains(&(epoch, task16, *m)))
                    .collect()
            };
            if todo.is_empty() {
                continue;
            }
            let passed = self.check_provider(a.providers[task], a.heights[task], &mine).await;
            tracing::debug!(epoch, task, provider = a.providers[task], passed, "audited");
            for (m, k) in todo {
                let vote = AuditVote::sign(&self.cfg.keys[k], chain_id, epoch, task16, passed, m);
                self.state.lock().audited.insert((epoch, task16, m));
                bolt_primitives::metrics::AUDIT_VOTES.inc();
                let _ = self.net.publish_storage(StorageMsg::Audit(vote.clone()).encode()).await;
                self.add_vote(vote)?;
            }
        }
        Ok(())
    }

    /// Whether `provider` serves data of block `height`: the first body chunk (or the envelope
    /// of an empty block), asked from its registered peer only.
    async fn check_provider(&self, provider: u32, height: u64, mine: &HashMap<u32, usize>) -> bool {
        let target = {
            let Ok(r) = self.chain.store().reader() else { return false };
            let Ok(Some(root)) = r.envelope_root(height) else { return false };
            match r.ipld(&root).ok().flatten().and_then(|b| Envelope::decode(&b).ok()) {
                Some(env) => env.chunks.first().copied().unwrap_or(root),
                None => root,
            }
        };
        if mine.contains_key(&provider) {
            return self
                .chain
                .store()
                .reader()
                .ok()
                .and_then(|r| r.ipld(&target).ok().flatten())
                .is_some();
        }
        let Ok(peer) = view(&self.chain, HISTORY, IHistoryRegistry::peerOfCall { id: provider })
        else {
            return false;
        };
        let Ok(peer) = PeerId::from_bytes(&peer) else { return false };
        self.net.probe(target, peer, self.cfg.audit_timeout).await.is_ok()
    }

    /// Prunes epochs older than the recent window that no local validator keeps, and completes
    /// the ones they keep.
    async fn prune_round(&self) -> Result<()> {
        let rules = *self.chain.rules();
        let head = self.chain.head()?.number;
        let Some(last_old) = rules.epoch_of(head).checked_sub(rules.history_recent_epochs + 1)
        else {
            return Ok(());
        };
        let mut validators =
            view(&self.chain, STAKING, IStakingManager::snapshotCall { minAge: 0 })?.ids;
        validators.sort_unstable();
        let storage = view(&self.chain, HISTORY, IHistoryRegistry::activeStorageProvidersCall {})?;
        let mine = self.my_ids()?;
        for e in 0..=last_old {
            let idx = view(&self.chain, HISTORY, IHistoryRegistry::epochIndexCall { epoch: e })?;
            if idx.is_empty() {
                continue;
            }
            let who = bolt_system::history::assignees(&idx, &validators, &storage);
            if who.iter().any(|id| mine.contains_key(id)) {
                let cid = Cid::try_from(idx.as_ref()).context("epoch index cid")?;
                self.ensure_epoch(cid).await?;
            } else if !self.state.lock().pruned.contains(&e) {
                let chain = self.chain.clone();
                let n = tokio::task::spawn_blocking(move || chain.prune_epoch(e)).await??;
                if n > 0 {
                    bolt_primitives::metrics::BLOCKS_PRUNED.add(n);
                    tracing::info!(epoch = e, blocks = n, "pruned history");
                }
                self.state.lock().pruned.insert(e);
            }
        }
        Ok(())
    }

    /// Makes sure every block of an epoch's history is in the local blockstore, fetching what is
    /// missing from peers.
    async fn ensure_epoch(&self, index: Cid) -> Result<()> {
        let idx = EpochIndex::decode(&self.get_or_fetch(&[index]).await?[&index])?;
        let groups = self.get_or_fetch(&idx.groups).await?;
        let mut envelopes = Vec::new();
        for g in &idx.groups {
            envelopes.extend(decode_group(&groups[g])?);
        }
        let envs = self.get_or_fetch(&envelopes).await?;
        let mut parts = Vec::new();
        for c in &envelopes {
            let e = Envelope::decode(&envs[c])?;
            parts.push(e.header);
            parts.extend(e.chunks);
        }
        let got = self.get_or_fetch(&parts).await?;
        if got.len() < parts.iter().collect::<HashSet<_>>().len() {
            bail!("epoch {} incomplete", idx.epoch);
        }
        Ok(())
    }

    /// Reads blocks locally, fetching (and storing) the missing ones.
    async fn get_or_fetch(&self, cids: &[Cid]) -> Result<HashMap<Cid, Vec<u8>>> {
        get_or_fetch(&self.chain, &self.net, cids, &[]).await
    }
}

/// Reads blocks from the local blockstore, fetching the missing ones from `peers` (then any
/// connected peer) in batches and storing them.
pub async fn get_or_fetch(
    chain: &Chain,
    net: &NetHandle,
    cids: &[Cid],
    peers: &[PeerId],
) -> Result<HashMap<Cid, Vec<u8>>> {
    let mut out = HashMap::new();
    let mut missing = Vec::new();
    {
        let r = chain.store().reader()?;
        for c in cids {
            match r.ipld(c)? {
                Some(b) => {
                    out.insert(*c, b);
                }
                None => missing.push(*c),
            }
        }
    }
    if missing.is_empty() {
        return Ok(out);
    }
    let mut sources = peers.to_vec();
    sources.extend(net.peers().await.into_iter().filter(|p| !peers.contains(p)));
    for batch in missing.chunks(64) {
        let mut got = HashMap::new();
        for attempt in 0..4 {
            let want: Vec<Cid> = batch.iter().filter(|c| !got.contains_key(*c)).copied().collect();
            match net.fetch(&want, &sources).await {
                Ok(m) => {
                    got.extend(m);
                    break;
                }
                Err(e) if attempt == 3 => return Err(e.into()),
                Err(_) => {}
            }
        }
        let w = chain.store().writer()?;
        for (c, b) in &got {
            w.put_ipld(c, b)?;
        }
        w.commit()?;
        out.extend(got);
    }
    Ok(out)
}

/// Starts a fresh node from the snapshot of trusted block `number` (`hash`): the snapshot root
/// is `root` if given, else taken from the first matching announcement on `storage`. Fetches the
/// state, the block and its recent ancestors over Bitswap, rebuilds and checks the state, and
/// makes the block the head. Blocks after it then sync as usual.
pub async fn checkpoint_sync(
    chain: &Arc<Chain>,
    net: &NetHandle,
    number: u64,
    hash: B256,
    root: Option<Cid>,
    storage: &mut mpsc::Receiver<(PeerId, Vec<u8>)>,
) -> Result<Header> {
    let head = chain.head()?;
    if head.number != 0 {
        let known = chain.store().reader()?.block_hash(number)?;
        if known == Some(hash) || head.number > number {
            return Ok(head);
        }
        bail!("datadir already holds another chain (head {})", head.number);
    }
    let started = Instant::now();
    let (root, via) = match root {
        Some(r) => (r, None),
        None => {
            tracing::info!(number, %hash, "waiting for a snapshot announcement of the checkpoint");
            loop {
                let (via, data) = storage.recv().await.context("network stopped")?;
                if let Some(StorageMsg::Snapshot(a)) = StorageMsg::decode(&data)
                    && a.number == number
                    && a.hash.as_slice() == hash.as_slice()
                {
                    break (a.root, Some(via));
                }
            }
        }
    };
    let peers: Vec<PeerId> = via.into_iter().collect();
    let snap = SnapshotRoot::decode(&get_or_fetch(chain, net, &[root], &peers).await?[&root])?;
    if snap.number != number || snap.hash.as_slice() != hash.as_slice() {
        bail!("snapshot {root} is of block {}, not the checkpoint", snap.number);
    }
    let parts = Chain::snapshot_parts(&snap);
    tracing::info!(number, chunks = parts.len(), "fetching state snapshot");
    let got = get_or_fetch(chain, net, &parts, &peers).await?;
    let env = Envelope::decode(&got[&snap.envelope])?;
    let mut block_parts = vec![env.header];
    block_parts.extend(env.chunks.iter().copied());
    get_or_fetch(chain, net, &block_parts, &peers).await?;
    // Ancestors back to the backfill limit (and the envelope just below it).
    let lower = chain.backfill_from(number);
    let mut parent = env.parent;
    while let Some(p) = parent {
        let e = Envelope::decode(&get_or_fetch(chain, net, &[p], &peers).await?[&p])?;
        if e.height < lower || e.height == 0 {
            break;
        }
        get_or_fetch(chain, net, &[e.header], &peers).await?;
        parent = e.parent;
    }
    let c = chain.clone();
    let header = tokio::task::spawn_blocking(move || c.import_checkpoint(&root, &hash)).await??;
    tracing::info!(
        number,
        secs = started.elapsed().as_secs_f32(),
        "checkpoint sync complete; following the chain from here"
    );
    Ok(header)
}

/// Parses `--checkpoint <number>:<hash>`.
pub fn parse_checkpoint(s: &str) -> Result<(u64, B256)> {
    let (n, h) = s.split_once(':').context("expected <number>:<hash>")?;
    Ok((n.parse()?, h.parse()?))
}
