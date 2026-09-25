//! Consensus messages, certificates and the validator set.

use alloy_primitives::{B256, keccak256};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fmt::Debug;

/// Consensus round (view).
pub type Round = u64;
/// Index of a validator (committee seat) in the validator set.
pub type ValidatorIndex = u16;

/// Signature scheme used by the engine. Production uses BLS12-381 ([`crate::bls::BlsScheme`]);
/// the simulator uses a cheap keyed hash so it can run thousands of scenarios.
pub trait Scheme: Clone + Debug + Send + Sync + 'static {
    /// Individual signature.
    type Sig: Clone + Debug + PartialEq + Eq + Serialize + DeserializeOwned + Send + Sync;
    /// Aggregate signature (all signers over one message).
    type Agg: Clone + Debug + PartialEq + Eq + Serialize + DeserializeOwned + Send + Sync;

    /// Signs as validator `me` (the scheme holds that validator's secret).
    fn sign(&self, me: ValidatorIndex, msg: &[u8]) -> Self::Sig;
    /// Verifies one signature.
    fn verify(&self, signer: ValidatorIndex, msg: &[u8], sig: &Self::Sig) -> bool;
    /// Aggregates signatures over the same message.
    fn aggregate(&self, sigs: &[Self::Sig]) -> Option<Self::Agg>;
    /// Verifies an aggregate by `signers` over `msg`.
    fn verify_aggregate(&self, signers: &[ValidatorIndex], msg: &[u8], agg: &Self::Agg) -> bool;
}

/// Validator set with voting weights (committee seats).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorSet {
    /// Weight of each validator.
    pub weights: Vec<u64>,
    /// Leader order: the validator holding each seat. Empty means round robin over validators.
    #[serde(default)]
    pub seats: Vec<ValidatorIndex>,
}

impl ValidatorSet {
    /// `n` validators of weight 1.
    pub fn equal(n: usize) -> Self {
        Self { weights: vec![1; n], seats: Vec::new() }
    }

    /// From seat assignments: weight of each validator = its number of seats; leaders rotate over
    /// the seats in order.
    pub fn from_seats(n: usize, seats: Vec<ValidatorIndex>) -> Self {
        let mut weights = vec![0; n];
        for s in &seats {
            weights[*s as usize] += 1;
        }
        Self { weights, seats }
    }

    /// Number of validators.
    pub fn len(&self) -> usize {
        self.weights.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    /// Total weight.
    pub fn total(&self) -> u64 {
        self.weights.iter().sum()
    }

    /// Weight of a set of signers (unknown indices count zero).
    pub fn weight_of(&self, signers: &[ValidatorIndex]) -> u64 {
        signers.iter().map(|i| self.weights.get(*i as usize).copied().unwrap_or(0)).sum()
    }

    /// More than two thirds of the total weight.
    pub fn is_quorum(&self, weight: u64) -> bool {
        weight * 3 > self.total() * 2
    }

    /// More than one third: at least one honest validator is included.
    pub fn is_one_third(&self, weight: u64) -> bool {
        weight * 3 > self.total()
    }

    /// Leader of `round`: rotates over the seats (so proportionally to weight).
    pub fn leader(&self, round: Round) -> ValidatorIndex {
        if self.seats.is_empty() {
            (round % self.len() as u64) as ValidatorIndex
        } else {
            self.seats[(round % self.seats.len() as u64) as usize]
        }
    }
}

/// Signer bitmap helpers.
pub mod bitmap {
    use super::ValidatorIndex;

    /// Bitmap with bit `i` set for each signer.
    pub fn encode(signers: &[ValidatorIndex], n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n.div_ceil(8)];
        for &i in signers {
            if (i as usize) < n {
                b[i as usize / 8] |= 1 << (i % 8);
            }
        }
        b
    }

    /// Signers in ascending order.
    pub fn decode(b: &[u8]) -> Vec<ValidatorIndex> {
        let mut out = Vec::new();
        for (byte_i, byte) in b.iter().enumerate() {
            for bit in 0..8 {
                if byte & (1 << bit) != 0 {
                    out.push((byte_i * 8 + bit) as ValidatorIndex);
                }
            }
        }
        out
    }
}

fn domain_msg(tag: &[u8], chain_id: u64, epoch: u64, round: Round, extra: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(tag.len() + 24 + extra.len());
    m.extend_from_slice(tag);
    m.extend_from_slice(&chain_id.to_be_bytes());
    m.extend_from_slice(&epoch.to_be_bytes());
    m.extend_from_slice(&round.to_be_bytes());
    m.extend_from_slice(extra);
    m
}

/// Bytes signed by a vote. `ConsensusRegistry` parses this layout for equivocation evidence.
pub fn vote_msg(chain_id: u64, epoch: u64, round: Round, block: &B256) -> Vec<u8> {
    domain_msg(b"boltchain/vote/v2", chain_id, epoch, round, block.as_slice())
}

/// Bytes signed by a timeout.
pub fn timeout_msg(chain_id: u64, epoch: u64, round: Round, high_qc_round: Round) -> Vec<u8> {
    domain_msg(b"boltchain/timeout/v2", chain_id, epoch, round, &high_qc_round.to_be_bytes())
}

/// Bytes signed by a proposal. `ConsensusRegistry` parses this layout for equivocation evidence.
pub fn proposal_msg(
    chain_id: u64,
    epoch: u64,
    round: Round,
    block: &B256,
    payload: &[u8],
) -> Vec<u8> {
    domain_msg(
        b"boltchain/proposal/v2",
        chain_id,
        epoch,
        round,
        &[block.as_slice(), keccak256(payload).as_slice()].concat(),
    )
}

/// Bytes a proposer signs for the RANDAO reveal at `height` (ADR 0006 §6).
pub fn randao_msg(chain_id: u64, height: u64) -> Vec<u8> {
    let mut m = b"boltchain/randao/v1".to_vec();
    m.extend_from_slice(&chain_id.to_be_bytes());
    m.extend_from_slice(&height.to_be_bytes());
    m
}

/// Hash of the nil block proposed in `round` on `parent` when the epoch's last block is reached
/// (ADR 0006 §3). Nil blocks carry no data and never enter the ledger.
pub fn nil_hash(epoch: u64, parent: &B256, round: Round) -> B256 {
    let mut m = b"boltchain/nil/v1".to_vec();
    m.extend_from_slice(&epoch.to_be_bytes());
    m.extend_from_slice(parent.as_slice());
    m.extend_from_slice(&round.to_be_bytes());
    keccak256(m)
}

/// Quorum certificate: more than 2/3 of the weight voted for `block` in `round`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Qc<S: Scheme> {
    /// Epoch.
    pub epoch: u64,
    /// Round of the certified block.
    pub round: Round,
    /// Certified block hash.
    pub block: B256,
    /// Height of the certified block.
    pub height: u64,
    /// Signer bitmap.
    #[serde(with = "serde_bytes")]
    pub signers: Vec<u8>,
    /// Aggregate signature; `None` only for the genesis certificate.
    pub sig: Option<S::Agg>,
}

impl<S: Scheme> Qc<S> {
    /// The certificate an epoch starts from: its anchor block (genesis for epoch 0, otherwise the
    /// previous epoch's last block, proven final by an [`EpochProof`]), round 0, unsigned.
    pub fn anchor(epoch: u64, block: B256, height: u64) -> Self {
        Self { epoch, round: 0, block, height, signers: Vec::new(), sig: None }
    }

    /// The genesis certificate (anchor of epoch 0).
    pub fn genesis(genesis_hash: B256) -> Self {
        Self::anchor(0, genesis_hash, 0)
    }

    /// Whether this is an anchor certificate.
    pub fn is_anchor(&self) -> bool {
        self.round == 0 && self.sig.is_none()
    }

    /// Whether this is the genesis certificate.
    pub fn is_genesis(&self) -> bool {
        self.is_anchor() && self.epoch == 0 && self.height == 0
    }

    /// Checks signatures and weight for `epoch`. An anchor certificate is valid only for `anchor`.
    pub fn verify(
        &self,
        scheme: &S,
        set: &ValidatorSet,
        chain_id: u64,
        epoch: u64,
        anchor: &B256,
    ) -> bool {
        if self.epoch != epoch {
            return false;
        }
        if self.is_anchor() {
            return self.block == *anchor;
        }
        let signers = bitmap::decode(&self.signers);
        let Some(sig) = &self.sig else { return false };
        self.round > 0
            && set.is_quorum(set.weight_of(&signers))
            && signers.iter().all(|i| (*i as usize) < set.len())
            && scheme.verify_aggregate(
                &signers,
                &vote_msg(chain_id, self.epoch, self.round, &self.block),
                sig,
            )
    }
}

/// Timeout certificate: more than 2/3 of the weight gave up on `round`. Carries the highest QC
/// any of them had, which the next leader must extend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Tc<S: Scheme> {
    /// Epoch.
    pub epoch: u64,
    /// Round that timed out.
    pub round: Round,
    /// Highest QC among the timeouts.
    pub high_qc: Qc<S>,
    /// (signer, round of that signer's high QC, signature over `timeout_msg`).
    pub entries: Vec<(ValidatorIndex, Round, S::Sig)>,
}

impl<S: Scheme> Tc<S> {
    /// Largest high-QC round reported.
    pub fn max_qc_round(&self) -> Round {
        self.entries.iter().map(|e| e.1).max().unwrap_or(0)
    }

    /// Checks every entry, the total weight, and that `high_qc` is the maximum reported.
    pub fn verify(
        &self,
        scheme: &S,
        set: &ValidatorSet,
        chain_id: u64,
        epoch: u64,
        anchor: &B256,
    ) -> bool {
        let mut signers: Vec<ValidatorIndex> = self.entries.iter().map(|e| e.0).collect();
        signers.sort_unstable();
        signers.dedup();
        self.epoch == epoch
            && signers.len() == self.entries.len()
            && set.is_quorum(set.weight_of(&signers))
            && signers.iter().all(|i| (*i as usize) < set.len())
            && self.entries.iter().all(|(i, qr, sig)| {
                *qr < self.round
                    && scheme.verify(*i, &timeout_msg(chain_id, epoch, self.round, *qr), sig)
            })
            && self.high_qc.round == self.max_qc_round()
            && self.high_qc.verify(scheme, set, chain_id, epoch, anchor)
    }
}

/// Consensus view of a block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockInfo {
    /// Block hash.
    pub hash: B256,
    /// Parent hash.
    pub parent: B256,
    /// Round it was proposed in.
    pub round: Round,
    /// Height.
    pub height: u64,
}

/// A leader's proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Proposal<S: Scheme> {
    /// Epoch.
    pub epoch: u64,
    /// The proposed block.
    pub block: BlockInfo,
    /// Certificate for the parent.
    pub qc: Qc<S>,
    /// Present when the previous round timed out.
    pub tc: Option<Tc<S>>,
    /// Proposer (must be the round's leader).
    pub proposer: ValidatorIndex,
    /// Opaque block data for the node (header, envelope, body chunks or their CIDs).
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Proposer's signature over `proposal_msg`.
    pub sig: S::Sig,
}

/// A vote, sent to the next round's leader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Vote<S: Scheme> {
    /// Epoch.
    pub epoch: u64,
    /// Round.
    pub round: Round,
    /// Block voted for.
    pub block: B256,
    /// Its height.
    pub height: u64,
    /// Voter.
    pub signer: ValidatorIndex,
    /// Signature over `vote_msg`.
    pub sig: S::Sig,
}

/// A timeout, broadcast to everyone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Timeout<S: Scheme> {
    /// Epoch.
    pub epoch: u64,
    /// Round given up on.
    pub round: Round,
    /// Sender's highest QC.
    pub high_qc: Qc<S>,
    /// Sender.
    pub signer: ValidatorIndex,
    /// Signature over `timeout_msg(round, high_qc.round)`.
    pub sig: S::Sig,
    /// The TC that moved the sender into `round`, if any. Self-certifying (not covered by
    /// `sig`); lets validators that missed it catch up.
    pub last_tc: Option<Box<Tc<S>>>,
}

/// Any consensus message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
#[allow(clippy::large_enum_variant)] // short-lived wire messages; boxing buys nothing
pub enum Message<S: Scheme> {
    /// Proposal.
    Proposal(Proposal<S>),
    /// Vote.
    Vote(Vote<S>),
    /// Timeout.
    Timeout(Timeout<S>),
}

impl<S: Scheme> Message<S> {
    /// Epoch the message belongs to.
    pub fn epoch(&self) -> u64 {
        match self {
            Message::Proposal(p) => p.epoch,
            Message::Vote(v) => v.epoch,
            Message::Timeout(t) => t.epoch,
        }
    }
}

/// Proof that a block is final (two-chain rule, ADR 0005 / 0006 §4).
///
/// `qc` certifies block Q in round r and `child_qc` certifies Q's child C in round r + 1, so Q and
/// its ancestors are final. Usually Q is the proven block itself and C a real block, bound to Q
/// by its header (`child_header`, whose hash `child_qc` signs and whose `parent_hash` is Q).
///
/// At an epoch's end Q and/or C may be nil blocks descending from the epoch's last block
/// `committed` (height `committed_height`): then `nil_rounds` lists the rounds of the nil chain
/// from `committed` down to C, which the verifier rebuilds with [`nil_hash`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct CommitProof<S: Scheme> {
    /// QC of Q (round r).
    pub qc: Qc<S>,
    /// QC of C (round r + 1).
    pub child_qc: Qc<S>,
    /// RLP of C's header when C is a real block (empty otherwise).
    #[serde(with = "serde_bytes")]
    pub child_header: Vec<u8>,
    /// Rounds of the nil chain from `committed` to C (empty when C is real).
    pub nil_rounds: Vec<Round>,
    /// The real block this proves final.
    pub committed: B256,
    /// Its height.
    pub committed_height: u64,
}

impl<S: Scheme> CommitProof<S> {
    /// Checks both certificates (under epoch `qc.epoch`'s committee `set`) and the chain linking
    /// `committed`, Q and C. Returns the proven block.
    pub fn verify(
        &self,
        scheme: &S,
        set: &ValidatorSet,
        chain_id: u64,
        anchor: &B256,
    ) -> Option<B256> {
        let epoch = self.qc.epoch;
        let certs_ok = self.child_qc.round == self.qc.round + 1
            && self.child_qc.height == self.qc.height + 1
            && self.child_qc.epoch == epoch
            && !self.qc.is_anchor()
            && self.qc.verify(scheme, set, chain_id, epoch, anchor)
            && self.child_qc.verify(scheme, set, chain_id, epoch, anchor);
        if !certs_ok {
            return None;
        }
        if self.nil_rounds.is_empty() {
            let child = <alloy_consensus::Header as alloy_rlp::Decodable>::decode(
                &mut self.child_header.as_slice(),
            )
            .ok()?;
            let ok = keccak256(&self.child_header) == self.child_qc.block
                && child.parent_hash == self.qc.block
                && child.number == self.qc.height + 1
                && self.committed == self.qc.block
                && self.committed_height == self.qc.height;
            return ok.then_some(self.committed);
        }
        let mut prev = self.committed;
        let mut cur = self.committed;
        for r in &self.nil_rounds {
            prev = cur;
            cur = nil_hash(epoch, &prev, *r);
        }
        let ok = cur == self.child_qc.block
            && prev == self.qc.block
            && self.child_qc.round == *self.nil_rounds.last()?
            && self.child_qc.height == self.committed_height + self.nil_rounds.len() as u64;
        ok.then_some(self.committed)
    }
}

/// The parent certificate a block's envelope carries (and `parent_beacon_block_root` hashes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub enum Cert<S: Scheme> {
    /// QC of the parent, from the same epoch.
    Qc(Qc<S>),
    /// First block of an epoch: proof that the parent (the previous epoch's last block) is final.
    Epoch(CommitProof<S>),
}

impl<S: Scheme> Cert<S> {
    /// (epoch, signer bitmap) of the votes this certificate records.
    pub fn votes(&self) -> (u64, &[u8]) {
        match self {
            Cert::Qc(q) => (q.epoch, &q.signers),
            Cert::Epoch(p) => (p.qc.epoch, &p.qc.signers),
        }
    }
}
