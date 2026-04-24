//! bench-micro::poll — spec §11.2 targets 1 + 2.
//!
//! `bench_poll_empty` measures `EngineNoEalHarness::poll_once()` with
//! no pre-populated timers or flow-table entries — matches the real
//! `Engine::poll_once`'s fixed per-iteration cost when no RX and no
//! timers fire.
//!
//! `bench_poll_idle_with_timers` pre-populates the wheel with 256
//! non-firing timers (scheduled at `u64::MAX / 2`) so `advance` walks
//! a real bucket chain during every iteration.
//!
//! Unblocked in T2.5 (replacing the earlier clock::now_ns() proxy).
//! Now exercises real code via dpdk-net-core's bench-internals
//! feature gate (spec §4.3).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use std::time::Duration;

fn bench_poll_empty(c: &mut Criterion) {
    c.bench_function("bench_poll_empty", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        b.iter(|| {
            h.poll_once();
            black_box(&h);
        });
    });
}

fn bench_poll_idle_with_timers(c: &mut Criterion) {
    c.bench_function("bench_poll_idle_with_timers", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        // Timers scheduled far in the future — advance walks the
        // bucket chain but never fires anything during the bench.
        let _ids = h.pre_populate_timers(256, u64::MAX / 2);
        b.iter(|| {
            h.poll_once();
            black_box(&h);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets = bench_poll_empty, bench_poll_idle_with_timers
}
criterion_main!(benches);
