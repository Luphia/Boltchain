//! Boltchain node binary.

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
    match Cli::parse().command {
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
