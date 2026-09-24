//! `bolt-sim --scenarios 10000 [--first 0]`: runs random fault scenarios in parallel and fails on
//! any safety violation or (where required) liveness failure.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |name: &str, default: u64| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let count = get("--scenarios", 10_000);
    let first = get("--first", 0);
    let started = Instant::now();
    let chunk = 100u64;
    let chunks: Vec<u64> = (0..count.div_ceil(chunk)).collect();
    use rayon::prelude::*;
    let parts: Vec<bolt_sim::Summary> = chunks
        .par_iter()
        .map(|c| {
            let start = first + c * chunk;
            let n = chunk.min(first + count - start);
            bolt_sim::run_many(start, n)
        })
        .collect();
    let mut total = bolt_sim::Summary::default();
    for p in parts {
        total.scenarios += p.scenarios;
        total.liveness_required += p.liveness_required;
        total.liveness_failures.extend(p.liveness_failures);
        total.safety_violations.extend(p.safety_violations);
        total.messages += p.messages;
    }
    println!(
        "scenarios {} | liveness required in {} | safety violations {} | liveness failures {} | {} messages | {:.1}s",
        total.scenarios,
        total.liveness_required,
        total.safety_violations.len(),
        total.liveness_failures.len(),
        total.messages,
        started.elapsed().as_secs_f64()
    );
    for v in total.safety_violations.iter().take(10) {
        println!("SAFETY: {v}");
    }
    if !total.liveness_failures.is_empty() {
        total.liveness_failures.sort_unstable();
        println!(
            "liveness failures (seeds): {:?}",
            &total.liveness_failures[..total.liveness_failures.len().min(20)]
        );
    }
    if !total.safety_violations.is_empty() || !total.liveness_failures.is_empty() {
        std::process::exit(1);
    }
}
