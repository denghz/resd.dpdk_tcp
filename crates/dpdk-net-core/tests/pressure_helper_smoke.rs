//! Smoke test for the pressure-test failure-bundle helper + counter-snapshot
//! DSL (A11.0 step 3 / T2).
//!
//! Exercises the `common::pressure` module end-to-end without standing up an
//! EAL or DPDK port:
//!   * Construct a stand-alone `Counters`.
//!   * Take a `CounterSnapshot::capture(&counters)` baseline.
//!   * Bump a known delta counter (`tcp.tx_data`) and a level counter
//!     (`tcp.tx_data_mempool_avail`).
//!   * Take a second snapshot.
//!   * Assert `delta_since` returns the expected magnitudes via
//!     `assert_delta` for each `Relation` variant.
//!   * Deliberately fail an `assert_delta` from inside `catch_unwind` and
//!     confirm a failure bundle landed under
//!     `target/pressure-test/<suite>/<bucket>/<timestamp>/`. Cleans up the
//!     written directory at the end so repeated test runs do not balloon
//!     `target/`.
//!
//! Gated behind the `pressure-test` cargo feature; default builds compile to
//! an empty test binary. Mirrors the gating used by
//! `tests/pressure_test_feature_smoke.rs`.
#![cfg(feature = "pressure-test")]

mod common;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

use common::pressure::{
    assert_delta, dump_failure_bundle, set_bundle_root, CounterSnapshot, FailureCtx, Relation,
};
use dpdk_net_core::counters::Counters;
use dpdk_net_core::engine::EngineConfig;

/// Process-wide bundle root, pinned once at first use under
/// `CARGO_TARGET_TMPDIR`. `OnceLock::set` returns the existing value on
/// subsequent calls so racing test threads converge on the same root. Each
/// test still uses a unique `suite` subdirectory and cleans only that
/// subdirectory at the end.
static SMOKE_ROOT: OnceLock<PathBuf> = OnceLock::new();

fn smoke_root() -> &'static PathBuf {
    SMOKE_ROOT.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("pressure-helper-smoke-root");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        set_bundle_root(root.clone());
        root
    })
}

#[test]
fn snapshot_delta_reports_known_delta_counter_bump() {
    let c = Counters::new();
    let before = CounterSnapshot::capture(&c);
    c.tcp.tx_data.fetch_add(3, Ordering::Relaxed);
    let after = CounterSnapshot::capture(&c);
    let delta = after.delta_since(&before);

    // `tx_data` rose by 3.
    assert_delta(&delta, "tcp.tx_data", Relation::Eq(3));
    assert_delta(&delta, "tcp.tx_data", Relation::Gt(2));
    assert_delta(&delta, "tcp.tx_data", Relation::Ge(3));
    assert_delta(&delta, "tcp.tx_data", Relation::Le(3));
    assert_delta(&delta, "tcp.tx_data", Relation::Range(1, 10));

    // An untouched counter stays at delta = 0.
    assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));
}

#[test]
fn snapshot_captures_level_counters() {
    let c = Counters::new();
    c.tcp.tx_data_mempool_avail.store(2048, Ordering::Relaxed);
    c.tcp.rx_mempool_avail.store(1024, Ordering::Relaxed);
    let before = CounterSnapshot::capture(&c);

    // Level counter dropped (mempool drained).
    c.tcp.tx_data_mempool_avail.store(7, Ordering::Relaxed);
    let after = CounterSnapshot::capture(&c);
    let delta = after.delta_since(&before);

    // Level deltas are signed: 7 - 2048 = -2041.
    let level_change = delta
        .level_u32
        .get("tcp.tx_data_mempool_avail")
        .copied()
        .expect("level counter present");
    assert_eq!(level_change, 7i64 - 2048i64);

    // RX side untouched.
    let rx_change = delta
        .level_u32
        .get("tcp.rx_mempool_avail")
        .copied()
        .expect("level counter present");
    assert_eq!(rx_change, 0);
}

#[test]
fn snapshot_includes_every_known_counter_name() {
    use dpdk_net_core::counters::ALL_COUNTER_NAMES;

    let c = Counters::new();
    let snap = CounterSnapshot::capture(&c);
    for name in ALL_COUNTER_NAMES {
        assert!(
            snap.delta.contains_key(*name),
            "snapshot missing delta counter `{name}`"
        );
    }
    assert!(snap.level_u32.contains_key("tcp.tx_data_mempool_avail"));
    assert!(snap.level_u32.contains_key("tcp.rx_mempool_avail"));
}

#[test]
fn assert_delta_panic_message_includes_actual_and_expected() {
    let c = Counters::new();
    let before = CounterSnapshot::capture(&c);
    let after = CounterSnapshot::capture(&c);
    let delta = after.delta_since(&before);

    let payload = std::panic::catch_unwind(|| {
        assert_delta(&delta, "tcp.tx_data", Relation::Eq(42));
    })
    .expect_err("expected panic");
    let msg = payload
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .unwrap_or("");
    assert!(
        msg.contains("tcp.tx_data") && msg.contains("actual=0") && msg.contains("expected"),
        "panic message must surface counter name + actual + expected; got: {msg}"
    );
}

#[test]
fn dump_failure_bundle_writes_artifacts_to_disk() {
    let root = smoke_root();
    let suite = "dump_failure_bundle_writes_artifacts_to_disk";
    let bucket = "primary";

    let c = Counters::new();
    let before = CounterSnapshot::capture(&c);
    c.tcp.tx_data.fetch_add(1, Ordering::Relaxed);
    let after = CounterSnapshot::capture(&c);

    let cfg = EngineConfig::default();
    let ctx = FailureCtx {
        before: &before,
        after: &after,
        config: &cfg,
        events: Vec::new(),
        last_error: "deliberate-failure-for-smoke-test".to_string(),
    };

    let bundle_dir = dump_failure_bundle(suite, bucket, &ctx);
    assert!(
        bundle_dir.exists(),
        "expected bundle dir {bundle_dir:?} to exist"
    );
    assert!(
        bundle_dir.starts_with(root),
        "bundle dir {bundle_dir:?} must live under smoke root {root:?}"
    );
    for f in [
        "counters_before.json",
        "counters_after.json",
        "counters_delta.json",
        "config.txt",
        "error.txt",
    ] {
        let p = bundle_dir.join(f);
        assert!(p.exists(), "expected {p:?} to exist");
        let len = std::fs::metadata(&p).expect("stat").len();
        assert!(len > 0, "expected {p:?} to be non-empty");
    }
    // events.log can legitimately be empty (no events provided in this
    // test) — assert presence only.
    assert!(bundle_dir.join("events.log").exists());

    // Cleanup: remove this test's suite dir only.
    let _ = std::fs::remove_dir_all(root.join(suite));
}

#[test]
fn assert_delta_failure_path_writes_bundle_via_catch_unwind() {
    let root = smoke_root();
    let suite = "assert_delta_failure_path";
    let bucket = "primary";

    let c = Counters::new();
    let before = CounterSnapshot::capture(&c);
    let after = CounterSnapshot::capture(&c);

    // Capture the panic from a deliberately-wrong assertion, then dump.
    let delta = after.delta_since(&before);
    let panic_msg = std::panic::catch_unwind(|| {
        assert_delta(&delta, "tcp.tx_data", Relation::Eq(999));
    })
    .err()
    .map(|p| {
        p.downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&'static str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "<non-string panic>".to_string())
    })
    .expect("expected panic");

    let cfg = EngineConfig::default();
    let ctx = FailureCtx {
        before: &before,
        after: &after,
        config: &cfg,
        events: Vec::new(),
        last_error: panic_msg.clone(),
    };
    let bundle_dir = dump_failure_bundle(suite, bucket, &ctx);
    let err_txt = std::fs::read_to_string(bundle_dir.join("error.txt")).expect("read error.txt");
    assert!(
        err_txt.contains("tcp.tx_data") && err_txt.contains("expected"),
        "error.txt must surface the captured panic message; got: {err_txt}"
    );

    let _ = std::fs::remove_dir_all(root.join(suite));
}
