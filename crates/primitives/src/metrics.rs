//! Process-wide counters and gauges, rendered in the Prometheus text format by the node's
//! `--metrics` endpoint. Plain atomics: incrementing costs one relaxed add, so any crate can
//! count events on hot paths.

use std::sync::atomic::{AtomicU64, Ordering};

/// A named counter or gauge.
#[derive(Debug)]
pub struct Metric {
    /// Prometheus name.
    pub name: &'static str,
    /// Help text.
    pub help: &'static str,
    /// `true` for a gauge (set), `false` for a counter (only increases).
    pub gauge: bool,
    value: AtomicU64,
}

impl Metric {
    const fn counter(name: &'static str, help: &'static str) -> Self {
        Self { name, help, gauge: false, value: AtomicU64::new(0) }
    }

    const fn gauge(name: &'static str, help: &'static str) -> Self {
        Self { name, help, gauge: true, value: AtomicU64::new(0) }
    }

    /// Adds one.
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds `n`.
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// Sets a gauge.
    pub fn set(&self, v: u64) {
        self.value.store(v, Ordering::Relaxed);
    }

    /// Current value.
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

macro_rules! metrics {
    ($($kind:ident $id:ident = $name:literal, $help:literal;)*) => {
        $(
            #[doc = $help]
            pub static $id: Metric = Metric::$kind(concat!("bolt_", $name), $help);
        )*
        static ALL: &[&Metric] = &[$(&$id),*];
        /// Every metric, in declaration order.
        pub fn all() -> &'static [&'static Metric] {
            ALL
        }
    };
}

metrics! {
    counter HASHES = "hashes_total", "RandomBOLT hashes computed by this node's miner";
    counter BLOCKS_MINED = "blocks_mined_total", "Blocks this node mined that extended its chain";
    counter BLOCKS_IMPORTED = "blocks_imported_total", "Blocks imported from the network";
    counter IMPORT_ERRORS = "import_errors_total", "Announcements that could not be imported";
    counter REORGS = "reorgs_total", "Chain reorganisations (mined phase)";
    counter PROPOSALS_REJECTED = "proposals_rejected_total", "Consensus proposals this node refused to vote for";
    counter BITSWAP_SERVED = "bitswap_blocks_served_total", "IPFS blocks sent to peers over Bitswap";
    counter BITSWAP_FETCH_FAILURES = "bitswap_fetch_failures_total", "Bitswap fetches that timed out";
    counter EVENTS_DROPPED = "net_events_dropped_total", "Network events dropped because the node fell behind";
    counter AUDIT_VOTES = "audit_votes_total", "Storage-audit votes this node signed";
    counter AUDITS_CERTIFIED = "audits_certified_total", "Storage-audit certificates assembled here";
    counter SNAPSHOTS = "snapshots_total", "State snapshots taken";
    counter BLOCKS_PRUNED = "blocks_pruned_total", "Block bodies pruned";
    counter TXS_ADMITTED = "txs_admitted_total", "Transactions admitted to the pool";
    counter CONSENSUS_MSGS = "consensus_messages_total", "Consensus messages received (proposals, votes, timeouts)";
    counter CONSENSUS_BYTES = "consensus_bytes_total", "Bytes of consensus messages received";
    counter BLS_VERIFIES = "bls_verifications_total", "BLS signature verifications (single and aggregate)";
    counter BLS_VERIFY_MICROS = "bls_verify_microseconds_total", "Time spent verifying BLS signatures, in microseconds";
    gauge PEERS = "peers", "Connected peers";
    gauge DOPPELGANGER = "doppelganger", "Validator keys of this node seen signing on another node (those keys stopped signing)";
}

/// Renders every metric plus `extra` gauges (name, help, value) in the Prometheus text format.
pub fn render(extra: &[(&str, &str, f64)]) -> String {
    let mut out = String::new();
    for m in all() {
        let kind = if m.gauge { "gauge" } else { "counter" };
        out += &format!(
            "# HELP {} {}\n# TYPE {} {kind}\n{} {}\n",
            m.name,
            m.help,
            m.name,
            m.name,
            m.get()
        );
    }
    for (name, help, v) in extra {
        out += &format!("# HELP bolt_{name} {help}\n# TYPE bolt_{name} gauge\nbolt_{name} {v}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_prometheus_text() {
        super::BLOCKS_PRUNED.add(3);
        let t = super::render(&[("head", "Head block", 7.0)]);
        assert!(
            t.contains("# TYPE bolt_blocks_pruned_total counter\nbolt_blocks_pruned_total 3\n")
        );
        assert!(t.contains("bolt_head 7\n"));
    }
}
