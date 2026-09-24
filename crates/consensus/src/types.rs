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
}

impl ValidatorSet {
    /// `n` validators of weight 1.
    pub fn equal(n: usize) -> Self {
        Self { weights: vec![1; n] }
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

    /// Leader of `round` (round robin; stake-weighted VRF sampling replaces this in M4).
    pub fn leader(&self, round: Round) -> ValidatorIndex {
        (round % self.len() as u64) as ValidatorIndex
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

fn domain_msg(tag: &[u8], chain_id: u64, round: Round, extra: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(tag.len() + 16 + extra.len());
    m.extend_from_slice(tag);
    m.extend_from_slice(&chain_id.to_be_bytes());
    m.extend_from_slice(&round.to_be_bytes());
    m.extend_from_slice(extra);
    m
}

/// Bytes signed by a vote.
pub fn vote_msg(chain_id: u64, round: Round, block: &B256) -> Vec<u8> {
    domain_msg(b"boltchain/vote/v1", chain_id, round, block.as_slice())
}

/// Bytes signed by a timeout.
pub fn timeout_msg(chain_id: u64, round: Round, high_qc_round: Round) -> Vec<u8> {
    domain_msg(b"boltchain/timeout/v1", chain_id, round, &high_qc_round.to_be_bytes())
}

/// Bytes signed by a proposal.
pub fn proposal_msg(chain_id: u64, round: Round, block: &B256, payload: &[u8]) -> Vec<u8> {
    domain_msg(
        b"boltchain/proposal/v1",
        chain_id,
        round,
        &[block.as_slice(), keccak256(payload).as_slice()].concat(),
    )
}

/// Quorum certificate: more than 2/3 of the weight voted for `block` in `round`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Qc<S: Scheme> {
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
    /// The certificate every node starts from: genesis, round 0.
    pub fn genesis(genesis_hash: B256) -> Self {
        Self { round: 0, block: genesis_hash, height: 0, signers: Vec::new(), sig: None }
    }

    /// Whether this is the genesis certificate.
    pub fn is_genesis(&self) -> bool {
        self.round == 0 && self.sig.is_none()
    }

    /// Checks signatures and weight. The genesis certificate is valid only for `genesis`.
    pub fn verify(&self, scheme: &S, set: &ValidatorSet, chain_id: u64, genesis: &B256) -> bool {
        if self.is_genesis() {
            return self.block == *genesis && self.height == 0;
        }
        let signers = bitmap::decode(&self.signers);
        let Some(sig) = &self.sig else { return false };
        set.is_quorum(set.weight_of(&signers))
            && signers.iter().all(|i| (*i as usize) < set.len())
            && scheme.verify_aggregate(&signers, &vote_msg(chain_id, self.round, &self.block), sig)
    }
}

/// Timeout certificate: more than 2/3 of the weight gave up on `round`. Carries the highest QC
/// any of them had, which the next leader must extend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Tc<S: Scheme> {
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
    pub fn verify(&self, scheme: &S, set: &ValidatorSet, chain_id: u64, genesis: &B256) -> bool {
        let mut signers: Vec<ValidatorIndex> = self.entries.iter().map(|e| e.0).collect();
        signers.sort_unstable();
        signers.dedup();
        signers.len() == self.entries.len()
            && set.is_quorum(set.weight_of(&signers))
            && self.entries.iter().all(|(i, qr, sig)| {
                *qr < self.round && scheme.verify(*i, &timeout_msg(chain_id, self.round, *qr), sig)
            })
            && self.high_qc.round == self.max_qc_round()
            && self.high_qc.verify(scheme, set, chain_id, genesis)
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
pub enum Message<S: Scheme> {
    /// Proposal.
    Proposal(Proposal<S>),
    /// Vote.
    Vote(Vote<S>),
    /// Timeout.
    Timeout(Timeout<S>),
}

/// Proof that a block is final (two-chain rule): its QC for round `r`, plus the header and QC of
/// a child proposed in round `r + 1`. The child header is what binds the child to the parent: the
/// child's hash (signed in `child_qc`) covers its `parent_hash`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct CommitProof<S: Scheme> {
    /// QC of the committed block (round r).
    pub qc: Qc<S>,
    /// QC of the child (round r + 1).
    pub child_qc: Qc<S>,
    /// RLP of the child header.
    #[serde(with = "serde_bytes")]
    pub child_header: Vec<u8>,
}

impl<S: Scheme> CommitProof<S> {
    /// Checks both certificates and that the child really extends the committed block.
    pub fn verify(&self, scheme: &S, set: &ValidatorSet, chain_id: u64, genesis: &B256) -> bool {
        let Ok(child) = <alloy_consensus::Header as alloy_rlp::Decodable>::decode(
            &mut self.child_header.as_slice(),
        ) else {
            return false;
        };
        keccak256(&self.child_header) == self.child_qc.block
            && child.parent_hash == self.qc.block
            && child.number == self.qc.height + 1
            && self.child_qc.height == child.number
            && self.child_qc.round == self.qc.round + 1
            && self.qc.verify(scheme, set, chain_id, genesis)
            && self.child_qc.verify(scheme, set, chain_id, genesis)
    }
}
