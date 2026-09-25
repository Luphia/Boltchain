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
    let bound = serve(addr, Arc::new(move |req: &str| respond(&chain, req))).await?;
    tracing::info!(%bound, "IPFS gateway listening");
    Ok(bound)
}

/// A request handler: raw request head in, (status, content type, body) out. Runs on a
/// blocking thread.
pub type Handler = Arc<dyn Fn(&str) -> Response + Send + Sync>;

/// Minimal HTTP/1.1 server (one request per connection, GET only in practice) shared by the
/// gateway and the metrics endpoint.
pub async fn serve(addr: SocketAddr, handler: Handler) -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let limits = Arc::new(Limits::default());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, peer)) = listener.accept().await else { continue };
            let handler = handler.clone();
            let limits = limits.clone();
            tokio::spawn(async move {
                // Public endpoint: bound concurrent requests and each client's request rate.
                let permit = limits.busy.clone().try_acquire_owned();
                let refusal = match (&permit, limits.allow(peer.ip())) {
                    (Err(_), _) => Some((503, "server busy, retry shortly")),
                    (_, false) => Some((429, "too many requests")),
                    _ => None,
                };
                if let Some((status, msg)) = refusal {
                    let head = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nRetry-After: 2\r\nConnection: close\r\n\r\n",
                        reason(status),
                        msg.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(msg.as_bytes()).await;
                    let _ = sock.shutdown().await;
                    return;
                }
                let mut buf = vec![0u8; 8192];
                let mut n = 0;
                while n < buf.len() {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        sock.read(&mut buf[n..]),
                    )
                    .await
                    {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                        Ok(Ok(k)) => n += k,
                    }
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, ctype, body) = tokio::task::spawn_blocking(move || handler(&req))
                    .await
                    .unwrap_or((500, "text/plain", b"internal error".to_vec()));
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
    Ok(bound)
}

/// Request limits of the public HTTP endpoint.
struct Limits {
    /// Requests served at once.
    busy: Arc<tokio::sync::Semaphore>,
    /// Token bucket per client address: (tokens, last refill).
    buckets:
        parking_lot::Mutex<std::collections::HashMap<std::net::IpAddr, (f64, std::time::Instant)>>,
}

/// Sustained requests per second per client, and the burst allowed.
const RATE: f64 = 20.0;
const BURST: f64 = 80.0;

impl Default for Limits {
    fn default() -> Self {
        Self {
            busy: Arc::new(tokio::sync::Semaphore::new(128)),
            buckets: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Limits {
    fn allow(&self, ip: std::net::IpAddr) -> bool {
        let now = std::time::Instant::now();
        let mut b = self.buckets.lock();
        if b.len() > 50_000 {
            b.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
        }
        let (tokens, last) = b.entry(ip).or_insert((BURST, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * RATE).min(BURST);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// (status, content type, body).
pub type Response = (u16, &'static str, Vec<u8>);

/// A plain-text response.
pub fn text(status: u16, msg: impl Into<String>) -> Response {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_client_is_rate_limited() {
        let l = Limits::default();
        let a: std::net::IpAddr = [1, 2, 3, 4].into();
        let b: std::net::IpAddr = [5, 6, 7, 8].into();
        let served = (0..200).filter(|_| l.allow(a)).count();
        assert!((80..=90).contains(&served), "burst then refused: {served}");
        assert!(l.allow(b), "other clients unaffected");
    }
}
