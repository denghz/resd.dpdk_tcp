//! TDD entry for Task 4.3 of the 2026-05-09 bench-suite overhaul:
//! `bench-rtt` MUST take a `--stack {dpdk_net|linux_kernel|fstack}` arg
//! and reject unknown values via clap. We assert the failure mode rather
//! than the success path because the success path requires a live engine
//! / Linux peer / F-Stack binary that aren't available in `cargo test`.

use std::process::Command;

#[test]
fn rejects_unknown_stack() {
    let bin = env!("CARGO_BIN_EXE_bench-rtt");
    let out = Command::new(bin)
        .args([
            "--stack",
            "wireshark",
            "--peer-ip",
            "127.0.0.1",
            "--peer-port",
            "10001",
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
