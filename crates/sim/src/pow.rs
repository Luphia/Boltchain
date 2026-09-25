//! Mining-phase simulator (ADR 0007 §1–§2): miners with hashrate shares, propagation delays,
//! partitions and attackers, under Boltchain's fork choice (most total difficulty, ties to the
//! lower hash, no automatic reorganisation deeper than 128 blocks) and ASERT difficulty.
//!
//! Blocks are abstract (no execution): the point is the fork-choice dynamics. Every scenario is
//! reproducible from its seed.

use alloy_primitives::U256;
use bolt_pow::{AsertParams, next_difficulty};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashSet},
};

/// Deepest automatic reorganisation (the node's rule).
pub const MAX_REORG_DEPTH: u64 = 128;

/// What the attacker does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Attack {
    /// No attacker.
    None,
    /// Mines a private chain from `start_s` and publishes it once it is at least `min_len`
    /// blocks long and has more total difficulty than the best public chain. (ASERT adapts both
    /// chains to 12 s blocks, so the private chain is heavier, not longer.)
    Private {
        /// Start time.
        start_s: f64,
        /// Minimum private length before release.
        min_len: u64,
    },
    /// Selfish mining (Eyal and Sirer): withhold blocks, release to override honest ones.
    Selfish,
}

/// A mining scenario.
#[derive(Debug, Clone)]
pub struct PowScenario {
    /// Seed.
    pub seed: u64,
    /// Honest miners' hashrate shares (normalised with the attacker's).
    pub honest: Vec<f64>,
    /// Attacker's share (0 for none).
    pub attacker: f64,
    /// Attack strategy.
    pub attack: Attack,
    /// Network hashrate in hashes per second.
    pub hashrate: f64,
    /// Mean propagation delay between honest nodes, seconds.
    pub delay_s: f64,
    /// Honest nodes split into two groups (first `split` miners vs the rest) during this window.
    pub partition: Option<(f64, f64, usize)>,
    /// Phase B: a committee finalizes the block this deep on every honest node's chain; fork
    /// choice never goes below it (finality is modelled as reaching everyone at once).
    pub finality_depth: Option<u64>,
    /// Simulated time.
    pub duration_s: f64,
    /// Difficulty rule.
    pub asert: AsertParams,
}

impl PowScenario {
    /// Mainnet timing: 12 s blocks, one-hour half-life, starting at the right difficulty.
    pub fn mainnet(seed: u64, honest: Vec<f64>, hashrate: f64) -> Self {
        Self {
            seed,
            honest,
            attacker: 0.0,
            attack: Attack::None,
            hashrate,
            delay_s: 1.0,
            partition: None,
            finality_depth: None,
            duration_s: 6.0 * 3600.0,
            asert: AsertParams {
                spacing: 12,
                half_life: 3600,
                initial: U256::from((hashrate * 12.0) as u64),
                minimum: U256::from(1),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct Block {
    parent: usize,
    height: u64,
    time: u64,
    difficulty: U256,
    td: U256,
    tag: u64, // stands in for the hash in tie-breaks
    miner: usize,
}

/// Outcome.
#[derive(Debug, Clone, Default)]
pub struct PowReport {
    /// Blocks mined.
    pub mined: usize,
    /// Height of the final canonical chain (node 0's view).
    pub height: u64,
    /// Share of mined blocks not on the final canonical chain.
    pub orphan_rate: f64,
    /// Deepest reorganisation an honest node performed.
    pub max_reorg: u64,
    /// Heavier branches honest nodes refused because they forked too deep.
    pub refused: usize,
    /// Share of canonical blocks mined by the attacker.
    pub attacker_share: f64,
    /// Mean block interval on the canonical chain, seconds.
    pub mean_interval_s: f64,
    /// Whether all honest nodes agree on the canonical chain below their lowest head minus 6.
    pub agree: bool,
}

struct Node {
    head: usize,
    known: HashSet<usize>,
}

/// Runs a scenario.
pub fn run_pow(sc: &PowScenario) -> PowReport {
    let mut rng = StdRng::seed_from_u64(sc.seed);
    let total: f64 = sc.honest.iter().sum::<f64>() + sc.attacker;
    let n = sc.honest.len();
    let attacker = n; // index of the attacker's miner
    let mut blocks = vec![Block {
        parent: 0,
        height: 0,
        time: 0,
        difficulty: U256::ZERO,
        td: U256::ZERO,
        tag: 0,
        miner: usize::MAX,
    }];
    let mut nodes: Vec<Node> =
        (0..n).map(|_| Node { head: 0, known: HashSet::from([0]) }).collect();
    let mut report = PowReport::default();

    // Attacker state.
    let mut private_tip = 0usize;
    let mut private: Vec<usize> = Vec::new(); // withheld blocks, oldest first
    let mut attacking = false;

    // Events: (time in ms, kind). Kind 0 = mining tick for miner i, 1 = delivery of block b to
    // node i.
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Ev {
        Mine(usize, u64), // miner, generation (stale draws are skipped)
        Deliver(usize, usize),
    }
    let mut queue: BinaryHeap<Reverse<(u64, Ev)>> = BinaryHeap::new();
    let mut generation = vec![0u64; n + 1];
    let draw = |rng: &mut StdRng, share: f64, difficulty: U256| -> u64 {
        let rate = share / total * sc.hashrate / difficulty.to::<u128>().max(1) as f64;
        let u: f64 = rng.random_range(1e-12..1.0);
        (-u.ln() / rate * 1000.0) as u64
    };
    let child_difficulty = |blocks: &[Block], parent: usize| -> U256 {
        let p = &blocks[parent];
        let solve = if p.height <= 1 { 0 } else { p.time - blocks[p.parent].time };
        next_difficulty(&sc.asert, p.height, p.difficulty, solve.max(1))
    };
    let tip_of = |i: usize, nodes: &[Node], private_tip: usize| {
        if i == attacker { private_tip } else { nodes[i].head }
    };
    let schedule = |queue: &mut BinaryHeap<Reverse<(u64, Ev)>>,
                    rng: &mut StdRng,
                    generation: &mut Vec<u64>,
                    blocks: &[Block],
                    now: u64,
                    i: usize,
                    tip: usize| {
        let share = if i == attacker { sc.attacker } else { sc.honest[i] };
        if share <= 0.0 {
            return;
        }
        generation[i] += 1;
        let d = child_difficulty(blocks, tip);
        queue.push(Reverse((now + draw(rng, share, d), Ev::Mine(i, generation[i]))));
    };
    for i in 0..=n {
        if i == attacker && sc.attacker <= 0.0 {
            continue;
        }
        schedule(&mut queue, &mut rng, &mut generation, &blocks, 0, i, 0);
    }
    let partitioned = |a: usize, b: usize, now: u64| match sc.partition {
        Some((from, to, split)) => {
            let t = now as f64 / 1000.0;
            t >= from && t < to && ((a < split) != (b < split))
        }
        None => false,
    };
    let end = (sc.duration_s * 1000.0) as u64;

    // Fork choice at node i for a newly known block b. Returns the reorg depth if it switched.
    let consider = |nodes: &mut [Node],
                    blocks: &[Block],
                    i: usize,
                    b: usize,
                    finalized: usize,
                    refused: &mut usize|
     -> Option<u64> {
        let head = nodes[i].head;
        let (nb, hb) = (&blocks[b], &blocks[head]);
        let better = nb.td > hb.td || (nb.td == hb.td && nb.tag < hb.tag);
        if !better {
            return None;
        }
        // Fork point.
        let (mut x, mut y) = (b, head);
        while blocks[x].height > blocks[y].height {
            x = blocks[x].parent;
        }
        while blocks[y].height > blocks[x].height {
            y = blocks[y].parent;
        }
        while x != y {
            x = blocks[x].parent;
            y = blocks[y].parent;
        }
        let depth = hb.height - blocks[x].height;
        if depth > MAX_REORG_DEPTH || blocks[x].height < blocks[finalized].height {
            *refused += 1;
            return None;
        }
        nodes[i].head = b;
        Some(depth)
    };

    let mut finalized = 0usize;
    let ancestor_at = |blocks: &[Block], mut x: usize, h: u64| {
        while blocks[x].height > h {
            x = blocks[x].parent;
        }
        x
    };
    while let Some(Reverse((now, ev))) = queue.pop() {
        if now > end {
            break;
        }
        if let Some(d) = sc.finality_depth {
            let low = nodes.iter().map(|nd| blocks[nd.head].height).min().unwrap_or(0);
            if low > d {
                let h = low - d;
                let cand = ancestor_at(&blocks, nodes[0].head, h);
                if blocks[cand].height > blocks[finalized].height
                    && nodes.iter().all(|nd| ancestor_at(&blocks, nd.head, h) == cand)
                {
                    finalized = cand;
                }
            }
        }
        match ev {
            Ev::Mine(i, g) if g == generation[i] => {
                let parent = tip_of(i, &nodes, private_tip);
                let difficulty = child_difficulty(&blocks, parent);
                let p = &blocks[parent];
                let time = (now / 1000).max(p.time + 1);
                let b = blocks.len();
                blocks.push(Block {
                    parent,
                    height: p.height + 1,
                    time,
                    difficulty,
                    td: p.td + difficulty,
                    tag: rng.random(),
                    miner: i,
                });
                report.mined += 1;
                if i == attacker {
                    if let Attack::Private { start_s, .. } = sc.attack
                        && now as f64 / 1000.0 >= start_s
                    {
                        attacking = true;
                    }
                    private_tip = b;
                    private.push(b);
                    let public_best =
                        (0..n).map(|j| blocks[nodes[j].head].td).max().unwrap_or_default();
                    let release = match sc.attack {
                        Attack::Private { min_len, .. } => {
                            !attacking
                                || (private.len() as u64 >= min_len && blocks[b].td > public_best)
                        }
                        Attack::Selfish => false,
                        Attack::None => true,
                    };
                    if release {
                        for &pb in &private {
                            for j in 0..n {
                                queue.push(Reverse((now + 100, Ev::Deliver(j, pb))));
                            }
                        }
                        private.clear();
                    }
                    schedule(&mut queue, &mut rng, &mut generation, &blocks, now, i, private_tip);
                } else {
                    nodes[i].known.insert(b);
                    nodes[i].head = b;
                    for j in 0..n {
                        if j != i && !partitioned(i, j, now) {
                            let d = (sc.delay_s * rng.random_range(0.5..1.5) * 1000.0) as u64;
                            queue.push(Reverse((now + d, Ev::Deliver(j, b))));
                        }
                    }
                    schedule(&mut queue, &mut rng, &mut generation, &blocks, now, i, b);
                    // The attacker sees honest blocks.
                    if sc.attacker > 0.0 {
                        match sc.attack {
                            Attack::Private { start_s, .. } => {
                                if !attacking && now as f64 / 1000.0 >= start_s {
                                    attacking = true;
                                }
                                if !attacking {
                                    private_tip = b;
                                    schedule(
                                        &mut queue,
                                        &mut rng,
                                        &mut generation,
                                        &blocks,
                                        now,
                                        attacker,
                                        b,
                                    );
                                }
                            }
                            Attack::Selfish => {
                                // Honest block at height h: compare with the private lead.
                                let h = blocks[b].height;
                                let lead =
                                    blocks[private_tip].height.saturating_sub(h.saturating_sub(1));
                                if private.is_empty() || blocks[private_tip].height < h {
                                    // Adopt the honest chain.
                                    private.clear();
                                    private_tip = b;
                                    schedule(
                                        &mut queue,
                                        &mut rng,
                                        &mut generation,
                                        &blocks,
                                        now,
                                        attacker,
                                        b,
                                    );
                                } else {
                                    // Release enough to match or override (lead 2 -> release all).
                                    let release = if lead <= 2 { private.len() } else { 1 };
                                    let out: Vec<usize> = private.drain(..release).collect();
                                    for pb in out {
                                        for j in 0..n {
                                            queue.push(Reverse((now + 50, Ev::Deliver(j, pb))));
                                        }
                                    }
                                }
                            }
                            Attack::None => {}
                        }
                    }
                }
            }
            Ev::Mine(..) => {}
            Ev::Deliver(i, b) => {
                if nodes[i].known.contains(&b) {
                    continue;
                }
                // Need the parent first: deliver ancestors (as the IPFS walk-back does).
                let mut chain = vec![b];
                let mut x = blocks[b].parent;
                while !nodes[i].known.contains(&x) {
                    chain.push(x);
                    x = blocks[x].parent;
                }
                for &c in chain.iter().rev() {
                    nodes[i].known.insert(c);
                }
                let before = nodes[i].head;
                if let Some(depth) =
                    consider(&mut nodes, &blocks, i, b, finalized, &mut report.refused)
                {
                    report.max_reorg = report.max_reorg.max(depth);
                    if nodes[i].head != before {
                        schedule(
                            &mut queue,
                            &mut rng,
                            &mut generation,
                            &blocks,
                            now,
                            i,
                            nodes[i].head,
                        );
                        for (j, nj) in nodes.iter().enumerate() {
                            if j != i && !partitioned(i, j, now) && !nj.known.contains(&b) {
                                let d = (sc.delay_s * rng.random_range(0.5..1.5) * 1000.0) as u64;
                                queue.push(Reverse((now + d, Ev::Deliver(j, b))));
                            }
                        }
                    }
                }
            }
        }
    }

    // Final canonical chain from node 0's view.
    let mut canonical = HashSet::new();
    let mut x = nodes[0].head;
    report.height = blocks[x].height;
    let mut attacker_blocks = 0usize;
    while x != 0 {
        canonical.insert(x);
        if blocks[x].miner == attacker {
            attacker_blocks += 1;
        }
        x = blocks[x].parent;
    }
    report.orphan_rate = 1.0 - canonical.len() as f64 / report.mined.max(1) as f64;
    report.attacker_share = attacker_blocks as f64 / canonical.len().max(1) as f64;
    report.mean_interval_s = blocks[nodes[0].head].time as f64 / report.height.max(1) as f64;
    let low = nodes.iter().map(|nd| blocks[nd.head].height).min().unwrap_or(0).saturating_sub(6);
    let at = |mut x: usize, h: u64| {
        while blocks[x].height > h {
            x = blocks[x].parent;
        }
        x
    };
    let reference = at(nodes[0].head, low);
    report.agree = nodes.iter().all(|nd| at(nd.head, low) == reference);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn honest(k: usize) -> Vec<f64> {
        (0..k).map(|i| 1.0 + (i % 5) as f64).collect()
    }

    #[test]
    fn honest_miners_converge_with_12_second_blocks() {
        let sc = PowScenario::mainnet(1, honest(20), 50_000.0);
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        assert!(r.agree, "honest nodes agree on buried blocks");
        assert!(r.orphan_rate < 0.15, "orphans {}", r.orphan_rate);
        assert!((10.0..14.0).contains(&r.mean_interval_s), "interval {}", r.mean_interval_s);
        assert!(r.max_reorg <= 3);
    }

    #[test]
    fn asert_follows_hashrate_that_arrives_late() {
        // The chain launches expecting 10x the hashrate that shows up.
        let mut sc = PowScenario::mainnet(2, honest(10), 5_000.0);
        sc.asert.initial = U256::from(50_000u64 * 12);
        sc.duration_s = 24.0 * 3600.0;
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        assert!(r.agree);
        // Slow at first, then back near 12 s: the day's average stays within 2x.
        assert!(r.mean_interval_s < 24.0, "interval {}", r.mean_interval_s);
    }

    #[test]
    fn majority_attack_within_the_cap_rewrites_history() {
        // The known weakness of the mining phase (ADR 0007 "Known risks"): 60% of the hashrate
        // mining in private for about ten minutes replaces that stretch.
        let mut sc = PowScenario::mainnet(3, honest(10), 50_000.0);
        sc.attacker = 30.0 / 0.4 * 0.6; // 60% of the total (honest shares sum to 30)
        sc.attack = Attack::Private { start_s: 3600.0, min_len: 50 };
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        assert!(r.max_reorg >= 20, "honest nodes followed the heavier private chain");
        assert!(r.attacker_share > 0.1);
        assert_eq!(r.refused, 0);
    }

    #[test]
    fn majority_attack_beyond_the_cap_is_refused() {
        let mut sc = PowScenario::mainnet(4, honest(10), 50_000.0);
        sc.attacker = 30.0 / 0.4 * 0.6;
        sc.attack = Attack::Private { start_s: 1800.0, min_len: 200 };
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        assert!(r.refused > 0, "the deep private chain was refused");
        assert!(r.max_reorg <= MAX_REORG_DEPTH);
        assert!(r.agree, "honest nodes stay together on their own chain");
        assert!(r.attacker_share < 0.2, "attacker share {}", r.attacker_share);
    }

    #[test]
    fn selfish_mining_at_a_third_does_not_pay_much() {
        let mut sc = PowScenario::mainnet(5, honest(10), 50_000.0);
        sc.attacker = 30.0 / 0.67 * 0.33;
        sc.attack = Attack::Selfish;
        sc.duration_s = 12.0 * 3600.0;
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        // Theory (γ ≈ 0): break-even at 1/3. Allow noise, but no large windfall.
        assert!(r.attacker_share < 0.45, "selfish share {}", r.attacker_share);
        assert!(r.agree);
    }

    #[test]
    fn stake_finality_stops_the_majority_attack() {
        // The same 60% private attack that rewrites history in phase A: with checkpoints at
        // depth 32 finalized by the committee, the heavier branch forks below a checkpoint and
        // every honest node refuses it.
        let mut sc = PowScenario::mainnet(3, honest(10), 50_000.0);
        sc.attacker = 30.0 / 0.4 * 0.6;
        sc.attack = Attack::Private { start_s: 3600.0, min_len: 50 };
        sc.finality_depth = Some(32);
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        assert!(r.refused > 0, "the private branch was refused");
        assert!(r.max_reorg < 32, "no reorganisation reached a checkpoint: {}", r.max_reorg);
        assert!(r.agree);
        assert!(r.attacker_share < 0.2, "attacker share {}", r.attacker_share);
    }

    #[test]
    fn short_partition_heals_long_one_needs_an_operator() {
        // Ten minutes apart (~50 blocks): the lighter side reorganises when it reconnects.
        let mut sc = PowScenario::mainnet(6, honest(10), 50_000.0);
        sc.partition = Some((3600.0, 4200.0, 6));
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        assert!(r.agree, "one chain again");
        assert!(r.max_reorg >= 5 && r.max_reorg <= MAX_REORG_DEPTH);

        // An hour apart (~300 blocks): beyond the cap, both sides keep their chain and the
        // operators must decide (the node stops following and alerts, ADR 0007 §2).
        sc.partition = Some((3600.0, 7200.0, 6));
        sc.seed = 7;
        let r = run_pow(&sc);
        eprintln!("{r:?}");
        assert!(r.refused > 0);
        assert!(!r.agree, "split persists without intervention");
    }
}
