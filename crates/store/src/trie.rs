//! Path-keyed, incrementally updated Merkle-Patricia trie.
//!
//! Every node is stored under its nibble path from the root (like geth's path-based scheme), so a
//! node never moves when an extension above it is split or merged. Updates are collected in a
//! dirty overlay; [`Trie::commit`] recomputes hashes only along modified paths and returns the new
//! root plus the node writes to persist.
//!
//! The encoding is the standard Ethereum hexary trie, so roots match `alloy_trie` exactly. Keys
//! are fixed 32-byte hashes (secure trie), which means a branch never carries a value.

use alloy_primitives::{B256, keccak256};
use alloy_rlp::{EMPTY_STRING_CODE, Encodable, Header};
use alloy_trie::EMPTY_ROOT_HASH;
use std::collections::BTreeMap;

/// Nibble path, one nibble (0..16) per byte.
pub type Path = Vec<u8>;

/// Source of persisted nodes, keyed by nibble path. Values are the node's RLP encoding.
pub trait NodeSource {
    /// Error type of the backing store.
    type Error;
    /// Returns the RLP of the node stored at `path`, if any.
    fn node(&self, path: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;
}

/// Node writes produced by a commit: `Some(rlp)` to store, `None` to delete.
pub type NodeWrites = BTreeMap<Path, Option<Vec<u8>>>;

/// Reference to a child as embedded in its parent: the RLP itself when shorter than 32 bytes,
/// otherwise the 32-byte hash. `None` means "modified, recompute at commit".
type ChildRef = Option<Vec<u8>>;

// Branches dominate the size, but nodes are short-lived and mostly branches anyway.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Leaf { key: Path, value: Vec<u8> },
    Ext { key: Path, child: ChildRef },
    Branch { children: [Option<ChildRef>; 16] },
}

/// Error from trie operations.
#[derive(Debug, thiserror::Error)]
pub enum TrieError<E> {
    /// The backing store failed.
    #[error("node source: {0:?}")]
    Source(E),
    /// A persisted node could not be decoded, or the structure is inconsistent.
    #[error("corrupt trie node at path {0:?}")]
    Corrupt(Path),
}

/// A trie being modified on top of a [`NodeSource`].
#[derive(Debug)]
pub struct Trie<'a, S> {
    source: &'a S,
    /// Nodes changed in this session. `None` = deleted.
    dirty: BTreeMap<Path, Option<Node>>,
}

impl<'a, S: NodeSource> Trie<'a, S> {
    /// Starts a modification session.
    pub fn new(source: &'a S) -> Self {
        Self { source, dirty: BTreeMap::new() }
    }

    fn get(&self, path: &[u8]) -> Result<Option<Node>, TrieError<S::Error>> {
        if let Some(n) = self.dirty.get(path) {
            return Ok(n.clone());
        }
        match self.source.node(path).map_err(TrieError::Source)? {
            None => Ok(None),
            Some(rlp) => {
                decode_node(&rlp).map(Some).ok_or_else(|| TrieError::Corrupt(path.to_vec()))
            }
        }
    }

    fn put(&mut self, path: Path, node: Node) {
        self.dirty.insert(path, Some(node));
    }

    fn remove(&mut self, path: Path) {
        self.dirty.insert(path, None);
    }

    /// Inserts or replaces the value at a 32-byte key.
    pub fn insert(&mut self, key: &B256, value: Vec<u8>) -> Result<(), TrieError<S::Error>> {
        debug_assert!(!value.is_empty(), "use remove() for empty values");
        let full = to_nibbles(key);
        let mut path: Path = Vec::with_capacity(64);
        let mut rest: &[u8] = &full;
        loop {
            match self.get(&path)? {
                None => {
                    self.put(path, Node::Leaf { key: rest.to_vec(), value });
                    return Ok(());
                }
                Some(Node::Leaf { key: lkey, value: lval }) => {
                    if lkey == rest {
                        self.put(path, Node::Leaf { key: lkey, value });
                        return Ok(());
                    }
                    let c = common_prefix(&lkey, rest);
                    let bpath = [&path[..], &rest[..c]].concat();
                    let mut children: [Option<ChildRef>; 16] = Default::default();
                    children[lkey[c] as usize] = Some(None);
                    children[rest[c] as usize] = Some(None);
                    self.put(
                        [&bpath[..], &[lkey[c]]].concat(),
                        Node::Leaf { key: lkey[c + 1..].to_vec(), value: lval },
                    );
                    self.put(
                        [&bpath[..], &[rest[c]]].concat(),
                        Node::Leaf { key: rest[c + 1..].to_vec(), value },
                    );
                    self.put(bpath, Node::Branch { children });
                    if c > 0 {
                        self.put(path, Node::Ext { key: rest[..c].to_vec(), child: None });
                    }
                    return Ok(());
                }
                Some(Node::Ext { key: ekey, .. }) => {
                    let c = common_prefix(&ekey, rest);
                    if c == ekey.len() {
                        self.put(path.clone(), Node::Ext { key: ekey.clone(), child: None });
                        path.extend_from_slice(&ekey);
                        rest = &rest[c..];
                        continue;
                    }
                    // Split: branch at path+c. The old child keeps its absolute path.
                    let bpath = [&path[..], &ekey[..c]].concat();
                    let mut children: [Option<ChildRef>; 16] = Default::default();
                    children[ekey[c] as usize] = Some(None);
                    children[rest[c] as usize] = Some(None);
                    if ekey.len() - c - 1 > 0 {
                        self.put(
                            [&bpath[..], &[ekey[c]]].concat(),
                            Node::Ext { key: ekey[c + 1..].to_vec(), child: None },
                        );
                    }
                    self.put(
                        [&bpath[..], &[rest[c]]].concat(),
                        Node::Leaf { key: rest[c + 1..].to_vec(), value },
                    );
                    self.put(bpath, Node::Branch { children });
                    if c > 0 {
                        self.put(path, Node::Ext { key: ekey[..c].to_vec(), child: None });
                    }
                    return Ok(());
                }
                Some(Node::Branch { mut children }) => {
                    let i = rest[0] as usize;
                    children[i] = Some(None);
                    self.put(path.clone(), Node::Branch { children });
                    path.push(rest[0]);
                    rest = &rest[1..];
                }
            }
        }
    }

    /// Removes the value at a 32-byte key. Removing a missing key is a no-op.
    pub fn remove_key(&mut self, key: &B256) -> Result<(), TrieError<S::Error>> {
        let full = to_nibbles(key);
        // Walk down, remembering (path, node) for every node visited.
        let mut stack: Vec<(Path, Node)> = Vec::new();
        let mut path: Path = Vec::new();
        let mut rest: &[u8] = &full;
        loop {
            let Some(node) = self.get(&path)? else { return Ok(()) };
            match &node {
                Node::Leaf { key, .. } => {
                    if key.as_slice() != rest {
                        return Ok(());
                    }
                    stack.push((path, node));
                    break;
                }
                Node::Ext { key, .. } => {
                    if !rest.starts_with(key) {
                        return Ok(());
                    }
                    let k = key.clone();
                    stack.push((path.clone(), node));
                    path.extend_from_slice(&k);
                    rest = &rest[k.len()..];
                }
                Node::Branch { children } => {
                    if children[rest[0] as usize].is_none() {
                        return Ok(());
                    }
                    stack.push((path.clone(), node));
                    path.push(rest[0]);
                    rest = &rest[1..];
                }
            }
        }

        // Remove the leaf, then repair ancestors bottom-up.
        let (leaf_path, _) = stack.pop().expect("leaf pushed");
        self.remove(leaf_path.clone());
        // `replaced`: what now sits at the child path we came from (None = empty).
        let mut child_path = leaf_path;
        let mut replaced: Option<Node> = None;
        while let Some((p, node)) = stack.pop() {
            match node {
                Node::Branch { mut children } => {
                    let slot = child_path[p.len()] as usize;
                    children[slot] = if replaced.is_some() { Some(None) } else { None };
                    let remaining: Vec<usize> =
                        (0..16).filter(|&i| children[i].is_some()).collect();
                    if remaining.len() >= 2 {
                        self.put(p.clone(), Node::Branch { children });
                        replaced = self.dirty.get(&p).cloned().flatten();
                    } else {
                        // Collapse the branch into its single remaining child.
                        let i = remaining[0] as u8;
                        let cpath = [&p[..], &[i]].concat();
                        let c =
                            self.get(&cpath)?.ok_or_else(|| TrieError::Corrupt(cpath.clone()))?;
                        let new = match c {
                            Node::Leaf { key, value } => {
                                self.remove(cpath);
                                Node::Leaf { key: [&[i][..], &key].concat(), value }
                            }
                            Node::Ext { key, .. } => {
                                self.remove(cpath);
                                Node::Ext { key: [&[i][..], &key].concat(), child: None }
                            }
                            Node::Branch { .. } => Node::Ext { key: vec![i], child: None },
                        };
                        self.put(p.clone(), new.clone());
                        replaced = Some(new);
                    }
                }
                Node::Ext { key, .. } => {
                    // An extension's child is always a branch; after a collapse it may be a
                    // leaf or extension that must be merged into this node.
                    let new = match replaced.take() {
                        Some(Node::Leaf { key: k2, value }) => {
                            self.remove(child_path.clone());
                            Node::Leaf { key: [&key[..], &k2].concat(), value }
                        }
                        Some(Node::Ext { key: k2, .. }) => {
                            self.remove(child_path.clone());
                            Node::Ext { key: [&key[..], &k2].concat(), child: None }
                        }
                        Some(Node::Branch { .. }) => Node::Ext { key, child: None },
                        None => return Err(TrieError::Corrupt(p)),
                    };
                    self.put(p.clone(), new.clone());
                    replaced = Some(new);
                }
                Node::Leaf { .. } => return Err(TrieError::Corrupt(p)),
            }
            child_path = p;
        }
        Ok(())
    }

    /// Recomputes hashes along modified paths. Returns the root hash and node writes.
    pub fn commit(mut self) -> Result<(B256, NodeWrites), TrieError<S::Error>> {
        let mut writes = NodeWrites::new();
        let root = match self.get(&[])? {
            None => EMPTY_ROOT_HASH,
            Some(_) => {
                let rlp = self.encode_at(&[], &mut writes)?;
                keccak256(rlp)
            }
        };
        // Deleted paths that were not re-created.
        for (path, node) in &self.dirty {
            if node.is_none() {
                writes.entry(path.clone()).or_insert(None);
            }
        }
        self.dirty.clear();
        Ok((root, writes))
    }

    /// Returns the RLP of the node at `path`, recomputing it (and recording the write) if dirty.
    fn encode_at(
        &mut self,
        path: &[u8],
        writes: &mut NodeWrites,
    ) -> Result<Vec<u8>, TrieError<S::Error>> {
        let node = match self.dirty.get(path) {
            Some(Some(n)) => n.clone(),
            Some(None) => return Err(TrieError::Corrupt(path.to_vec())),
            None => {
                return self
                    .source
                    .node(path)
                    .map_err(TrieError::Source)?
                    .ok_or_else(|| TrieError::Corrupt(path.to_vec()));
            }
        };
        let resolved = match node {
            Node::Leaf { .. } => node,
            Node::Ext { key, child } => {
                let child = match child {
                    Some(r) => r,
                    None => child_ref(&self.encode_at(&[path, &key[..]].concat(), writes)?),
                };
                Node::Ext { key, child: Some(child) }
            }
            Node::Branch { mut children } => {
                for (i, c) in children.iter_mut().enumerate() {
                    if let Some(None) = c {
                        let rlp = self.encode_at(&[path, &[i as u8]].concat(), writes)?;
                        *c = Some(Some(child_ref(&rlp)));
                    }
                }
                Node::Branch { children }
            }
        };
        let rlp = encode_node(&resolved);
        self.dirty.insert(path.to_vec(), Some(resolved));
        writes.insert(path.to_vec(), Some(rlp.clone()));
        Ok(rlp)
    }
}

/// Splits a 32-byte key into 64 nibbles.
pub fn to_nibbles(key: &B256) -> Path {
    key.iter().flat_map(|b| [b >> 4, b & 0x0f]).collect()
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn child_ref(rlp: &[u8]) -> Vec<u8> {
    if rlp.len() < 32 { rlp.to_vec() } else { keccak256(rlp).to_vec() }
}

/// Hex-prefix encoding of a nibble path.
fn hex_prefix(nibbles: &[u8], leaf: bool) -> Vec<u8> {
    let odd = nibbles.len() % 2 == 1;
    let flag = (if leaf { 2 } else { 0 }) + u8::from(odd);
    let mut out = Vec::with_capacity(nibbles.len() / 2 + 1);
    let mut iter = nibbles.iter();
    if odd {
        out.push((flag << 4) | iter.next().copied().unwrap_or(0));
    } else {
        out.push(flag << 4);
    }
    let rest: Vec<u8> = iter.copied().collect();
    for pair in rest.chunks(2) {
        out.push((pair[0] << 4) | pair[1]);
    }
    out
}

fn hex_prefix_decode(bytes: &[u8]) -> Option<(Path, bool)> {
    let first = *bytes.first()?;
    let flag = first >> 4;
    if flag > 3 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 2);
    if flag & 1 == 1 {
        out.push(first & 0x0f);
    }
    for b in &bytes[1..] {
        out.push(b >> 4);
        out.push(b & 0x0f);
    }
    Some((out, flag & 2 == 2))
}

/// Appends a child reference: raw RLP if inline, else an RLP string of the 32-byte hash.
fn encode_ref(r: &[u8], out: &mut Vec<u8>) {
    if r.len() == 32 {
        r.encode(out);
    } else {
        out.extend_from_slice(r);
    }
}

fn encode_node(node: &Node) -> Vec<u8> {
    let mut payload = Vec::new();
    match node {
        Node::Leaf { key, value } => {
            hex_prefix(key, true).as_slice().encode(&mut payload);
            value.as_slice().encode(&mut payload);
        }
        Node::Ext { key, child } => {
            hex_prefix(key, false).as_slice().encode(&mut payload);
            encode_ref(child.as_ref().expect("resolved before encoding"), &mut payload);
        }
        Node::Branch { children } => {
            for c in children {
                match c {
                    None => payload.push(EMPTY_STRING_CODE),
                    Some(r) => {
                        encode_ref(r.as_ref().expect("resolved before encoding"), &mut payload)
                    }
                }
            }
            payload.push(EMPTY_STRING_CODE); // branch value: always empty in a secure trie
        }
    }
    let mut out = Vec::with_capacity(payload.len() + 3);
    Header { list: true, payload_length: payload.len() }.encode(&mut out);
    out.extend_from_slice(&payload);
    out
}

/// Splits an RLP list payload into its raw items (each item including its own header).
fn raw_items(mut buf: &[u8]) -> Option<Vec<&[u8]>> {
    let mut items = Vec::new();
    while !buf.is_empty() {
        let start = buf;
        let h = Header::decode(&mut buf).ok()?;
        let hlen = start.len() - buf.len();
        let total = hlen + h.payload_length;
        items.push(start.get(..total)?);
        buf = start.get(total..)?;
    }
    Some(items)
}

fn string_payload(item: &[u8]) -> Option<&[u8]> {
    let mut b = item;
    let h = Header::decode(&mut b).ok()?;
    if h.list {
        return None;
    }
    b.get(..h.payload_length)
}

fn decode_ref(item: &[u8]) -> Option<Vec<u8>> {
    let mut b = item;
    let h = Header::decode(&mut b).ok()?;
    if h.list {
        Some(item.to_vec()) // inline child node
    } else if h.payload_length == 32 {
        Some(b[..32].to_vec())
    } else {
        None
    }
}

fn decode_node(rlp: &[u8]) -> Option<Node> {
    let mut b = rlp;
    let h = Header::decode(&mut b).ok()?;
    if !h.list {
        return None;
    }
    let items = raw_items(b.get(..h.payload_length)?)?;
    match items.len() {
        2 => {
            let (key, leaf) = hex_prefix_decode(string_payload(items[0])?)?;
            if leaf {
                Some(Node::Leaf { key, value: string_payload(items[1])?.to_vec() })
            } else {
                Some(Node::Ext { key, child: Some(decode_ref(items[1])?) })
            }
        }
        17 => {
            let mut children: [Option<ChildRef>; 16] = Default::default();
            for (i, item) in items[..16].iter().enumerate() {
                if *item != [EMPTY_STRING_CODE] {
                    children[i] = Some(Some(decode_ref(item)?));
                }
            }
            Some(Node::Branch { children })
        }
        _ => None,
    }
}

/// In-memory node store, used in tests and as a reference.
#[derive(Debug, Default, Clone)]
pub struct MemNodes(pub BTreeMap<Path, Vec<u8>>);

impl NodeSource for MemNodes {
    type Error = std::convert::Infallible;
    fn node(&self, path: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.0.get(path).cloned())
    }
}

impl MemNodes {
    /// Applies commit writes.
    pub fn apply(&mut self, writes: NodeWrites) {
        for (p, w) in writes {
            match w {
                Some(v) => self.0.insert(p, v),
                None => self.0.remove(&p),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use alloy_trie::root::storage_root_unhashed;
    use proptest::prelude::*;

    /// Reference root: storage trie of (slot -> value), computed from scratch by alloy-trie.
    fn reference(map: &BTreeMap<B256, U256>) -> B256 {
        storage_root_unhashed(map.iter().filter(|(_, v)| !v.is_zero()).map(|(k, v)| (*k, *v)))
    }

    /// Applies (slot, value) updates to our trie (value 0 = delete), returns the new root.
    fn apply(nodes: &mut MemNodes, updates: &[(B256, U256)]) -> B256 {
        let mut t = Trie::new(&*nodes);
        for (slot, v) in updates {
            let key = keccak256(slot);
            if v.is_zero() {
                t.remove_key(&key).unwrap();
            } else {
                t.insert(&key, alloy_rlp::encode(v)).unwrap();
            }
        }
        let (root, writes) = t.commit().unwrap();
        nodes.apply(writes);
        root
    }

    #[test]
    fn empty_and_single() {
        let mut nodes = MemNodes::default();
        assert_eq!(apply(&mut nodes, &[]), EMPTY_ROOT_HASH);
        let upd = [(B256::with_last_byte(1), U256::from(7))];
        let mut m = BTreeMap::new();
        m.insert(upd[0].0, upd[0].1);
        assert_eq!(apply(&mut nodes, &upd), reference(&m));
        assert_eq!(apply(&mut nodes, &[(upd[0].0, U256::ZERO)]), EMPTY_ROOT_HASH);
        assert!(nodes.0.is_empty(), "all nodes deleted: {:?}", nodes.0.keys());
    }

    #[test]
    fn node_codec_roundtrip() {
        let leaf = Node::Leaf { key: vec![1, 2, 3], value: vec![0x82, 1, 2] };
        assert_eq!(decode_node(&encode_node(&leaf)), Some(leaf));
        let ext = Node::Ext { key: vec![4, 5], child: Some(vec![9; 32]) };
        assert_eq!(decode_node(&encode_node(&ext)), Some(ext));
    }

    fn slot_strategy() -> impl Strategy<Value = B256> {
        // Small domain so batches collide: exercises replace, delete and re-insert.
        (0u16..400).prop_map(|i| B256::from(U256::from(i)))
    }

    fn value_strategy() -> impl Strategy<Value = U256> {
        prop_oneof![
            3 => (1u64..u64::MAX).prop_map(U256::from),
            1 => Just(U256::ZERO),
            1 => (1u8..16).prop_map(U256::from), // short values -> inline (<32 byte) nodes
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        /// Incremental updates across several commits always equal a from-scratch recomputation,
        /// and the persisted node set equals the node set of a trie built in one shot.
        #[test]
        fn incremental_matches_full_recompute(
            batches in prop::collection::vec(
                prop::collection::vec((slot_strategy(), value_strategy()), 0..60), 1..8)
        ) {
            let mut nodes = MemNodes::default();
            let mut model: BTreeMap<B256, U256> = BTreeMap::new();
            for batch in &batches {
                let root = apply(&mut nodes, batch);
                for (k, v) in batch {
                    if v.is_zero() { model.remove(k); } else { model.insert(*k, *v); }
                }
                prop_assert_eq!(root, reference(&model));
            }
            // No stale nodes: rebuilding from the final model yields the identical node set.
            let mut fresh = MemNodes::default();
            let all: Vec<_> = model.iter().map(|(k, v)| (*k, *v)).collect();
            apply(&mut fresh, &all);
            prop_assert_eq!(&nodes.0, &fresh.0);
        }
    }
}
