//! Phase 9: validate the 5-bucket Hw attribution path on c7i (or any
//! NIC that returns a non-zero `rx_hw_ts_ns`). Without this test, two
//! buckets silently report 0 ns even when the rx HW-TS is working —
//! defeating the purpose of the decomposition.
//!
//! These tests don't touch DPDK — they validate the pure-Rust
//! composition primitive [`compose_iter_record`] by feeding synthetic
//! TSC ticks at 1 GHz tsc_hz (so ticks == ns for arithmetic clarity).

use bench_rtt::attribution::{compose_iter_record, AttributionMode, IterInputs};

#[test]
fn hw_buckets_populated_when_rx_hw_ts_nonzero() {
    let rec = compose_iter_record(IterInputs {
        t_user_send: 1_000,
        t_tx_sched: 2_000,
        t_enqueued: 3_500,
        t_user_return: 4_000,
        rx_hw_ts_ns: 3_200, // nonzero → Hw mode
        tsc_hz: 1_000_000_000,
    });
    assert_eq!(rec.mode, AttributionMode::Hw);
    let buckets = rec.hw_buckets.expect("expected Hw mode");

    // total_ns must equal rtt_ns
    assert_eq!(buckets.total_ns(), rec.rtt_ns);

    // Three buckets we DO measure must be > 0 with the synthetic
    // timestamps above (t_tx_sched > t_user_send by 1000 ticks; etc.).
    assert!(
        buckets.user_send_to_tx_sched_ns > 0,
        "user_send_to_tx_sched_ns should be measured (t_tx_sched > t_user_send)"
    );
    assert!(
        buckets.nic_tx_wire_to_nic_rx_ns > 0,
        "nic_tx_wire_to_nic_rx_ns should be measured (host-span)"
    );
    assert!(
        buckets.enqueued_to_user_return_ns > 0,
        "enqueued_to_user_return_ns should be measured"
    );

    // Two buckets we don't measure must be flagged unsupported, not
    // silently 0. A future phase that wires DPDK TX HW-TS or an
    // engine-side NIC-RX-to-enqueued probe will flip these.
    assert!(
        buckets.is_tx_sched_to_nic_tx_wire_unsupported(),
        "without DPDK TX HW-TS, this bucket must be flagged unsupported"
    );
    assert!(
        buckets.is_nic_rx_to_enqueued_unsupported(),
        "without engine NIC-RX-to-enqueued probe, this bucket must be flagged unsupported"
    );

    // Unsupported buckets contribute 0 ns to total_ns() — the sum-
    // identity invariant survives the unsupported markers.
    assert_eq!(buckets.tx_sched_to_nic_tx_wire_ns, 0);
    assert_eq!(buckets.nic_rx_to_enqueued_ns, 0);
}

#[test]
fn tsc_fallback_unaffected_by_hw_path_changes() {
    // Verify the existing TscFallbackBuckets path still composes
    // correctly when rx_hw_ts_ns is 0 (ENA steady state).
    let rec = compose_iter_record(IterInputs {
        t_user_send: 1_000,
        t_tx_sched: 2_000,
        t_enqueued: 3_500,
        t_user_return: 4_000,
        rx_hw_ts_ns: 0,
        tsc_hz: 1_000_000_000,
    });
    assert_eq!(rec.mode, AttributionMode::Tsc);
    let buckets = rec.tsc_buckets.expect("expected Tsc mode");
    assert_eq!(buckets.total_ns(), rec.rtt_ns);
    assert!(buckets.user_send_to_tx_sched_ns > 0);
    assert!(buckets.tx_sched_to_enqueued_ns > 0);
    assert!(buckets.enqueued_to_user_return_ns > 0);
}

#[test]
fn hw_buckets_total_excludes_unsupported() {
    // The two unsupported buckets must contribute 0 to total_ns even
    // though they're flagged. Rebuild the synthetic expected sum from
    // the three measured buckets and assert it matches total_ns().
    let rec = compose_iter_record(IterInputs {
        t_user_send: 1_000,
        t_tx_sched: 2_000,
        t_enqueued: 3_500,
        t_user_return: 4_000,
        rx_hw_ts_ns: 3_200,
        tsc_hz: 1_000_000_000,
    });
    let buckets = rec.hw_buckets.expect("expected Hw mode");
    let measured_sum = buckets.user_send_to_tx_sched_ns
        + buckets.nic_tx_wire_to_nic_rx_ns
        + buckets.enqueued_to_user_return_ns;
    assert_eq!(buckets.total_ns(), measured_sum);
    assert_eq!(rec.rtt_ns, measured_sum);
}

#[test]
fn rtt_ns_matches_user_send_to_user_return_span() {
    // Sanity: rtt_ns is the t_user_return - t_user_send delta in ns
    // (at tsc_hz=1GHz, ticks==ns). This pins the contract for
    // downstream sum-identity asserts.
    let rec = compose_iter_record(IterInputs {
        t_user_send: 10_000,
        t_tx_sched: 20_000,
        t_enqueued: 35_000,
        t_user_return: 45_000,
        rx_hw_ts_ns: 99_999,
        tsc_hz: 1_000_000_000,
    });
    assert_eq!(rec.rtt_ns, 35_000); // 45_000 - 10_000
}
