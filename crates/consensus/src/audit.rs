//! Storage-audit votes and certificates (ADR 0009). The epoch's audit panel (16 committee members)
//! fetches each task's data from its provider and signs the outcome; 2/3 of the panel agreeing
//! form a certificate that block producers include and the chain verifies before recording it.

use crate::bitmap;
use bolt_primitives::bls::{self, BlsPublicKey, BlsSecretKey, BlsSignature};
use serde::{Deserialize, Serialize};

/// The message an auditor signs.
pub fn audit_msg(chain_id: u64, epoch: u64, task: u16, passed: bool) -> Vec<u8> {
    [
        b"boltchain/audit/v1".as_slice(),
        &chain_id.to_be_bytes(),
        &epoch.to_be_bytes(),
        &task.to_be_bytes(),
        &[passed as u8],
    ]
    .concat()
}

/// One auditor's vote (gossiped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditVote {
    /// Epoch of the task.
    pub epoch: u64,
    /// Task index.
    pub task: u16,
    /// Outcome.
    pub passed: bool,
    /// Index of the auditor in the epoch's panel.
    pub member: u16,
    /// Signature over [`audit_msg`].
    pub sig: BlsSignature,
}

impl AuditVote {
    /// Signs a vote.
    pub fn sign(
        key: &BlsSecretKey,
        chain_id: u64,
        epoch: u64,
        task: u16,
        passed: bool,
        member: u16,
    ) -> Self {
        Self {
            epoch,
            task,
            passed,
            member,
            sig: key.sign(&audit_msg(chain_id, epoch, task, passed)),
        }
    }

    /// dag-cbor.
    pub fn encode(&self) -> Vec<u8> {
        serde_ipld_dagcbor::to_vec(self).expect("vote encodes")
    }

    /// From dag-cbor.
    pub fn decode(b: &[u8]) -> Option<Self> {
        serde_ipld_dagcbor::from_slice(b).ok()
    }

    /// Whether the signature is `panel[member]`'s.
    pub fn verify(&self, chain_id: u64, panel: &[BlsPublicKey]) -> bool {
        panel.get(self.member as usize).is_some_and(|pk| {
            bls::verify(pk, &audit_msg(chain_id, self.epoch, self.task, self.passed), &self.sig)
        })
    }
}

/// A panel certificate: more than 2/3 of the panel signed the same outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditCert {
    /// Epoch of the task.
    pub epoch: u64,
    /// Task index.
    pub task: u16,
    /// Outcome.
    pub passed: bool,
    /// Signers, as a bitmap over the panel.
    #[serde(with = "serde_bytes")]
    pub signers: Vec<u8>,
    /// Aggregate signature.
    pub sig: BlsSignature,
}

impl AuditCert {
    /// Aggregates `votes` (same epoch, task and outcome; distinct members) for a panel of `n`.
    pub fn aggregate(votes: &[AuditVote], n: usize) -> Option<Self> {
        let first = votes.first()?;
        if votes
            .iter()
            .any(|v| v.epoch != first.epoch || v.task != first.task || v.passed != first.passed)
        {
            return None;
        }
        let mut members: Vec<u16> = votes.iter().map(|v| v.member).collect();
        members.sort_unstable();
        members.dedup();
        if members.len() != votes.len() || members.len() * 3 <= n * 2 {
            return None;
        }
        let sigs: Vec<BlsSignature> = votes.iter().map(|v| v.sig).collect();
        Some(Self {
            epoch: first.epoch,
            task: first.task,
            passed: first.passed,
            signers: bitmap::encode(&members, n),
            sig: bls::aggregate(&sigs)?,
        })
    }

    /// dag-cbor.
    pub fn encode(&self) -> Vec<u8> {
        serde_ipld_dagcbor::to_vec(self).expect("certificate encodes")
    }

    /// From dag-cbor.
    pub fn decode(b: &[u8]) -> Option<Self> {
        serde_ipld_dagcbor::from_slice(b).ok()
    }

    /// Whether more than 2/3 of `panel` signed this outcome.
    pub fn verify(&self, chain_id: u64, panel: &[BlsPublicKey]) -> bool {
        let signers = bitmap::decode(&self.signers);
        if signers.len() * 3 <= panel.len() * 2
            || signers.iter().any(|i| *i as usize >= panel.len())
        {
            return false;
        }
        let pks: Vec<BlsPublicKey> = signers.iter().map(|i| panel[*i as usize]).collect();
        bls::fast_aggregate_verify(
            &pks,
            &audit_msg(chain_id, self.epoch, self.task, self.passed),
            &self.sig,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificates_need_two_thirds_of_the_panel() {
        let keys: Vec<BlsSecretKey> = (0..16).map(bls::dev_key).collect();
        let panel: Vec<BlsPublicKey> = keys.iter().map(|k| k.public_key()).collect();
        let votes: Vec<AuditVote> =
            (0..11u16).map(|m| AuditVote::sign(&keys[m as usize], 7, 3, 2, true, m)).collect();
        assert!(votes.iter().all(|v| v.verify(7, &panel)));
        assert!(AuditCert::aggregate(&votes[..10], 16).is_none(), "10 of 16 is not > 2/3");
        let cert = AuditCert::aggregate(&votes, 16).unwrap();
        assert!(cert.verify(7, &panel));
        let back = AuditCert::decode(&cert.encode()).unwrap();
        assert_eq!(back, cert);
        let mut flipped = cert.clone();
        flipped.passed = false;
        assert!(!flipped.verify(7, &panel));
        assert!(!cert.verify(8, &panel), "other chain");
        let mut forged = cert.clone();
        forged.signers = bitmap::encode(&(0..11u16).map(|i| i + 5).collect::<Vec<_>>(), 16);
        assert!(!forged.verify(7, &panel));
        let v = AuditVote::decode(&votes[0].encode()).unwrap();
        assert_eq!(v, votes[0]);
    }
}
