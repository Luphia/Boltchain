//! History sharding and audit draws (ADR 0009). Pure functions of chain data, so every node
//! computes the same assignments, tasks and panels.

use alloy_primitives::{B256, keccak256};
use bolt_primitives::params::{AUDIT_PANEL, AUDIT_TASKS, HISTORY_REPLICATION};

/// The validators that keep epoch `index_cid`'s data (rendezvous hashing: the
/// [`HISTORY_REPLICATION`] ids with the smallest `keccak(cid ‖ id)`).
pub fn assignees(index_cid: &[u8], validators: &[u32]) -> Vec<u32> {
    let mut scored: Vec<(B256, u32)> = validators
        .iter()
        .map(|id| (keccak256([index_cid, &id.to_be_bytes()].concat()), *id))
        .collect();
    scored.sort();
    scored.into_iter().take(HISTORY_REPLICATION as usize).map(|(_, id)| id).collect()
}

fn draw(seed: &B256, tag: &[u8], epoch: u64, i: u32) -> u64 {
    let h = keccak256([seed.as_slice(), tag, &epoch.to_be_bytes(), &i.to_be_bytes()].concat());
    u64::from_be_bytes(h[..8].try_into().expect("8 bytes"))
}

/// One audit task: provider `provider` must serve data of block `height` (in epoch `target`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditTask {
    /// Validator id.
    pub provider: u32,
    /// Epoch sampled.
    pub target: u64,
    /// Block sampled.
    pub height: u64,
}

/// Target epochs for the audits of `epoch`, one per task (epochs `0..=last_old`).
pub fn draw_targets(seed: &B256, epoch: u64, last_old: u64) -> Vec<u64> {
    (0..AUDIT_TASKS).map(|i| draw(seed, b"audit/target", epoch, i) % (last_old + 1)).collect()
}

/// The provider and block of task `i`, given its target's assignees and block range.
pub fn draw_task(
    seed: &B256,
    epoch: u64,
    i: u32,
    target: u64,
    assignees: &[u32],
    first: u64,
    count: u64,
) -> Option<AuditTask> {
    if assignees.is_empty() || count == 0 {
        return None;
    }
    let provider =
        assignees[(draw(seed, b"audit/provider", epoch, i) % assignees.len() as u64) as usize];
    let height = first + draw(seed, b"audit/block", epoch, i) % count;
    Some(AuditTask { provider, target, height })
}

/// The audit panel of `epoch`: up to [`AUDIT_PANEL`] distinct committee members, in draw order.
pub fn draw_panel(seed: &B256, epoch: u64, members: &[u32]) -> Vec<u32> {
    let mut scored: Vec<(u64, u32)> =
        members.iter().map(|id| (draw(seed, b"audit/panel", epoch, *id), *id)).collect();
    scored.sort();
    scored.into_iter().take(AUDIT_PANEL as usize).map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_is_stable_and_spread() {
        let vals: Vec<u32> = (1..=100).collect();
        let a = assignees(b"cid-1", &vals);
        assert_eq!(a.len(), 16);
        assert_eq!(a, assignees(b"cid-1", &vals));
        assert_ne!(a, assignees(b"cid-2", &vals));
        // Rendezvous: removing a non-assignee changes nothing; removing an assignee replaces
        // only that one.
        let outsider = *vals.iter().find(|v| !a.contains(v)).unwrap();
        let fewer: Vec<u32> = vals.iter().copied().filter(|v| *v != outsider).collect();
        assert_eq!(assignees(b"cid-1", &fewer), a);
        let gone = a[3];
        let fewer: Vec<u32> = vals.iter().copied().filter(|v| *v != gone).collect();
        let b = assignees(b"cid-1", &fewer);
        assert_eq!(b.iter().filter(|x| a.contains(x)).count(), 15);
        // Small sets: everyone keeps everything.
        assert_eq!(assignees(b"x", &[3, 1, 2]).len(), 3);
    }

    #[test]
    fn draws_are_deterministic() {
        let seed = B256::repeat_byte(7);
        let t = draw_targets(&seed, 20, 11);
        assert_eq!(t.len(), AUDIT_TASKS as usize);
        assert!(t.iter().all(|e| *e <= 11));
        assert_eq!(t, draw_targets(&seed, 20, 11));
        let task = draw_task(&seed, 20, 0, t[0], &[5, 6, 7], 100, 50).unwrap();
        assert!([5, 6, 7].contains(&task.provider) && (100..150).contains(&task.height));
        let p = draw_panel(&seed, 20, &(1..=40).collect::<Vec<_>>());
        assert_eq!(p.len(), 16);
        let mut d = p.clone();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), 16);
    }
}
