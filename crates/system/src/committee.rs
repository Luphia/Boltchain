//! Committees: stake-weighted seat sampling (ADR 0006 §2).

use alloy_primitives::{B256, Bytes, U256, keccak256};

/// A committee as stored in `ConsensusRegistry`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Committee {
    /// Validator ids, in consensus index order (order of first seat).
    pub members: Vec<u32>,
    /// Seats per member.
    pub weights: Vec<u16>,
    /// Member index of each seat, in leader order.
    pub seats: Vec<u16>,
}

impl Committee {
    /// Every validator gets one seat, in the given order (the bootstrap committee).
    pub fn equal(ids: &[u32]) -> Self {
        Self {
            members: ids.to_vec(),
            weights: vec![1; ids.len()],
            seats: (0..ids.len() as u16).collect(),
        }
    }

    /// Samples `size` seats with replacement, each validator with probability proportional to its
    /// stake. `ids` and `stakes` must be sorted by id. Deterministic in (seed, epoch, inputs).
    pub fn sample(seed: B256, epoch: u64, ids: &[u32], stakes: &[U256], size: u32) -> Self {
        assert_eq!(ids.len(), stakes.len());
        let mut cumulative = Vec::with_capacity(stakes.len());
        let mut total = U256::ZERO;
        for s in stakes {
            total += *s;
            cumulative.push(total);
        }
        let mut c = Self::default();
        if total.is_zero() {
            return c;
        }
        let mut index_of = std::collections::HashMap::<u32, u16>::new();
        for i in 0..size {
            let mut buf = [0u8; 44];
            buf[..32].copy_from_slice(seed.as_slice());
            buf[32..40].copy_from_slice(&epoch.to_be_bytes());
            buf[40..44].copy_from_slice(&i.to_be_bytes());
            let r = U256::from_be_bytes(keccak256(buf).0) % total;
            // first validator whose cumulative stake exceeds r
            let k = cumulative.partition_point(|c| *c <= r);
            let id = ids[k];
            let m = *index_of.entry(id).or_insert_with(|| {
                c.members.push(id);
                c.weights.push(0);
                (c.members.len() - 1) as u16
            });
            c.weights[m as usize] += 1;
            c.seats.push(m);
        }
        c
    }

    /// Seats as big-endian u16s (the `bytes seats` argument of `ConsensusRegistry`).
    pub fn seats_bytes(&self) -> Bytes {
        self.seats.iter().flat_map(|s| s.to_be_bytes()).collect::<Vec<u8>>().into()
    }

    /// From the values returned by `ConsensusRegistry.committee(epoch)`.
    pub fn from_parts(members: Vec<u32>, weights: Vec<u16>, seats: &[u8]) -> Self {
        let seats = seats.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        Self { members, weights, seats }
    }

    /// Whether the committee has no members.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_is_proportional_and_deterministic() {
        let ids: Vec<u32> = (1..=4).collect();
        let stakes: Vec<U256> = [1u64, 1, 2, 4].iter().map(|s| U256::from(*s)).collect();
        let a = Committee::sample(B256::repeat_byte(7), 3, &ids, &stakes, 4096);
        let b = Committee::sample(B256::repeat_byte(7), 3, &ids, &stakes, 4096);
        assert_eq!(a, b);
        assert_eq!(a.seats.len(), 4096);
        assert_eq!(a.weights.iter().map(|w| *w as u32).sum::<u32>(), 4096);
        let seats_of =
            |id: u32| a.members.iter().position(|m| *m == id).map(|i| a.weights[i] as f64).unwrap();
        // expected 512, 512, 1024, 2048
        assert!((seats_of(4) / 2048.0 - 1.0).abs() < 0.08);
        assert!((seats_of(1) / 512.0 - 1.0).abs() < 0.15);
        let c = Committee::sample(B256::repeat_byte(8), 3, &ids, &stakes, 4096);
        assert_ne!(a.seats, c.seats, "different seed, different draw");
        assert_eq!(
            Committee::from_parts(a.members.clone(), a.weights.clone(), &a.seats_bytes()),
            a
        );
    }
}
