//! Boltchain node binary.

use boltchain::{bench, devnet, follow, keys, validator};

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
    /// Validator key tools.
    #[command(subcommand)]
    Keys(keys::KeysCmd),
    /// Run a single-producer development network with JSON-RPC, announcing blocks over IPFS.
    Devnet(devnet::DevnetArgs),
    /// Follow a producer: sync blocks over IPFS, serve JSON-RPC, forward transactions.
    Follow(follow::FollowArgs),
    /// Run a validator: take part in consensus, produce and finalize blocks.
    Validator(validator::ValidatorArgs),
    /// Mine: follow the chain and seal blocks with RandomBOLT until PoS starts, then keep
    /// following it (same as `validator --mine` without keys).
    Mine(validator::ValidatorArgs),
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
    /// Write a dev-chain genesis. With validators (insecure deterministic dev keys, staked at
    /// genesis) the chain runs PoS from block 1; with `--validators 0` it starts mined.
    MakeDev {
        /// Output file.
        #[arg(long)]
        out: PathBuf,
        /// Chain id (8017 produces a mainnet-shaped test genesis: no validators, no funds).
        #[arg(long, default_value_t = 1337)]
        chain_id: u64,
        /// Number of genesis validators (dev chains only).
        #[arg(long, default_value_t = 7)]
        validators: u32,
        /// Accounts to fund with 10,000 BOLT each (dev chains only).
        #[arg(long)]
        fund: Vec<alloy_primitives::Address>,
        /// Header extra data.
        #[arg(long, default_value = "Boltchain dev")]
        extra: String,
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
        Command::Keys(cmd) => keys::run(cmd)?,
        Command::Follow(args) => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(follow::run(args))?;
        }
        Command::Mine(mut args) => {
            args.mining.mine = true;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(validator::run(args))?;
        }
        Command::Validator(args) => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(validator::run(args))?;
        }
        Command::NodeId { node_key } => {
            let key = bolt_net::load_or_create_key(&node_key)?;
            println!("{}", key.public().to_peer_id());
        }
        Command::Genesis(GenesisCmd::MakeDev { out, chain_id, validators, fund, extra }) => {
            let g = make_dev_genesis(chain_id, validators, &fund, &extra)?;
            std::fs::write(&out, serde_json::to_string_pretty(&g)? + "\n")?;
            println!("wrote {} (genesis hash {})", out.display(), bolt_system::genesis_hash(&g)?);
        }
        Command::Genesis(GenesisCmd::Inspect { path }) => {
            let json = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let genesis = Genesis::from_json(&json)?;
            let header = bolt_system::genesis_header(&genesis)?;
            println!("genesis valid");
            println!("  chain id          {}", genesis.config.chain_id);
            println!("  block hash        {}", header.hash_slow());
            println!("  state root        {}", header.state_root);
            println!("  initial seed      {}", header.mix_hash);
            if genesis.starts_with_pow() {
                let p = &genesis.config.pow;
                let (stakers, stake, streak) = genesis.config.pos_thresholds();
                println!(
                    "  starts            mined ({:?}, difficulty {}, {}s blocks)",
                    p.algorithm, p.initial_difficulty, p.block_seconds
                );
                println!(
                    "  PoS when          {stakers} stakers and {stake} BOLT staked for {streak} epochs"
                );
            } else {
                println!(
                    "  starts            PoS ({} genesis validators)",
                    genesis.dev_validators.len()
                );
            }
            println!("  governance        none (rules change by hard fork only)");
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
            println!(
                "mining            RandomBOLT, {POW_BLOCK_SECONDS}s blocks, ASERT half-life {POW_HALF_LIFE_SECONDS}s, reorgs <= {MAX_REORG_DEPTH}"
            );
            println!(
                "PoS starts        {POS_MIN_STAKERS} stakers, {POS_MIN_TOTAL_STAKE_BOLT} BOLT, held {POS_STREAK_EPOCHS} epochs"
            );
        }
    }
    Ok(())
}

/// Dev genesis with deterministic validator keys staked at genesis (PoS from block 1), or a
/// mined start without validators.
fn make_dev_genesis(
    chain_id: u64,
    validators: u32,
    fund: &[alloy_primitives::Address],
    extra: &str,
) -> Result<Genesis> {
    use bolt_primitives::{bls, genesis::*, params::*};
    let dev = chain_id != CHAIN_ID;
    if !dev && (!fund.is_empty() || validators > 0) {
        anyhow::bail!("chain id {CHAIN_ID} genesis cannot fund accounts or list validators");
    }
    let dev_validators = (0..validators)
        .map(|i| {
            let k = bls::dev_key(i);
            let sk = k256::ecdsa::SigningKey::from_slice(&bls::dev_eth_key(i)).expect("valid key");
            let addr = alloy_primitives::Address::from_public_key(sk.verifying_key());
            DevValidator {
                name: format!("dev-{i}"),
                bls_pubkey: k.public_key(),
                proof_of_possession: k.proof_of_possession(),
                owner: addr,
                fee_recipient: addr,
                stake_bolt: 1_000,
            }
        })
        .collect();
    let alloc = fund
        .iter()
        .map(|a| {
            (
                *a,
                GenesisAccount {
                    balance: alloy_primitives::U256::from(10_000u128 * WEI_PER_BOLT),
                    ..Default::default()
                },
            )
        })
        .collect();
    let g = Genesis {
        dev,
        config: ChainConfig { chain_id, ..ChainConfig::default() },
        timestamp: 0,
        extra_data: extra.as_bytes().to_vec().into(),
        dev_validators,
        alloc,
    };
    g.validate()?;
    Ok(g)
}
