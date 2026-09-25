//! Public read-only HTTP gateway (ADR 0009): a subset of the IPFS trustless gateway over the
//! node's blockstore, so anyone can fetch and verify history without running a node.
//!
//! - `GET /ipfs/<cid>?format=raw` (or `Accept: application/vnd.ipld.raw`): one block.
//! - `GET /ipfs/<cid>?format=car` (or `Accept: application/vnd.ipld.car`): a CAR v1 of the DAG
//!   under `<cid>` (an envelope's `parent` link is not followed, so an epoch index gives exactly
//!   that epoch).
//! - `GET /history/epoch/<n>.car`: the CAR of epoch `n`, located through `HistoryRegistry`.
//!
//! Every block is content-addressed, so clients verify what they receive and need not trust the
//! gateway. Blocks the node pruned are answered with 404.

use anyhow::Result;
use bolt_chain::Chain;
use bolt_ipld::{Cid, car};
use bolt_store::StateView;
use bolt_system::{abi::IHistoryRegistry, addresses::HISTORY, queries};
use std::{net::SocketAddr, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Starts the gateway; returns the bound address.
pub async fn start(addr: SocketAddr, chain: Arc<Chain>) -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { continue };
            let chain = chain.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut n = 0;
                while n < buf.len() {
                    match sock.read(&mut buf[n..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(k) => n += k,
                    }
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let chain2 = chain.clone();
                let (status, ctype, body) =
                    tokio::task::spawn_blocking(move || respond(&chain2, &req)).await.unwrap_or((
                        500,
                        "text/plain",
                        b"internal error".to_vec(),
                    ));
                let head = format!(
                    "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                    reason(status),
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    tracing::info!(%bound, "IPFS gateway listening");
    Ok(bound)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    }
}

type Response = (u16, &'static str, Vec<u8>);

fn text(status: u16, msg: impl Into<String>) -> Response {
    (status, "text/plain; charset=utf-8", msg.into().into_bytes())
}

/// Answers one request (`req` is the raw request head).
pub fn respond(chain: &Chain, req: &str) -> Response {
    let mut lines = req.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let (method, target) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
    if method != "GET" {
        return text(405, "only GET is supported");
    }
    let accept = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("accept"))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let format = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("format="))
        .map(str::to_string)
        .unwrap_or_else(|| {
            if accept.contains("application/vnd.ipld.car") { "car".into() } else { "raw".into() }
        });
    let Ok(r) = chain.store().reader() else { return text(500, "store unavailable") };
    let get = |c: &Cid| r.ipld(c).ok().flatten();
    let root = if let Some(c) = path.strip_prefix("/ipfs/") {
        match Cid::try_from(c.trim_end_matches('/')) {
            Ok(c) => c,
            Err(_) => return text(400, "bad CID"),
        }
    } else if let Some(n) =
        path.strip_prefix("/history/epoch/").and_then(|s| s.strip_suffix(".car"))
    {
        let Ok(epoch) = n.parse::<u64>() else { return text(400, "bad epoch") };
        let idx = queries::call(
            &StateView::latest(&r),
            chain.config().chain_id,
            HISTORY,
            IHistoryRegistry::epochIndexCall { epoch },
        );
        match idx.ok().and_then(|b| Cid::try_from(b.as_ref()).ok()) {
            Some(c) => return car_of(c, get),
            None => return text(404, format!("epoch {epoch} is not indexed yet")),
        }
    } else {
        return text(404, "not found");
    };
    match format.as_str() {
        "raw" => match get(&root) {
            Some(b) => (200, "application/vnd.ipld.raw", b),
            None => text(404, format!("block {root} not held by this node")),
        },
        "car" => car_of(root, get),
        _ => text(400, "format must be raw or car"),
    }
}

fn car_of(root: Cid, get: impl FnMut(&Cid) -> Option<Vec<u8>>) -> Response {
    match car::traverse(root, get) {
        Ok(blocks) => (200, "application/vnd.ipld.car; version=1", car::write(&[root], &blocks)),
        Err(e) => text(404, format!("{e} (pruned or never held by this node)")),
    }
}

/// Writes the CAR of epoch `epoch` from a datadir (offline export).
pub fn export_epoch(chain: &Chain, epoch: u64) -> Result<Vec<u8>> {
    let (status, _, body) =
        respond(chain, &format!("GET /history/epoch/{epoch}.car HTTP/1.1\r\n\r\n"));
    if status != 200 {
        anyhow::bail!("{}", String::from_utf8_lossy(&body));
    }
    Ok(body)
}
