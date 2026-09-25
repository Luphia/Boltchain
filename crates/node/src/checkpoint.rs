//! Phase B (ADR 0007 §4): the epoch's committee runs the same BFT engine as under PoS, but over
//! the mined chain. A "proposal" names the next mined block after the last certified one, once
//! enough blocks bury it; members vote only for a block on their own best chain. A committed
//! block is final: fork choice never goes below it again. The proofs have the same format as PoS
//! finality proofs (the child's header is the next mined block).

use anyhow::{Context, Result, bail, ensure};
use bolt_chain::Chain;
use bolt_consensus::{BlockInfo, BlsScheme, Proposal, Qc};
use std::{sync::Arc, time::Duration};

/// Blocks that must bury mined block `height` before it can be proposed: the checkpoint depth,
/// or less in the last blocks before the terminal block (nothing is mined after it).
pub fn required_depth(chain: &Chain, height: u64) -> u64 {
    let depth = chain.rules().checkpoint_depth;
    match chain.phase().ok().and_then(|p| p.terminal_height(chain.rules())) {
        Some(t) if height <= t => depth.min(t - height),
        _ => depth,
    }
}

/// The leader's checkpoint for the round: the canonical block after `qc`'s, once buried deep
/// enough. Waits for it until `deadline_ms`.
pub async fn propose(
    chain: &Arc<Chain>,
    round: u64,
    qc: &Qc<BlsScheme>,
    deadline_ms: u64,
) -> Result<BlockInfo> {
    let height = qc.height + 1;
    let started = std::time::Instant::now();
    loop {
        let head = chain.head()?.number;
        let header = chain.store().reader()?.header(height)?;
        if let Some(h) = header
            && head.saturating_sub(height) >= required_depth(chain, height)
        {
            ensure!(
                h.parent_hash == qc.block,
                "our chain does not extend the certified checkpoint {}",
                qc.block
            );
            return Ok(BlockInfo { hash: h.hash_slow(), parent: qc.block, round, height });
        }
        if started.elapsed() > Duration::from_millis(deadline_ms) {
            bail!("block {height} not buried deep enough yet (head {head})");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A member votes for a checkpoint only if it is the canonical block at its height on its own
/// chain, extends the certified parent and is buried (half the required depth is enough, to
/// allow for propagation).
pub async fn validate(chain: &Arc<Chain>, p: &Proposal<BlsScheme>) -> Result<()> {
    ensure!(p.payload.is_empty(), "checkpoint proposals carry no payload");
    let height = p.block.height;
    // The block may still be on its way to us.
    let mut header = None;
    for _ in 0..30 {
        if let Some(h) = chain.store().reader()?.header(height)?
            && h.hash_slow() == p.block.hash
        {
            header = Some(h);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let h = header.context("not the canonical block at that height here")?;
    ensure!(h.parent_hash == p.block.parent, "parent differs from the certified checkpoint");
    ensure!(!h.difficulty.is_zero(), "not a mined block");
    let head = chain.head()?.number;
    let need = required_depth(chain, height).div_ceil(2);
    ensure!(head.saturating_sub(height) >= need, "block {height} is not buried deep enough");
    Ok(())
}
