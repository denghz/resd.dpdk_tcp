//! Dynamic counter-coverage audit per spec §3.3 / roadmap §A8.
//!
//! One `#[test]` per counter in `ALL_COUNTER_NAMES`. Each test builds
//! a fresh engine, drives the minimal packet/call sequence to exercise
//! the counter's increment site, and asserts the counter > 0.
//!
//! Scenario naming: `cover_<group>_<field>` — the test name carries the
//! counter path so CI failures map directly to the un-covered counter.
//!
//! Feature-gated counters (listed in `feature-gated-counters.txt`) are
//! guarded by `#[cfg(feature = "...")]` so the default-features build
//! does not require a scenario.
//!
//! T4 (this file at its initial landing) establishes the harness + 3
//! warm-up scenarios. T5–T9 fill in the remaining ~110 counters +
//! 121-cell state_trans matrix.
//!
//! **Scenario isolation.** Scenarios run serialized through a
//! binary-wide Mutex inside `CovHarness`: each scenario owns its fresh
//! `Engine`, tests its counter, then drops the engine so the next
//! scenario's `Engine::new` can reuse the DPDK mempool names (which
//! `Engine::new` keys by `lcore_id` — two concurrent engines in one
//! process would collide on the mempool name). See
//! `common::CovHarness` module comment for details.
//!
//! The whole file is gated on `feature = "test-server"` because
//! `CovHarness` reaches for `Engine::inject_rx_frame`, `Engine::listen`,
//! and the test-packet builders — all of which are test-server-only.

#![cfg(feature = "test-server")]

mod common;
use common::CovHarness;

// ---------------------------------------------------------------------
// Warm-up scenarios (T4). Three counters chosen to exercise three
// distinct increment sites:
//   - eth.rx_pkts:      per-burst bump (poll_once analog via CovHarness).
//   - eth.rx_bytes:     per-burst bytes accumulator (same analog).
//   - eth.rx_drop_short: L2Drop::Short arm inside rx_frame (reached
//                       directly by the test-server bypass).
// Collectively these validate that the harness + lookup_counter +
// assertion pattern all work before T5-T9 scale out the scenario set.
// ---------------------------------------------------------------------

/// Covers: `eth.rx_pkts` — per-burst per-mbuf RX-packet counter.
/// Increment site: `poll_once` ~engine.rs:2041 (mirrored by
/// `CovHarness::inject_valid_syn_to_closed_port` — see harness docstring
/// for the rationale on why the test-server bypass can't invoke the
/// real site directly).
#[test]
fn cover_eth_rx_pkts() {
    let mut h = CovHarness::new();
    h.inject_valid_syn_to_closed_port();
    h.assert_counter_gt_zero("eth.rx_pkts");
}

/// Covers: `eth.rx_bytes` — per-burst RX-bytes accumulator. Same
/// injection scenario + increment-site analog as `eth.rx_pkts` (both
/// bumps happen in the same `poll_once` burst-loop iteration).
#[test]
fn cover_eth_rx_bytes() {
    let mut h = CovHarness::new();
    h.inject_valid_syn_to_closed_port();
    h.assert_counter_gt_zero("eth.rx_bytes");
}

/// Covers: `eth.rx_drop_short` — L2 decode short-frame drop. A 10-byte
/// frame is below `ETH_HDR_LEN` (14) so `l2_decode` returns
/// `L2Drop::Short`, bumping this counter at engine.rs:3041. Reached
/// directly via the test-server bypass (the drop site lives inside
/// `rx_frame` which `inject_rx_frame` drives).
#[test]
fn cover_eth_rx_drop_short() {
    let mut h = CovHarness::new();
    h.inject_raw_bytes(&[0u8; 10]);
    h.assert_counter_gt_zero("eth.rx_drop_short");
}
