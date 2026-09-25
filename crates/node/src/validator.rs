//! Validator: runs the consensus engine of the current epoch against the chain, the transaction
//! pool and the network (ADR 0005, 0006).
//!
//! Data-availability rule: a validator only votes for a proposal after it holds every part of the
//! block (fetched over Bitswap and written to its blockstore) and has executed it on top of the
//! parent and matched the header. A QC therefore also certifies that more than 2/3 of the seats
//! hold the data.
//!
//! Epochs: the committee of each epoch is read from `ConsensusRegistry` (written one epoch ahead).
//! One node may hold several validator keys; every key on the committee signs through the same
//! engine. When the epoch's last block is final the engine reports it with its proof; the node
//! stores the proof (the next epoch's first block carries it) and starts the next epoch's engine.
//!
//! Mining phase (ADR 0007): until PoS starts the node follows the mined chain (and may mine
//! itself). The first PoS epoch starts from the last mined block (the terminal block) as its
//! anchor, like epoch 0 starts from genesis: its first block carries no certificate. If a heavier
//! mined branch replaces the terminal block before the committee certified anything, the engine
//! restarts on the new anchor without ever signing again in a round it already used.

use crate::keys;
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes};
use anyhow::{Context, Result};
use bolt_chain::{Chain, EXTRA_DATA_LEN};
use bolt_consensus::{
    Action, BlockInfo, BlsScheme, Cert, CommitProof, Config, Engine, Message, Persisted, Proposal,
    Qc, Tc, ValidatorIndex, ValidatorSet, decode_cert, encode_cert, randao_msg,
};
use bolt_ipld::{Envelope, decode_block, verify};
use bolt_net::{Announce, NetEvent, NetHandle};
use bolt_primitives::bls::{self, BlsSecretKey, BlsSignature};
use bolt_rpc::HeadState;
use bolt_store::StateView;
use bolt_sync::{FinalityCheck, Follower, Verdict};
use bolt_system::queries::{self, EpochCommittee};
use bolt_txpool::TxPool;
use libp2p::PeerId;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

/// How often a node re-publishes its head while that head is a mined block.
const REANNOUNCE: Duration = Duration::from_secs(4);

/// Validator options.
#[derive(Debug, Clone)]
pub struct ValidatorConfig {
    /// Minimum time between a block and its child (the slot), in milliseconds.
    pub slot_ms: u64,
    /// Base round timeout in milliseconds.
    pub base_timeout_ms: u64,
    /// Directory for consensus safety state, epoch proofs and equivocation evidence.
    pub state_dir: PathBuf,
}

/// Block an epoch starts from: genesis for epoch 0, else the previous epoch's last block (which
/// must be final locally).
pub fn epoch_anchor(chain: &Chain, epoch: u64) -> Result<Option<BlockInfo>> {
    let height = if epoch == 0 { 0 } else { chain.rules().epoch_end(epoch - 1) };
    let r = chain.store().reader()?;
    Ok(r.header(height)?.map(|h: Header| BlockInfo {
        hash: h.hash_slow(),
        parent: h.parent_hash,
        round: 0,
        height,
    }))
}

/// Whether `qc` is the anchor certificate of a chain start: genesis (epoch 0), or the terminal
/// mined block for the first PoS epoch. The block after it carries no certificate.
pub fn is_start_anchor(chain: &Chain, qc: &Qc<BlsScheme>) -> bool {
    if qc.is_genesis() {
        return true;
    }
    if !qc.is_anchor() {
        return false;
    }
    let Ok(phase) = chain.phase() else { return false };
    phase.pos_epoch == Some(qc.epoch) && phase.terminal_height(chain.rules()) == Some(qc.height)
}

/// Whether PoS runs from the block after the local head.
pub fn pos_ready(chain: &Chain) -> Result<bool> {
    let head = chain.head()?.number;
    Ok(chain.phase()?.is_pos(chain.rules(), head + 1))
}

/// The committee of `epoch` from local state, if known yet.
pub fn committee_of(chain: &Chain, epoch: u64) -> Result<Option<EpochCommittee>> {
    let r = chain.store().reader()?;
    Ok(queries::committee_at(&StateView::latest(&r), chain.config().chain_id, epoch)?)
}

fn validator_set(c: &EpochCommittee) -> ValidatorSet {
    ValidatorSet::from_seats(
        c.committee.members.len(),
        c.committee.seats.iter().map(|s| *s as ValidatorIndex).collect(),
    )
}

/// Verifies finality proofs against the committees in local state.
#[derive(Debug, Clone)]
pub struct Finality {
    chain: Arc<Chain>,
}

impl Finality {
    /// For `chain`.
    pub fn new(chain: Arc<Chain>) -> Self {
        Self { chain }
    }
}

impl FinalityCheck for Finality {
    fn check(&self, height: u64, hash: &B256, proof: &[u8]) -> Verdict {
        let Ok(p) = serde_ipld_dagcbor::from_slice::<CommitProof<BlsScheme>>(proof) else {
            return Verdict::NotFinal;
        };
        let rules = *self.chain.rules();
        let epoch = p.qc.epoch;
        if rules.epoch_of(height) != epoch {
            return Verdict::NotFinal;
        }
        let (Ok(Some(c)), Ok(Some(anchor))) =
            (committee_of(&self.chain, epoch), epoch_anchor(&self.chain, epoch))
        else {
            return Verdict::Unknown;
        };
        let scheme = BlsScheme::new(c.pubkeys.clone(), &[]);
        let ok = p.verify(&scheme, &validator_set(&c), self.chain.config().chain_id, &anchor.hash)
            == Some(*hash)
            && p.committed_height == height
            && (p.nil_rounds.is_empty() || height == rules.epoch_end(epoch));
        if ok { Verdict::Final } else { Verdict::NotFinal }
    }

    fn parent_proof(&self, cert: &[u8]) -> Option<Vec<u8>> {
        match decode_cert::<BlsScheme>(cert)? {
            Cert::Epoch(p) => serde_ipld_dagcbor::to_vec(&p).ok(),
            Cert::Qc(_) => None,
        }
    }

    fn is_epoch_start(&self, height: u64) -> bool {
        self.chain.rules().is_epoch_start(height)
    }
}

struct Built {
    block: BlockInfo,
    qc: Qc<BlsScheme>,
    tc: Option<Tc<BlsScheme>>,
    payload: Vec<u8>,
    epoch: u64,
}

enum Internal {
    /// The catch-up worker imported blocks.
    Imported,
    Timer(u64, u64),
    Validated(u64, B256, bool),
    Built(Box<Built>),
    Fetched(u64, BlockInfo),
}

/// Everything about the epoch being run.
struct EpochCtx {
    epoch: u64,
    engine: Engine<BlsScheme>,
    committee: EpochCommittee,
    end: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn round_of(extra: &Bytes) -> Option<u64> {
    (extra.len() == EXTRA_DATA_LEN)
        .then(|| u64::from_be_bytes(extra[..8].try_into().unwrap_or_default()))
}

/// Everything the validator task needs.
pub struct Validator {
    /// Chain.
    pub chain: Arc<Chain>,
    /// Transaction pool.
    pub pool: Arc<TxPool>,
    /// Network.
    pub net: NetHandle,
    /// Validator keys held by this node.
    pub keys: Vec<BlsSecretKey>,
    /// Options.
    pub cfg: ValidatorConfig,
    /// Mine while the chain is in its mining phase.
    pub miner: Option<crate::miner::MinerConfig>,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator").field("keys", &self.keys.len()).finish()
    }
}

/// Votes seen per (epoch, round, signer), for equivocation detection.
type SeenVotes = HashMap<(u64, u64, u16), (B256, BlsSignature)>;

impl Validator {
    fn state_path(&self) -> PathBuf {
        self.cfg.state_dir.join("consensus-state.json")
    }

    fn proof_path(&self, epoch: u64) -> PathBuf {
        self.cfg.state_dir.join("epoch-proofs").join(format!("{epoch}.cbor"))
    }

    /// The envelope certificate for a block whose parent is certified by `qc`.
    fn cert_bytes(&self, qc: &Qc<BlsScheme>) -> Option<Vec<u8>> {
        if is_start_anchor(&self.chain, qc) {
            return Some(Vec::new());
        }
        if !qc.is_anchor() {
            return Some(encode_cert(&Cert::Qc(qc.clone())));
        }
        // First block of an epoch: the proof that the anchor is final.
        if let Ok(b) = std::fs::read(self.proof_path(qc.epoch - 1)) {
            return Some(b);
        }
        // Someone else already built it (we joined late): take it from the chain.
        let r = self.chain.store().reader().ok()?;
        let root = r.envelope_root(qc.height + 1).ok()??;
        let env = Envelope::decode(&r.ipld(&root).ok()??).ok()?;
        Some(env.qc)
    }

    fn save_proof(&self, epoch: u64, proof: &CommitProof<BlsScheme>) {
        save_proof_at(&self.proof_path(epoch), epoch, proof);
    }

    fn enter_epoch(&self, epoch: u64) -> Result<EpochCtx> {
        let anchor = epoch_anchor(&self.chain, epoch)?.context("epoch anchor not final yet")?;
        let committee = committee_of(&self.chain, epoch)?.context("committee not in state yet")?;
        let scheme = BlsScheme::new(committee.pubkeys.clone(), &self.keys);
        let me = scheme.signers();
        let end = self.chain.rules().epoch_end(epoch);
        let cfg = Config {
            chain_id: self.chain.config().chain_id,
            epoch,
            validators: validator_set(&committee),
            me: me.clone(),
            anchor: anchor.clone(),
            end_height: end,
            base_timeout_ms: self.cfg.base_timeout_ms,
        };
        let engine = match std::fs::read(self.state_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Persisted<BlsScheme>>(&b).ok())
        {
            // Saved for another terminal block (a mined reorganisation replaced it before
            // anything was certified): start over on this anchor, above every used round.
            Some(p)
                if p.epoch == epoch
                    && p.high_qc.is_anchor()
                    && p.committed.height == anchor.height
                    && p.committed.hash != anchor.hash =>
            {
                tracing::warn!(
                    epoch,
                    last_voted = p.last_voted,
                    "anchor changed; restarting epoch"
                );
                Engine::reanchored(cfg, scheme, p.last_voted)
            }
            Some(p) if p.epoch == epoch => {
                tracing::info!(
                    epoch,
                    committed = p.committed.height,
                    last_voted = p.last_voted,
                    "resuming consensus state"
                );
                Engine::resume(cfg, scheme, p)
            }
            _ => Engine::new(cfg, scheme),
        };
        tracing::info!(
            epoch,
            members = committee.committee.members.len(),
            seats = committee.committee.seats.len(),
            local = me.len(),
            start = anchor.height + 1,
            end,
            "epoch starting"
        );
        Ok(EpochCtx { epoch, engine, committee, end })
    }

    /// Epoch to run given the local head: the one after the last fully final epoch.
    fn current_epoch(&self) -> Result<u64> {
        let head = self.chain.head()?.number;
        Ok(self.chain.rules().epoch_of(head + 1))
    }

    /// Runs until the network event stream ends.
    pub async fn run(self, mut events: mpsc::Receiver<NetEvent>) -> Result<()> {
        std::fs::create_dir_all(&self.cfg.state_dir)?;
        let (itx, mut irx) = mpsc::unbounded_channel::<Internal>();
        let catch_up = self.spawn_catch_up(itx.clone());
        let mut sources: HashMap<B256, PeerId> = HashMap::new();
        let mut future: VecDeque<Message<BlsScheme>> = VecDeque::new();
        if !self.mining_phase(&mut events, &mut irx, &catch_up, &mut future).await? {
            return Ok(());
        }
        let mut ctx = self.enter_epoch(self.current_epoch()?)?;
        let mut seen: SeenVotes = HashMap::new();
        let mut actions = ctx.engine.start();
        actions.extend(self.replay(&mut ctx, &mut future));
        let mut reannounce = tokio::time::interval(REANNOUNCE);
        loop {
            self.handle(&mut ctx, actions, &itx, &sources, &mut future).await;
            actions = tokio::select! {
                _ = reannounce.tick() => {
                    // Until the committee certified a block, keep the terminal block visible.
                    self.reannounce_mined_head().await;
                    vec![]
                }
                ev = events.recv() => match ev {
                    None => break,
                    Some(NetEvent::Consensus { via, data }) => match bolt_consensus::decode::<BlsScheme>(&data) {
                        Some(msg) => {
                            self.watch_equivocation(&ctx, &msg, &mut seen);
                            if let Message::Proposal(p) = &msg {
                                sources.insert(p.block.hash, via);
                                if sources.len() > 256 {
                                    sources.clear();
                                }
                            }
                            if msg.epoch() == ctx.epoch {
                                ctx.engine.on_message(msg)
                            } else {
                                if msg.epoch() > ctx.epoch {
                                    if future.len() >= 4096 {
                                        future.pop_front();
                                    }
                                    future.push_back(msg);
                                }
                                vec![]
                            }
                        }
                        None => vec![],
                    },
                    Some(NetEvent::Announce { via, announce }) => {
                        // Catch-up path for blocks finalized while we were behind, off the main
                        // loop so consensus keeps up while importing.
                        let head = self.chain.head().map(|h| h.number).unwrap_or(0);
                        if announce.height > head {
                            let _ = catch_up.try_send((via, announce));
                        }
                        vec![]
                    }
                    Some(NetEvent::Tx { raw, reply }) => {
                        let r = self.chain.store().reader().ok();
                        let res = match &r {
                            Some(r) => self.pool.add_raw(&raw, &HeadState(r)).map_err(|e| e.to_string()),
                            None => Err("store unavailable".into()),
                        };
                        let _ = reply.send(res);
                        vec![]
                    }
                    Some(NetEvent::Connected(_)) => vec![],
                },
                i = irx.recv() => match i {
                    None => break,
                    Some(Internal::Imported) => {
                        let mut out = self.reanchor(&mut ctx, &mut future);
                        out.extend(self.catch_up_epochs(&mut ctx, &mut future));
                        out
                    }
                    Some(Internal::Timer(e, r)) if e == ctx.epoch => ctx.engine.on_timeout(r),
                    Some(Internal::Validated(e, h, ok)) if e == ctx.epoch => ctx.engine.on_validated(h, ok),
                    Some(Internal::Built(b)) if b.epoch == ctx.epoch => {
                        let Built { block, qc, tc, payload, .. } = *b;
                        ctx.engine.on_proposed(block, qc, tc, payload)
                    }
                    Some(Internal::Fetched(e, b)) if e == ctx.epoch => ctx.engine.on_block(b),
                    Some(_) => vec![], // from an epoch we left
                },
            };
        }
        Ok(())
    }

    /// Until PoS starts: follow (and optionally mine) the mined chain, keep transactions, buffer
    /// early consensus messages. Returns `false` if the network stopped.
    async fn mining_phase(
        &self,
        events: &mut mpsc::Receiver<NetEvent>,
        irx: &mut mpsc::UnboundedReceiver<Internal>,
        catch_up: &mpsc::Sender<(PeerId, Announce)>,
        future: &mut VecDeque<Message<BlsScheme>>,
    ) -> Result<bool> {
        if pos_ready(&self.chain)? {
            return Ok(true);
        }
        tracing::info!(head = self.chain.head()?.number, "mining phase: following mined blocks");
        let miner = self.miner.clone().map(|cfg| {
            tokio::spawn(crate::miner::run(
                self.chain.clone(),
                self.pool.clone(),
                self.net.clone(),
                cfg,
            ))
        });
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        let mut last_announce = std::time::Instant::now();
        let ready = loop {
            if last_announce.elapsed() > REANNOUNCE {
                self.reannounce_mined_head().await;
                last_announce = std::time::Instant::now();
            }
            if pos_ready(&self.chain)? {
                break true;
            }
            tokio::select! {
                ev = events.recv() => match ev {
                    None => break false,
                    Some(NetEvent::Consensus { data, .. }) => {
                        if let Some(msg) = bolt_consensus::decode::<BlsScheme>(&data) {
                            if future.len() >= 4096 {
                                future.pop_front();
                            }
                            future.push_back(msg);
                        }
                    }
                    Some(NetEvent::Announce { via, announce }) => {
                        let _ = catch_up.try_send((via, announce));
                    }
                    Some(NetEvent::Tx { raw, reply }) => {
                        let r = self.chain.store().reader().ok();
                        let res = match &r {
                            Some(r) => self.pool.add_raw(&raw, &HeadState(r)).map_err(|e| e.to_string()),
                            None => Err("store unavailable".into()),
                        };
                        let _ = reply.send(res);
                    }
                    Some(NetEvent::Connected(_)) => {}
                },
                _ = irx.recv() => {
                    if let Ok(r) = self.chain.store().reader() {
                        self.pool.on_new_block(&HeadState(&r));
                    }
                }
                _ = tick.tick() => {}
            }
        };
        if let Some(m) = miner {
            m.abort();
        }
        if ready {
            let phase = self.chain.phase()?;
            tracing::info!(
                epoch = ?phase.pos_epoch,
                terminal = ?phase.terminal_height(self.chain.rules()),
                "PoS starts: the committee takes over from the last mined block"
            );
        }
        Ok(ready)
    }

    /// Re-publishes the head while it is a mined block, so nodes that missed a block (or sit on a
    /// lighter branch) converge; this matters most at the terminal block, after which nothing is
    /// mined any more.
    async fn reannounce_mined_head(&self) {
        let Ok(head) = self.chain.head() else { return };
        if head.number == 0 || head.difficulty.is_zero() {
            return;
        }
        if let Some(a) = bolt_sync::announce_stored(&self.chain, head.number, Vec::new()) {
            let _ = self.net.publish(a).await;
        }
    }

    /// First PoS epoch: if a mined reorganisation replaced the terminal block before anything
    /// was certified, restart the engine on the new anchor (never reusing a round).
    fn reanchor(
        &self,
        ctx: &mut EpochCtx,
        future: &mut VecDeque<Message<BlsScheme>>,
    ) -> Vec<Action<BlsScheme>> {
        let Ok(Some(anchor)) = epoch_anchor(&self.chain, ctx.epoch) else { return vec![] };
        let current = ctx.engine.config().anchor.clone();
        if anchor.hash == current.hash
            || ctx.engine.committed().height != current.height
            || !ctx.engine.high_qc().is_anchor()
        {
            return vec![];
        }
        tracing::warn!(epoch = ctx.epoch, old = %current.hash, new = %anchor.hash, "terminal block replaced; restarting the epoch");
        self.persist(&ctx.engine);
        match self.enter_epoch(ctx.epoch) {
            Ok(next) => {
                *ctx = next;
                let mut out = ctx.engine.start();
                out.extend(self.replay(ctx, future));
                out
            }
            Err(e) => {
                tracing::warn!("cannot restart epoch {}: {e:#}", ctx.epoch);
                vec![]
            }
        }
    }

    /// Worker importing announced final blocks (with proofs) one announcement at a time.
    fn spawn_catch_up(
        &self,
        itx: mpsc::UnboundedSender<Internal>,
    ) -> mpsc::Sender<(PeerId, Announce)> {
        let (tx, mut rx) = mpsc::channel::<(PeerId, Announce)>(16);
        let follower = Follower::with_finality(
            self.chain.clone(),
            self.net.clone(),
            Arc::new(Finality::new(self.chain.clone())),
        );
        let (chain, dir) = (self.chain.clone(), self.cfg.state_dir.clone());
        tokio::spawn(async move {
            while let Some((via, a)) = rx.recv().await {
                let (height, proof) = (a.height, a.proof.clone());
                match follower.on_announce(via, a).await {
                    Ok(_) => {
                        // The last block of an epoch comes with the next epoch's anchor proof.
                        let rules = *chain.rules();
                        let epoch = rules.epoch_of(height);
                        if height == rules.epoch_end(epoch)
                            && let Ok(p) =
                                serde_ipld_dagcbor::from_slice::<CommitProof<BlsScheme>>(&proof)
                        {
                            let path = dir.join("epoch-proofs").join(format!("{epoch}.cbor"));
                            save_proof_at(&path, epoch, &p);
                        }
                        let _ = itx.send(Internal::Imported);
                    }
                    Err(e) => tracing::debug!("catch-up import skipped: {e}"),
                }
            }
        });
        tx
    }

    /// Moves to later epochs if the chain (imported through the catch-up path) passed the current
    /// one's end. Returns the new engine's start actions.
    fn catch_up_epochs(
        &self,
        ctx: &mut EpochCtx,
        future: &mut VecDeque<Message<BlsScheme>>,
    ) -> Vec<Action<BlsScheme>> {
        let mut out = Vec::new();
        loop {
            let head = self.chain.head().map(|h| h.number).unwrap_or(0);
            if head < ctx.end {
                return out;
            }
            match self.enter_epoch(ctx.epoch + 1) {
                Ok(next) => {
                    *ctx = next;
                    out = ctx.engine.start();
                    out.extend(self.replay(ctx, future));
                }
                Err(e) => {
                    tracing::warn!("cannot enter epoch {}: {e:#}", ctx.epoch + 1);
                    return out;
                }
            }
        }
    }

    fn replay(
        &self,
        ctx: &mut EpochCtx,
        future: &mut VecDeque<Message<BlsScheme>>,
    ) -> Vec<Action<BlsScheme>> {
        let mut out = Vec::new();
        let msgs: Vec<_> = future.drain(..).collect();
        for m in msgs {
            if m.epoch() == ctx.epoch {
                out.extend(ctx.engine.on_message(m));
            } else if m.epoch() > ctx.epoch {
                future.push_back(m);
            }
        }
        out
    }

    /// Two votes by one signer in the same epoch and round for different blocks: slashable.
    /// Writes ready-to-send `ConsensusRegistry.submitEvidence` calldata to `evidence/`.
    fn watch_equivocation(&self, ctx: &EpochCtx, msg: &Message<BlsScheme>, seen: &mut SeenVotes) {
        let Message::Vote(v) = msg else { return };
        if v.epoch != ctx.epoch {
            return;
        }
        let key = (v.epoch, v.round, v.signer);
        match seen.get(&key) {
            None => {
                if seen.len() > 100_000 {
                    seen.clear();
                }
                seen.insert(key, (v.block, v.sig));
            }
            Some((block, sig)) if *block != v.block => {
                let Some(pk) = ctx.committee.pubkeys.get(v.signer as usize) else { return };
                let chain_id = self.chain.config().chain_id;
                let (ma, mb) = (
                    bolt_consensus::vote_msg(chain_id, v.epoch, v.round, block),
                    bolt_consensus::vote_msg(chain_id, v.epoch, v.round, &v.block),
                );
                if !bls::verify(pk, &ma, sig) || !bls::verify(pk, &mb, &v.sig) {
                    return;
                }
                let id = ctx.committee.committee.members[v.signer as usize];
                tracing::error!(
                    validator = id,
                    epoch = v.epoch,
                    round = v.round,
                    "equivocation: two votes in one round"
                );
                if let Some(data) =
                    bolt_system::evidence::submit_evidence_calldata(id, pk, &ma, sig, &mb, &v.sig)
                {
                    let dir = self.cfg.state_dir.join("evidence");
                    let _ = std::fs::create_dir_all(&dir);
                    let file = dir.join(format!("{}-{}-{id}.json", v.epoch, v.round));
                    let json = serde_json::json!({
                        "to": bolt_system::addresses::CONSENSUS,
                        "data": data,
                        "validator": id,
                        "epoch": v.epoch,
                        "round": v.round,
                    });
                    let _ =
                        std::fs::write(file, serde_json::to_vec_pretty(&json).unwrap_or_default());
                }
            }
            Some(_) => {}
        }
    }

    fn persist(&self, engine: &Engine<BlsScheme>) {
        let bytes = serde_json::to_vec(&engine.persisted()).expect("state serializes");
        let path = self.state_path();
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path)).is_err() {
            tracing::error!("could not persist consensus state");
        }
    }

    async fn handle(
        &self,
        ctx: &mut EpochCtx,
        actions: Vec<Action<BlsScheme>>,
        itx: &mpsc::UnboundedSender<Internal>,
        sources: &HashMap<B256, PeerId>,
        future: &mut VecDeque<Message<BlsScheme>>,
    ) {
        let mut queue: VecDeque<Action<BlsScheme>> = actions.into();
        while !queue.is_empty() {
            // Safety state hits the disk before any vote or timeout leaves this node.
            if queue.iter().any(|a| {
                matches!(
                    a,
                    Action::SendTo(..)
                        | Action::Broadcast(Message::Timeout(_))
                        | Action::Broadcast(Message::Vote(_))
                        | Action::Commit { .. }
                )
            }) {
                self.persist(&ctx.engine);
            }
            let batch: Vec<_> = queue.drain(..).collect();
            for a in batch {
                match a {
                    Action::Broadcast(m) | Action::SendTo(_, m) => {
                        let _ = self.net.publish_consensus(bolt_consensus::encode(&m)).await;
                    }
                    Action::ScheduleTimeout { round, after_ms } => {
                        let (itx, epoch) = (itx.clone(), ctx.epoch);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(after_ms)).await;
                            let _ = itx.send(Internal::Timer(epoch, round));
                        });
                    }
                    Action::Validate(p) => {
                        let (chain, net, itx) = (self.chain.clone(), self.net.clone(), itx.clone());
                        let via = sources.get(&p.block.hash).copied();
                        let (epoch, keys) = (ctx.epoch, ctx.committee.pubkeys.clone());
                        tokio::spawn(async move {
                            let hash = p.block.hash;
                            let ok = match validate(&chain, &net, &p, via, &keys).await {
                                Ok(()) => true,
                                Err(e) => {
                                    tracing::warn!(
                                        round = p.block.round,
                                        height = p.block.height,
                                        "proposal rejected: {e:#}"
                                    );
                                    false
                                }
                            };
                            let _ = itx.send(Internal::Validated(epoch, hash, ok));
                        });
                    }
                    Action::Propose { round, qc, tc } => {
                        let Some(cert) = self.cert_bytes(&qc) else {
                            tracing::warn!(
                                round,
                                "no epoch proof for the anchor yet; skipping proposal"
                            );
                            continue;
                        };
                        let leader = ctx.engine.validators().leader(round);
                        let fee_recipient = ctx
                            .committee
                            .fee_recipients
                            .get(leader as usize)
                            .copied()
                            .unwrap_or(Address::ZERO);
                        let Some(sk) = ctx.committee.pubkeys.get(leader as usize).and_then(|pk| {
                            self.keys.iter().find(|k| k.public_key() == *pk).cloned()
                        }) else {
                            continue;
                        };
                        let (chain, pool, itx) =
                            (self.chain.clone(), self.pool.clone(), itx.clone());
                        let (slot, epoch) = (self.cfg.slot_ms, ctx.epoch);
                        tokio::spawn(async move {
                            match propose(&chain, &pool, round, &qc, cert, slot, fee_recipient, &sk)
                                .await
                            {
                                Ok((block, payload)) => {
                                    let b = Built { block, qc, tc, payload, epoch };
                                    let _ = itx.send(Internal::Built(Box::new(b)));
                                }
                                Err(e) => tracing::warn!(round, "could not build proposal: {e:#}"),
                            }
                        });
                    }
                    Action::Commit { blocks, proof } => self.commit(blocks, proof).await,
                    Action::EpochEnd { block, proof } => {
                        tracing::info!(epoch = ctx.epoch, height = block.height, "epoch final");
                        self.save_proof(ctx.epoch, &proof);
                        queue.extend(self.catch_up_epochs(ctx, future));
                    }
                    Action::FetchBlock(hash) => {
                        // Blocks are content-addressed: if we have the header (pending or final) we
                        // can describe it; otherwise the catch-up path brings it with a proof.
                        if let Ok(Some(h)) = self.chain.header_of(&hash)
                            && let Some(round) = round_of(&h.extra_data)
                        {
                            let b =
                                BlockInfo { hash, parent: h.parent_hash, round, height: h.number };
                            let _ = itx.send(Internal::Fetched(ctx.epoch, b));
                        }
                    }
                }
            }
        }
    }

    async fn commit(&self, blocks: Vec<BlockInfo>, mut proof: CommitProof<BlsScheme>) {
        let chain = self.chain.clone();
        let last = blocks.last().cloned();
        let res = tokio::task::spawn_blocking(move || {
            for b in &blocks {
                let head = chain.head()?;
                if head.number >= b.height {
                    continue; // already imported (e.g. via the catch-up path)
                }
                chain.commit_pending(&b.hash)?;
            }
            anyhow::Ok(())
        })
        .await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("commit deferred to catch-up: {e:#}");
                return;
            }
            Err(e) => {
                tracing::error!("commit task failed: {e}");
                return;
            }
        }
        if let Ok(r) = self.chain.store().reader() {
            self.pool.on_new_block(&HeadState(&r));
        }
        let Some(last) = last else { return };
        // Publish the final block with its proof, for followers and lagging validators.
        if proof.nil_rounds.is_empty() {
            match self.chain.header_of(&proof.child_qc.block) {
                Ok(Some(h)) => proof.child_header = alloy_rlp::encode(&h),
                _ => return,
            }
        }
        let r = match self.chain.store().reader() {
            Ok(r) => r,
            Err(_) => return,
        };
        let Ok(Some(root)) = r.envelope_root(last.height) else { return };
        let Some(envelope) = r.ipld(&root).ok().flatten() else { return };
        let Ok(env) = Envelope::decode(&envelope) else { return };
        let header = r.ipld(&env.header).ok().flatten().unwrap_or_default();
        let announce = Announce {
            height: last.height,
            root,
            envelope,
            header,
            inline: Vec::new(),
            proof: serde_ipld_dagcbor::to_vec(&proof).expect("proof serializes"),
        };
        tracing::info!(height = last.height, hash = %last.hash, round = last.round, "finalized");
        let _ = self.net.publish(announce).await;
    }
}

fn save_proof_at(path: &std::path::Path, epoch: u64, proof: &CommitProof<BlsScheme>) {
    let _ = std::fs::create_dir_all(path.parent().expect("has parent"));
    if std::fs::write(path, encode_cert(&Cert::Epoch(proof.clone()))).is_err() {
        tracing::error!(epoch, "could not store epoch proof");
    }
}

/// Builds this node's proposal for `round` on the block certified by `qc`.
#[allow(clippy::too_many_arguments)]
async fn propose(
    chain: &Arc<Chain>,
    pool: &Arc<TxPool>,
    round: u64,
    qc: &Qc<BlsScheme>,
    cert: Vec<u8>,
    slot_ms: u64,
    fee_recipient: Address,
    key: &BlsSecretKey,
) -> Result<(BlockInfo, Vec<u8>)> {
    // The parent may still be validating when its QC forms; give it a moment.
    let mut parent = None;
    for _ in 0..40 {
        if let Some(h) = chain.header_of(&qc.block)? {
            parent = Some(h);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let parent = parent.context("parent block not available")?;
    // Pace blocks: at least one slot after the parent.
    let earliest = parent.timestamp * 1000 + slot_ms;
    let now = now_ms();
    if earliest > now {
        tokio::time::sleep(Duration::from_millis(earliest - now)).await;
    }
    let timestamp = (now_ms() / 1000).max(parent.timestamp + 1);
    let height = parent.number + 1;
    let reveal = key.sign(&randao_msg(chain.config().chain_id, height));
    let extra: Bytes = [round.to_be_bytes().as_slice(), reveal.as_slice()].concat().into();
    let candidates = {
        let r = chain.store().reader()?;
        let base_fee = chain.next_base_fee()?;
        pool.best(base_fee, &HeadState(&r))
    };
    let (chain2, parent_hash) = (chain.clone(), qc.block);
    let built = tokio::task::spawn_blocking(move || {
        chain2.build_on(&parent_hash, candidates, timestamp, fee_recipient, extra, cert)
    })
    .await??;
    let payload = bolt_sync::announce_for(&built.bundle).encode();
    Ok((
        BlockInfo { hash: built.hash, parent: qc.block, round, height: built.header.number },
        payload,
    ))
}

/// Checks a proposal: data present and verified against CIDs, certificate and RANDAO reveal
/// consistent with the consensus metadata, block executes on its parent.
async fn validate(
    chain: &Arc<Chain>,
    net: &NetHandle,
    p: &Proposal<BlsScheme>,
    via: Option<PeerId>,
    committee_keys: &[bls::BlsPublicKey],
) -> Result<()> {
    let a = Announce::decode(&p.payload).context("payload is not an announcement")?;
    verify(&a.root, &a.envelope)?;
    let env = Envelope::decode(&a.envelope)?;
    anyhow::ensure!(env.height == p.block.height, "envelope height");
    anyhow::ensure!(env.block_hash() == Some(p.block.hash), "envelope names a different block");
    if is_start_anchor(chain, &p.qc) {
        anyhow::ensure!(
            env.qc.is_empty(),
            "first block after genesis or the terminal block carries a certificate"
        );
    } else if p.qc.is_anchor() {
        // First block of an epoch: the envelope must prove the anchor final.
        let f = Finality::new(chain.clone());
        let proof =
            f.parent_proof(&env.qc).context("first block of the epoch lacks the epoch proof")?;
        anyhow::ensure!(
            f.check(p.qc.height, &p.qc.block, &proof) == Verdict::Final,
            "invalid epoch proof"
        );
    } else {
        anyhow::ensure!(
            env.qc == encode_cert(&Cert::Qc(p.qc.clone())),
            "envelope certificate differs from the proposal's"
        );
    }
    let mut have: HashMap<bolt_ipld::Cid, Vec<u8>> = HashMap::new();
    have.insert(env.header, a.header.clone());
    for b in a.inline {
        have.insert(b.cid, b.data);
    }
    let missing: Vec<_> = env.chunks.iter().filter(|c| !have.contains_key(c)).copied().collect();
    if !missing.is_empty() {
        let mut peers: Vec<PeerId> = via.into_iter().collect();
        peers.extend(net.peers().await.into_iter().filter(|x| Some(*x) != via));
        have.extend(net.fetch(&missing, &peers).await?);
    }
    let qc = env.qc.clone();
    let block = decode_block(env, |c| have.get(c).cloned())?;
    let h = &block.header;
    anyhow::ensure!(h.parent_hash == p.block.parent, "header parent differs");
    anyhow::ensure!(round_of(&h.extra_data) == Some(p.block.round), "header round differs");
    let reveal = BlsSignature::from_slice(&h.extra_data[8..]);
    let pk = committee_keys.get(p.proposer as usize).context("unknown proposer")?;
    anyhow::ensure!(
        bls::verify(pk, &randao_msg(chain.config().chain_id, h.number), &reveal),
        "bad RANDAO reveal"
    );
    anyhow::ensure!(h.timestamp * 1000 <= now_ms() + 3_000, "timestamp in the future");
    let chain = chain.clone();
    tokio::task::spawn_blocking(move || chain.verify_block(&block.header, block.transactions, qc))
        .await??;
    Ok(())
}

/// Validator CLI options.
#[derive(Debug, clap::Args)]
pub struct ValidatorArgs {
    /// Genesis file.
    #[arg(long, default_value = "genesis/dev.json")]
    pub genesis: PathBuf,
    /// Data directory.
    #[arg(long, default_value = "data/validator")]
    pub datadir: PathBuf,
    /// BLS key file (`boltchain keys new` / `keys dev`); repeat for several validators. Without
    /// any, the node follows the chain (and mines with `--mine`) but does not validate.
    #[arg(long)]
    pub key: Vec<PathBuf>,
    /// JSON-RPC listen address.
    #[arg(long, default_value = "127.0.0.1:8545")]
    pub rpc: std::net::SocketAddr,
    /// Minimum seconds between blocks (defaults to the genesis slot).
    #[arg(long)]
    pub block_time: Option<u64>,
    /// Base round timeout in milliseconds (defaults to two slots).
    #[arg(long)]
    pub timeout_ms: Option<u64>,
    /// Mining while the chain is in its mining phase.
    #[command(flatten)]
    pub mining: crate::miner::MiningArgs,
    /// P2P options.
    #[command(flatten)]
    pub p2p: crate::p2p::P2pArgs,
}

/// Runs a validator node until Ctrl-C.
pub async fn run(args: ValidatorArgs) -> Result<()> {
    let genesis = crate::devnet::load_genesis(&args.genesis)?;
    let keys = args.key.iter().map(|p| keys::load_key(p)).collect::<Result<Vec<_>>>()?;
    let chain = Arc::new(Chain::open(&args.datadir, &genesis)?);
    let cfg = chain.config().clone();
    let pool = Arc::new(TxPool::new(bolt_txpool::PoolConfig::new(
        cfg.chain_id,
        cfg.gas_limit,
        cfg.min_base_fee_wei,
    )));
    if let Some(g) = args.mining.gas_target {
        chain.set_gas_target(g);
    }
    let (net, events) = args.p2p.start(&args.datadir, chain.clone(), None).await?;
    let ctx = bolt_rpc::RpcContext {
        chain: chain.clone(),
        pool: pool.clone(),
        client_version: format!("boltchain/v{}", env!("CARGO_PKG_VERSION")),
        forwarder: None,
    };
    let (addr, handle) = bolt_rpc::start(args.rpc, ctx).await?;
    tracing::info!(%addr, keys = keys.len(), "JSON-RPC listening");
    let slot_ms = args.block_time.unwrap_or(cfg.slot_seconds) * 1000;
    let miner = args.mining.config()?;
    let v = Validator {
        chain,
        pool,
        net,
        keys,
        cfg: ValidatorConfig {
            slot_ms,
            base_timeout_ms: args.timeout_ms.unwrap_or(slot_ms * 2),
            state_dir: args.datadir.join("consensus"),
        },
        miner,
    };
    let task = tokio::spawn(v.run(events));
    tokio::signal::ctrl_c().await?;
    task.abort();
    handle.stop()?;
    Ok(())
}
