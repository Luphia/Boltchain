//! Boltchain consensus: two-chain HotStuff with Jolteon's voting and timeout rules.
//!
//! See [`engine`] for the rules. The engine is a pure state machine generic over a signature
//! [`Scheme`]: [`BlsScheme`] (BLS12-381, production) or [`MockScheme`] (simulation).

pub mod engine;
pub mod types;

pub use engine::{Action, Config, Engine, Persisted};
pub use types::*;

use alloy_primitives::{B256, keccak256};
use bolt_primitives::bls::{self, BlsPublicKey, BlsSecretKey, BlsSignature};
use std::sync::Arc;

/// BLS12-381 signatures; QCs carry one aggregate signature plus a signer bitmap.
#[derive(Clone)]
pub struct BlsScheme {
    keys: Arc<Vec<BlsPublicKey>>,
    secret: Option<Arc<BlsSecretKey>>,
}

impl std::fmt::Debug for BlsScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlsScheme").field("validators", &self.keys.len()).finish()
    }
}

impl BlsScheme {
    /// `keys[i]` is validator `i`'s public key (PoP already checked); `secret` is this node's key.
    pub fn new(keys: Vec<BlsPublicKey>, secret: Option<BlsSecretKey>) -> Self {
        Self { keys: Arc::new(keys), secret: secret.map(Arc::new) }
    }
}

impl Scheme for BlsScheme {
    type Sig = BlsSignature;
    type Agg = BlsSignature;

    fn sign(&self, _me: ValidatorIndex, msg: &[u8]) -> Self::Sig {
        self.secret.as_ref().expect("signing requires this node's validator key").sign(msg)
    }

    fn verify(&self, signer: ValidatorIndex, msg: &[u8], sig: &Self::Sig) -> bool {
        self.keys.get(signer as usize).is_some_and(|pk| bls::verify(pk, msg, sig))
    }

    fn aggregate(&self, sigs: &[Self::Sig]) -> Option<Self::Agg> {
        bls::aggregate(sigs)
    }

    fn verify_aggregate(&self, signers: &[ValidatorIndex], msg: &[u8], agg: &Self::Agg) -> bool {
        let Some(pks) =
            signers.iter().map(|i| self.keys.get(*i as usize).copied()).collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        !pks.is_empty() && bls::fast_aggregate_verify(&pks, msg, agg)
    }
}

/// Keyed-hash "signatures" for the simulator: cheap, deterministic, and unforgeable within the
/// simulation because byzantine behaviours only ever sign as themselves.
#[derive(Clone, Debug, Default)]
pub struct MockScheme;

fn mock_sig(signer: ValidatorIndex, msg: &[u8]) -> B256 {
    keccak256([&signer.to_be_bytes()[..], msg].concat())
}

impl Scheme for MockScheme {
    type Sig = B256;
    type Agg = Vec<B256>;

    fn sign(&self, me: ValidatorIndex, msg: &[u8]) -> Self::Sig {
        mock_sig(me, msg)
    }

    fn verify(&self, signer: ValidatorIndex, msg: &[u8], sig: &Self::Sig) -> bool {
        mock_sig(signer, msg) == *sig
    }

    fn aggregate(&self, sigs: &[Self::Sig]) -> Option<Self::Agg> {
        Some(sigs.to_vec())
    }

    fn verify_aggregate(&self, signers: &[ValidatorIndex], msg: &[u8], agg: &Self::Agg) -> bool {
        signers.len() == agg.len() && signers.iter().zip(agg).all(|(s, a)| mock_sig(*s, msg) == *a)
    }
}

/// Encodes a message for the wire (dag-cbor).
pub fn encode<S: Scheme>(m: &Message<S>) -> Vec<u8> {
    serde_ipld_dagcbor::to_vec(m).expect("consensus message serializes")
}

/// Decodes a wire message.
pub fn decode<S: Scheme>(b: &[u8]) -> Option<Message<S>> {
    serde_ipld_dagcbor::from_slice(b).ok()
}

#[cfg(test)]
mod tests;
