//! TDD entry for Phase 8 Task 8.2 — `bench-rx-burst` CLI surface.
//!
//! Asserts the `--stack` enum rejects unknown values, and that the
//! help output advertises the RX-burst-specific flags
//! (`--segment-sizes`, `--burst-counts`, `--peer-control-port`). The
//! live-engine paths aren't unit-testable without a peer, so this is
//! a CLI-shape regression guard.

use std::process::Command;

#[test]
fn rejects_unknown_stack() {
    let bin = env!("CARGO_BIN_EXE_bench-rx-burst");
    let out = Command::new(bin)
        .args([
            "--stack",
            "wireshark",
            "--peer-ip",
            "127.0.0.1",
            "--output-csv",
            "/tmp/x.csv",
        ])
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "expected non-zero exit for invalid stack");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid value 'wireshark' for '--stack"),
        "stderr does not mention the bogus stack: {stderr}"
    );
}

#[test]
fn shows_segment_size_and_burst_count_args() {
    let bin = env!("CARGO_BIN_EXE_bench-rx-burst");
    let out = Command::new(bin).args(["--help"]).output().expect("spawn");
    assert!(out.status.success(), "--help should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--segment-sizes"),
        "--help output missing --segment-sizes: {stdout}"
    );
    assert!(
        stdout.contains("--burst-counts"),
        "--help output missing --burst-counts: {stdout}"
    );
    assert!(
        stdout.contains("--peer-control-port"),
        "--help output missing --peer-control-port: {stdout}"
    );
}
