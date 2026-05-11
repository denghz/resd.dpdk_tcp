//! Phase 11 Task 11.1: tcp.tx_retrans is split into per-trigger sub-counters.
//!
//! The aggregate `tcp.tx_retrans` retains its original semantics for back-
//! compat (every retransmit emit bumps it). The new sub-counters
//! `tcp.tx_retrans_rto`, `tcp.tx_retrans_rack`, `tcp.tx_retrans_tlp` partition
//! the aggregate by which timer/loss-detector triggered the retransmission,
//! so bench-stress-style assertions can distinguish recovery mechanisms.
//!
//! These tests exercise the split helper API (`inc_tx_retrans_rto`,
//! `inc_tx_retrans_rack`, `inc_tx_retrans_tlp`) directly against a fresh
//! `Counters` instance — they don't need a live engine. Integration with
//! the actual emit sites (`engine.rs::on_rto_fire`, the RACK loop after
//! `handle_established`, `engine.rs::on_tlp_fire`) is tested indirectly via
//! the existing TAP suites (`tcp_rack_rto_retrans_tap.rs` etc.) once the
//! emit-site rewires from raw `inc(&counters.tcp.tx_retrans)` to the new
//! split helpers.
//!
//! Closes Phase 11 task 11.1 (claim C-E2: no RTO vs RACK vs TLP breakdown).

use dpdk_net_core::counters::{
    inc_tx_retrans_rack, inc_tx_retrans_rto, inc_tx_retrans_tlp, Counters,
};
use std::sync::atomic::Ordering;

#[test]
fn rto_increments_only_rto_counter_and_aggregate() {
    let c = Counters::new();
    inc_tx_retrans_rto(&c.tcp);
    assert_eq!(c.tcp.tx_retrans_rto.load(Ordering::Relaxed), 1);
    assert_eq!(c.tcp.tx_retrans_rack.load(Ordering::Relaxed), 0);
    assert_eq!(c.tcp.tx_retrans_tlp.load(Ordering::Relaxed), 0);
    // Aggregate is also bumped — back-compat with pre-split callers.
    assert_eq!(c.tcp.tx_retrans.load(Ordering::Relaxed), 1);
}

#[test]
fn rack_increments_only_rack_counter_and_aggregate() {
    let c = Counters::new();
    inc_tx_retrans_rack(&c.tcp);
    assert_eq!(c.tcp.tx_retrans_rack.load(Ordering::Relaxed), 1);
    assert_eq!(c.tcp.tx_retrans.load(Ordering::Relaxed), 1);
    assert_eq!(c.tcp.tx_retrans_rto.load(Ordering::Relaxed), 0);
    assert_eq!(c.tcp.tx_retrans_tlp.load(Ordering::Relaxed), 0);
}

#[test]
fn tlp_increments_only_tlp_counter_and_aggregate() {
    let c = Counters::new();
    inc_tx_retrans_tlp(&c.tcp);
    assert_eq!(c.tcp.tx_retrans_tlp.load(Ordering::Relaxed), 1);
    assert_eq!(c.tcp.tx_retrans.load(Ordering::Relaxed), 1);
    assert_eq!(c.tcp.tx_retrans_rto.load(Ordering::Relaxed), 0);
    assert_eq!(c.tcp.tx_retrans_rack.load(Ordering::Relaxed), 0);
}

#[test]
fn aggregate_equals_sum_of_split_counters() {
    let c = Counters::new();
    // Mix triggers: 3 RTO + 2 RACK + 1 TLP = 6 total.
    inc_tx_retrans_rto(&c.tcp);
    inc_tx_retrans_rto(&c.tcp);
    inc_tx_retrans_rack(&c.tcp);
    inc_tx_retrans_rto(&c.tcp);
    inc_tx_retrans_tlp(&c.tcp);
    inc_tx_retrans_rack(&c.tcp);
    let rto = c.tcp.tx_retrans_rto.load(Ordering::Relaxed);
    let rack = c.tcp.tx_retrans_rack.load(Ordering::Relaxed);
    let tlp = c.tcp.tx_retrans_tlp.load(Ordering::Relaxed);
    let agg = c.tcp.tx_retrans.load(Ordering::Relaxed);
    assert_eq!(rto, 3, "RTO sub-counter");
    assert_eq!(rack, 2, "RACK sub-counter");
    assert_eq!(tlp, 1, "TLP sub-counter");
    assert_eq!(agg, 6, "aggregate matches sum");
    assert_eq!(rto + rack + tlp, agg, "split counters partition aggregate");
}

#[test]
fn split_counters_zero_at_construction() {
    let c = Counters::new();
    assert_eq!(c.tcp.tx_retrans_rto.load(Ordering::Relaxed), 0);
    assert_eq!(c.tcp.tx_retrans_rack.load(Ordering::Relaxed), 0);
    assert_eq!(c.tcp.tx_retrans_tlp.load(Ordering::Relaxed), 0);
}
