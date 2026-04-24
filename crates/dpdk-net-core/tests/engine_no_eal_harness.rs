//! Integration tests for `EngineNoEalHarness`. Gated on `bench-internals`.
//!
//! a10-perf-23.11 T2.4: the harness (src/engine.rs `test_support`) is
//! itself feature-gated, so this entire file compiles away when the
//! default feature set is used. Running these tests therefore requires
//! `--features bench-internals`.
#![cfg(feature = "bench-internals")]

use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use dpdk_net_core::flow_table::FourTuple;
use dpdk_net_core::tcp_conn::TcpConn;

#[test]
fn constructs_without_eal() {
    // The constructor must not touch DPDK EAL. If it did, this test
    // would fail (no EAL initialized) before any assertion.
    let _h = EngineNoEalHarness::new(64, 1_000_000);
}

#[test]
fn poll_once_is_noop_when_idle() {
    let mut h = EngineNoEalHarness::new(64, 1_000_000);
    h.poll_once();
    h.poll_once();
    h.poll_once();
    // No panic, no state corruption — the harness walked an empty wheel,
    // empty event queue, and zero-handle flow table without incident.
    // `now_ns()` should be non-zero after poll_once reads the real clock.
    assert!(h.now_ns() > 0, "poll_once should snapshot clock::now_ns()");
}

#[test]
fn timer_add_cancel_roundtrip() {
    let mut h = EngineNoEalHarness::new(64, 1_000_000);
    let id = h.timer_add(10_000_000, 0xDEADBEEF);
    let cancelled = h.timer_cancel(id);
    assert!(cancelled, "timer_cancel should return true for a valid timer id");

    // Second cancel on the same id must be false — the node has been
    // tombstoned and `cancel` short-circuits on already-cancelled slots.
    let second = h.timer_cancel(id);
    assert!(!second, "double-cancel should return false");
}

#[test]
fn pre_populated_timers_do_not_fire_prematurely() {
    let mut h = EngineNoEalHarness::new(64, 1_000_000);
    // Schedule 32 timers far in the future — advance will walk the
    // bucket chain but fire nothing. `u64::MAX / 2` is unreachable by
    // the real wall-clock that `poll_once` reads from `clock::now_ns()`.
    let ids = h.pre_populate_timers(32, u64::MAX / 2);
    assert_eq!(ids.len(), 32);

    // 100 poll_once calls should not cause any timer to fire, since
    // the real `clock::now_ns()` is nowhere near `u64::MAX / 2`.
    for _ in 0..100 {
        h.poll_once();
    }

    // Every id must still be cancellable. In the real TimerWheel
    // semantics, `advance` only fires+removes timers when `now_ns`
    // passes their `fire_at_ns`. Since we scheduled at u64::MAX / 2
    // and the real clock is sub-nanosecond of Unix epoch, all 32
    // timers are still live and `cancel` returns true for each.
    for id in ids {
        assert!(
            h.timer_cancel(id),
            "far-future timer should still be live after 100 poll_once calls"
        );
    }
}

#[test]
fn flow_table_insert_and_lookup() {
    let mut h = EngineNoEalHarness::new(64, 1_000_000);
    let t = FourTuple {
        local_ip: 0x0a_00_00_02,
        local_port: 40000,
        peer_ip: 0x0a_00_00_01,
        peer_port: 5000,
    };
    // Real `TcpConn::new_client` signature (verified against src/tcp_conn.rs):
    //   new_client(tuple, iss, our_mss, recv_buf_bytes, send_buf_bytes,
    //              min_rto_us, initial_rto_us, max_rto_us)
    // Matching the knobs used in tools/bench-micro/benches/flow_lookup.rs.
    let conn = TcpConn::new_client(t, 1_000, 1460, 1024, 2048, 5_000, 5_000, 1_000_000);
    let inserted = h.insert_conn(conn);
    assert!(inserted, "insert_conn should return true on a fresh table");

    let found = h.tuple_lookup(&t);
    assert!(found.is_some(), "tuple_lookup should find the just-inserted conn");

    // poll_once must iterate the inserted handle without panicking —
    // exercises the `delivered_segments.clear()` / `readable_scratch_iovecs.clear()`
    // scratch path on a real TcpConn.
    h.poll_once();
}
