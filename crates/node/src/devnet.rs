//! Single-producer development network (M1): one node produces every block.
//! M2 adds followers that sync over IPFS; M3 replaces the single producer with HotStuff-2.

use alloy_primitives::Address;
use anyhow::{Context, Result};
use bolt_chain::Chain;
use bolt_primitives::Genesis;
use bolt_rpc::{HeadState, RpcContext};
use bolt_txpool::{PoolConfig, TxPool};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

/// Devnet options.
#[derive(Debug, clap::Args)]
pub struct DevnetArgs {
    /// Genesis file.
    #[arg(long, default_value = "genesis/dev.json")]
    pub genesis: PathBuf,
    /// Data directory.
    #[arg(long, default_value = "data/dev")]
    pub datadir: PathBuf,
    /// JSON-RPC listen address.
    #[arg(long, default_value = "127.0.0.1:8545")]
    pub rpc: SocketAddr,
    /// Fee recipient of produced blocks.
    #[arg(long, default_value_t = Address::ZERO)]
    pub coinbase: Address,
    /// Seconds between blocks (defaults to the genesis slot length).
    #[arg(long)]
    pub block_time: Option<u64>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Runs the devnet until Ctrl-C.
pub async fn run(args: DevnetArgs) -> Result<()> {
    let json = std::fs::read_to_string(&args.genesis)
        .with_context(|| format!("reading {}", args.genesis.display()))?;
    let genesis = Genesis::from_json(&json)?;
    let chain = Arc::new(Chain::open(&args.datadir, &genesis)?);
    let cfg = chain.config().clone();
    let pool =
        Arc::new(TxPool::new(PoolConfig::new(cfg.chain_id, cfg.gas_limit, cfg.min_base_fee_wei)));

    let ctx = RpcContext {
        chain: chain.clone(),
        pool: pool.clone(),
        client_version: format!("boltchain/v{}", env!("CARGO_PKG_VERSION")),
    };
    let (addr, handle) = bolt_rpc::start(args.rpc, ctx).await?;
    tracing::info!(%addr, chain_id = cfg.chain_id, genesis = %chain.genesis_hash(), "JSON-RPC listening");

    let block_time = Duration::from_secs(args.block_time.unwrap_or(cfg.slot_seconds).max(1));
    let producer = {
        let chain = chain.clone();
        let pool = pool.clone();
        let coinbase = args.coinbase;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(block_time);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let (chain, pool) = (chain.clone(), pool.clone());
                let res =
                    tokio::task::spawn_blocking(move || produce(&chain, &pool, coinbase)).await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::error!("block production failed: {e:#}"),
                    Err(e) => tracing::error!("producer task panicked: {e}"),
                }
            }
        })
    };

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    producer.abort();
    handle.stop()?;
    Ok(())
}

/// Produces one block from the pool.
pub fn produce(chain: &Chain, pool: &TxPool, coinbase: Address) -> Result<()> {
    let base_fee = chain.next_base_fee()?;
    let candidates = {
        let r = chain.store().reader()?;
        pool.best(base_fee, &HeadState(&r))
    };
    let started = std::time::Instant::now();
    let built = chain.build_block(candidates, now(), coinbase)?;
    {
        let r = chain.store().reader()?;
        pool.on_new_block(&HeadState(&r));
        let invalid: Vec<_> = built.rejected.iter().filter(|r| r.2).map(|r| r.0).collect();
        pool.remove(&invalid);
    }
    tracing::info!(
        number = built.header.number,
        hash = %built.hash,
        txs = built.included.len(),
        rejected = built.rejected.len(),
        gas = built.header.gas_used,
        ms = started.elapsed().as_millis() as u64,
        "produced block"
    );
    for (h, why, _) in &built.rejected {
        tracing::debug!(tx = %h, reason = %why, "rejected");
    }
    Ok(())
}
