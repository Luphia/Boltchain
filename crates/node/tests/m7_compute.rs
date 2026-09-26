//! ADR 0011 from the command-line side: on a dev PoS chain (7 genesis validators on 3 nodes),
//! a provider registers and serves jobs with an OpenAI-compatible backend (a mock here); a
//! requester posts an encrypted prompt through its own node, the provider's node fetches it,
//! the provider runs it and delivers an encrypted result, the requester reads it back and pays.
//! A second job is disputed: the verifier panel's nodes run `boltchain judge` (re-running on a
//! backend that answers differently), the requester shares the files with the panel, and the
//! certified verdict refunds the requester.

use alloy_primitives::{Address, Bytes, U256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use bolt_chain::Chain;
use bolt_net::{NetConfig, NetEvent, NetHandle};
use bolt_primitives::{
    Genesis,
    bls::{BlsSecretKey, dev_eth_key, dev_key},
    genesis::GenesisAccount,
};
use bolt_store::StateView;
use bolt_sync::ChainBlocks;
use bolt_system::{abi::*, addresses::*, queries};
use bolt_txpool::TxPool;
use boltchain::{
    compute::{NewJob, Provider, job_result, post_job},
    storage::{RpcHost, Storage, StorageConfig},
    validator::{Validator, ValidatorConfig},
};
use libp2p::{Multiaddr, identity};
use std::{sync::Arc, time::Duration};

const EPOCH: u64 = 6;

fn owner(i: u32) -> PrivateKeySigner {
    PrivateKeySigner::from_slice(&dev_eth_key(i)).unwrap()
}

fn genesis() -> Genesis {
    let mut g = Genesis::from_json(include_str!("../../../genesis/dev.json")).unwrap();
    g.config.epoch_slots = EPOCH;
    g.config.committee_size = 16;
    g.config.history_recent_epochs = Some(1);
    // Validator owners pay for their `setPeer` transactions.
    for i in 0..7 {
        g.alloc.insert(
            owner(i).address(),
            GenesisAccount { balance: U256::from(10u128.pow(19)), ..Default::default() },
        );
    }
    g
}

fn funded() -> PrivateKeySigner {
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap()
}

async fn start_net(
    chain: Arc<Chain>,
    boot: Vec<Multiaddr>,
) -> (NetHandle, tokio::sync::mpsc::Receiver<NetEvent>) {
    bolt_net::start(
        NetConfig {
            chain_id: chain.config().chain_id,
            keypair: identity::Keypair::generate_ed25519(),
            listen: vec!["/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()],
            bootnodes: boot,
            producer: None,
            fork_id: chain.fork_id().unwrap().to_string(),
            fork_check: None,
            relay_server: false,
        },
        Arc::new(ChainBlocks(chain)),
    )
    .await
    .unwrap()
}

async fn addr_of(net: &NetHandle) -> Multiaddr {
    loop {
        if let Some(a) = net.listen_addrs().await.into_iter().next() {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn view<C: SolCall>(chain: &Chain, to: Address, c: C) -> C::Return {
    let r = chain.store().reader().unwrap();
    queries::call(&StateView::latest(&r), 1337, to, c).unwrap()
}

async fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

struct Node {
    chain: Arc<Chain>,
    _pool: Option<Arc<TxPool>>,
    net: NetHandle,
    _storage: Arc<Storage>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

/// A minimal OpenAI-compatible backend: answers every chat completion with `reply(last message)`
/// and fixed token counts; `/tokenize` counts words.
fn mock_backend(reply: fn(&str) -> String) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            use std::io::{Read, Write};
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text[..i]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let body = &text[text.find("\r\n\r\n").unwrap() + 4..];
            let req: serde_json::Value = serde_json::from_str(body).unwrap();
            if text.starts_with("POST /tokenize") {
                let n = req["content"].as_str().unwrap().split_whitespace().count();
                let resp = serde_json::json!({"tokens": vec![1; n]}).to_string();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                    resp.len()
                );
                continue;
            }
            let last = req["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .to_owned();
            let resp = serde_json::json!({
                "model": req["model"],
                "choices": [{"index": 0, "message": {"role": "assistant", "content": reply(&last)}}],
                "usage": {"prompt_tokens": 12, "completion_tokens": 7}
            })
            .to_string();
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                resp.len()
            );
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_is_served_by_a_provider_and_paid() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
    let g = genesis();
    let key_sets: Vec<Vec<u32>> = vec![vec![0, 3, 6], vec![1, 4], vec![2, 5]];
    let mut nodes: Vec<Node> = Vec::new();
    let mut boot: Vec<Multiaddr> = Vec::new();
    let mut rpcs = Vec::new();
    let mut handles = Vec::new();
    // Verifiers re-run on a backend that answers differently: the provider will be at fault.
    let judge_backend = mock_backend(|_| "a completely unrelated reply about the weather".into());
    for ids in &key_sets {
        let dir = tempfile::tempdir().unwrap();
        let chain = Arc::new(Chain::open(dir.path(), &g).unwrap());
        let cfg = chain.config().clone();
        let (net, events) = start_net(chain.clone(), boot.clone()).await;
        boot.push(addr_of(&net).await);
        let (events, srx) = boltchain::storage::split(events);
        let keys: Vec<BlsSecretKey> = ids.iter().map(|i| dev_key(*i)).collect();
        let pool = Arc::new(TxPool::new(bolt_txpool::PoolConfig::new(
            cfg.chain_id,
            cfg.gas_limit,
            cfg.min_base_fee_wei,
        )));
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let rpc_url = format!("http://127.0.0.1:{port}");
        let storage = Storage::new(
            chain.clone(),
            net.clone(),
            StorageConfig {
                keys: keys.clone(),
                verifier: Some(Arc::new(boltchain::storage::CommandJudge {
                    cmd: format!(
                        "{} judge --backend {judge_backend} --model 1=mock-model",
                        env!("CARGO_BIN_EXE_boltchain")
                    ),
                    rpc: rpc_url.clone(),
                })),
                ..Default::default()
            },
        );
        let s = tokio::spawn(storage.clone().run(srx));
        let ctx = bolt_rpc::RpcContext {
            chain: chain.clone(),
            pool: pool.clone(),
            client_version: "test".into(),
            forwarder: None,
            host: Some(Arc::new(RpcHost {
                storage: storage.clone(),
                rt: tokio::runtime::Handle::current(),
            })),
        };
        let (_, handle) =
            bolt_rpc::start(format!("127.0.0.1:{port}").parse().unwrap(), ctx).await.unwrap();
        rpcs.push(rpc_url);
        handles.push(handle);
        let v = Validator {
            chain: chain.clone(),
            pool: pool.clone(),
            net: net.clone(),
            keys,
            cfg: ValidatorConfig {
                slot_ms: 1000,
                base_timeout_ms: 3000,
                state_dir: dir.path().join("consensus"),
            },
            miner: None,
        };
        let t = tokio::spawn(async move {
            if let Err(e) = v.run(events).await {
                eprintln!("validator: {e:#}");
            }
        });
        nodes.push(Node {
            chain,
            _pool: Some(pool),
            net,
            _storage: storage,
            tasks: vec![s, t],
            _dir: dir,
        });
    }
    let head = |n: &Node| n.chain.head().unwrap().number;
    wait_until("block 1", 60, || nodes.iter().all(|n| head(n) >= 1)).await;

    let backend = mock_backend(|last| format!("echo: {last}"));
    let provider = owner(1);
    let requester = funded();
    let (req_rpc, prov_rpc) = (rpcs[0].clone(), rpcs[1].clone());
    let (p2, r2, backend1) = (provider.clone(), requester.clone(), backend.clone());
    let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, serde_json::Value)> {
        // The provider registers its encryption key (paying gas itself).
        let (_, pk) = boltchain::swarm::vault_key(&p2);
        let reg = IComputeMarket::registerCall {
            encKey: alloy_primitives::B256::from(pk.to_bytes()),
            peerId: Bytes::from_static(b"provider"),
        };
        boltchain::wallet::send_tx(&prov_rpc, &p2, COMPUTE, U256::ZERO, reg.abi_encode().into())?;
        // The requester posts a prompt through its own node.
        let job = NewJob {
            provider: p2.address(),
            model: 1,
            request: serde_json::json!({"messages": [{"role": "user", "content": "hello Boltchain"}], "max_tokens": 64}),
            price_in: U256::from(10u128.pow(15)),
            price_out: U256::from(2 * 10u128.pow(15)),
            max_in: 1000,
            max_out: 64,
            deadline: 3600,
            window: 600,
        };
        let id = post_job(&req_rpc, &r2, &job)?;
        // The provider's scan accepts, runs and delivers.
        let models = [(1u64, "mock-model".to_string())].into_iter().collect();
        let mut p = Provider::new(p2.clone(), &prov_rpc, &backend1, models);
        for _ in 0..30 {
            if p.round()?.contains(&id) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        // The requester reads the result and pays.
        let v = job_result(&req_rpc, &r2, id)?;
        let settle = IComputeMarket::settleCall { id: U256::from(id) };
        boltchain::wallet::send_tx(&req_rpc, &r2, COMPUTE, U256::ZERO, settle.abi_encode().into())?;
        Ok((id, v))
    })
    .await
    .unwrap()
    .unwrap();
    let (id, result) = outcome;
    assert_eq!(result["choices"][0]["message"]["content"], "echo: hello Boltchain");
    assert_eq!(result["model"], "mock-model");
    let c0 = nodes[0].chain.clone();
    let j = view(&c0, COMPUTE, IComputeMarket::jobCall { id: U256::from(id) });
    assert_eq!((j.state, j.tokensIn, j.tokensOut), (5, 12, 7));
    // Paid (12 × 0.001 + 7 × 0.002) / 1000 = 0.000026 BOLT; 20% of it builds the provider's bond.
    let cost = U256::from(26u128 * 10u128.pow(12));
    let bond = cost * U256::from(2000) / U256::from(10000);
    assert_eq!(
        view(&c0, COMPUTE, IComputeMarket::balanceOfCall { account: provider.address() }),
        cost - bond
    );
    assert_eq!(
        view(&c0, COMPUTE, IComputeMarket::providerCall { p: provider.address() }).bond,
        bond
    );

    // A second job, disputed by the requester.
    let (req_rpc, prov_rpc) = (rpcs[0].clone(), rpcs[1].clone());
    let (p2, r2, backend2) = (provider.clone(), requester.clone(), backend.clone());
    let id2 = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let job = NewJob {
            provider: p2.address(),
            model: 1,
            request: serde_json::json!({"messages": [{"role": "user", "content": "what is two plus two"}]}),
            price_in: U256::from(10u128.pow(15)),
            price_out: U256::from(2 * 10u128.pow(15)),
            max_in: 1000,
            max_out: 64,
            deadline: 3600,
            window: 600,
        };
        let id = post_job(&req_rpc, &r2, &job)?;
        let models = [(1u64, "mock-model".to_string())].into_iter().collect();
        let mut p = Provider::new(p2.clone(), &prov_rpc, &backend2, models);
        for _ in 0..30 {
            if p.round()?.contains(&id) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        // The dispute: 1 BOLT deposit, reason encrypted for the requester only.
        let (_, mine) = boltchain::swarm::vault_key(&r2);
        let reason =
            boltchain::compute::seal_and_host(&req_rpc, br#"{"reason": "wrong"}"#, "dispute.json", &[mine])?;
        let d = IComputeMarket::disputeCall { id: U256::from(id), reason: Bytes::from(reason.to_bytes()) };
        boltchain::wallet::send_tx(&req_rpc, &r2, COMPUTE, U256::from(10u128.pow(18)), d.abi_encode().into())?;
        Ok(id)
    })
    .await
    .unwrap()
    .unwrap();
    wait_until("panel assigned", 60, || {
        view(&c0, COMPUTE, IComputeMarket::disputesCall { id: U256::from(id2) }).panelEpoch != 0
    })
    .await;
    // The requester shares the files with the panel (once its node heard the verifier keys).
    let (req_rpc, r2) = (rpcs[0].clone(), requester.clone());
    let shared = tokio::task::spawn_blocking(move || {
        for _ in 0..90 {
            match boltchain::compute::share_dispute(&req_rpc, &r2, id2) {
                Ok(n) => return n,
                Err(e) => eprintln!("share: {e:#}"),
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        0
    })
    .await
    .unwrap();
    assert!(shared > 0, "files shared with the panel");
    wait_until("verdict", 90, || {
        view(&c0, COMPUTE, IComputeMarket::jobCall { id: U256::from(id2) }).state == 6
    })
    .await;
    let p = view(&c0, COMPUTE, IComputeMarket::providerCall { p: provider.address() });
    assert_eq!(p.faults, 1);
    assert!(p.bond < bond, "10% of the bond slashed");

    for h in handles {
        let _ = h.stop();
    }
    for n in nodes {
        for t in n.tasks {
            t.abort();
        }
        n.net.shutdown().await;
    }
}
