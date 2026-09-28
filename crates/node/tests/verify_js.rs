//! Runs the explorer's evidence verification library (`src/explorer/verify.js`) against
//! reference vectors from independent implementations (see `verify_js.mjs`). Needs Node.js; the
//! test is skipped with a message when `node` is not installed.

#[test]
fn verify_js_matches_reference_vectors() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = dir.join("tests/verify_js.mjs");
    let out = match std::process::Command::new("node").arg(&script).output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: node is not installed");
            return;
        }
        Err(e) => panic!("running node: {e}"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "verify_js.mjs failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("checks passed"), "{stdout}");
}
