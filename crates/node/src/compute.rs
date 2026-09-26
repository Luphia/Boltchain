//! `boltchain provider` and `boltchain job`: the AI compute market from the command line
//! (ADR 0011).
//!
//! Files are bolt-vault envelopes on IPFS, handed to and fetched from the user's own node through
//! `bolt_hostBlocks` / `bolt_getBlocks` (the node runs with `--rpc-storage`). Every account's
//! vault key is derived from its account key (the same key `boltchain storage` uses), and a
//! provider registers its public half in `ComputeMarket` so requesters can encrypt for it.
//!
//! - Input (requester → provider): JSON `{"messages": [...], "max_tokens": n}`, the body of an
//!   OpenAI-compatible chat completion request without the model name.
//! - Output (provider → requester): the backend's JSON response (`choices`, `usage`, …).
//!
//! The provider serves jobs addressed to it with any OpenAI-compatible HTTP backend
//! (llama.cpp's `llama-server`, Ollama, vLLM): `--backend http://127.0.0.1:8080` and
//! `--model <id>=<backend model name>` for each model id it runs. Token counts come from the
//! backend's `usage` and are capped at the job's maxima.

use crate::{
    swarm::vault_key,
    wallet::{format_bolt, load_wallet, parse_bolt, rpc, send_tx},
};
use alloy_primitives::{Address, B256, Bytes, U256, hex, keccak256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, bail};
use bolt_ipld::Cid;
use bolt_system::{abi::IComputeMarket, addresses::COMPUTE};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};

const RPC: &str = "http://127.0.0.1:8545";

/// Job states in `ComputeMarket`.
pub mod state {
    /// Posted, waiting for its provider.
    pub const OPEN: u8 = 1;
    /// Taken by the provider.
    pub const ACCEPTED: u8 = 2;
    /// Result delivered, dispute window running.
    pub const DELIVERED: u8 = 3;
    /// Disputed, waiting for the verifier panel.
    pub const DISPUTED: u8 = 4;
    /// Paid.
    pub const SETTLED: u8 = 5;
    /// Escrow returned.
    pub const REFUNDED: u8 = 6;
}

fn state_name(s: u8) -> &'static str {
    match s {
        state::OPEN => "open",
        state::ACCEPTED => "accepted",
        state::DELIVERED => "delivered",
        state::DISPUTED => "disputed",
        state::SETTLED => "settled",
        state::REFUNDED => "refunded",
        _ => "none",
    }
}

/// Provider commands.
#[derive(Debug, clap::Subcommand)]
pub enum ProviderCmd {
    /// Register this account as a compute provider (publishes its encryption key). Needs a
    /// little BOLT for gas.
    Register {
        /// Account key file (receives the earnings).
        #[arg(long)]
        wallet: PathBuf,
        /// The node's identity key (`<datadir>/node.key`), published as the provider's peer.
        #[arg(long)]
        node_key: PathBuf,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Serve jobs addressed to this account with an OpenAI-compatible backend until stopped.
    Run {
        /// Account key file.
        #[arg(long)]
        wallet: PathBuf,
        /// Backend base URL (http only), e.g. llama-server on `http://127.0.0.1:8080`.
        #[arg(long)]
        backend: String,
        /// Model a job id maps to on the backend: `<id>=<name>` (repeatable), e.g. `1=qwen2.5-7b`.
        #[arg(long = "model", value_name = "ID=NAME", required = true)]
        models: Vec<String>,
        /// JSON-RPC endpoint of your node (started with `--rpc-storage`).
        #[arg(long, default_value = RPC)]
        rpc: String,
        /// Seconds between scans for new jobs.
        #[arg(long, default_value_t = 5)]
        interval: u64,
    },
    /// Show a provider's registration, bond, jobs and balance.
    Status {
        /// Provider address.
        address: Address,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Settle this provider's delivered jobs whose dispute window has passed, then withdraw.
    Collect {
        /// Account key file.
        #[arg(long)]
        wallet: PathBuf,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
}

/// Requester commands.
#[derive(Debug, clap::Subcommand)]
pub enum JobCmd {
    /// Encrypt a prompt for a provider, hand it to your node and escrow the maximum cost.
    Post {
        /// Account key file (pays).
        #[arg(long)]
        wallet: PathBuf,
        /// Provider address (must be registered).
        #[arg(long)]
        provider: Address,
        /// Model id (as the provider maps it).
        #[arg(long)]
        model: u64,
        /// Prompt text (a single user message).
        #[arg(long, conflicts_with = "input")]
        prompt: Option<String>,
        /// Request JSON file: `{"messages": [...]}` (OpenAI chat format, without `model`).
        #[arg(long)]
        input: Option<PathBuf>,
        /// Price in BOLT per 1,000 input tokens.
        #[arg(long)]
        price_in: String,
        /// Price in BOLT per 1,000 output tokens.
        #[arg(long)]
        price_out: String,
        /// Largest number of input tokens paid for.
        #[arg(long, default_value_t = 4096)]
        max_in: u64,
        /// Largest number of output tokens paid for (also sent as `max_tokens`).
        #[arg(long, default_value_t = 1024)]
        max_out: u64,
        /// Minutes the provider has to deliver.
        #[arg(long, default_value_t = 60)]
        deadline: u64,
        /// Minutes you have to dispute a delivery (10 to 10,080).
        #[arg(long, default_value_t = 30)]
        window: u64,
        /// JSON-RPC endpoint of your node (started with `--rpc-storage`).
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Show a job.
    Status {
        /// Job id.
        id: u64,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Fetch and decrypt a delivered result (prints the answer; `--out` saves the full JSON).
    Result {
        /// Account key file (the requester).
        #[arg(long)]
        wallet: PathBuf,
        /// Job id.
        id: u64,
        /// Save the backend's full response here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// JSON-RPC endpoint of your node (started with `--rpc-storage`).
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Accept a delivery and pay the provider now.
    Approve {
        /// Account key file (the requester).
        #[arg(long)]
        wallet: PathBuf,
        /// Job id.
        id: u64,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Dispute a delivery within its window (1 BOLT deposit, returned if the provider is found
    /// at fault).
    Dispute {
        /// Account key file (the requester).
        #[arg(long)]
        wallet: PathBuf,
        /// Job id.
        id: u64,
        /// What is wrong with the result.
        #[arg(long)]
        reason: String,
        /// JSON-RPC endpoint of your node (started with `--rpc-storage`).
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// After a verifier panel is assigned to your dispute, re-encrypt the job's input, output and
    /// your reason for the panel members and send it to them (through your node).
    Share {
        /// Account key file (the requester).
        #[arg(long)]
        wallet: PathBuf,
        /// Job id.
        id: u64,
        /// JSON-RPC endpoint of your node (started with `--rpc-storage`).
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Get the escrow back from a job nobody delivered by its deadline.
    Refund {
        /// Account key file (the requester).
        #[arg(long)]
        wallet: PathBuf,
        /// Job id.
        id: u64,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Withdraw your ComputeMarket balance (refunds, returned deposits, earnings).
    Withdraw {
        /// Account key file.
        #[arg(long)]
        wallet: PathBuf,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
}

// ---------------------------------------------------------------------------------------------
// Chain and node access.

/// `eth_call` to ComputeMarket.
pub fn call<C: SolCall>(url: &str, c: C) -> Result<C::Return> {
    let out = rpc(
        url,
        "eth_call",
        json!([{"to": COMPUTE, "data": hex::encode_prefixed(c.abi_encode())}, "latest"]),
    )?;
    let bytes: Bytes = serde_json::from_value(out)?;
    Ok(C::abi_decode_returns(&bytes)?)
}

fn chain_time(url: &str) -> Result<u64> {
    let b = rpc(url, "eth_getBlockByNumber", json!(["latest", false]))?;
    let t = b["timestamp"].as_str().context("block timestamp")?;
    Ok(u64::from_str_radix(t.trim_start_matches("0x"), 16)?)
}

fn to_ipld(c: &bolt_vault::Cid) -> Cid {
    Cid::try_from(c.to_bytes().as_slice()).expect("vault CIDs are raw sha2-256 CIDv1")
}

/// Seals `data` for `recipients` and hands every block to the node; returns the envelope CID.
pub fn seal_and_host(
    url: &str,
    data: &[u8],
    name: &str,
    recipients: &[bolt_vault::PublicKey],
) -> Result<Cid> {
    let sealed = bolt_vault::seal(data, name, "application/json", recipients, &Default::default())?;
    let root = to_ipld(&sealed.envelope.cid);
    let blocks: Vec<Value> = sealed
        .blocks()
        .map(|b| json!([to_ipld(&b.cid).to_string(), hex::encode_prefixed(&b.data)]))
        .collect();
    for chunk in blocks.chunks(64) {
        rpc(url, "bolt_hostBlocks", json!([root.to_string(), chunk]))?;
    }
    Ok(root)
}

/// Fetches (through the node) and decrypts the bolt-vault file named by envelope CID bytes.
pub fn fetch_and_open(url: &str, envelope: &[u8], sk: &bolt_vault::SecretKey) -> Result<Vec<u8>> {
    let root = Cid::try_from(envelope).context("envelope CID")?;
    let get = |cids: &[Cid]| -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for chunk in cids.chunks(64) {
            let names: Vec<String> = chunk.iter().map(|c| c.to_string()).collect();
            let v = rpc(url, "bolt_getBlocks", json!([names, null, root.to_string()]))?;
            let got: Vec<Bytes> = serde_json::from_value(v)?;
            out.extend(got.into_iter().map(|b| b.to_vec()));
        }
        Ok(out)
    };
    let env_block = get(&[root])?.remove(0);
    let env = bolt_vault::parse_envelope(&env_block)?;
    let key = bolt_vault::open_key(&env, sk)?;
    let mblock = get(&[to_ipld(&env.manifest)])?.remove(0);
    let manifest = bolt_vault::open_manifest(&env, &mblock, &key)?;
    let cids: Vec<Cid> = manifest.shards.iter().map(to_ipld).collect();
    let shards: Vec<Option<Vec<u8>>> = get(&cids)?.into_iter().map(Some).collect();
    Ok(bolt_vault::recover(&manifest, &key, &shards)?)
}

/// The account's registered encryption key.
fn encryption_key(url: &str, account: Address) -> Result<bolt_vault::PublicKey> {
    let k = call(url, IComputeMarket::encryptionKeyCall { account })?;
    if k == B256::ZERO {
        bail!("{account} has no encryption key in ComputeMarket (not a registered provider?)");
    }
    Ok(bolt_vault::PublicKey::from_bytes(k.as_slice())?)
}

/// POSTs JSON to an `http://` URL; returns the JSON response.
pub fn http_post_json(url: &str, body: &Value, timeout: Duration) -> Result<Value> {
    let rest = url.strip_prefix("http://").context("only http:// backends are supported")?;
    let (host, path) =
        rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let body = body.to_string();
    let mut s =
        std::net::TcpStream::connect(host).with_context(|| format!("connecting to {host}"))?;
    s.set_read_timeout(Some(timeout))?;
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)?;
    let resp = String::from_utf8_lossy(&resp);
    let (head, json_body) = resp.split_once("\r\n\r\n").context("bad HTTP response")?;
    let status = head.split_whitespace().nth(1).unwrap_or("?");
    if !status.starts_with('2') {
        bail!(
            "backend answered HTTP {status}: {}",
            json_body.chars().take(300).collect::<String>()
        );
    }
    let start = json_body.find('{').context("no JSON in response")?;
    let end = json_body.rfind('}').context("no JSON in response")?;
    Ok(serde_json::from_str(&json_body[start..=end])?)
}

fn event_topic(sig: &str) -> B256 {
    keccak256(sig.as_bytes())
}

fn first_indexed(url: &str, hash: B256, sig: &str) -> Result<U256> {
    let topic = event_topic(sig);
    let r = rpc(url, "eth_getTransactionReceipt", json!([hash]))?;
    for log in r["logs"].as_array().into_iter().flatten() {
        let topics: Vec<B256> = serde_json::from_value(log["topics"].clone()).unwrap_or_default();
        if topics.first() == Some(&topic) && topics.len() > 1 {
            return Ok(U256::from_be_bytes(topics[1].0));
        }
    }
    bail!("no {sig} log in {hash}")
}

// ---------------------------------------------------------------------------------------------
// Provider.

/// A provider's serving loop state.
pub struct Provider {
    signer: PrivateKeySigner,
    sk: bolt_vault::SecretKey,
    pk: bolt_vault::PublicKey,
    rpc: String,
    backend: String,
    models: HashMap<u64, String>,
    /// Jobs below this id are finished or not ours.
    cursor: u64,
    /// Jobs that failed permanently (not retried).
    failed: HashSet<u64>,
}

/// Parses `--model <id>=<name>` values.
pub fn parse_models(v: &[String]) -> Result<HashMap<u64, String>> {
    v.iter()
        .map(|m| {
            let (id, name) =
                m.split_once('=').with_context(|| format!("--model {m}: expected ID=NAME"))?;
            Ok((
                id.trim().parse::<u64>().with_context(|| format!("--model {m}"))?,
                name.trim().to_owned(),
            ))
        })
        .collect()
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider").field("address", &self.signer.address()).finish()
    }
}

impl Provider {
    /// A provider serving jobs for `signer` with `backend`.
    pub fn new(
        signer: PrivateKeySigner,
        rpc: &str,
        backend: &str,
        models: HashMap<u64, String>,
    ) -> Self {
        let (sk, pk) = vault_key(&signer);
        Self {
            signer,
            sk,
            pk,
            rpc: rpc.to_owned(),
            backend: backend.trim_end_matches('/').to_owned(),
            models,
            cursor: 0,
            failed: HashSet::new(),
        }
    }

    /// One scan: accept the open jobs addressed to this provider (for a model it runs), run the
    /// accepted ones and deliver. Returns the jobs delivered.
    pub fn round(&mut self) -> Result<Vec<u64>> {
        let me = self.signer.address();
        let count = call(&self.rpc, IComputeMarket::jobCountCall {})?.to::<u64>();
        let now = chain_time(&self.rpc)?;
        let mut delivered = Vec::new();
        let mut advancing = true;
        for id in self.cursor..count {
            let j = call(&self.rpc, IComputeMarket::jobCall { id: U256::from(id) })?;
            let mine = j.provider == me;
            let live = matches!(j.state, state::OPEN | state::ACCEPTED) && j.deadline > now;
            if advancing && (!live || (!mine && j.provider != Address::ZERO)) {
                self.cursor = id + 1;
                continue;
            }
            advancing = false;
            if !mine || !live || self.failed.contains(&id) {
                continue;
            }
            let Some(model) = self.models.get(&j.model).cloned() else {
                tracing::debug!(job = id, model = j.model, "model not served here");
                continue;
            };
            match self.serve(id, &j, &model) {
                Ok(()) => delivered.push(id),
                Err(e) => {
                    eprintln!("job {id}: {e:#}");
                    self.failed.insert(id);
                }
            }
        }
        Ok(delivered)
    }

    fn serve(&self, id: u64, j: &IComputeMarket::Job, model: &str) -> Result<()> {
        if j.state == state::OPEN {
            let data = IComputeMarket::acceptCall { id: U256::from(id) }.abi_encode();
            send_tx(&self.rpc, &self.signer, COMPUTE, U256::ZERO, data.into())?;
            println!("job {id}: accepted");
        }
        let input = fetch_and_open(&self.rpc, &j.input, &self.sk).context("reading the input")?;
        let mut req: Value = serde_json::from_slice(&input).context("input is not JSON")?;
        let obj = req.as_object_mut().context("input is not a JSON object")?;
        obj.insert("model".into(), json!(model));
        obj.insert("stream".into(), json!(false));
        let cap = obj.get("max_tokens").and_then(Value::as_u64).unwrap_or(j.maxOut).min(j.maxOut);
        obj.insert("max_tokens".into(), json!(cap));
        let resp = http_post_json(
            &format!("{}/v1/chat/completions", self.backend),
            &req,
            Duration::from_secs(900),
        )?;
        let tin = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0).min(j.maxIn);
        let tout = resp["usage"]["completion_tokens"].as_u64().unwrap_or(0).min(j.maxOut);
        let requester = encryption_key(&self.rpc, j.requester)
            .context("the requester has no encryption key (`job post` registers it)")?;
        let out = serde_json::to_vec(&resp)?;
        let env = seal_and_host(&self.rpc, &out, "output.json", &[requester, self.pk.clone()])?;
        let data = IComputeMarket::deliverCall {
            id: U256::from(id),
            output: Bytes::from(env.to_bytes()),
            tokensIn: tin,
            tokensOut: tout,
        }
        .abi_encode();
        send_tx(&self.rpc, &self.signer, COMPUTE, U256::ZERO, data.into())?;
        println!("job {id}: delivered ({tin} input, {tout} output tokens)");
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Disputes.

/// A copy of the envelope `env` (CID bytes) that also lets each of `to` read the file, handed to
/// the node; returns its CID. Only a current recipient (`sk`) can do this.
pub fn reshare(
    url: &str,
    env: &[u8],
    sk: &bolt_vault::SecretKey,
    to: &[bolt_vault::PublicKey],
) -> Result<Cid> {
    let root = Cid::try_from(env).context("envelope CID")?;
    let v = rpc(url, "bolt_getBlocks", json!([[root.to_string()], null, root.to_string()]))?;
    let got: Vec<Bytes> = serde_json::from_value(v)?;
    let mut e = bolt_vault::parse_envelope(got.first().context("envelope missing")?)?;
    let key = bolt_vault::open_key(&e, sk)?;
    let mut block = None;
    for pk in to {
        let b = bolt_vault::add_recipient(&e, &key, pk);
        e = bolt_vault::parse_envelope(&b.data)?;
        block = Some(b);
    }
    let b = block.context("no recipients to add")?;
    let cid = to_ipld(&b.cid);
    rpc(
        url,
        "bolt_hostBlocks",
        json!([cid.to_string(), [[cid.to_string(), hex::encode_prefixed(&b.data)]]]),
    )?;
    Ok(cid)
}

/// Shares a disputed job's files with its verifier panel. Returns how many panel members could
/// be given access (those whose verifier keys the node has heard).
pub fn share_dispute(url: &str, s: &PrivateKeySigner, id: u64) -> Result<usize> {
    let j = call(url, IComputeMarket::jobCall { id: U256::from(id) })?;
    if j.requester != s.address() {
        bail!("job {id} is not yours");
    }
    if j.state != state::DISPUTED {
        bail!("job {id} is not disputed ({})", state_name(j.state));
    }
    let d = call(url, IComputeMarket::disputesCall { id: U256::from(id) })?;
    if d.panelEpoch == 0 {
        bail!("no verifier panel yet: it is assigned at the next epoch");
    }
    let panel = call(url, IComputeMarket::panelCall { epoch: d.panelEpoch })?;
    let known: Vec<Option<String>> =
        serde_json::from_value(rpc(url, "bolt_verifierKeys", json!([panel]))?)?;
    let missing = known.iter().filter(|k| k.is_none()).count();
    if missing > 0 {
        // Every member must be able to read the files, or its node abstains (keys are announced
        // every minute).
        bail!(
            "the node has not heard {missing} of {} panel members' verifier keys yet; try again in a minute",
            panel.len()
        );
    }
    let keys: Vec<bolt_vault::PublicKey> = known
        .into_iter()
        .flatten()
        .map(|k| Ok(bolt_vault::PublicKey::from_bytes(&hex::decode(k)?)?))
        .collect::<Result<_>>()?;
    let (sk, _) = vault_key(s);
    let input = reshare(url, &j.input, &sk, &keys)?;
    let output = reshare(url, &j.output, &sk, &keys)?;
    let reason = reshare(url, &d.reason, &sk, &keys)?;
    let chain_id = u64::from_str_radix(
        rpc(url, "eth_chainId", json!([]))?.as_str().context("chain id")?.trim_start_matches("0x"),
        16,
    )?;
    let digest = crate::storage::DisputeShare::digest(
        chain_id,
        id,
        &input.to_bytes(),
        &output.to_bytes(),
        &reason.to_bytes(),
    );
    let sig = alloy_signer::SignerSync::sign_hash_sync(s, &digest)?;
    rpc(
        url,
        "bolt_shareDispute",
        json!([
            format!("0x{id:x}"),
            input.to_string(),
            output.to_string(),
            reason.to_string(),
            hex::encode_prefixed(sig.as_bytes())
        ]),
    )?;
    Ok(keys.len())
}

/// `boltchain judge`: a `--verifier-cmd` program that re-runs a disputed job.
#[derive(Debug, clap::Args)]
pub struct JudgeArgs {
    /// OpenAI-compatible backend (http only) to re-run the job on.
    #[arg(long)]
    pub backend: String,
    /// Model a job id maps to on the backend: `<id>=<name>` (repeatable).
    #[arg(long = "model", value_name = "ID=NAME", required = true)]
    pub models: Vec<String>,
    /// Smallest word overlap (Jaccard, 0–1) between the delivered answer and the re-run.
    #[arg(long, default_value_t = 0.3)]
    pub min_similarity: f64,
    /// Allowed excess of the claimed token counts over the recount (fraction).
    #[arg(long, default_value_t = 0.1)]
    pub token_slack: f64,
}

/// The first line of a verdict: `fault`, `ok`, or anything else to abstain; then the reasons.
pub type Verdict = (String, Vec<String>);

fn words(t: &str) -> HashSet<String> {
    t.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// Word-set overlap of two texts (1 when both are empty).
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (words(a), words(b));
    let union = a.union(&b).count();
    if union == 0 { 1.0 } else { a.intersection(&b).count() as f64 / union as f64 }
}

/// Tokens of `text` by the backend's `/tokenize` (llama.cpp), else about 4 characters each.
fn count_tokens(backend: &str, text: &str) -> u64 {
    http_post_json(
        &format!("{backend}/tokenize"),
        &json!({"content": text}),
        Duration::from_secs(60),
    )
    .ok()
    .and_then(|v| v["tokens"].as_array().map(|t| t.len() as u64))
    .unwrap_or_else(|| (text.chars().count() as u64).div_ceil(4))
}

/// Judges the disputed job described by the `BOLT_*` environment (see `CommandJudge`).
pub fn judge(args: &JudgeArgs, env: &HashMap<String, String>) -> Result<Verdict> {
    let get = |k: &str| env.get(k).cloned().with_context(|| format!("{k} not set"));
    let abstain = |why: String| Ok(("abstain".to_string(), vec![why]));
    if get("BOLT_SHARED")? != "1" {
        return abstain("the requester has not shared the files with the panel yet".into());
    }
    let url = get("BOLT_RPC")?;
    let backend = args.backend.trim_end_matches('/').to_owned();
    let models = parse_models(&args.models)?;
    let model_id: u64 = get("BOLT_MODEL")?.parse()?;
    let Some(model) = models.get(&model_id) else {
        return abstain(format!("model {model_id} is not served by this verifier"));
    };
    let sk = bolt_vault::SecretKey::from_bytes(&hex::decode(get("BOLT_VERIFIER_KEY")?)?)?;
    let cid = |k: &str| -> Result<Vec<u8>> { Ok(get(k)?.parse::<Cid>()?.to_bytes()) };
    let input = match fetch_and_open(&url, &cid("BOLT_INPUT")?, &sk) {
        Ok(i) => i,
        Err(e) => return abstain(format!("cannot read the input: {e:#}")),
    };
    let output = match fetch_and_open(&url, &cid("BOLT_OUTPUT")?, &sk) {
        Ok(o) => o,
        Err(e) => return abstain(format!("cannot read the output: {e:#}")),
    };
    let mut notes = Vec::new();
    let fault = |mut notes: Vec<String>, why: String| {
        notes.push(why);
        Ok(("fault".to_string(), notes))
    };
    let Ok(delivered) = serde_json::from_slice::<Value>(&output) else {
        return fault(notes, "the delivered output is not JSON".into());
    };
    let Some(answer) = delivered["choices"][0]["message"]["content"].as_str().map(str::to_owned)
    else {
        return fault(notes, "the delivered output has no answer".into());
    };
    let mut req: Value = serde_json::from_slice(&input).context("input is not JSON")?;
    let obj = req.as_object_mut().context("input is not a JSON object")?;
    obj.insert("model".into(), json!(model));
    obj.insert("stream".into(), json!(false));
    obj.entry("temperature").or_insert(json!(0));
    let rerun =
        http_post_json(&format!("{backend}/v1/chat/completions"), &req, Duration::from_secs(900))?;
    let again = rerun["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned();
    let claimed_in: u64 = get("BOLT_TOKENS_IN")?.parse()?;
    let claimed_out: u64 = get("BOLT_TOKENS_OUT")?.parse()?;
    let slack = |n: u64| (n as f64 * (1.0 + args.token_slack)).ceil() as u64 + 8;
    if let Some(p) = rerun["usage"]["prompt_tokens"].as_u64()
        && claimed_in > slack(p)
    {
        return fault(notes, format!("claimed {claimed_in} input tokens, the prompt has {p}"));
    }
    let counted = count_tokens(&backend, &answer);
    if claimed_out > slack(counted) {
        return fault(
            notes,
            format!("claimed {claimed_out} output tokens, the answer has about {counted}"),
        );
    }
    let sim = similarity(&answer, &again);
    notes.push(format!("similarity to the re-run {sim:.2}"));
    if sim < args.min_similarity {
        return fault(
            notes,
            format!("the answer differs from the re-run (below {:.2})", args.min_similarity),
        );
    }
    Ok(("ok".to_string(), notes))
}

// ---------------------------------------------------------------------------------------------
// Commands.

/// Runs a provider command.
pub fn run_provider(cmd: ProviderCmd) -> Result<()> {
    match cmd {
        ProviderCmd::Register { wallet, node_key, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let (_, pk) = vault_key(&s);
            let peer = bolt_net::load_or_create_key(&node_key)?.public().to_peer_id();
            let data = IComputeMarket::registerCall {
                encKey: B256::from(pk.to_bytes()),
                peerId: Bytes::copy_from_slice(&peer.to_bytes()),
            }
            .abi_encode();
            send_tx(&url, &s, COMPUTE, U256::ZERO, data.into())?;
            println!("{} registered as a compute provider (peer {peer})", s.address());
        }
        ProviderCmd::Run { wallet, backend, models, rpc: url, interval } => {
            let s = load_wallet(&wallet)?;
            let models = parse_models(&models)?;
            let reg = call(&url, IComputeMarket::providerCall { p: s.address() })?;
            if !reg.registered {
                bail!("{} is not registered: run `boltchain provider register` first", s.address());
            }
            println!("serving jobs for {} with {backend} (models {models:?})", s.address());
            let mut p = Provider::new(s, &url, &backend, models);
            loop {
                if let Err(e) = p.round() {
                    eprintln!("scan: {e:#}");
                }
                std::thread::sleep(Duration::from_secs(interval));
            }
        }
        ProviderCmd::Status { address, rpc: url } => {
            let p = call(&url, IComputeMarket::providerCall { p: address })?;
            let limit = call(&url, IComputeMarket::jobLimitCall { p: address })?;
            let bal = call(&url, IComputeMarket::balanceOfCall { account: address })?;
            println!(
                "{address}: {}registered, bond {} BOLT, job limit {} BOLT, {} jobs done, {} faults, balance {} BOLT{}",
                if p.registered { "" } else { "not " },
                format_bolt(p.bond),
                format_bolt(limit),
                p.jobsDone,
                p.faults,
                format_bolt(bal),
                if p.debt.is_zero() {
                    String::new()
                } else {
                    format!(", debt {} BOLT", format_bolt(p.debt))
                }
            );
        }
        ProviderCmd::Collect { wallet, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let count = call(&url, IComputeMarket::jobCountCall {})?.to::<u64>();
            let now = chain_time(&url)?;
            for id in 0..count {
                let j = call(&url, IComputeMarket::jobCall { id: U256::from(id) })?;
                if j.provider == s.address()
                    && j.state == state::DELIVERED
                    && now >= j.deliveredAt + j.window
                {
                    let data = IComputeMarket::settleCall { id: U256::from(id) }.abi_encode();
                    match send_tx(&url, &s, COMPUTE, U256::ZERO, data.into()) {
                        Ok(_) => println!("job {id}: settled"),
                        Err(e) => println!("job {id}: {e:#}"),
                    }
                }
            }
            withdraw(&url, &s)?;
        }
    }
    Ok(())
}

fn withdraw(url: &str, s: &PrivateKeySigner) -> Result<()> {
    let b = call(url, IComputeMarket::balanceOfCall { account: s.address() })?;
    if b.is_zero() {
        println!("nothing to withdraw");
        return Ok(());
    }
    let data = IComputeMarket::withdrawCall { to: s.address() }.abi_encode();
    send_tx(url, s, COMPUTE, U256::ZERO, data.into())?;
    println!("withdrew {} BOLT", format_bolt(b));
    Ok(())
}

/// Parameters of a new job.
#[derive(Debug, Clone)]
pub struct NewJob {
    /// Provider.
    pub provider: Address,
    /// Model id.
    pub model: u64,
    /// Request JSON (`{"messages": ...}`).
    pub request: Value,
    /// Wei per 1,000 input tokens.
    pub price_in: U256,
    /// Wei per 1,000 output tokens.
    pub price_out: U256,
    /// Input token cap.
    pub max_in: u64,
    /// Output token cap.
    pub max_out: u64,
    /// Seconds to deliver.
    pub deadline: u64,
    /// Seconds to dispute.
    pub window: u64,
}

/// Encrypts the request for the provider, hands it to the node, registers the requester's own
/// encryption key if needed, and posts the job. Returns its id.
pub fn post_job(url: &str, s: &PrivateKeySigner, job: &NewJob) -> Result<u64> {
    let (_, mine) = vault_key(s);
    let provider_key = encryption_key(url, job.provider)?;
    // The provider encrypts the result for the requester's registered key.
    let have = call(url, IComputeMarket::encryptionKeyCall { account: s.address() })?;
    if have != B256::from(mine.to_bytes()) {
        let data =
            IComputeMarket::setEncryptionKeyCall { key: B256::from(mine.to_bytes()) }.abi_encode();
        send_tx(url, s, COMPUTE, U256::ZERO, data.into())?;
    }
    let body = serde_json::to_vec(&job.request)?;
    let env = seal_and_host(url, &body, "input.json", &[mine, provider_key])?;
    let now = chain_time(url)?;
    let cost = |t: u64, p: U256| U256::from(t) * p / U256::from(1000);
    let value = cost(job.max_in, job.price_in) + cost(job.max_out, job.price_out);
    let data = IComputeMarket::postCall {
        provider_: job.provider,
        model: job.model,
        input: Bytes::from(env.to_bytes()),
        priceIn: job.price_in.to::<u128>(),
        priceOut: job.price_out.to::<u128>(),
        maxIn: job.max_in,
        maxOut: job.max_out,
        deadline: now + job.deadline,
        window: job.window,
        relayFee: 0,
    }
    .abi_encode();
    println!("escrow {} BOLT", format_bolt(value));
    let hash = send_tx(url, s, COMPUTE, value, data.into())?;
    Ok(first_indexed(url, hash, "JobPosted(uint256,address,address,uint64,bytes)")?.to::<u64>())
}

/// Fetches and decrypts a delivered job's result.
pub fn job_result(url: &str, s: &PrivateKeySigner, id: u64) -> Result<Value> {
    let j = call(url, IComputeMarket::jobCall { id: U256::from(id) })?;
    if j.output.is_empty() {
        bail!("job {id} has no result yet ({})", state_name(j.state));
    }
    let (sk, _) = vault_key(s);
    let out = fetch_and_open(url, &j.output, &sk)?;
    Ok(serde_json::from_slice(&out)?)
}

/// Runs a job command.
pub fn run_job(cmd: JobCmd) -> Result<()> {
    match cmd {
        JobCmd::Post {
            wallet,
            provider,
            model,
            prompt,
            input,
            price_in,
            price_out,
            max_in,
            max_out,
            deadline,
            window,
            rpc: url,
        } => {
            let s = load_wallet(&wallet)?;
            let mut request: Value = match (prompt, input) {
                (Some(p), _) => json!({"messages": [{"role": "user", "content": p}]}),
                (None, Some(f)) => serde_json::from_slice(&std::fs::read(&f)?)
                    .with_context(|| format!("{} is not JSON", f.display()))?,
                (None, None) => bail!("give --prompt or --input"),
            };
            request.as_object_mut().context("the request must be a JSON object")?.remove("model");
            request["max_tokens"] = json!(max_out);
            let job = NewJob {
                provider,
                model,
                request,
                price_in: parse_bolt(&price_in)?,
                price_out: parse_bolt(&price_out)?,
                max_in,
                max_out,
                deadline: deadline * 60,
                window: window * 60,
            };
            let id = post_job(&url, &s, &job)?;
            println!("job {id} posted; follow it with `boltchain job status {id}`");
        }
        JobCmd::Status { id, rpc: url } => {
            let j = call(&url, IComputeMarket::jobCall { id: U256::from(id) })?;
            if j.requester == Address::ZERO {
                bail!("no job {id}");
            }
            let cost = |t: u64, p: u128| U256::from(t) * U256::from(p) / U256::from(1000);
            println!("job {id}: {}", state_name(j.state));
            println!("  requester {}, provider {}, model {}", j.requester, j.provider, j.model);
            println!(
                "  price {} / {} BOLT per 1K tokens (in / out), escrow {} BOLT",
                format_bolt(U256::from(j.priceIn)),
                format_bolt(U256::from(j.priceOut)),
                format_bolt(U256::from(j.escrow))
            );
            if j.state >= state::DELIVERED {
                println!(
                    "  delivered {} input + {} output tokens, cost {} BOLT",
                    j.tokensIn,
                    j.tokensOut,
                    format_bolt(cost(j.tokensIn, j.priceIn) + cost(j.tokensOut, j.priceOut))
                );
            }
            if j.state == state::DELIVERED {
                println!("  dispute window ends at {} (unix)", j.deliveredAt + j.window);
            }
            if j.state == state::DISPUTED {
                let d = call(&url, IComputeMarket::disputesCall { id: U256::from(id) })?;
                if d.panelEpoch == 0 {
                    println!("  waiting for a verifier panel (next epoch)");
                } else {
                    println!("  decided by the verifier panel of epoch {}", d.panelEpoch);
                }
            }
        }
        JobCmd::Result { wallet, id, out, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let v = job_result(&url, &s, id)?;
            if let Some(f) = out {
                std::fs::write(&f, serde_json::to_vec_pretty(&v)?)?;
                println!("saved to {}", f.display());
            }
            match v["choices"][0]["message"]["content"].as_str() {
                Some(text) => println!("{text}"),
                None => println!("{}", serde_json::to_string_pretty(&v)?),
            }
        }
        JobCmd::Approve { wallet, id, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let data = IComputeMarket::settleCall { id: U256::from(id) }.abi_encode();
            send_tx(&url, &s, COMPUTE, U256::ZERO, data.into())?;
            println!("job {id} paid; the rest of the escrow is in your balance (`job withdraw`)");
        }
        JobCmd::Dispute { wallet, id, reason, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let (_, mine) = vault_key(&s);
            let body = serde_json::to_vec(&json!({"job": id, "reason": reason}))?;
            let env = seal_and_host(&url, &body, "dispute.json", &[mine])?;
            let data = IComputeMarket::disputeCall {
                id: U256::from(id),
                reason: Bytes::from(env.to_bytes()),
            }
            .abi_encode();
            send_tx(&url, &s, COMPUTE, U256::from(10u128.pow(18)), data.into())?;
            println!("job {id} disputed; a verifier panel is assigned at the next epoch, then run");
            println!(
                "`boltchain job share --wallet <wallet> {id}` so the panel can read the files"
            );
        }
        JobCmd::Share { wallet, id, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let n = share_dispute(&url, &s, id)?;
            println!("job {id}: files shared with {n} panel members");
        }
        JobCmd::Refund { wallet, id, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let data = IComputeMarket::refundCall { id: U256::from(id) }.abi_encode();
            send_tx(&url, &s, COMPUTE, U256::ZERO, data.into())?;
            println!("job {id} refunded to your balance (`job withdraw`)");
        }
        JobCmd::Withdraw { wallet, rpc: url } => {
            let s = load_wallet(&wallet)?;
            withdraw(&url, &s)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_of_answers() {
        assert_eq!(similarity("Hello, world!", "hello world"), 1.0);
        assert!(similarity("the cat sat", "a dog ran") < 0.1);
        assert!((similarity("a b c d", "a b x y") - 2.0 / 6.0).abs() < 1e-9);
        assert_eq!(similarity("", ""), 1.0);
    }

    #[test]
    fn model_maps_parse() {
        let m = parse_models(&["1=qwen2.5-7b".into(), " 2 = llama3 ".into()]).unwrap();
        assert_eq!(m[&1], "qwen2.5-7b");
        assert_eq!(m[&2], "llama3");
        assert!(parse_models(&["x=y".into()]).is_err());
        assert!(parse_models(&["1".into()]).is_err());
    }
}
