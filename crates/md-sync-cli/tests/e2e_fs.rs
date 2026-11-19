//! End-to-end test: runs the demo script against the built `md-sync`
//! binary. The script itself asserts every behaviour (two-way sync,
//! no-op second run, edit/delete propagation, both conflict policies,
//! dry-run plan, push mirror, backup protection, list + JSON).

#[test]
fn demo_script_passes() {
    let bin = env!("CARGO_BIN_EXE_md-sync");
    let script = format!("{}/../../demo/run-demo.sh", env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new("bash")
        .arg(&script)
        .arg(bin)
        .output()
        .expect("run demo script");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "demo script failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(stdout.contains("ALL DEMO CHECKS PASSED"), "{stdout}");
}
