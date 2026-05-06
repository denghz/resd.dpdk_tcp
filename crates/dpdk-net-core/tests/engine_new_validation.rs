//! Release-build validation tests for Engine::new config bounds (Task A3).
//!
//! Source: Part 3 BLOCK-A11 B1, Pattern P6 (`debug_assert!`-only validation).
//!
//! `RttEstimator::new` previously guarded `min <= initial <= max` only with
//! `debug_assert!`, so a release-build caller that passed inverted bounds
//! (e.g. `tcp_min_rto_us=1_000_000, tcp_max_rto_us=100`) reached
//! `u32::clamp(min, max)` on the first RTT sample and panicked. The fix is a
//! release-safe `min > max` check at the top of `Engine::new` — before any
//! DPDK / clock APIs — so the failure surfaces at engine bring-up rather
//! than at the first connection's first RTT update.
//!
//! These tests run without EAL initialization. The bounds check is placed
//! before `clock::init()` precisely so that `Err(InvalidRtoBounds)` can be
//! observed without standing up DPDK.

use dpdk_net_core::engine::{Engine, EngineConfig};

#[test]
fn inverted_rto_bounds_returns_err_not_panic() {
    // min > max — would panic at rto.clamp(min, max) on first RTT sample.
    let cfg = EngineConfig {
        tcp_min_rto_us: 1_000_000,
        tcp_max_rto_us: 100,
        ..EngineConfig::default()
    };
    let result = Engine::new(cfg);
    assert!(
        matches!(result, Err(dpdk_net_core::Error::InvalidRtoBounds { .. })),
        "expected Err(InvalidRtoBounds), got {:?}",
        result.err()
    );
}

#[test]
fn inverted_rto_bounds_carries_caller_values() {
    // The error variant carries the offending values — useful for the C ABI
    // shim that turns the Rust error into a logged diagnostic.
    let cfg = EngineConfig {
        tcp_min_rto_us: 9_999,
        tcp_max_rto_us: 42,
        ..EngineConfig::default()
    };
    match Engine::new(cfg) {
        Err(dpdk_net_core::Error::InvalidRtoBounds { min, max }) => {
            assert_eq!(min, 9_999);
            assert_eq!(max, 42);
        }
        other => panic!("expected Err(InvalidRtoBounds), got {:?}", other.err()),
    }
}

#[test]
fn valid_rto_bounds_do_not_trip_invalid_rto_bounds() {
    // min <= max — must NOT return InvalidRtoBounds. Without EAL the call
    // still fails (e.g. clock::init or DPDK FFI), but never with this
    // particular variant. A false positive here would mean the bounds check
    // is rejecting a healthy configuration.
    let cfg = EngineConfig::default(); // 5_000 / 5_000 / 1_000_000
    let result = Engine::new(cfg);
    if let Err(dpdk_net_core::Error::InvalidRtoBounds { min, max }) = result {
        panic!(
            "default EngineConfig must not trip RTO bounds check, got min={} max={}",
            min, max
        );
    }
}

#[test]
fn equal_rto_bounds_are_valid() {
    // min == max is intentional in latency-sensitive production configs
    // (e.g. multiseg_retrans_tap.rs sets both to 1_000_000 us). The guard
    // uses strict `>`, so equal bounds must never return InvalidRtoBounds.
    let cfg = EngineConfig {
        tcp_min_rto_us: 1_000_000,
        tcp_max_rto_us: 1_000_000,
        ..EngineConfig::default()
    };
    let result = Engine::new(cfg);
    if let Err(dpdk_net_core::Error::InvalidRtoBounds { min, max }) = result {
        panic!(
            "min == max must not trip RTO bounds check, got min={} max={}",
            min, max
        );
    }
}
