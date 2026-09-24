//! Follower node: syncs blocks from IPFS announcements, serves JSON-RPC, forwards transactions
//! to the producer.

use crate::{devnet::load_genesis, p2p::P2pArgs};
use alloy_primitives::B256;
use anyhow::Result;
use bolt_chain::Chain;
use bolt_net::NetHandle;
use bolt_rpc::{RpcContext, TxForwarder};
use bolt_sync::Follower;
use bolt_txpool::{PoolConfig, TxPool};
use libp2p::PeerId;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

/// Follower options.
#[derive(Debug, clap::Args)]
pub struct FollowArgs {
    /// Genesis file.
    #[arg(long, default_value = "genesis/dev.json")]
    pub genesis: PathBuf,
    /// Data directory.
    #[arg(long, default_value = "data/follower")]
    pub datadir: PathBuf,
    /// JSON-RPC listen address.
    #[arg(long, default_value = "127.0.0.1:8546")]
    pub rpc: SocketAddr,
    /// Peer id of the block producer; only its announcements are accepted.
    #[arg(long)]
    pub producer: PeerId,
    /// P2P options.
    #[command(flatten)]
    pub p2p: P2pArgs,
}

#[derive(Debug)]
struct Forward {
    net: NetHandle,
    producer: PeerId,
    rt: tokio::runtime::Handle,
}

impl TxForwarder for Forward {
    fn forward(&self, raw: Vec<u8>) -> Result<B256, String> {
        self.rt.block_on(self.net.forward_tx(self.producer, raw))
    }
}

/// Runs a follower until Ctrl-C.
pub async fn run(args: FollowArgs) -> Result<()> {
    let genesis = load_genesis(&args.genesis)?;
    let chain = Arc::new(Chain::open(&args.datadir, &genesis)?);
    let cfg = chain.config().clone();
    let (net, events) = args.p2p.start(&args.datadir, chain.clone(), Some(args.producer)).await?;

    let ctx = RpcContext {
        chain: chain.clone(),
        pool: Arc::new(TxPool::new(PoolConfig::new(
            cfg.chain_id,
            cfg.gas_limit,
            cfg.min_base_fee_wei,
        ))),
        client_version: format!("boltchain/v{}", env!("CARGO_PKG_VERSION")),
        forwarder: Some(Arc::new(Forward {
            net: net.clone(),
            producer: args.producer,
            rt: tokio::runtime::Handle::current(),
        })),
    };
    let (addr, handle) = bolt_rpc::start(args.rpc, ctx).await?;
    tracing::info!(%addr, head = chain.head()?.number, "follower JSON-RPC listening");

    let follower = tokio::spawn(bolt_sync::run_follower(Follower::new(chain, net), events));
    tokio::signal::ctrl_c().await?;
    follower.abort();
    handle.stop()?;
    Ok(())
}
