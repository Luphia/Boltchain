//! Boltchain node binary.

use boltchain::{bench, devnet, follow};

use anyhow::{Context, Result};
use bolt_primitives::Genesis;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "boltchain", version, about = "Boltchain node")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Genesis file tools.
    #[command(subcommand)]
    Genesis(GenesisCmd),
    /// Print the protocol constants this binary was built with.
    Params,
    /// Run a single-producer development network with JSON-RPC, announcing blocks over IPFS.
    Devnet(devnet::DevnetArgs),
    /// Follow a producer: sync blocks over IPFS, serve JSON-RPC, forward transactions.
    Follow(follow::FollowArgs),
    /// Print the peer id of a node key (creating the key if missing).
    NodeId {
        /// Node key file.
        #[arg(long)]
        node_key: PathBuf,
    },
    /// Benchmark block execution (transfers and the worst-case pairing block).
    Bench(bench::BenchArgs),
}

#[derive(Debug, Subcommand)]
enum GenesisCmd {
    /// Validate a genesis file and print its block hash and state root.
    Inspect {
        /// Path to genesis.json.
        path: PathBuf,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,libmdbx=warn".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Devnet(args) => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(devnet::run(args))?;
        }
        Command::Bench(args) => bench::run(args)?,
        Command::Follow(args) => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(follow::run(args))?;
        }
        Command::NodeId { node_key } => {
            let key = bolt_net::load_or_create_key(&node_key)?;
            println!("{}", key.public().to_peer_id());
        }
        Command::Genesis(GenesisCmd::Inspect { path }) => {
            let json = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let genesis = Genesis::from_json(&json)?;
            let header = genesis.header();
            println!("genesis valid");
            println!("  chain id          {}", genesis.config.chain_id);
            println!("  block hash        {}", header.hash_slow());
            println!("  state root        {}", header.state_root);
            println!("  initial seed      {}", header.mix_hash);
            println!("  bootstrap vals    {}", genesis.bootstrap_validators.len());
            println!(
                "  governance        {}-of-{} multisig",
                genesis.governance.threshold,
                genesis.governance.owners.len()
            );
        }
        Command::Params => {
            use bolt_primitives::params::*;
            println!("evm spec          {:?}", bolt_exec::SPEC);
            println!("chain id          {CHAIN_ID}");
            println!("slot              {SLOT_SECONDS}s, epoch {EPOCH_SLOTS} slots");
            println!("committee floor   {MIN_COMMITTEE_SIZE}");
            println!("gas limit         {DEFAULT_GAS_LIMIT} (range {GAS_LIMIT_RANGE:?})");
            println!("supply cap        {SUPPLY_CAP_BOLT} BOLT");
            println!("min stake         {} BOLT", MIN_STAKE_WEI / WEI_PER_BOLT);
            println!("emission          half-life {EMISSION_HALF_LIFE_EPOCHS} epochs");
        }
    }
    Ok(())
}
