//! Operations: a Prometheus endpoint (`--metrics <addr>`: `/metrics`, `/health`) and a status
//! line in the log every minute, so testnet operators see what their node is doing.

use anyhow::Result;
use bolt_chain::Chain;
use bolt_net::NetHandle;
use bolt_primitives::metrics;
use bolt_txpool::TxPool;
use std::{net::SocketAddr, sync::Arc, time::Duration};

/// Node state sampled for gauges.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    /// Head block.
    pub head: u64,
    /// Seconds since the head's timestamp.
    pub head_age_secs: u64,
    /// Latest block final by stake (0 before phase B).
    pub finalized: u64,
    /// "mining", "checkpoints" or "pos" (for the next block).
    pub phase: &'static str,
    /// Difficulty of the head (0 under PoS).
    pub difficulty: f64,
    /// Connected peers.
    pub peers: u64,
    /// Pooled transactions.
    pub pool: usize,
    /// First block with local state history and body (0 unless started from a snapshot).
    pub base: u64,
    /// Latest local snapshot.
    pub snapshot: u64,
    /// Chain id.
    pub chain_id: u64,
}

/// Samples the node's state.
pub fn status(chain: &Chain, pool: Option<&TxPool>) -> Status {
    let mut s = Status {
        chain_id: chain.config().chain_id,
        peers: metrics::PEERS.get(),
        pool: pool.map(|p| p.len()).unwrap_or(0),
        ..Default::default()
    };
    if let Ok(h) = chain.head() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        s.head = h.number;
        s.head_age_secs = now.saturating_sub(h.timestamp);
        s.difficulty = f64::from(h.difficulty);
        s.phase = match chain.phase() {
            Ok(p) if p.is_pos(chain.rules(), h.number + 1) => "pos",
            Ok(p) if p.is_checkpointing(chain.rules(), h.number + 1) => "checkpoints",
            _ => "mining",
        };
    }
    s.finalized = chain.finalized().ok().flatten().map(|(n, _)| n).unwrap_or(0);
    s.base = chain.store().reader().ok().and_then(|r| r.base().ok()).unwrap_or(0);
    s.snapshot = chain.snapshots().ok().and_then(|v| v.last().map(|(n, _)| *n)).unwrap_or(0);
    s
}

/// Serves `/metrics` (Prometheus text) and `/health` (JSON) on `addr`.
pub async fn start(
    addr: SocketAddr,
    chain: Arc<Chain>,
    pool: Option<Arc<TxPool>>,
) -> Result<SocketAddr> {
    let handler = move |req: &str| {
        let path = req.split_whitespace().nth(1).unwrap_or("/");
        let s = status(&chain, pool.as_deref());
        match path {
            "/metrics" => {
                let phase = match s.phase {
                    "pos" => 2.0,
                    "checkpoints" => 1.0,
                    _ => 0.0,
                };
                let body = metrics::render(&[
                    ("head", "Head block number", s.head as f64),
                    (
                        "head_age_seconds",
                        "Seconds since the head block's timestamp",
                        s.head_age_secs as f64,
                    ),
                    ("finalized", "Latest block final by stake", s.finalized as f64),
                    ("phase", "0 mining, 1 mining with stake checkpoints, 2 PoS", phase),
                    ("difficulty", "Difficulty of the head block", s.difficulty),
                    ("pool_txs", "Pooled transactions", s.pool as f64),
                    ("history_base", "First block with local history", s.base as f64),
                    ("snapshot", "Latest local state snapshot", s.snapshot as f64),
                ]);
                (200, "text/plain; version=0.0.4", body.into_bytes())
            }
            "/health" => (200, "application/json", serde_json::to_vec(&s).unwrap_or_default()),
            _ => crate::gateway::text(404, "try /metrics or /health"),
        }
    };
    let bound = crate::gateway::serve(addr, Arc::new(handler)).await?;
    tracing::info!(%bound, "metrics listening");
    Ok(bound)
}

/// Keeps the peer gauge current and logs a status line every minute (with the miner's
/// hashrate when mining).
pub fn spawn_status(chain: Arc<Chain>, net: NetHandle, pool: Option<Arc<TxPool>>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut n = 0u64;
        let mut last_hashes = metrics::HASHES.get();
        let mut last = std::time::Instant::now();
        loop {
            tick.tick().await;
            metrics::PEERS.set(net.peers().await.len() as u64);
            n += 1;
            if !n.is_multiple_of(12) {
                continue;
            }
            let (c, p) = (chain.clone(), pool.clone());
            let Ok(s) = tokio::task::spawn_blocking(move || status(&c, p.as_deref())).await else {
                continue;
            };
            let hashes = metrics::HASHES.get();
            let rate = (hashes - last_hashes) as f64 / last.elapsed().as_secs_f64();
            (last_hashes, last) = (hashes, std::time::Instant::now());
            tracing::info!(
                head = s.head,
                age = s.head_age_secs,
                finalized = s.finalized,
                phase = s.phase,
                peers = s.peers,
                pool = s.pool,
                hashrate = format!("{rate:.0} H/s"),
                imported = metrics::BLOCKS_IMPORTED.get(),
                mined = metrics::BLOCKS_MINED.get(),
                "status"
            );
        }
    });
}
