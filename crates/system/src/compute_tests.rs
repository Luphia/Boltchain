//! ComputeMarket (ADR 0011): a provider with no BOLT registers, takes, delivers and gets paid
//! entirely through signatures others submit; disputes go to a verifier panel; the compute share
//! of the emission pays verified work.

use crate::{abi::*, addresses::*, tests::Evm};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue;

fn bolt(milli: u64) -> U256 {
    U256::from(milli) * U256::from(1_000_000_000_000_000u64)
}

struct Market {
    evm: Evm,
    provider: PrivateKeySigner,
    relayer: Address,
    requester: Address,
}

fn market() -> Market {
    let mut evm = Evm::from_genesis(&crate::tests::dev_genesis());
    evm.timestamp = evm.timestamp.max(1_000_000);
    let relayer = Address::repeat_byte(0xa1);
    let requester = Address::repeat_byte(0xa2);
    evm.fund(relayer, bolt(100_000));
    evm.fund(requester, bolt(100_000));
    Market { evm, provider: PrivateKeySigner::random(), relayer, requester }
}

impl Market {
    /// The provider's signature over a signed action.
    fn sign(&mut self, action: &str, data: Vec<u8>, deadline: u64) -> Bytes {
        let who = self.provider.address();
        let digest = self.evm.view(
            COMPUTE,
            IComputeMarket::actionDigestCall {
                action: B256::right_padding_from(action.as_bytes()),
                signer: who,
                data: data.into(),
                deadline: U256::from(deadline),
            },
        );
        let sig = self.provider.sign_hash_sync(&digest).unwrap();
        Bytes::from(sig.as_bytes().to_vec())
    }

    fn deadline(&self) -> u64 {
        self.evm.timestamp + 3600
    }

    fn register_via_relayer(&mut self, fee: U256) {
        let p = self.provider.address();
        let (key, peer) = (B256::repeat_byte(0x11), Bytes::from_static(b"peer"));
        let d = self.deadline();
        let sig = self.sign("register", (key, peer.clone(), fee).abi_encode_params(), d);
        let relayer = self.relayer;
        self.evm.call(
            relayer,
            COMPUTE,
            U256::ZERO,
            IComputeMarket::registerForCall {
                provider_: p,
                encKey: key,
                peerId: peer,
                fee,
                deadline: U256::from(d),
                sig,
            },
        );
    }

    /// Posts a job (1 BOLT per 1k input tokens, 2 BOLT per 1k output tokens, up to 1,000 of
    /// each, 0.1 BOLT relay fee) and returns its id.
    fn post(&mut self, provider: Address) -> U256 {
        let deadline = self.evm.timestamp + 86_400;
        self.evm.call(
            self.requester,
            COMPUTE,
            bolt(3_100),
            IComputeMarket::postCall {
                provider_: provider,
                model: 1,
                input: Bytes::from_static(b"envelope-cid-in"),
                priceIn: 10u128.pow(18),
                priceOut: 2 * 10u128.pow(18),
                maxIn: 1000,
                maxOut: 1000,
                deadline,
                window: 3600,
                relayFee: 10u128.pow(17),
            },
        )
    }

    fn accept_signed(&mut self, id: U256) -> bool {
        let d = self.deadline();
        let sig = self.sign("accept", id.abi_encode(), d);
        let p = self.provider.address();
        self.evm.try_call(
            self.requester,
            COMPUTE,
            U256::ZERO,
            IComputeMarket::acceptForCall { id, provider_: p, deadline: U256::from(d), sig },
        )
    }

    fn deliver_signed(&mut self, id: U256, tokens_in: u64, tokens_out: u64) {
        let d = self.deadline();
        let out = Bytes::from_static(b"envelope-cid-out");
        let sig =
            self.sign("deliver", (id, out.clone(), tokens_in, tokens_out).abi_encode_params(), d);
        let p = self.provider.address();
        self.evm.call(
            self.relayer,
            COMPUTE,
            U256::ZERO,
            IComputeMarket::deliverForCall {
                id,
                provider_: p,
                output: out,
                tokensIn: tokens_in,
                tokensOut: tokens_out,
                deadline: U256::from(d),
                sig,
            },
        );
    }

    fn balance_in_market(&mut self, a: Address) -> U256 {
        self.evm.view(COMPUTE, IComputeMarket::balanceOfCall { account: a })
    }

    fn bond(&mut self) -> U256 {
        let p = self.provider.address();
        self.evm.view(COMPUTE, IComputeMarket::providerCall { p }).bond
    }
}

#[test]
fn a_provider_without_bolt_is_paid_for_a_job() {
    let mut m = market();
    let p = m.provider.address();
    assert_eq!(m.evm.balance(p), U256::ZERO);

    // A relayer registers the provider for a 0.5 BOLT fee, repaid from the first earnings.
    m.register_via_relayer(bolt(500));
    let info = m.evm.view(COMPUTE, IComputeMarket::providerCall { p });
    assert!(info.registered);
    assert_eq!(info.debt, bolt(500));
    assert_eq!(
        m.evm.view(COMPUTE, IComputeMarket::encryptionKeyCall { account: p }),
        B256::repeat_byte(0x11)
    );

    // The requester posts, and submits the provider's signed acceptance.
    let id = m.post(Address::ZERO);
    assert!(m.accept_signed(id));
    m.evm.call(
        m.requester,
        COMPUTE,
        U256::ZERO,
        IComputeMarket::shareInputCall { id, input: Bytes::from_static(b"envelope-with-provider") },
    );

    // Signed receipt: 500 input and 1,000 output tokens = 0.5 + 2 = 2.5 BOLT.
    m.deliver_signed(id, 500, 1000);
    // Only the requester may settle before the dispute window ends.
    assert!(!m.evm.try_call(m.relayer, COMPUTE, U256::ZERO, IComputeMarket::settleCall { id }));
    m.evm.timestamp += 3601;
    let relayer_before = m.balance_in_market(m.relayer);
    m.evm.call(m.relayer, COMPUTE, U256::ZERO, IComputeMarket::settleCall { id });

    // 2.5 BOLT: 0.5 repays the relayer, 20% of the remaining 2.0 goes to the bond, 1.6 to the
    // provider. The relayer also earns the 0.1 relay fee; the requester gets 0.5 back.
    assert_eq!(m.balance_in_market(m.relayer) - relayer_before, bolt(600));
    assert_eq!(m.bond(), bolt(400));
    assert_eq!(m.balance_in_market(p), bolt(1_600));
    assert_eq!(m.balance_in_market(m.requester), bolt(500));
    assert_eq!(m.evm.view(COMPUTE, IComputeMarket::providerCall { p }).debt, U256::ZERO);

    // Anyone can push the provider's balance to it: it never spent gas.
    m.evm.call(m.relayer, COMPUTE, U256::ZERO, IComputeMarket::payoutCall { account: p });
    assert_eq!(m.evm.balance(p), bolt(1_600));
}

#[test]
fn signatures_are_bound_to_signer_action_and_nonce() {
    let mut m = market();
    m.register_via_relayer(U256::ZERO);
    let id = m.post(Address::ZERO);
    let p = m.provider.address();
    let d = m.deadline();
    // A signature for another action does not accept the job.
    let wrong = m.sign("deliver", id.abi_encode(), d);
    assert!(!m.evm.try_call(
        m.requester,
        COMPUTE,
        U256::ZERO,
        IComputeMarket::acceptForCall { id, provider_: p, deadline: U256::from(d), sig: wrong },
    ));
    let sig = m.sign("accept", id.abi_encode(), d);
    let call = IComputeMarket::acceptForCall { id, provider_: p, deadline: U256::from(d), sig };
    assert!(m.evm.try_call(m.requester, COMPUTE, U256::ZERO, call.clone()));
    // Replayed on a second job: the nonce moved on.
    let id2 = m.post(Address::ZERO);
    let replay = IComputeMarket::acceptForCall { id: id2, ..call };
    assert!(!m.evm.try_call(m.requester, COMPUTE, U256::ZERO, replay));
    // Expired deadline.
    let sig = m.sign("accept", id2.abi_encode(), m.evm.timestamp - 1);
    assert!(!m.evm.try_call(
        m.requester,
        COMPUTE,
        U256::ZERO,
        IComputeMarket::acceptForCall {
            id: id2,
            provider_: p,
            deadline: U256::from(m.evm.timestamp - 1),
            sig
        },
    ));
}

#[test]
fn job_limits_refunds_and_lateness() {
    let mut m = market();
    m.register_via_relayer(U256::ZERO);
    let p = m.provider.address();
    // A new provider (no bond) may take jobs up to 5 BOLT of escrow.
    assert_eq!(m.evm.view(COMPUTE, IComputeMarket::jobLimitCall { p }), bolt(5_000));
    let big = m.evm.call(
        m.requester,
        COMPUTE,
        bolt(6_000),
        IComputeMarket::postCall {
            provider_: Address::ZERO,
            model: 1,
            input: Bytes::new(),
            priceIn: 3 * 10u128.pow(18),
            priceOut: 3 * 10u128.pow(18),
            maxIn: 1000,
            maxOut: 1000,
            deadline: m.evm.timestamp + 100,
            window: 600,
            relayFee: 0,
        },
    );
    assert!(!m.accept_signed(big), "over the limit");
    // The requester cancels an open job.
    m.evm.call(m.requester, COMPUTE, U256::ZERO, IComputeMarket::refundCall { id: big });
    assert_eq!(m.balance_in_market(m.requester), bolt(6_000));

    // Accepted but not delivered by the deadline: refunded, and the provider's fault count grows.
    let id = m.post(Address::ZERO);
    assert!(m.accept_signed(id));
    assert!(!m.evm.try_call(m.relayer, COMPUTE, U256::ZERO, IComputeMarket::refundCall { id }));
    m.evm.timestamp += 86_401;
    m.evm.call(m.relayer, COMPUTE, U256::ZERO, IComputeMarket::refundCall { id });
    assert_eq!(m.evm.view(COMPUTE, IComputeMarket::providerCall { p }).faults, 1);
    // A job reserved for another provider cannot be taken.
    let other = m.post(Address::repeat_byte(0x77));
    assert!(!m.accept_signed(other));
}

#[test]
fn disputes_go_to_the_verifier_panel() {
    let mut m = market();
    m.register_via_relayer(U256::ZERO);
    let p = m.provider.address();
    // Build some bond first: one settled job (2.5 BOLT -> 0.5 bond).
    let id = m.post(Address::ZERO);
    assert!(m.accept_signed(id));
    m.deliver_signed(id, 500, 1000);
    m.evm.call(m.requester, COMPUTE, U256::ZERO, IComputeMarket::settleCall { id });
    assert_eq!(m.bond(), bolt(500));

    // Job 2: disputed; the panel finds the provider at fault.
    let id = m.post(Address::ZERO);
    assert!(m.accept_signed(id));
    m.deliver_signed(id, 500, 1000);
    let disp = IComputeMarket::disputeCall { id, reason: Bytes::from_static(b"statement") };
    assert!(!m.evm.try_call(m.requester, COMPUTE, bolt(100), disp.clone()), "deposit is 1 BOLT");
    assert!(!m.evm.try_call(m.relayer, COMPUTE, bolt(1_000), disp.clone()), "requester only");
    m.evm.call(m.requester, COMPUTE, bolt(1_000), disp);
    // Neither settlement nor a verdict before a panel is assigned.
    assert!(!m.evm.try_call(m.requester, COMPUTE, U256::ZERO, IComputeMarket::settleCall { id }));
    assert!(
        !m.evm.system(COMPUTE, IComputeMarket::recordVerdictCall { id, providerAtFault: true })
    );
    assert_eq!(m.evm.view(COMPUTE, IComputeMarket::pendingDisputesCall {}), vec![id]);
    m.evm.system(COMPUTE, IComputeMarket::assignDisputesCall { epoch: 3, members: vec![1, 2, 3] });
    assert_eq!(m.evm.view(COMPUTE, IComputeMarket::panelCall { epoch: 3 }), vec![1, 2, 3]);
    let before = m.balance_in_market(m.requester);
    assert!(m.evm.system(COMPUTE, IComputeMarket::recordVerdictCall { id, providerAtFault: true }));
    // Escrow 3.1 + deposit 1 + 10% of the 0.5 bond.
    assert_eq!(m.balance_in_market(m.requester) - before, bolt(3_100 + 1_000 + 50));
    assert_eq!(m.bond(), bolt(450));
    assert_eq!(m.evm.view(COMPUTE, IComputeMarket::providerCall { p }).faults, 1);

    // Job 3: disputed, provider cleared: paid, and the deposit compensates it.
    let id = m.post(Address::ZERO);
    assert!(m.accept_signed(id));
    m.deliver_signed(id, 0, 500);
    m.evm.call(
        m.requester,
        COMPUTE,
        bolt(1_000),
        IComputeMarket::disputeCall { id, reason: Bytes::new() },
    );
    m.evm.system(COMPUTE, IComputeMarket::assignDisputesCall { epoch: 4, members: vec![4] });
    let before = m.balance_in_market(p);
    assert!(
        m.evm.system(COMPUTE, IComputeMarket::recordVerdictCall { id, providerAtFault: false })
    );
    // 1 BOLT cost: 0.2 to the bond, 0.8 paid; plus the 1 BOLT deposit.
    assert_eq!(m.balance_in_market(p) - before, bolt(1_800));

    // Job 4: no verdict for 7 days: settles for the provider, the requester's deposit returns.
    let id = m.post(Address::ZERO);
    assert!(m.accept_signed(id));
    m.deliver_signed(id, 0, 500);
    m.evm.call(
        m.requester,
        COMPUTE,
        bolt(1_000),
        IComputeMarket::disputeCall { id, reason: Bytes::new() },
    );
    assert!(!m.evm.try_call(
        m.relayer,
        COMPUTE,
        U256::ZERO,
        IComputeMarket::expireDisputeCall { id }
    ));
    m.evm.timestamp += 7 * 86_400;
    let before = m.balance_in_market(m.requester);
    m.evm.call(m.relayer, COMPUTE, U256::ZERO, IComputeMarket::expireDisputeCall { id });
    assert_eq!(m.balance_in_market(m.requester) - before, bolt(1_000 + 2_100));
}

#[test]
fn only_the_node_records_verdicts() {
    let mut m = market();
    m.register_via_relayer(U256::ZERO);
    assert!(!m.evm.try_call(
        m.requester,
        COMPUTE,
        U256::ZERO,
        IComputeMarket::recordVerdictCall { id: U256::ZERO, providerAtFault: true },
    ));
}

#[test]
fn bond_is_capped_from_earnings() {
    let mut m = market();
    m.register_via_relayer(U256::ZERO);
    let p = m.provider.address();
    m.evm.fund(m.requester, bolt(1_000_000));
    // Each job earns 3 BOLT (1,000 + 1,000 tokens), 0.6 of it to the bond: 107 jobs pass 64.
    for _ in 0..107 {
        let id = m.post(p);
        assert!(m.accept_signed(id));
        m.deliver_signed(id, 1000, 1000);
        m.evm.call(m.requester, COMPUTE, U256::ZERO, IComputeMarket::settleCall { id });
    }
    assert_eq!(m.bond(), bolt(64_000));
    assert_eq!(m.balance_in_market(p), bolt(107 * 3_000 - 64_000));
    // Bond larger: larger jobs (5 + 10 x 64 BOLT).
    assert_eq!(m.evm.view(COMPUTE, IComputeMarket::jobLimitCall { p }), bolt(645_000));
}
