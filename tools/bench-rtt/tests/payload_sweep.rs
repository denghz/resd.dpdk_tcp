//! TDD entry for Task 4.5 of the 2026-05-09 bench-suite overhaul:
//! bench-rtt MUST expose `--payload-bytes-sweep`,  `--connections`,
//! and `--raw-samples-csv` arguments. Smoke-check via `--help` so we
//! do not need a live engine / kernel peer.

use std::process::Command;

#[test]
fn accepts_payload_sweep_arg() {
    let bin = env!("CARGO_BIN_EXE_bench-rtt");
    let out = Command::new(bin).args(["--help"]).output().expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--payload-bytes-sweep"),
        "--payload-bytes-sweep not in --help output:\n{stdout}"
    );
    assert!(
        stdout.contains("--raw-samples-csv"),
        "--raw-samples-csv not in --help output:\n{stdout}"
    );
    assert!(
        stdout.contains("--connections"),
        "--connections not in --help output:\n{stdout}"
    );
}
