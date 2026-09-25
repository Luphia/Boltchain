//! CAR v1 archives (used for epoch archives and state snapshots, and for moving blocks over HTTP
//! gateways).
//!
//! Traversal for exports follows every link of a dag-cbor block except an envelope's `parent`: an
//! epoch index exports that epoch's blocks, not the whole chain behind them (ADR 0009).

use crate::{Cid, DAG_CBOR, IpldError, verify};
use ipld_core::ipld::Ipld;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize)]
struct CarHeader {
    // Field order is the canonical dag-cbor key order (shorter keys first).
    roots: Vec<Cid>,
    version: u64,
}

fn put_varint(out: &mut Vec<u8>, n: usize) {
    let mut buf = unsigned_varint::encode::usize_buffer();
    out.extend_from_slice(unsigned_varint::encode::usize(n, &mut buf));
}

fn take_varint(buf: &mut &[u8]) -> Result<usize, IpldError> {
    let (n, rest) =
        unsigned_varint::decode::usize(buf).map_err(|_| IpldError::Malformed("car varint"))?;
    *buf = rest;
    Ok(n)
}

/// Writes a CAR v1 archive.
pub fn write(roots: &[Cid], blocks: &[(Cid, Vec<u8>)]) -> Vec<u8> {
    let header = serde_ipld_dagcbor::to_vec(&CarHeader { roots: roots.to_vec(), version: 1 })
        .expect("car header serializes");
    let mut out = Vec::new();
    put_varint(&mut out, header.len());
    out.extend_from_slice(&header);
    for (cid, data) in blocks {
        let c = cid.to_bytes();
        put_varint(&mut out, c.len() + data.len());
        out.extend_from_slice(&c);
        out.extend_from_slice(data);
    }
    out
}

/// A decoded archive: its roots and blocks, in file order.
pub type Archive = (Vec<Cid>, Vec<(Cid, Vec<u8>)>);

/// Reads a CAR v1 archive, verifying every block against its CID.
pub fn read(mut buf: &[u8]) -> Result<Archive, IpldError> {
    let hlen = take_varint(&mut buf)?;
    let hbytes = buf.get(..hlen).ok_or(IpldError::Malformed("car header"))?;
    let header: CarHeader =
        serde_ipld_dagcbor::from_slice(hbytes).map_err(|_| IpldError::Malformed("car header"))?;
    if header.version != 1 {
        return Err(IpldError::Malformed("car version"));
    }
    buf = &buf[hlen..];
    let mut blocks = Vec::new();
    while !buf.is_empty() {
        let len = take_varint(&mut buf)?;
        let section = buf.get(..len).ok_or(IpldError::Malformed("car section"))?;
        let mut cursor = std::io::Cursor::new(section);
        let cid = Cid::read_bytes(&mut cursor).map_err(|_| IpldError::Malformed("car cid"))?;
        let data = section[cursor.position() as usize..].to_vec();
        verify(&cid, &data)?;
        blocks.push((cid, data));
        buf = &buf[len..];
    }
    Ok((header.roots, blocks))
}

/// Links of one block that an export follows (see the module docs).
pub fn links(cid: &Cid, data: &[u8]) -> Vec<Cid> {
    if cid.codec() != DAG_CBOR {
        return Vec::new();
    }
    let Ok(node) = serde_ipld_dagcbor::from_slice::<Ipld>(data) else { return Vec::new() };
    let mut out = Vec::new();
    collect(&node, &mut out);
    out
}

fn collect(node: &Ipld, out: &mut Vec<Cid>) {
    match node {
        Ipld::Link(c) => out.push(*c),
        Ipld::List(items) => items.iter().for_each(|i| collect(i, out)),
        Ipld::Map(m) => {
            let envelope = m.contains_key("header") && m.contains_key("parent");
            for (k, v) in m {
                if !(envelope && k == "parent") {
                    collect(v, out);
                }
            }
        }
        _ => {}
    }
}

/// Every block reachable from `root` (depth first, each once), read with `get`. Blocks `get`
/// does not have are reported in the error.
pub fn traverse(
    root: Cid,
    mut get: impl FnMut(&Cid) -> Option<Vec<u8>>,
) -> Result<Vec<(Cid, Vec<u8>)>, IpldError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(c) = stack.pop() {
        if !seen.insert(c) {
            continue;
        }
        let data = get(&c).ok_or(IpldError::Missing(c))?;
        let mut l = links(&c, &data);
        l.reverse();
        stack.extend(l);
        out.push((c, data));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RAW, sha256_cid};
    use std::collections::HashMap;

    #[test]
    fn car_roundtrip_and_epoch_traversal() {
        let chunk = b"chunk".to_vec();
        let chunk_cid = sha256_cid(RAW, &chunk);
        let parent = sha256_cid(DAG_CBOR, b"previous epoch");
        let header = crate::header_cid(alloy_primitives::B256::repeat_byte(3));
        let env = crate::Envelope {
            v: crate::ENVELOPE_VERSION,
            height: 5,
            header,
            parent: Some(parent),
            chunks: vec![chunk_cid],
            qc: vec![],
            audits: vec![],
        };
        let env_bytes = env.encode();
        let env_cid = sha256_cid(DAG_CBOR, &env_bytes);
        let (root, index_blocks) = crate::history::epoch_index(0, 5, &[env_cid]);
        let mut store: HashMap<Cid, Vec<u8>> = index_blocks.into_iter().collect();
        store.insert(env_cid, env_bytes);
        store.insert(chunk_cid, chunk);
        store.insert(header, vec![0xc0]); // stands in for the header (never decoded here)
        let blocks = traverse(root, |c| store.get(c).cloned()).unwrap();
        // index root, group, envelope, header, chunk; not the parent envelope.
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0].0, root);
        assert!(!blocks.iter().any(|(c, _)| *c == parent));
        let car = write(&[root], &blocks[..2]);
        let (roots, back) = read(&car).unwrap();
        assert_eq!(roots, vec![root]);
        assert_eq!(back, blocks[..2].to_vec());
        let mut bad = car.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(read(&bad).is_err());
    }
}
