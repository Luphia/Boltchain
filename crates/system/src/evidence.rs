//! Equivocation evidence transactions (ADR 0006 §9).

use crate::abi::IConsensusRegistry;
use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use bolt_primitives::bls::{BlsPublicKey, BlsSignature, pubkey_point, signature_point};

/// Calldata for `ConsensusRegistry.submitEvidence`: validator `id` signed both `(msg_a, sig_a)`
/// and `(msg_b, sig_b)`. `None` if a key or signature does not decode.
pub fn submit_evidence_calldata(
    id: u32,
    pubkey: &BlsPublicKey,
    msg_a: &[u8],
    sig_a: &BlsSignature,
    msg_b: &[u8],
    sig_b: &BlsSignature,
) -> Option<Bytes> {
    Some(
        IConsensusRegistry::submitEvidenceCall {
            id,
            pubkeyPoint: Bytes::copy_from_slice(&pubkey_point(pubkey)?),
            msgA: Bytes::copy_from_slice(msg_a),
            sigA: Bytes::copy_from_slice(&signature_point(sig_a)?),
            msgB: Bytes::copy_from_slice(msg_b),
            sigB: Bytes::copy_from_slice(&signature_point(sig_b)?),
        }
        .abi_encode()
        .into(),
    )
}
