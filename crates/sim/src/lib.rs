//! Deterministic discrete-event simulator for the consensus engine.
//!
//! Every scenario is reproducible from its seed. The simulator models message delays, drops and
//! network partitions before a global stabilisation time (GST), crashed and recovering nodes, and
//! byzantine validators that equivocate as leaders, vote for everything, or stay silent. It
//! checks:
//!
//! * **Safety** (always): no two honest nodes ever finalize different blocks at the same height.
//! * **Liveness** (when every committee's faulty seats are under 1/3 and the network is
//!   synchronous after GST): honest nodes keep finalizing blocks after GST.
//!
//! Multi-epoch scenarios (ADR 0006) give every epoch a different committee (a random subset of the
//! nodes with random seat weights). Nodes switch engines when their epoch's last block is final,
//! buffer messages from future epochs, and a node that missed the end of an epoch (crashed,
//! partitioned, not in the committee) catches up from the published epoch proof, as a real node
//! does from the finality proofs in announcements.

pub mod pow;

use alloy_primitives::{B256, keccak256};
use bolt_consensus::{
    Action, BlockInfo, CommitProof, Config, Engine, Message, MockScheme, Scheme, ValidatorIndex,
    ValidatorSet, proposal_msg,
};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashMap},
};

/// Byzantine behaviours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Byzantine {
    /// Never sends anything.
    Silent,
    /// As leader, sends conflicting proposals to different halves of the network.
    Equivocate,
    /// Votes for every proposal it sees, ignoring the rules, and times out at random.
    VoteForAll,
    /// Proposes blocks that fail validation (bad data).
    InvalidBlocks,
    /// Colluding attacker aimed at the locking rule: as a leader it sends its proposal (and so the
    /// QC it carries) to a single honest validator only, and after a timeout it proposes a fork
    /// from genesis justified by the TC, hoping honest validators forget what they locked on.
    Adversarial,
}

/// A scenario.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Seed (everything random derives from it).
    pub seed: u64,
    /// Validators.
    pub n: usize,
    /// Byzantine validators and their behaviour.
    pub byzantine: Vec<(ValidatorIndex, Byzantine)>,
    /// Honest validators that crash at `at_ms` and optionally recover.
    pub crashes: Vec<(ValidatorIndex, u64, Option<u64>)>,
    /// Global stabilisation time.
    pub gst_ms: u64,
    /// Message loss probability before GST.
    pub drop_before_gst: f64,
    /// Network partition before GST: nodes are split into two sides by index parity.
    pub partition_until_ms: u64,
    /// Message delay range after GST (before GST delays can be up to 10x).
    pub delay_ms: (u64, u64),
    /// Base round timeout.
    pub timeout_ms: u64,
    /// Simulated duration.
    pub duration_ms: u64,
    /// Blocks per epoch (`u64::MAX`: a single epoch).
    pub epoch_len: u64,
    /// Committee of each epoch (the last one repeats).
    pub committees: Vec<SimCommittee>,
}

/// A committee: which nodes sit on it and the leader order over its seats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimCommittee {
    /// Node id of each member (member index = position).
    pub members: Vec<usize>,
    /// Member index of each seat, in leader order.
    pub seats: Vec<ValidatorIndex>,
}

impl SimCommittee {
    /// All `n` nodes, one seat each.
    pub fn all(n: usize) -> Self {
        Self { members: (0..n).collect(), seats: (0..n as ValidatorIndex).collect() }
    }

    fn set(&self) -> ValidatorSet {
        ValidatorSet::from_seats(self.members.len(), self.seats.clone())
    }

    fn index_of(&self, node: usize) -> Option<ValidatorIndex> {
        self.members.iter().position(|m| *m == node).map(|i| i as ValidatorIndex)
    }
}

impl Scenario {
    /// Nodes that count as faulty for liveness: byzantine, or crashed and not recovered by GST.
    fn faulty(&self) -> Vec<usize> {
        let mut f: Vec<usize> = self.byzantine.iter().map(|(b, _)| *b as usize).collect();
        f.extend(
            self.crashes
                .iter()
                .filter(|(_, _, rec)| rec.is_none_or(|r| r > self.gst_ms))
                .map(|(c, _, _)| *c as usize),
        );
        f
    }

    /// Committee of `epoch`.
    pub fn committee(&self, epoch: u64) -> &SimCommittee {
        &self.committees[(epoch as usize).min(self.committees.len() - 1)]
    }

    /// Whether liveness must hold: in every committee, faulty seats are under a third.
    pub fn expects_liveness(&self) -> bool {
        let faulty = self.faulty();
        self.committees.iter().all(|c| {
            let set = c.set();
            let bad: Vec<ValidatorIndex> = faulty.iter().filter_map(|f| c.index_of(*f)).collect();
            set.weight_of(&bad) * 3 < set.total()
        })
    }

    /// Random scenario for `seed`.
    pub fn random(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let n = rng.random_range(4..=10);
        let f = (n - 1) / 3;
        // Up to f byzantine; sometimes more faults than f (safety-only scenarios, via crashes).
        let nbyz = rng.random_range(0..=f);
        let mut idx: Vec<ValidatorIndex> = (0..n as ValidatorIndex).collect();
        let colluding = nbyz >= 1 && rng.random_bool(0.35);
        if colluding {
            // Consecutive leaders, so the attacker controls back-to-back rounds.
            let start = rng.random_range(0..n);
            idx = (0..n).map(|k| ((start + k) % n) as ValidatorIndex).collect();
        } else {
            for i in (1..idx.len()).rev() {
                let j = rng.random_range(0..=i);
                idx.swap(i, j);
            }
        }
        let behaviours = [
            Byzantine::Silent,
            Byzantine::Equivocate,
            Byzantine::VoteForAll,
            Byzantine::InvalidBlocks,
        ];
        let byzantine: Vec<_> = idx[..nbyz]
            .iter()
            .map(|i| {
                let b = if colluding {
                    Byzantine::Adversarial
                } else {
                    behaviours[rng.random_range(0..behaviours.len())]
                };
                (*i, b)
            })
            .collect();
        let gst_ms = rng.random_range(0..=8_000);
        let ncrash = rng.random_range(0..=(f + 1).min(n - nbyz));
        let crashes = idx[nbyz..nbyz + ncrash]
            .iter()
            .map(|i| {
                let at = rng.random_range(0..10_000);
                let rec = if rng.random_bool(0.6) {
                    Some(at + rng.random_range(500..8_000))
                } else {
                    None
                };
                (*i, at, rec)
            })
            .collect();
        let dmin = rng.random_range(1..60);
        // Half the scenarios change committees every few blocks.
        let (epoch_len, committees) = if rng.random_bool(0.5) {
            let len = rng.random_range(3..=12);
            let committees = (0..8)
                .map(|_| {
                    let size = rng.random_range(n.min(4).max(n.saturating_sub(3))..=n);
                    let mut nodes: Vec<usize> = (0..n).collect();
                    for i in (1..nodes.len()).rev() {
                        let j = rng.random_range(0..=i);
                        nodes.swap(i, j);
                    }
                    nodes.truncate(size);
                    let mut seats: Vec<ValidatorIndex> = Vec::new();
                    for m in 0..size {
                        for _ in 0..rng.random_range(1..=3) {
                            seats.push(m as ValidatorIndex);
                        }
                    }
                    // BFT safety assumes byzantine seats below 1/3 in every committee (crashes
                    // do not matter for safety): top up honest members until that holds.
                    let is_byz = |m: usize| byzantine.iter().any(|(b, _)| *b as usize == nodes[m]);
                    let honest: Vec<usize> = (0..size).filter(|m| !is_byz(*m)).collect();
                    loop {
                        let bad = seats.iter().filter(|s| is_byz(**s as usize)).count();
                        if bad * 3 < seats.len() || honest.is_empty() {
                            break;
                        }
                        seats.push(honest[rng.random_range(0..honest.len())] as ValidatorIndex);
                    }
                    for i in (1..seats.len()).rev() {
                        let j = rng.random_range(0..=i);
                        seats.swap(i, j);
                    }
                    SimCommittee { members: nodes, seats }
                })
                .collect();
            (len, committees)
        } else {
            (u64::MAX, vec![SimCommittee::all(n)])
        };
        Self {
            seed,
            n,
            byzantine,
            crashes,
            gst_ms,
            drop_before_gst: rng.random_range(0.0..0.3),
            partition_until_ms: if rng.random_bool(0.3) {
                rng.random_range(0..=gst_ms.max(1))
            } else {
                0
            },
            delay_ms: (dmin, dmin + rng.random_range(1..150)),
            timeout_ms: 1_000,
            duration_ms: 60_000,
            epoch_len,
            committees,
        }
    }
}

/// Outcome of a scenario.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Safety violations (should always be empty).
    pub safety_violations: Vec<String>,
    /// Lowest committed height among honest, up nodes at the end.
    pub min_honest_height: u64,
    /// Committed height reached by honest nodes at GST.
    pub height_at_gst: u64,
    /// Whether liveness was required and achieved.
    pub liveness_ok: bool,
    /// Messages delivered.
    pub messages: u64,
    /// Highest epoch any honest node reached.
    pub max_epoch: u64,
}

#[derive(Debug, Clone)]
enum Event {
    Deliver {
        to: usize,
        msg: Message<MockScheme>,
    },
    Timer {
        node: usize,
        round: u64,
    },
    Validated {
        node: usize,
        block: B256,
        ok: bool,
    },
    Built {
        node: usize,
        block: BlockInfo,
        qc: bolt_consensus::Qc<MockScheme>,
        tc: Option<bolt_consensus::Tc<MockScheme>>,
        payload: Vec<u8>,
    },
    Fetched {
        node: usize,
        block: BlockInfo,
    },
    Crash {
        node: usize,
    },
    Recover {
        node: usize,
    },
}

struct Node {
    engine: Engine<MockScheme>,
    byz: Option<Byzantine>,
    up: bool,
    /// height -> hash committed by this node.
    committed: BTreeMap<u64, B256>,
    /// Messages for epochs this node has not reached yet.
    future: Vec<Message<MockScheme>>,
}

fn engine_for(sc: &Scenario, node: usize, epoch: u64, anchor: BlockInfo) -> Engine<MockScheme> {
    let c = sc.committee(epoch);
    let end_height = if sc.epoch_len == u64::MAX { u64::MAX } else { anchor.height + sc.epoch_len };
    Engine::new(
        Config {
            chain_id: 1337,
            epoch,
            validators: c.set(),
            me: c.index_of(node).into_iter().collect(),
            anchor,
            end_height,
            base_timeout_ms: sc.timeout_ms,
        },
        MockScheme,
    )
}

/// Epoch proofs published by nodes (the announcement path), by epoch.
type Proofs = BTreeMap<u64, (BlockInfo, CommitProof<MockScheme>)>;

/// Moves `node` into the next epoch(s) using published proofs; returns the start actions.
fn catch_up(
    sc: &Scenario,
    node: &mut Node,
    id: usize,
    proofs: &Proofs,
    canonical: &mut HashMap<u64, B256>,
    report: &mut Report,
) -> Vec<Action<MockScheme>> {
    let mut out = Vec::new();
    while let Some((block, proof)) = proofs.get(&node.engine.config().epoch) {
        let cfg = node.engine.config().clone();
        // Verify as a real node would (committee of that epoch, its anchor).
        if proof.verify(&MockScheme, &cfg.validators, 1337, &cfg.anchor.hash) != Some(block.hash)
            || block.height != cfg.end_height
        {
            report.safety_violations.push(format!(
                "seed {}: bad epoch proof accepted for epoch {}",
                sc.seed, cfg.epoch
            ));
            break;
        }
        record_commit(sc, node, id, block, canonical, report);
        let next = cfg.epoch + 1;
        node.engine = engine_for(sc, id, next, block.clone());
        out.extend(node.engine.start());
        let (now, later): (Vec<_>, Vec<_>) =
            std::mem::take(&mut node.future).into_iter().partition(|m| m.epoch() == next);
        node.future = later.into_iter().filter(|m| m.epoch() > next).collect();
        for m in now {
            out.extend(node.engine.on_message(m));
        }
    }
    out
}

fn record_commit(
    sc: &Scenario,
    node: &mut Node,
    id: usize,
    b: &BlockInfo,
    canonical: &mut HashMap<u64, B256>,
    report: &mut Report,
) {
    if node.byz.is_none() {
        match canonical.get(&b.height) {
            Some(h) if *h != b.hash => report.safety_violations.push(format!(
                "seed {}: node {id} committed {} at height {} but {} was committed before",
                sc.seed, b.hash, b.height, h
            )),
            None => {
                canonical.insert(b.height, b.hash);
            }
            _ => {}
        }
    }
    node.committed.insert(b.height, b.hash);
}

/// Runs one scenario.
pub fn run(sc: &Scenario) -> Report {
    let genesis = keccak256(b"sim-genesis");
    let anchor = BlockInfo { hash: genesis, parent: B256::ZERO, round: 0, height: 0 };
    let mut rng = StdRng::seed_from_u64(sc.seed ^ 0x5eed);
    let mut nodes: Vec<Node> = (0..sc.n)
        .map(|i| Node {
            engine: engine_for(sc, i, 0, anchor.clone()),
            byz: sc.byzantine.iter().find(|(b, _)| *b as usize == i).map(|(_, k)| *k),
            up: true,
            committed: BTreeMap::new(),
            future: Vec::new(),
        })
        .collect();
    let mut proofs: Proofs = BTreeMap::new();

    let mut queue: BinaryHeap<Reverse<(u64, u64, usize)>> = BinaryHeap::new();
    let mut events: Vec<Option<Event>> = Vec::new();
    let mut seq = 0u64;
    let mut push = |queue: &mut BinaryHeap<Reverse<(u64, u64, usize)>>,
                    events: &mut Vec<Option<Event>>,
                    at: u64,
                    e: Event| {
        events.push(Some(e));
        queue.push(Reverse((at, seq, events.len() - 1)));
        seq += 1;
    };
    for (node, at, rec) in &sc.crashes {
        push(&mut queue, &mut events, *at, Event::Crash { node: *node as usize });
        if let Some(r) = rec {
            push(&mut queue, &mut events, *r, Event::Recover { node: *node as usize });
        }
    }

    let mut report = Report::default();
    let mut canonical: HashMap<u64, B256> = HashMap::new();
    let mut archive: HashMap<B256, BlockInfo> = HashMap::new();
    let mut pending_actions: Vec<(usize, u64, Vec<Action<MockScheme>>)> = Vec::new();
    let mut pending_later: Vec<(usize, u64, Vec<Action<MockScheme>>)> = Vec::new();
    for (i, n) in nodes.iter_mut().enumerate() {
        if n.byz != Some(Byzantine::Silent) {
            let acts = n.engine.start();
            pending_actions.push((i, 0, acts));
        }
    }

    let trace = std::env::var("SIM_TRACE").is_ok();
    let trace_all = std::env::var("SIM_TRACE_ALL").is_ok();
    let mut now = 0u64;
    loop {
        // Apply actions produced at `now`.
        while let Some((i, t, acts)) = pending_actions.pop() {
            for a in acts {
                handle_action(
                    sc,
                    &mut proofs,
                    &mut nodes,
                    &mut rng,
                    i,
                    t,
                    a,
                    &mut |at, e| push(&mut queue, &mut events, at, e),
                    &mut canonical,
                    &mut report,
                    &mut archive,
                    &mut pending_later,
                );
            }
            pending_actions.append(&mut pending_later);
        }
        let Some(Reverse((at, _, idx))) = queue.pop() else { break };
        if at > sc.duration_ms {
            break;
        }
        if now < sc.gst_ms && at >= sc.gst_ms {
            report.height_at_gst = honest_min_height(sc, &nodes);
        }
        now = at;
        let Some(ev) = events[idx].take() else { continue };
        if trace_all {
            eprintln!(
                "t={now} {:?}",
                match &ev {
                    Event::Deliver { to, msg } => format!(
                        "deliver to {to} {}",
                        match msg {
                            Message::Proposal(p) => format!("proposal r{}", p.block.round),
                            Message::Vote(v) => format!("vote r{} from {}", v.round, v.signer),
                            Message::Timeout(t) =>
                                format!("timeout r{} from {}", t.round, t.signer),
                        }
                    ),
                    Event::Timer { node, round } =>
                        format!("timer node {node} r{round} (at r{})", nodes[*node].engine.round()),
                    e => format!("{e:?}").chars().take(80).collect(),
                }
            );
        }
        match ev {
            Event::Crash { node } => nodes[node].up = false,
            Event::Recover { node } => {
                nodes[node].up = true;
                // A recovered node re-arms its timer; state was kept (crash-recovery with durable
                // consensus state, which the node persists).
                let r = nodes[node].engine.round();
                pending_actions.push((
                    node,
                    now,
                    vec![Action::ScheduleTimeout { round: r, after_ms: sc.timeout_ms }],
                ));
                let acts =
                    catch_up(sc, &mut nodes[node], node, &proofs, &mut canonical, &mut report);
                pending_actions.push((node, now, acts));
            }
            Event::Deliver { to, msg } => {
                if !nodes[to].up || nodes[to].byz == Some(Byzantine::Silent) {
                    continue;
                }
                report.messages += 1;
                let epoch = nodes[to].engine.config().epoch;
                if msg.epoch() > epoch {
                    nodes[to].future.push(msg);
                    continue;
                }
                if nodes[to].byz == Some(Byzantine::VoteForAll)
                    && msg.epoch() == epoch
                    && let Message::Proposal(p) = &msg
                    && let Some(me) = nodes[to].engine.config().me.first().copied()
                {
                    // Vote for anything, ignoring the rules.
                    let v = bolt_consensus::Vote {
                        epoch,
                        round: p.block.round,
                        block: p.block.hash,
                        height: p.block.height,
                        signer: me,
                        sig: MockScheme.sign(
                            me,
                            &bolt_consensus::vote_msg(1337, epoch, p.block.round, &p.block.hash),
                        ),
                    };
                    let next = nodes[to].engine.config().validators.leader(p.block.round + 1);
                    pending_actions.push((to, now, vec![Action::SendTo(next, Message::Vote(v))]));
                }
                let acts = nodes[to].engine.on_message(msg);
                pending_actions.push((to, now, acts));
            }
            Event::Timer { node, round } => {
                if !nodes[node].up || nodes[node].byz == Some(Byzantine::Silent) {
                    continue;
                }
                if trace && round == nodes[node].engine.round() {
                    eprintln!(
                        "t={now} node {node} timeout in round {round} committed {}",
                        nodes[node].engine.committed().height
                    );
                }
                let acts = nodes[node].engine.on_timeout(round);
                pending_actions.push((node, now, acts));
                // A node stuck in an old epoch picks up the published proof (announcement path).
                if proofs.contains_key(&nodes[node].engine.config().epoch) {
                    let acts =
                        catch_up(sc, &mut nodes[node], node, &proofs, &mut canonical, &mut report);
                    pending_actions.push((node, now, acts));
                }
            }
            Event::Validated { node, block, ok } => {
                if !nodes[node].up {
                    continue;
                }
                let acts = nodes[node].engine.on_validated(block, ok);
                pending_actions.push((node, now, acts));
            }
            Event::Fetched { node, block } => {
                if !nodes[node].up {
                    continue;
                }
                let acts = nodes[node].engine.on_block(block);
                pending_actions.push((node, now, acts));
            }
            Event::Built { node, block, qc, tc, payload } => {
                if !nodes[node].up {
                    continue;
                }
                let acts = nodes[node].engine.on_proposed(block, qc, tc, payload);
                pending_actions.push((node, now, acts));
            }
        }
    }

    if trace {
        for (i, n) in nodes.iter().enumerate() {
            eprintln!(
                "node {i}: round {} committed {} high_qc {}",
                n.engine.round(),
                n.engine.committed().height,
                n.engine.high_qc().round
            );
        }
        eprintln!("messages {} end time {now}", report.messages);
    }
    report.min_honest_height = honest_min_height(sc, &nodes);
    report.max_epoch = nodes
        .iter()
        .filter(|n| n.byz.is_none())
        .map(|n| n.engine.config().epoch)
        .max()
        .unwrap_or(0);
    if sc.gst_ms >= sc.duration_ms {
        report.height_at_gst = report.min_honest_height;
    }
    // Liveness: after GST, every honest node that is up keeps finalizing blocks.
    let window_ok = report.min_honest_height >= report.height_at_gst + 2;
    report.liveness_ok = !sc.expects_liveness() || window_ok;
    report
}

fn honest_min_height(sc: &Scenario, nodes: &[Node]) -> u64 {
    nodes
        .iter()
        .enumerate()
        .filter(|(i, n)| {
            n.byz.is_none()
                && n.up
                && !sc.crashes.iter().any(|(c, _, r)| *c as usize == *i && r.is_none())
        })
        .map(|(_, n)| n.committed.keys().next_back().copied().unwrap_or(0))
        .min()
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn handle_action(
    sc: &Scenario,
    proofs: &mut Proofs,
    nodes: &mut [Node],
    rng: &mut StdRng,
    from: usize,
    now: u64,
    action: Action<MockScheme>,
    push: &mut dyn FnMut(u64, Event),
    canonical: &mut HashMap<u64, B256>,
    report: &mut Report,
    archive: &mut HashMap<B256, BlockInfo>,
    later: &mut Vec<(usize, u64, Vec<Action<MockScheme>>)>,
) {
    let _ = nodes[from].engine.config().epoch;
    let send = |rng: &mut StdRng,
                to: usize,
                msg: Message<MockScheme>,
                push: &mut dyn FnMut(u64, Event)| {
        if to == from {
            return;
        }
        let before_gst = now < sc.gst_ms;
        if before_gst {
            if rng.random_bool(sc.drop_before_gst) {
                return;
            }
            if now < sc.partition_until_ms && (to % 2) != (from % 2) {
                return;
            }
        }
        let (lo, hi) = sc.delay_ms;
        let delay =
            if before_gst { rng.random_range(lo..=hi * 10) } else { rng.random_range(lo..=hi) };
        push(now + delay, Event::Deliver { to, msg });
    };
    match action {
        Action::Broadcast(msg) => {
            if let (Some(Byzantine::Equivocate), Message::Proposal(p)) = (nodes[from].byz, &msg) {
                // Conflicting twin with a different hash, signed properly by this node.
                let mut twin = p.clone();
                twin.block.hash = keccak256([p.block.hash.as_slice(), b"twin"].concat());
                twin.sig = MockScheme.sign(
                    p.proposer,
                    &proposal_msg(1337, p.epoch, twin.block.round, &twin.block.hash, &twin.payload),
                );
                // Half the network gets one version; everyone gets the other too, later or
                // earlier at random, so honest validators see both.
                for to in 0..nodes.len() {
                    let (a, b) = if to % 2 == 0 {
                        (msg.clone(), Message::Proposal(twin.clone()))
                    } else {
                        (Message::Proposal(twin.clone()), msg.clone())
                    };
                    send(rng, to, a, push);
                    if rng.random_bool(0.5) {
                        send(rng, to, b, push);
                    }
                }
                return;
            }
            if let (Some(Byzantine::Adversarial), Message::Proposal(p)) = (nodes[from].byz, &msg)
                && p.tc.is_none()
            {
                // Withhold: one honest validator and the fellow attackers see the proposal.
                let honest: Vec<usize> =
                    (0..nodes.len()).filter(|i| nodes[*i].byz.is_none()).collect();
                if !honest.is_empty() {
                    let lucky = honest[rng.random_range(0..honest.len())];
                    send(rng, lucky, msg.clone(), push);
                }
                for to in (0..nodes.len()).filter(|i| nodes[*i].byz == Some(Byzantine::Adversarial))
                {
                    send(rng, to, msg.clone(), push);
                }
                return;
            }
            for to in 0..nodes.len() {
                send(rng, to, msg.clone(), push);
            }
        }
        Action::SendTo(to, msg) => {
            // The index refers to the committee of the message's epoch (the node may have moved on).
            let node = sc.committee(msg.epoch()).members[to as usize];
            send(rng, node, msg, push)
        }
        Action::Validate(p) => {
            let ok = p.payload.as_slice() != b"invalid";
            push(
                now + rng.random_range(1..=20),
                Event::Validated { node: from, block: p.block.hash, ok },
            );
        }
        Action::Propose { round, mut qc, tc } => {
            let epoch = qc.epoch;
            if nodes[from].byz == Some(Byzantine::Adversarial)
                && tc.is_some()
                && nodes[from].engine.config().epoch == epoch
            {
                // Fork from the epoch's anchor, justified only by the TC.
                let a = &nodes[from].engine.config().anchor;
                qc = bolt_consensus::Qc::anchor(epoch, a.hash, a.height);
            }
            let payload = if nodes[from].byz == Some(Byzantine::InvalidBlocks) {
                b"invalid".to_vec()
            } else {
                vec![]
            };
            let hash = keccak256(
                [
                    &(from as u64).to_be_bytes()[..],
                    &epoch.to_be_bytes(),
                    &round.to_be_bytes(),
                    qc.block.as_slice(),
                ]
                .concat(),
            );
            let block = BlockInfo { hash, parent: qc.block, round, height: qc.height + 1 };
            archive.insert(hash, block.clone());
            // An equivocator's twin is published too.
            archive.insert(
                keccak256([hash.as_slice(), b"twin"].concat()),
                BlockInfo { hash: keccak256([hash.as_slice(), b"twin"].concat()), ..block.clone() },
            );
            push(
                now + rng.random_range(1..=30),
                Event::Built { node: from, block, qc, tc, payload },
            );
        }
        Action::Commit { blocks, .. } => {
            for b in blocks {
                if b.height > nodes[from].engine.config().end_height {
                    report
                        .safety_violations
                        .push(format!("seed {}: nil block {} committed as real", sc.seed, b.hash));
                }
                record_commit(sc, &mut nodes[from], from, &b, canonical, report);
            }
        }
        Action::EpochEnd { block, proof } => {
            proofs.entry(proof.qc.epoch).or_insert((block, proof));
            let acts = catch_up(sc, &mut nodes[from], from, proofs, canonical, report);
            later.push((from, now, acts));
        }
        Action::FetchBlock(hash) => {
            // Blocks are content-addressed and published to IPFS when built, so any block that was
            // ever proposed can be fetched.
            if let Some(b) = archive.get(&hash).cloned() {
                push(now + rng.random_range(20..=200), Event::Fetched { node: from, block: b });
            }
        }
        Action::ScheduleTimeout { round, after_ms } => {
            if nodes[from].byz == Some(Byzantine::VoteForAll) && rng.random_bool(0.3) {
                // Times out early at random.
                push(now + after_ms / 4, Event::Timer { node: from, round });
            } else {
                push(now + after_ms, Event::Timer { node: from, round });
            }
        }
    }
}

/// Summary over many scenarios.
#[derive(Debug, Default)]
pub struct Summary {
    /// Scenarios run.
    pub scenarios: u64,
    /// Scenarios where liveness was required.
    pub liveness_required: u64,
    /// Liveness failures (seeds).
    pub liveness_failures: Vec<u64>,
    /// Safety violations.
    pub safety_violations: Vec<String>,
    /// Messages delivered in total.
    pub messages: u64,
    /// Scenarios with committee rotation.
    pub multi_epoch: u64,
    /// Epoch transitions completed (highest honest epoch, summed over scenarios).
    pub epoch_transitions: u64,
}

/// Runs scenarios `first..first + count` and aggregates.
pub fn run_many(first: u64, count: u64) -> Summary {
    let mut s = Summary::default();
    for seed in first..first + count {
        let sc = Scenario::random(seed);
        let r = run(&sc);
        s.scenarios += 1;
        s.messages += r.messages;
        if sc.epoch_len != u64::MAX {
            s.multi_epoch += 1;
            s.epoch_transitions += r.max_epoch;
        }
        if sc.expects_liveness() {
            s.liveness_required += 1;
            if !r.liveness_ok {
                s.liveness_failures.push(seed);
            }
        }
        s.safety_violations.extend(r.safety_violations);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_commits_every_round() {
        let sc = Scenario {
            seed: 1,
            n: 4,
            byzantine: vec![],
            crashes: vec![],
            gst_ms: 0,
            drop_before_gst: 0.0,
            partition_until_ms: 0,
            delay_ms: (5, 20),
            timeout_ms: 1_000,
            duration_ms: 10_000,
            epoch_len: u64::MAX,
            committees: vec![SimCommittee::all(4)],
        };
        let r = run(&sc);
        assert!(r.safety_violations.is_empty());
        assert!(r.min_honest_height > 50, "only {} blocks", r.min_honest_height);
    }

    #[test]
    fn survives_f_crashed_and_equivocating_leader() {
        let sc = Scenario {
            seed: 2,
            n: 7,
            byzantine: vec![(3, Byzantine::Equivocate)],
            crashes: vec![(5, 2_000, None)],
            gst_ms: 0,
            drop_before_gst: 0.0,
            partition_until_ms: 0,
            delay_ms: (5, 50),
            timeout_ms: 1_000,
            duration_ms: 20_000,
            epoch_len: u64::MAX,
            committees: vec![SimCommittee::all(7)],
        };
        let r = run(&sc);
        assert!(r.safety_violations.is_empty(), "{:?}", r.safety_violations);
        assert!(r.liveness_ok);
        assert!(r.min_honest_height > 10, "only {}", r.min_honest_height);
    }

    #[test]
    fn committees_rotate_every_few_blocks() {
        // 6 nodes; each epoch (5 blocks) a different committee with uneven seats.
        let committees = vec![
            SimCommittee { members: vec![0, 1, 2, 3], seats: vec![0, 1, 2, 3] },
            SimCommittee { members: vec![2, 3, 4, 5], seats: vec![0, 0, 1, 2, 3] },
            SimCommittee { members: vec![5, 0, 1, 4], seats: vec![3, 2, 1, 0, 0] },
            SimCommittee { members: vec![1, 2, 3, 4, 5], seats: vec![0, 1, 2, 3, 4] },
        ];
        let sc = Scenario {
            seed: 3,
            n: 6,
            byzantine: vec![(4, Byzantine::Equivocate)],
            crashes: vec![(2, 3_000, Some(9_000))],
            gst_ms: 0,
            drop_before_gst: 0.0,
            partition_until_ms: 0,
            delay_ms: (5, 40),
            timeout_ms: 1_000,
            duration_ms: 40_000,
            epoch_len: 5,
            committees,
        };
        let r = run(&sc);
        assert!(r.safety_violations.is_empty(), "{:?}", r.safety_violations);
        assert!(r.max_epoch >= 4, "reached epoch {}", r.max_epoch);
        assert!(r.min_honest_height >= 20, "only {}", r.min_honest_height);
    }

    #[test]
    fn random_scenarios_quick() {
        let s = run_many(1_000, 150);
        assert!(
            s.safety_violations.is_empty(),
            "{:?}",
            &s.safety_violations[..s.safety_violations.len().min(5)]
        );
        assert!(
            s.liveness_failures.is_empty(),
            "liveness failed for seeds {:?}",
            s.liveness_failures
        );
    }
}

#[cfg(test)]
mod debug {
    /// `SIM_SEED=<n> cargo test -p bolt-sim debug_seed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn debug_seed() {
        let seed: u64 = std::env::var("SIM_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        let sc = super::Scenario::random(seed);
        println!("{sc:#?}");
        let r = super::run(&sc);
        println!("{r:#?}");
    }
}
