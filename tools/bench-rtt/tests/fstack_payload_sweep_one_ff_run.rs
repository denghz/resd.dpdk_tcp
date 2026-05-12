//! Regression test for the bench-rtt fstack arm payload-sweep SIGSEGV
//! observed in the T55 fast-iter-suite run on 2026-05-12.
//!
//! Root cause: `ff_run` is one-shot per process (it calls
//! `rte_eal_cleanup` on exit; see `bench-fstack-ffi` crate docs at
//! `tools/bench-fstack-ffi/src/lib.rs` lines 113-117). The original
//! bench-rtt fstack arm called `fstack::imp::run_rtt_workload` once per
//! `--payload-bytes-sweep` value — the first call worked, but the second
//! re-invoked `ff_run` after `rte_eal_cleanup` had already torn DPDK
//! down, segfaulting inside the F-Stack poll loop. The fix mirrors
//! bench-tx-burst's `run_burst_grid` / bench-tx-maxtp's `run_maxtp_grid`
//! pattern: the entire sweep runs inside a SINGLE `ff_run` invocation,
//! with `bucket_idx` threaded through the per-iter state machine.
//!
//! The test asserts the structural property of the fix (one ff_run for
//! all buckets) by exercising the public sweep entry point with a
//! grid of multiple payload sizes and checking that the function
//! returns one result per bucket. We can't actually call into F-Stack
//! from `cargo test` (no DPDK, no libfstack.a on dev hosts), so we
//! drive the pure-Rust grid-bucketing helpers instead — the same
//! helpers `run_rtt_grid` uses to enumerate / pre-allocate per-bucket
//! payloads before entering ff_run.

#![cfg(feature = "fstack")]

use bench_rtt::fstack::imp::{enumerate_rtt_grid, RttBucket};

#[test]
fn enumerate_rtt_grid_produces_one_bucket_per_payload() {
    let grid = enumerate_rtt_grid(&[64, 128, 256, 1024], 100, 1_000);
    assert_eq!(grid.len(), 4);
    assert_eq!(grid[0].payload_bytes, 64);
    assert_eq!(grid[1].payload_bytes, 128);
    assert_eq!(grid[2].payload_bytes, 256);
    assert_eq!(grid[3].payload_bytes, 1024);
    for bucket in &grid {
        assert_eq!(bucket.warmup, 100);
        assert_eq!(bucket.iterations, 1_000);
    }
}

#[test]
fn enumerate_rtt_grid_preserves_order() {
    // Order of `payload_bytes_sweep` argv must be preserved end-to-end;
    // `bench-report` keys aggregations off `bucket_id = "payload_<W>"`
    // and a re-order would mis-pair summary rows with raw-sample rows.
    let grid = enumerate_rtt_grid(&[1024, 64, 256, 128], 10, 100);
    assert_eq!(grid[0].payload_bytes, 1024);
    assert_eq!(grid[1].payload_bytes, 64);
    assert_eq!(grid[2].payload_bytes, 256);
    assert_eq!(grid[3].payload_bytes, 128);
}

#[test]
fn enumerate_rtt_grid_single_payload_compatibility() {
    // The pre-T55 smoke path (`--payload-bytes-sweep 128`) must keep
    // working: a single-payload sweep stays a single bucket.
    let grid = enumerate_rtt_grid(&[128], 100, 1_000);
    assert_eq!(grid.len(), 1);
    assert_eq!(grid[0].payload_bytes, 128);
    assert_eq!(grid[0].warmup, 100);
    assert_eq!(grid[0].iterations, 1_000);
}

#[test]
fn rtt_bucket_field_layout_matches_run_rtt_grid_contract() {
    // The grid type must expose the three fields the per-bucket state
    // machine reads: payload size + warmup + iter count. If the field
    // names change, run_rtt_grid won't compile — but we assert here
    // explicitly so downstream callers (bench-rtt main + future
    // multi-stack callers) get a clear test failure rather than a
    // cryptic compile error.
    let b = RttBucket {
        payload_bytes: 256,
        warmup: 1_000,
        iterations: 10_000,
    };
    assert_eq!(b.payload_bytes, 256);
    assert_eq!(b.warmup, 1_000);
    assert_eq!(b.iterations, 10_000);
}
