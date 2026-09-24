//! Validator: runs the consensus engine against the chain, the transaction pool and the network.
//!
//! Data-availability rule: a validator only votes for a proposal after it holds every part of the
//! block (fetched over Bitswap and written to its blockstore) and has executed it on top of the
//! parent and matched the header. A QC therefore also certifies that more than 2/3 of the stake
//! holds the data.

use crate::keys;
use alloy_primitives::{Address, B256, Bytes};
use anyhow::{Context, Result};
use bolt_chain::Chain;
use bolt_consensus::{
    Action, BlockInfo, BlsScheme, CommitProof, Config, Engine, Message, Persisted, Proposal, Qc,
    Tc, ValidatorIndex, ValidatorSet,
};
use bolt_ipld::{Envelope, decode_block, verify};
use bolt_net::{Announce, NetEvent, NetHandle};
use bolt_primitives::{Genesis, bls::BlsSecretKey};
use bolt_rpc::HeadState;
use bolt_sync::{FinalityCheck, Follower};
use bolt_txpool::TxPool;
use libp2p::PeerId;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::mpsc;

/// Validator options.
#[derive(Debug, Clone)]
pub struct ValidatorConfig {
    /// Minimum time between a block and its child (the slot), in milliseconds.
    pub slot_ms: u64,
    /// Base round timeout in milliseconds.
    pub base_timeout_ms: u64,
    /// Where consensus safety state is persisted.
    pub state_path: PathBuf,
}

/// Public keys and weights of the genesis validators.
pub fn genesis_validators(g: &Genesis) -> (Vec<bolt_primitives::bls::BlsPublicKey>, ValidatorSet) {
    let keys: Vec<_> = g.bootstrap_validators.iter().map(|v| v.bls_pubkey).collect();
    let set = ValidatorSet::equal(keys.len());
    (keys, set)
}

/// Verifies finality proofs against the genesis validator set.
#[derive(Debug, Clone)]
pub struct Finality {
    scheme: BlsScheme,
    set: ValidatorSet,
    chain_id: u64,
    genesis: B256,
}

impl Finality {
    /// From a genesis file.
    pub fn new(g: &Genesis) -> Self {
        let (keys, set) = genesis_validators(g);
        Self {
            scheme: BlsScheme::new(keys, None),
            set,
            chain_id: g.config.chain_id,
            genesis: g.hash(),
        }
    }
}

impl FinalityCheck for Finality {
    fn is_final(&self, block: &B256, proof: &[u8]) -> bool {
        let Ok(p) = serde_ipld_dagcbor::from_slice::<CommitProof<BlsScheme>>(proof) else {
            return false;
        };
        p.qc.block == *block && p.verify(&self.scheme, &self.set, self.chain_id, &self.genesis)
    }
}

struct Built {
    block: BlockInfo,
    qc: Qc<BlsScheme>,
    tc: Option<Tc<BlsScheme>>,
    payload: Vec<u8>,
}

enum Internal {
    Timer(u64),
    Validated(B256, bool),
    Built(Box<Built>),
    Fetched(BlockInfo),
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn round_of(extra: &Bytes) -> Option<u64> {
    (extra.len() == 8).then(|| u64::from_be_bytes(extra[..8].try_into().unwrap_or_default()))
}

fn encode_qc(qc: &Qc<BlsScheme>) -> Vec<u8> {
    if qc.is_genesis() {
        Vec::new()
    } else {
        serde_ipld_dagcbor::to_vec(qc).expect("qc serializes")
    }
}

/// Everything the validator task needs.
pub struct Validator {
    /// Chain.
    pub chain: Arc<Chain>,
    /// Transaction pool.
    pub pool: Arc<TxPool>,
    /// Network.
    pub net: NetHandle,
    /// Genesis.
    pub genesis: Genesis,
    /// This validator's key.
    pub key: BlsSecretKey,
    /// Options.
    pub cfg: ValidatorConfig,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator").field("key", &self.key.public_key()).finish()
    }
}

impl Validator {
    /// Runs until the network event stream ends.
    pub async fn run(self, mut events: mpsc::Receiver<NetEvent>) -> Result<()> {
        let (keys, set) = genesis_validators(&self.genesis);
        let me = keys.iter().position(|k| *k == self.key.public_key()).map(|i| i as ValidatorIndex);
        let fee_recipient = me
            .map(|i| self.genesis.bootstrap_validators[i as usize].fee_recipient)
            .unwrap_or(Address::ZERO);
        let scheme = BlsScheme::new(keys, Some(self.key.clone()));
        let genesis_hash = self.genesis.hash();
        let ecfg = Config {
            chain_id: self.genesis.config.chain_id,
            validators: set.clone(),
            me,
            genesis: genesis_hash,
            base_timeout_ms: self.cfg.base_timeout_ms,
        };
        let mut engine = match std::fs::read(&self.cfg.state_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Persisted<BlsScheme>>(&b).ok())
        {
            Some(p) => {
                tracing::info!(
                    committed = p.committed.height,
                    last_voted = p.last_voted,
                    "resuming consensus state"
                );
                Engine::resume(ecfg, scheme, p)
            }
            None => Engine::new(ecfg, scheme),
        };
        tracing::info!(?me, validators = set.len(), "consensus starting");

        let follower = Follower::with_finality(
            self.chain.clone(),
            self.net.clone(),
            Arc::new(Finality::new(&self.genesis)),
        );
        let (itx, mut irx) = mpsc::unbounded_channel::<Internal>();
        let mut sources: HashMap<B256, PeerId> = HashMap::new();
        let mut actions = engine.start();
        loop {
            self.handle(&mut engine, actions, &itx, &sources, fee_recipient).await;
            actions = tokio::select! {
                ev = events.recv() => match ev {
                    None => break,
                    Some(NetEvent::Consensus { via, data }) => match bolt_consensus::decode::<BlsScheme>(&data) {
                        Some(msg) => {
                            if let Message::Proposal(p) = &msg {
                                sources.insert(p.block.hash, via);
                                if sources.len() > 256 {
                                    sources.clear();
                                }
                            }
                            engine.on_message(msg)
                        }
                        None => vec![],
                    },
                    Some(NetEvent::Announce { via, announce }) => {
                        // Catch-up path for blocks finalized while we were behind.
                        let head = self.chain.head().map(|h| h.number).unwrap_or(0);
                        if announce.height > head
                            && let Err(e) = follower.on_announce(via, announce).await
                        {
                            tracing::debug!("catch-up import skipped: {e}");
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
                    Some(Internal::Timer(r)) => engine.on_timeout(r),
                    Some(Internal::Validated(h, ok)) => engine.on_validated(h, ok),
                    Some(Internal::Built(b)) => { let Built { block, qc, tc, payload } = *b; engine.on_proposed(block, qc, tc, payload) }
                    Some(Internal::Fetched(b)) => engine.on_block(b),
                },
            };
        }
        Ok(())
    }

    fn persist(&self, engine: &Engine<BlsScheme>) {
        let bytes = serde_json::to_vec(&engine.persisted()).expect("state serializes");
        let tmp = self.cfg.state_path.with_extension("tmp");
        if std::fs::write(&tmp, bytes)
            .and_then(|_| std::fs::rename(&tmp, &self.cfg.state_path))
            .is_err()
        {
            tracing::error!("could not persist consensus state");
        }
    }

    async fn handle(
        &self,
        engine: &mut Engine<BlsScheme>,
        actions: Vec<Action<BlsScheme>>,
        itx: &mpsc::UnboundedSender<Internal>,
        sources: &HashMap<B256, PeerId>,
        fee_recipient: Address,
    ) {
        // Safety state hits the disk before any vote or timeout leaves this node.
        if actions.iter().any(|a| {
            matches!(
                a,
                Action::SendTo(..)
                    | Action::Broadcast(Message::Timeout(_))
                    | Action::Broadcast(Message::Vote(_))
            )
        }) || actions.iter().any(|a| matches!(a, Action::Commit { .. }))
        {
            self.persist(engine);
        }
        for a in actions {
            match a {
                Action::Broadcast(m) | Action::SendTo(_, m) => {
                    let _ = self.net.publish_consensus(bolt_consensus::encode(&m)).await;
                }
                Action::ScheduleTimeout { round, after_ms } => {
                    let itx = itx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(after_ms)).await;
                        let _ = itx.send(Internal::Timer(round));
                    });
                }
                Action::Validate(p) => {
                    let (chain, net, itx) = (self.chain.clone(), self.net.clone(), itx.clone());
                    let via = sources.get(&p.block.hash).copied();
                    tokio::spawn(async move {
                        let hash = p.block.hash;
                        let ok = match validate(&chain, &net, &p, via).await {
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
                        let _ = itx.send(Internal::Validated(hash, ok));
                    });
                }
                Action::Propose { round, qc, tc } => {
                    let (chain, pool, itx, slot) =
                        (self.chain.clone(), self.pool.clone(), itx.clone(), self.cfg.slot_ms);
                    tokio::spawn(async move {
                        match propose(&chain, &pool, round, &qc, slot, fee_recipient).await {
                            Ok((block, payload)) => {
                                let _ = itx.send(Internal::Built(Box::new(Built {
                                    block,
                                    qc,
                                    tc,
                                    payload,
                                })));
                            }
                            Err(e) => tracing::warn!(round, "could not build proposal: {e:#}"),
                        }
                    });
                }
                Action::Commit { blocks, qc, child_qc } => self.commit(blocks, qc, child_qc).await,
                Action::FetchBlock(hash) => {
                    // Blocks are content-addressed: if we have the header (pending or final) we can
                    // describe it; otherwise the catch-up path will bring it with a finality proof.
                    if let Ok(Some(h)) = self.chain.header_of(&hash)
                        && let Some(round) = round_of(&h.extra_data)
                    {
                        let _ = itx.send(Internal::Fetched(BlockInfo {
                            hash,
                            parent: h.parent_hash,
                            round,
                            height: h.number,
                        }));
                    }
                }
            }
        }
    }

    async fn commit(&self, blocks: Vec<BlockInfo>, qc: Qc<BlsScheme>, child_qc: Qc<BlsScheme>) {
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
        let child_header = match self.chain.header_of(&child_qc.block) {
            Ok(Some(h)) => alloy_rlp::encode(&h),
            _ => return,
        };
        let proof = CommitProof { qc, child_qc, child_header };
        let r = match self.chain.store().reader() {
            Ok(r) => r,
            Err(_) => return,
        };
        let (Ok(Some(root)), Ok(Some(_))) = (r.envelope_root(last.height), r.header(last.height))
        else {
            return;
        };
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

/// Builds this node's proposal for `round` on the block certified by `qc`.
async fn propose(
    chain: &Arc<Chain>,
    pool: &Arc<TxPool>,
    round: u64,
    qc: &Qc<BlsScheme>,
    slot_ms: u64,
    fee_recipient: Address,
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
    let candidates = {
        let base_fee = bolt_exec::next_base_fee(&parent, chain.config().min_base_fee_wei);
        let r = chain.store().reader()?;
        pool.best(base_fee, &HeadState(&r))
    };
    let (chain2, parent_hash, qc_bytes) = (chain.clone(), qc.block, encode_qc(qc));
    let built = tokio::task::spawn_blocking(move || {
        chain2.build_on(
            &parent_hash,
            candidates,
            timestamp,
            fee_recipient,
            Bytes::from(round.to_be_bytes().to_vec()),
            qc_bytes,
        )
    })
    .await??;
    let payload = bolt_sync::announce_for(&built.bundle).encode();
    Ok((
        BlockInfo { hash: built.hash, parent: qc.block, round, height: built.header.number },
        payload,
    ))
}

/// Checks a proposal: data present and verified against CIDs, header consistent with the
/// consensus metadata, block executes on its parent.
async fn validate(
    chain: &Arc<Chain>,
    net: &NetHandle,
    p: &Proposal<BlsScheme>,
    via: Option<PeerId>,
) -> Result<()> {
    let a = Announce::decode(&p.payload).context("payload is not an announcement")?;
    verify(&a.root, &a.envelope)?;
    let env = Envelope::decode(&a.envelope)?;
    anyhow::ensure!(env.height == p.block.height, "envelope height");
    anyhow::ensure!(env.block_hash() == Some(p.block.hash), "envelope names a different block");
    anyhow::ensure!(env.qc == encode_qc(&p.qc), "envelope certificate differs from the proposal's");
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
    /// BLS key file (`boltchain keys new` / `keys dev`).
    #[arg(long)]
    pub key: PathBuf,
    /// JSON-RPC listen address.
    #[arg(long, default_value = "127.0.0.1:8545")]
    pub rpc: std::net::SocketAddr,
    /// Minimum seconds between blocks (defaults to the genesis slot).
    #[arg(long)]
    pub block_time: Option<u64>,
    /// Base round timeout in milliseconds (defaults to two slots).
    #[arg(long)]
    pub timeout_ms: Option<u64>,
    /// P2P options.
    #[command(flatten)]
    pub p2p: crate::p2p::P2pArgs,
}

/// Runs a validator node until Ctrl-C.
pub async fn run(args: ValidatorArgs) -> Result<()> {
    let genesis = crate::devnet::load_genesis(&args.genesis)?;
    let key = keys::load_key(&args.key)?;
    let chain = Arc::new(Chain::open(&args.datadir, &genesis)?);
    let cfg = chain.config().clone();
    let pool = Arc::new(TxPool::new(bolt_txpool::PoolConfig::new(
        cfg.chain_id,
        cfg.gas_limit,
        cfg.min_base_fee_wei,
    )));
    let (net, events) = args.p2p.start(&args.datadir, chain.clone(), None).await?;
    let ctx = bolt_rpc::RpcContext {
        chain: chain.clone(),
        pool: pool.clone(),
        client_version: format!("boltchain/v{}", env!("CARGO_PKG_VERSION")),
        forwarder: None,
    };
    let (addr, handle) = bolt_rpc::start(args.rpc, ctx).await?;
    tracing::info!(%addr, "JSON-RPC listening");
    let slot_ms = args.block_time.unwrap_or(cfg.slot_seconds) * 1000;
    let v = Validator {
        chain,
        pool,
        net,
        genesis,
        key,
        cfg: ValidatorConfig {
            slot_ms,
            base_timeout_ms: args.timeout_ms.unwrap_or(slot_ms * 2),
            state_path: args.datadir.join("consensus-state.json"),
        },
    };
    let task = tokio::spawn(v.run(events));
    tokio::signal::ctrl_c().await?;
    task.abort();
    handle.stop()?;
    Ok(())
}
