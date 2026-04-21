//! Sum-identity per spec §6: the sum of attribution buckets must
//! equal the end-to-end wall-clock RTT within the caller tolerance
//! (default ±50 ns).
//!
//! These tests don't touch DPDK — they validate the pure-Rust
//! accounting primitives in the library façade, so they run on any
//! host without an ENA VF or EAL init.

use bench_e2e::attribution::{AttributionMode, HwTsBuckets, TscFallbackBuckets};
use bench_e2e::hw_task_18::{
    assert_all_events_rx_hw_ts_ns_zero, assert_hw_task_18_post_run, HwTask18Expectations,
};
use bench_e2e::sum_identity::assert_sum_identity;

use dpdk_net_core::counters::Counters;
use std::sync::atomic::Ordering;

#[test]
fn hw_ts_mode_sums_to_rtt_exactly() {
    let buckets = HwTsBuckets {
        user_send_to_tx_sched_ns: 100,
        tx_sched_to_nic_tx_wire_ns: 200,
        nic_tx_wire_to_nic_rx_ns: 10_000,
        nic_rx_to_enqueued_ns: 50,
        enqueued_to_user_return_ns: 80,
    };
    let rtt_ns = 10_430;
    assert_sum_identity(buckets.total_ns(), rtt_ns, 50).unwrap();
}

#[test]
fn hw_ts_mode_within_tolerance_passes() {
    let buckets = HwTsBuckets {
        user_send_to_tx_sched_ns: 100,
        tx_sched_to_nic_tx_wire_ns: 200,
        nic_tx_wire_to_nic_rx_ns: 10_000,
        nic_rx_to_enqueued_ns: 50,
        enqueued_to_user_return_ns: 80,
    };
    // 40 ns below bucket sum — within ±50 ns tolerance.
    assert_sum_identity(buckets.total_ns(), 10_390, 50).unwrap();
}

#[test]
fn hw_ts_mode_mismatch_beyond_tolerance_errors() {
    let buckets = HwTsBuckets {
        user_send_to_tx_sched_ns: 100,
        tx_sched_to_nic_tx_wire_ns: 200,
        nic_tx_wire_to_nic_rx_ns: 10_000,
        nic_rx_to_enqueued_ns: 50,
        enqueued_to_user_return_ns: 80,
    };
    let rtt_ns = 11_000; // 570 ns off — well beyond ±50 ns.
    let err = assert_sum_identity(buckets.total_ns(), rtt_ns, 50).unwrap_err();
    assert!(err.contains("sum_identity"));
    assert!(err.contains("diff=570"));
}

#[test]
fn tsc_fallback_mode_three_buckets() {
    let buckets = TscFallbackBuckets {
        user_send_to_tx_sched_ns: 100,
        tx_sched_to_enqueued_ns: 10_250,
        enqueued_to_user_return_ns: 80,
    };
    let rtt_ns = 10_430;
    assert_sum_identity(buckets.total_ns(), rtt_ns, 50).unwrap();
}

#[test]
fn tsc_fallback_mode_mismatch_errors() {
    let buckets = TscFallbackBuckets {
        user_send_to_tx_sched_ns: 100,
        tx_sched_to_enqueued_ns: 10_250,
        enqueued_to_user_return_ns: 80,
    };
    // bucket_sum = 10_430; rtt = 12_000; diff = 1_570 > tol=50.
    let err = assert_sum_identity(buckets.total_ns(), 12_000, 50).unwrap_err();
    assert!(err.contains("sum_identity"));
}

#[test]
fn attribution_mode_selection_on_zero_and_nonzero() {
    // On ENA every Readable event carries rx_hw_ts_ns = 0 — select
    // the collapsed 3-bucket TSC-fallback schema.
    assert_eq!(AttributionMode::from_rx_hw_ts(0), AttributionMode::Tsc);
    // On mlx5 / ice / future-gen ENA, a nonzero rx_hw_ts_ns
    // selects the 5-bucket HW-TS schema.
    assert_eq!(AttributionMode::from_rx_hw_ts(12_345), AttributionMode::Hw);
}

#[test]
fn hw_task_18_default_expects_ena_steady_state() {
    let d = HwTask18Expectations::default();
    assert!(d.expect_mbuf_fast_free_missing);
    assert!(d.expect_rss_hash_missing);
    assert!(d.expect_rx_timestamp_missing);
    assert!(d.expect_all_cksum_advertised);
    assert!(!d.expect_llq_missing);
    assert!(d.expect_rx_drop_cksum_bad_zero);
    assert!(d.expect_all_rx_hw_ts_ns_zero);
}

#[test]
fn hw_task_18_post_run_passes_on_ena_steady_state() {
    let counters = Counters::new();
    // Bump the three counters ENA's driver doesn't advertise.
    counters
        .eth
        .offload_missing_mbuf_fast_free
        .fetch_add(1, Ordering::Relaxed);
    counters
        .eth
        .offload_missing_rss_hash
        .fetch_add(1, Ordering::Relaxed);
    counters
        .eth
        .offload_missing_rx_timestamp
        .fetch_add(1, Ordering::Relaxed);
    assert!(assert_hw_task_18_post_run(&counters, &HwTask18Expectations::default()).is_ok());
}

#[test]
fn hw_task_18_post_run_errors_on_cksum_regression() {
    let counters = Counters::new();
    counters
        .eth
        .offload_missing_mbuf_fast_free
        .fetch_add(1, Ordering::Relaxed);
    counters
        .eth
        .offload_missing_rss_hash
        .fetch_add(1, Ordering::Relaxed);
    counters
        .eth
        .offload_missing_rx_timestamp
        .fetch_add(1, Ordering::Relaxed);
    counters
        .eth
        .offload_missing_tx_cksum_tcp
        .fetch_add(1, Ordering::Relaxed);
    let err = assert_hw_task_18_post_run(&counters, &HwTask18Expectations::default())
        .unwrap_err();
    assert!(err.contains("tx_cksum_tcp"));
}

#[test]
fn all_events_rx_hw_ts_ns_zero_ena_steady_state() {
    let samples = vec![0u64; 100_000];
    assert!(assert_all_events_rx_hw_ts_ns_zero(&samples).is_ok());
}

#[test]
fn all_events_rx_hw_ts_ns_zero_errors_on_contamination() {
    let mut samples = vec![0u64; 100];
    samples[42] = 123_456;
    let err = assert_all_events_rx_hw_ts_ns_zero(&samples).unwrap_err();
    assert!(err.contains("123456"));
}
