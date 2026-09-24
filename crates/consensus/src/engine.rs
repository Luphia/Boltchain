//! The consensus state machine: two-chain HotStuff with Jolteon's voting and timeout rules.
//!
//! Pure logic, no I/O: inputs are messages, validation results and timer expiries; outputs are
//! [`Action`]s the node carries out. This makes every behaviour reproducible in the simulator.
//!
//! Rules (round `r`, leader `L(r)`):
//! * **Propose.** `L(r)` proposes a block extending the block certified by the highest QC, carrying
//!   that QC, plus a TC for `r - 1` if the previous round timed out.
//! * **Vote.** Vote for a proposal in round `r` at most once, only if `r` is the current round,
//!   the block extends the QC's block, the node's execution/data-availability check passed, and
//!   either the QC is for `r - 1`, or there is a TC for `r - 1` and the QC is at least as high as
//!   every QC reported in that TC. Votes go to `L(r + 1)`.
//! * **Advance.** A QC or TC for round `r` moves everyone to `r + 1`.
//! * **Timeout.** When the round timer fires, stop voting in `r` and broadcast a timeout carrying
//!   the highest QC; more than 2/3 timeouts form a TC. Seeing more than 1/3 timeouts for the
//!   current round makes a node time out too (it cannot be the only one left waiting).
//! * **Commit.** When a block `B` of round `r` is certified and `B`'s parent is certified in round
//!   `r - 1`, the parent and all its ancestors are final.

use crate::types::*;
use alloy_primitives::B256;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Engine configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Chain id (in every signed message).
    pub chain_id: u64,
    /// Validators and weights.
    pub validators: ValidatorSet,
    /// This node's validator index, or `None` for an observer.
    pub me: Option<ValidatorIndex>,
    /// Genesis block hash.
    pub genesis: B256,
    /// Base round timeout in milliseconds (doubles on consecutive timeouts, up to 8x).
    pub base_timeout_ms: u64,
}

/// State a validator persists so a restart can neither vote twice in a round nor forget what it
/// locked on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(bound = "")]
pub struct Persisted<S: Scheme> {
    /// Last final block.
    pub committed: BlockInfo,
    /// Its certificate (genesis certificate at genesis).
    pub committed_qc: Qc<S>,
    /// Highest certificate known.
    pub high_qc: Qc<S>,
    /// Highest round voted or timed out in.
    pub last_voted: Round,
}

/// What the node must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action<S: Scheme> {
    /// Send to every validator.
    Broadcast(Message<S>),
    /// Send to one validator.
    SendTo(ValidatorIndex, Message<S>),
    /// Check the proposal's block (fetch all data, execute, verify the header), then call
    /// [`Engine::on_validated`].
    Validate(Proposal<S>),
    /// This node leads `round`: build a block on `parent` and call [`Engine::on_proposed`].
    Propose {
        /// Round.
        round: Round,
        /// Certificate of the parent block.
        qc: Qc<S>,
        /// Timeout certificate for `round - 1`, if any.
        tc: Option<Tc<S>>,
    },
    /// These blocks are final, oldest first; `proof` certifies the newest.
    Commit {
        /// Newly final blocks, oldest first.
        blocks: Vec<BlockInfo>,
        /// Finality proof material for the last block: its QC and its child's QC and hash.
        qc: Qc<S>,
        /// QC of the child that completed the two-chain.
        child_qc: Qc<S>,
    },
    /// A certified block's ancestor is unknown locally (e.g. an equivocating leader sent us a
    /// different proposal). Fetch the block (content-addressed, so any peer can serve it) and
    /// call [`Engine::on_block`].
    FetchBlock(B256),
    /// Call [`Engine::on_timeout`] with `round` after `after_ms`.
    ScheduleTimeout {
        /// Round.
        round: Round,
        /// Delay.
        after_ms: u64,
    },
}

#[derive(Debug)]
struct VoteSet<S: Scheme> {
    height: u64,
    sigs: BTreeMap<ValidatorIndex, S::Sig>,
    done: bool,
}

#[derive(Debug)]
struct TimeoutSet<S: Scheme> {
    entries: BTreeMap<ValidatorIndex, (Round, S::Sig)>,
    high_qc: Qc<S>,
    done: bool,
}

/// The consensus engine.
#[derive(Debug)]
pub struct Engine<S: Scheme> {
    cfg: Config,
    scheme: S,
    round: Round,
    last_voted: Round,
    high_qc: Qc<S>,
    /// TC that moved us into the current round.
    entry_tc: Option<Tc<S>>,
    /// Well-formed proposed blocks above the committed height, by hash (the block tree).
    blocks: HashMap<B256, BlockInfo>,
    /// QCs seen, by certified block.
    qcs: HashMap<B256, Qc<S>>,
    committed: BlockInfo,
    votes: BTreeMap<(Round, B256), VoteSet<S>>,
    timeouts: BTreeMap<Round, TimeoutSet<S>>,
    /// Proposals waiting for the node's validation, by block hash.
    pending: HashMap<B256, Proposal<S>>,
    timed_out: HashSet<Round>,
    proposed: HashSet<Round>,
    consecutive_timeouts: u32,
    /// Latest QC whose commit is blocked on a missing ancestor.
    blocked_commit: Option<Qc<S>>,
    requested: HashSet<B256>,
}

impl<S: Scheme> Engine<S> {
    /// Creates an engine at genesis.
    pub fn new(cfg: Config, scheme: S) -> Self {
        let genesis = BlockInfo { hash: cfg.genesis, parent: B256::ZERO, round: 0, height: 0 };
        let gqc = Qc::genesis(cfg.genesis);
        let mut blocks = HashMap::new();
        blocks.insert(genesis.hash, genesis.clone());
        let mut qcs = HashMap::new();
        qcs.insert(genesis.hash, gqc.clone());
        Self {
            cfg,
            scheme,
            round: 0,
            last_voted: 0,
            high_qc: gqc,
            entry_tc: None,
            blocks,
            qcs,
            committed: genesis,
            votes: BTreeMap::new(),
            timeouts: BTreeMap::new(),
            pending: HashMap::new(),
            timed_out: HashSet::new(),
            proposed: HashSet::new(),
            consecutive_timeouts: 0,
            blocked_commit: None,
            requested: HashSet::new(),
        }
    }

    /// Restores an engine from persisted state; call [`Engine::start`] next.
    pub fn resume(cfg: Config, scheme: S, p: Persisted<S>) -> Self {
        let mut e = Self::new(cfg, scheme);
        if p.committed.height > 0 {
            e.blocks.clear();
            e.qcs.clear();
            e.blocks.insert(p.committed.hash, p.committed.clone());
            e.qcs.insert(p.committed.hash, p.committed_qc.clone());
            e.committed = p.committed;
        }
        e.qcs.insert(p.high_qc.block, p.high_qc.clone());
        e.high_qc = p.high_qc;
        e.last_voted = p.last_voted;
        e.round = e.high_qc.round.max(e.last_voted);
        e
    }

    /// State to persist (write it before sending any vote or timeout this engine produced).
    pub fn persisted(&self) -> Persisted<S> {
        Persisted {
            committed: self.committed.clone(),
            committed_qc: self
                .qcs
                .get(&self.committed.hash)
                .cloned()
                .unwrap_or_else(|| Qc::genesis(self.cfg.genesis)),
            high_qc: self.high_qc.clone(),
            last_voted: self.last_voted,
        }
    }

    /// Current round.
    pub fn round(&self) -> Round {
        self.round
    }

    /// Last final block.
    pub fn committed(&self) -> &BlockInfo {
        &self.committed
    }

    /// Highest QC known.
    pub fn high_qc(&self) -> &Qc<S> {
        &self.high_qc
    }

    /// A block in this node's block tree.
    pub fn known_block(&self, hash: &B256) -> Option<BlockInfo> {
        self.blocks.get(hash).cloned()
    }

    /// Validator set.
    pub fn validators(&self) -> &ValidatorSet {
        &self.cfg.validators
    }

    fn leader(&self, round: Round) -> ValidatorIndex {
        self.cfg.validators.leader(round)
    }

    fn timeout_ms(&self) -> u64 {
        self.cfg.base_timeout_ms << self.consecutive_timeouts.min(3)
    }

    /// Starts the engine: enters round 1.
    pub fn start(&mut self) -> Vec<Action<S>> {
        let mut out = Vec::new();
        let next = self.round + 1;
        self.enter_round(next, None, &mut out);
        out
    }

    fn enter_round(&mut self, round: Round, tc: Option<Tc<S>>, out: &mut Vec<Action<S>>) {
        if round <= self.round {
            return;
        }
        self.round = round;
        self.entry_tc = tc;
        // Outstanding block fetches may have been lost (e.g. while offline): allow retries.
        self.requested.clear();
        if let Some(qc) = self.blocked_commit.clone() {
            self.try_commit(&qc, out);
        }
        out.push(Action::ScheduleTimeout { round, after_ms: self.timeout_ms() });
        self.prune();
        if self.cfg.me == Some(self.leader(round)) && self.proposed.insert(round) {
            out.push(Action::Propose {
                round,
                qc: self.high_qc.clone(),
                tc: self.entry_tc.clone(),
            });
        }
    }

    fn prune(&mut self) {
        let keep_from = self.round.saturating_sub(2);
        self.votes.retain(|(r, _), _| *r + 1 >= keep_from);
        self.timeouts.retain(|r, _| *r + 1 >= keep_from);
        self.timed_out.retain(|r| *r + 1 >= keep_from);
        self.proposed.retain(|r| *r + 1 >= keep_from);
        let committed_height = self.committed.height;
        self.pending.retain(|_, p| p.block.height > committed_height);
        let h = self.committed.height;
        self.blocks.retain(|_, b| b.height >= h);
        let blocks = &self.blocks;
        self.qcs.retain(|k, _| blocks.contains_key(k));
    }

    /// Records a QC: may raise the high QC, commit, and advance the round.
    fn process_qc(&mut self, qc: &Qc<S>, out: &mut Vec<Action<S>>) {
        self.qcs.entry(qc.block).or_insert_with(|| qc.clone());
        if qc.round > self.high_qc.round {
            self.high_qc = qc.clone();
        }
        self.try_commit(qc, out);
        if qc.round >= self.round {
            self.consecutive_timeouts = 0;
            self.enter_round(qc.round + 1, None, out);
        }
    }

    /// Two-chain commit: `qc` certifies B; if B's parent P was certified in B.round - 1, P is final.
    fn try_commit(&mut self, qc: &Qc<S>, out: &mut Vec<Action<S>>) {
        let Some(b) = self.blocks.get(&qc.block).cloned() else {
            self.block_on(qc, qc.block, out);
            return;
        };
        let Some(p) = self.blocks.get(&b.parent).cloned() else {
            if b.height > self.committed.height + 1 {
                self.block_on(qc, b.parent, out);
            }
            return;
        };
        if p.round + 1 != b.round || p.height <= self.committed.height {
            return;
        }
        let Some(pqc) = self.qcs.get(&p.hash).cloned() else { return };
        // Collect P and its ancestors down to the last committed block.
        let mut chain = vec![p.clone()];
        let mut cur = p.clone();
        while cur.parent != self.committed.hash {
            match self.blocks.get(&cur.parent) {
                Some(parent) if parent.height > self.committed.height => {
                    cur = parent.clone();
                    chain.push(cur.clone());
                }
                Some(_) => return, // fork below the committed block: impossible with f < n/3
                None => {
                    let missing = cur.parent;
                    self.block_on(qc, missing, out);
                    return;
                }
            }
        }
        chain.reverse();
        self.committed = p;
        if self.blocked_commit.as_ref().is_some_and(|q| q.round <= qc.round) {
            self.blocked_commit = None;
        }
        out.push(Action::Commit { blocks: chain, qc: pqc, child_qc: qc.clone() });
    }

    fn block_on(&mut self, qc: &Qc<S>, missing: B256, out: &mut Vec<Action<S>>) {
        if self.blocked_commit.as_ref().is_none_or(|q| q.round < qc.round) {
            self.blocked_commit = Some(qc.clone());
        }
        if self.requested.insert(missing) {
            out.push(Action::FetchBlock(missing));
        }
    }

    /// A block fetched after [`Action::FetchBlock`] (the node verified it against its hash).
    pub fn on_block(&mut self, block: BlockInfo) -> Vec<Action<S>> {
        let mut out = Vec::new();
        self.requested.remove(&block.hash);
        if block.height > self.committed.height {
            self.blocks.insert(block.hash, block);
        }
        if let Some(qc) = self.blocked_commit.clone() {
            self.try_commit(&qc, &mut out);
        }
        out
    }

    fn process_tc(&mut self, tc: &Tc<S>, out: &mut Vec<Action<S>>) {
        self.process_qc(&tc.high_qc.clone(), out);
        if tc.round >= self.round {
            self.enter_round(tc.round + 1, Some(tc.clone()), out);
        }
    }

    /// Handles a message from the network (signatures are verified here).
    pub fn on_message(&mut self, msg: Message<S>) -> Vec<Action<S>> {
        let mut out = Vec::new();
        match msg {
            Message::Proposal(p) => self.on_proposal(p, &mut out),
            Message::Vote(v) => self.on_vote(v, &mut out),
            Message::Timeout(t) => self.on_timeout_msg(t, &mut out),
        }
        out
    }

    fn verify_qc(&self, qc: &Qc<S>) -> bool {
        qc.verify(&self.scheme, &self.cfg.validators, self.cfg.chain_id, &self.cfg.genesis)
    }

    fn verify_tc(&self, tc: &Tc<S>) -> bool {
        tc.verify(&self.scheme, &self.cfg.validators, self.cfg.chain_id, &self.cfg.genesis)
    }

    fn on_proposal(&mut self, p: Proposal<S>, out: &mut Vec<Action<S>>) {
        let b = &p.block;
        if p.proposer != self.leader(b.round)
            || b.round == 0
            || b.parent != p.qc.block
            || b.height != p.qc.height + 1
            || b.round <= p.qc.round
            || !self.scheme.verify(
                p.proposer,
                &proposal_msg(self.cfg.chain_id, b.round, &b.hash, &p.payload),
                &p.sig,
            )
            || !self.verify_qc(&p.qc)
        {
            return;
        }
        if let Some(tc) = &p.tc
            && (tc.round + 1 != b.round || !self.verify_tc(tc))
        {
            return;
        }
        // Remember the block's place in the tree even if we never validate or vote for it:
        // commits walk parent links through every certified block.
        if !self.blocks.contains_key(&b.hash) && b.height > self.committed.height {
            self.blocks.insert(b.hash, b.clone());
        }
        // Certificates are useful even if we do not vote.
        self.process_qc(&p.qc.clone(), out);
        if let Some(tc) = &p.tc {
            self.process_tc(&tc.clone(), out);
        }
        if b.round != self.round || b.round <= self.last_voted || self.me().is_none() {
            return;
        }
        if !self.safe_to_vote(&p) {
            return;
        }
        if self.pending.contains_key(&b.hash) {
            return;
        }
        self.pending.insert(b.hash, p.clone());
        out.push(Action::Validate(p));
    }

    fn safe_to_vote(&self, p: &Proposal<S>) -> bool {
        let r = p.block.round;
        if p.qc.round + 1 == r {
            return true;
        }
        match &p.tc {
            Some(tc) => tc.round + 1 == r && p.qc.round >= tc.max_qc_round(),
            None => false,
        }
    }

    fn me(&self) -> Option<ValidatorIndex> {
        self.cfg.me
    }

    /// Result of the node's check of a proposed block.
    pub fn on_validated(&mut self, block: B256, valid: bool) -> Vec<Action<S>> {
        let mut out = Vec::new();
        let Some(p) = self.pending.remove(&block) else { return out };
        if !valid {
            return out;
        }
        let r = p.block.round;
        // Conditions may have changed while validating (timeout, newer round).
        if r != self.round || r <= self.last_voted || !self.safe_to_vote(&p) {
            return out;
        }
        let Some(me) = self.me() else { return out };
        self.last_voted = r;
        let vote = Vote {
            round: r,
            block: p.block.hash,
            height: p.block.height,
            signer: me,
            sig: self.scheme.sign(me, &vote_msg(self.cfg.chain_id, r, &p.block.hash)),
        };
        let next = self.leader(r + 1);
        if next == me {
            self.on_vote(vote, &mut out);
        } else {
            out.push(Action::SendTo(next, Message::Vote(vote)));
        }
        out
    }

    /// The node built the block this node was asked to propose.
    pub fn on_proposed(
        &mut self,
        block: BlockInfo,
        qc: Qc<S>,
        tc: Option<Tc<S>>,
        payload: Vec<u8>,
    ) -> Vec<Action<S>> {
        let mut out = Vec::new();
        let Some(me) = self.me() else { return out };
        if block.round != self.round || self.leader(block.round) != me {
            return out;
        }
        let sig = self
            .scheme
            .sign(me, &proposal_msg(self.cfg.chain_id, block.round, &block.hash, &payload));
        let p = Proposal { block, qc, tc, proposer: me, payload, sig };
        out.push(Action::Broadcast(Message::Proposal(p.clone())));
        // Our own block needs no validation: we just built and executed it.
        self.on_proposal(p.clone(), &mut out);
        if let Some(pos) = out
            .iter()
            .position(|a| matches!(a, Action::Validate(x) if x.block.hash == p.block.hash))
        {
            out.remove(pos);
            out.extend(self.on_validated(p.block.hash, true));
        }
        out
    }

    fn on_vote(&mut self, v: Vote<S>, out: &mut Vec<Action<S>>) {
        if self.me() != Some(self.leader(v.round + 1))
            || (v.signer as usize) >= self.cfg.validators.len()
            || v.round + 2 < self.round
            || !self.scheme.verify(
                v.signer,
                &vote_msg(self.cfg.chain_id, v.round, &v.block),
                &v.sig,
            )
        {
            return;
        }
        let set = self.votes.entry((v.round, v.block)).or_insert_with(|| VoteSet {
            height: v.height,
            sigs: BTreeMap::new(),
            done: false,
        });
        if set.done || set.height != v.height {
            return;
        }
        set.sigs.insert(v.signer, v.sig);
        let signers: Vec<ValidatorIndex> = set.sigs.keys().copied().collect();
        if !self.cfg.validators.is_quorum(self.cfg.validators.weight_of(&signers)) {
            return;
        }
        let sigs: Vec<S::Sig> = set.sigs.values().cloned().collect();
        let Some(agg) = self.scheme.aggregate(&sigs) else { return };
        set.done = true;
        let qc = Qc {
            round: v.round,
            block: v.block,
            height: set.height,
            signers: bitmap::encode(&signers, self.cfg.validators.len()),
            sig: Some(agg),
        };
        self.process_qc(&qc, out);
    }

    /// The round timer fired.
    pub fn on_timeout(&mut self, round: Round) -> Vec<Action<S>> {
        let mut out = Vec::new();
        if round != self.round {
            return out;
        }
        self.local_timeout(&mut out);
        out
    }

    fn local_timeout(&mut self, out: &mut Vec<Action<S>>) {
        let round = self.round;
        self.last_voted = self.last_voted.max(round);
        self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
        // Re-arm: if no TC forms, the timeout is re-broadcast.
        out.push(Action::ScheduleTimeout { round, after_ms: self.timeout_ms() });
        let Some(me) = self.me() else { return };
        self.timed_out.insert(round);
        let t = Timeout {
            round,
            high_qc: self.high_qc.clone(),
            signer: me,
            sig: self.scheme.sign(me, &timeout_msg(self.cfg.chain_id, round, self.high_qc.round)),
            last_tc: self.entry_tc.clone().map(Box::new),
        };
        out.push(Action::Broadcast(Message::Timeout(t.clone())));
        self.on_timeout_msg(t, out);
    }

    fn on_timeout_msg(&mut self, t: Timeout<S>, out: &mut Vec<Action<S>>) {
        if (t.signer as usize) >= self.cfg.validators.len()
            || t.round + 2 < self.round
            || t.high_qc.round >= t.round
            || !self.scheme.verify(
                t.signer,
                &timeout_msg(self.cfg.chain_id, t.round, t.high_qc.round),
                &t.sig,
            )
            || !self.verify_qc(&t.high_qc)
        {
            return;
        }
        if let Some(tc) = &t.last_tc
            && tc.round + 1 == t.round
            && tc.round >= self.round
            && self.verify_tc(tc)
        {
            self.process_tc(&tc.clone(), out);
        }
        self.process_qc(&t.high_qc.clone(), out);
        let set = self.timeouts.entry(t.round).or_insert_with(|| TimeoutSet {
            entries: BTreeMap::new(),
            high_qc: t.high_qc.clone(),
            done: false,
        });
        if set.done {
            return;
        }
        if t.high_qc.round > set.high_qc.round {
            set.high_qc = t.high_qc.clone();
        }
        set.entries.insert(t.signer, (t.high_qc.round, t.sig));
        let signers: Vec<ValidatorIndex> = set.entries.keys().copied().collect();
        let weight = self.cfg.validators.weight_of(&signers);
        if self.cfg.validators.is_quorum(weight) {
            set.done = true;
            let tc = Tc {
                round: t.round,
                high_qc: set.high_qc.clone(),
                entries: set.entries.iter().map(|(i, (r, s))| (*i, *r, s.clone())).collect(),
            };
            self.process_tc(&tc, out);
        } else if self.cfg.validators.is_one_third(weight)
            && t.round == self.round
            && !self.timed_out.contains(&t.round)
        {
            // Some honest validator gave up on this round: join so a TC can form.
            self.local_timeout(out);
        }
    }

    /// The node imported final blocks through another path (e.g. syncing from IPFS after being
    /// offline). Moves the committed pointer forward and adopts the certificate.
    pub fn on_external_commit(&mut self, block: BlockInfo, qc: Qc<S>) -> Vec<Action<S>> {
        let mut out = Vec::new();
        if block.height <= self.committed.height || !self.verify_qc(&qc) || qc.block != block.hash {
            return out;
        }
        self.blocks.insert(block.hash, block.clone());
        self.committed = block;
        self.process_qc(&qc, &mut out);
        out
    }
}
