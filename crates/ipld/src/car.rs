//! CAR v1 archives (used for epoch archives and state snapshots, and for moving blocks over HTTP
//! gateways).

use crate::{Cid, IpldError, verify};
use serde::{Deserialize, Serialize};

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
