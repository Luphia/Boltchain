use super::*;
use parking_lot::RwLock;
use std::collections::HashMap;

#[derive(Default)]
struct Mem(RwLock<HashMap<Cid, Vec<u8>>>);
impl BlockSource for Mem {
    fn get(&self, cid: &Cid) -> Option<Vec<u8>> {
        self.0.read().get(cid).cloned()
    }
}

fn local() -> Vec<Multiaddr> {
    vec!["/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()]
}

async fn node(
    producer: Option<PeerId>,
    bootnodes: Vec<Multiaddr>,
    key: identity::Keypair,
) -> (NetHandle, mpsc::Receiver<NetEvent>, Arc<Mem>) {
    let mem = Arc::new(Mem::default());
    let (h, rx) = start(
        NetConfig { chain_id: 1337, keypair: key, listen: local(), bootnodes, producer },
        mem.clone(),
    )
    .await
    .unwrap();
    (h, rx, mem)
}

async fn addrs(h: &NetHandle) -> Vec<Multiaddr> {
    for _ in 0..50 {
        let a = h.listen_addrs().await;
        if !a.is_empty() {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no listen address");
}

#[tokio::test(flavor = "multi_thread")]
async fn announce_fetch_and_forward() {
    let ka = identity::Keypair::generate_ed25519();
    let a_id = ka.public().to_peer_id();
    let (a, mut a_rx, a_mem) = node(None, vec![], ka).await;
    let (b, mut b_rx, _b_mem) =
        node(Some(a_id), addrs(&a).await, identity::Keypair::generate_ed25519()).await;

    // A holds two blocks, one of them larger than a gossip message.
    let small = b"small block".to_vec();
    let big = vec![7u8; 900 * 1024];
    let (c1, c2) = (
        bolt_ipld::sha256_cid(bolt_ipld::RAW, &small),
        bolt_ipld::sha256_cid(bolt_ipld::RAW, &big),
    );
    a_mem.0.write().insert(c1, small.clone());
    a_mem.0.write().insert(c2, big.clone());

    // Wait until the gossipsub mesh has B subscribed, then announce.
    let ann = Announce { height: 1, root: c1, envelope: vec![1], header: vec![2], inline: vec![] };
    let got = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            a.publish(ann.clone()).await.unwrap();
            if let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(500), b_rx.recv()).await
                && let NetEvent::Announce { via, announce } = ev
            {
                return (via, announce);
            }
        }
    })
    .await
    .expect("announcement delivered");
    assert_eq!(got.0, a_id);
    assert_eq!(got.1, ann);

    // B fetches both blocks from A over bitswap.
    let t = std::time::Instant::now();
    let blocks = b.fetch(&[c1, c2], &[a_id]).await.unwrap();
    println!("fetched {} KiB in {:?}", (small.len() + big.len()) / 1024, t.elapsed());
    for i in 0..3u8 {
        let d = vec![i; 190 * 1024];
        let c = bolt_ipld::sha256_cid(bolt_ipld::RAW, &d);
        a_mem.0.write().insert(c, d);
        let t = std::time::Instant::now();
        b.fetch(&[c], &[a_id]).await.unwrap();
        println!("fetched 190 KiB in {:?}", t.elapsed());
        let d = vec![i + 10; 10];
        let c = bolt_ipld::sha256_cid(bolt_ipld::RAW, &d);
        a_mem.0.write().insert(c, d);
        let t = std::time::Instant::now();
        b.fetch(&[c], &[a_id]).await.unwrap();
        println!("fetched 10 B in {:?}", t.elapsed());
    }
    assert_eq!(blocks[&c1], small);
    assert_eq!(blocks[&c2], big);

    // Unknown blocks time out instead of hanging.
    let missing = bolt_ipld::sha256_cid(bolt_ipld::RAW, b"nobody has this");
    assert!(b.fetch(&[missing], &[a_id]).await.is_err());

    // B forwards a transaction to A; A answers.
    let fwd = tokio::spawn({
        let b = b.clone();
        async move { b.forward_tx(a_id, vec![0xde, 0xad]).await }
    });
    loop {
        match a_rx.recv().await.unwrap() {
            NetEvent::Tx { raw, reply } => {
                assert_eq!(raw, vec![0xde, 0xad]);
                reply.send(Ok(B256::repeat_byte(9))).unwrap();
                break;
            }
            _ => continue,
        }
    }
    assert_eq!(fwd.await.unwrap(), Ok(B256::repeat_byte(9)));
}

#[tokio::test(flavor = "multi_thread")]
async fn announcements_from_other_authors_are_dropped() {
    let producer = identity::Keypair::generate_ed25519().public().to_peer_id();
    let (a, _a_rx, _) = node(None, vec![], identity::Keypair::generate_ed25519()).await;
    let (_b, mut b_rx, _) =
        node(Some(producer), addrs(&a).await, identity::Keypair::generate_ed25519()).await;
    let ann = Announce {
        height: 1,
        root: bolt_ipld::sha256_cid(bolt_ipld::RAW, b"x"),
        envelope: vec![],
        header: vec![],
        inline: vec![],
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline {
        a.publish(ann.clone()).await.unwrap();
        if let Ok(Some(NetEvent::Announce { .. })) =
            tokio::time::timeout(Duration::from_millis(300), b_rx.recv()).await
        {
            panic!("announcement from a non-producer must be rejected");
        }
    }
}

#[test]
fn mainnet_bootnodes_are_dnsaddr() {
    let b = mainnet_bootnodes();
    assert_eq!(b.len(), 9);
    assert_eq!(b[0].to_string(), "/dnsaddr/node001.cafeca.io");
    assert_eq!(b[8].to_string(), "/dnsaddr/node009.cafeca.io");
}
