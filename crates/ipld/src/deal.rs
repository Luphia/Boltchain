//! Deal index (ADR 0014): the blocks a SwarmStorage deal pays to keep, in order, grouped like the
//! epoch index. The root also names the application entry point (e.g. a bolt-vault envelope).

use crate::{Cid, DAG_CBOR, IpldError, sha256_cid};
use serde::{Deserialize, Serialize};

/// Block CIDs per deal-index group.
pub const DEAL_GROUP: usize = 1024;

/// Root of a deal index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealIndex {
    /// Format version (1).
    pub v: u8,
    /// Total bytes of the listed blocks.
    pub size: u64,
    /// Number of listed blocks.
    pub count: u64,
    /// Groups of up to [`DEAL_GROUP`] block CIDs.
    pub groups: Vec<Cid>,
    /// Application entry point (one of the listed blocks), if any.
    pub root: Option<Cid>,
}

fn block<T: Serialize>(v: &T) -> (Cid, Vec<u8>) {
    let bytes = serde_ipld_dagcbor::to_vec(v).expect("dag-cbor encodes");
    (sha256_cid(DAG_CBOR, &bytes), bytes)
}

/// Builds the index of `blocks` (CID and size of each) with entry point `root`. Returns the root
/// CID, the index itself and its blocks (groups, then the root).
pub fn deal_index(
    blocks: &[(Cid, u64)],
    root: Option<Cid>,
) -> (Cid, DealIndex, Vec<(Cid, Vec<u8>)>) {
    let mut out = Vec::new();
    let mut groups = Vec::new();
    for g in blocks.chunks(DEAL_GROUP) {
        let list: Vec<Cid> = g.iter().map(|(c, _)| *c).collect();
        let (cid, bytes) = block(&list);
        groups.push(cid);
        out.push((cid, bytes));
    }
    let size = blocks.iter().map(|(_, n)| n).sum();
    let idx = DealIndex { v: 1, size, count: blocks.len() as u64, groups, root };
    let (cid, bytes) = block(&idx);
    out.push((cid, bytes));
    (cid, idx, out)
}

impl DealIndex {
    /// Decodes an index root.
    pub fn decode(bytes: &[u8]) -> Result<Self, IpldError> {
        let i: Self = serde_ipld_dagcbor::from_slice(bytes)
            .map_err(|_| IpldError::Malformed("deal index"))?;
        if i.v != 1 || i.groups.len() as u64 != i.count.div_ceil(DEAL_GROUP as u64) {
            return Err(IpldError::Malformed("deal index"));
        }
        Ok(i)
    }

    /// Group and position of block `index`.
    pub fn locate(&self, index: u64) -> Option<(Cid, usize)> {
        if index >= self.count {
            return None;
        }
        let g = (index / DEAL_GROUP as u64) as usize;
        Some((self.groups[g], (index % DEAL_GROUP as u64) as usize))
    }

    /// Every index block (groups and the root) besides the listed blocks: what a provider keeps.
    pub fn index_blocks(&self, root: Cid) -> Vec<Cid> {
        let mut v = self.groups.clone();
        v.push(root);
        v
    }
}

/// Decodes a deal-index group.
pub fn decode_group(bytes: &[u8]) -> Result<Vec<Cid>, IpldError> {
    serde_ipld_dagcbor::from_slice(bytes).map_err(|_| IpldError::Malformed("deal index group"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RAW;

    #[test]
    fn index_roundtrip_and_locate() {
        let blocks: Vec<(Cid, u64)> =
            (0..2500u32).map(|i| (sha256_cid(RAW, &i.to_be_bytes()), 4)).collect();
        let (root, idx, out) = deal_index(&blocks, Some(blocks[7].0));
        assert_eq!(idx.count, 2500);
        assert_eq!(idx.size, 10_000);
        assert_eq!(idx.groups.len(), 3);
        assert_eq!(out.last().unwrap().0, root);
        let back = DealIndex::decode(&out.last().unwrap().1).unwrap();
        assert_eq!(back, idx);
        let (g, pos) = idx.locate(2049).unwrap();
        assert_eq!(g, idx.groups[2]);
        let list = decode_group(&out[2].1).unwrap();
        assert_eq!(list[pos], blocks[2049].0);
        assert!(idx.locate(2500).is_none());
    }
}
