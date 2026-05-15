//! bench-micro::poll — spec §11.2 targets 1 + 2.
//!
//! Both targets measure `EngineNoEalHarness::poll_once()`, which covers
//! ONLY the SCHEDULER-TICK subset of the real `Engine::poll_once`:
//!
//!   - clock read (`now_ns`)
//!   - flow_table iteration
//!   - timer-wheel advance + firing
//!   - event drain
//!
//! It does NOT exercise:
//!
//!   - `rte_eth_rx_burst` / per-mbuf parse / dispatch
//!   - `rte_eth_tx_burst` / drain_tx_pending_data
//!   - any path requiring an active EAL or live NIC queues
//!
//! The bench-internals harness (spec §4.3) zero-mocks the NIC, so
//! these are unmeasured by bench-micro. End-to-end RX/TX latency is
//! measured by bench-stress under EAL + a peer (`bench-pair`).
//!
//! - `bench_poll_scheduler_tick_empty` — no pre-populated timers or
//!   flow-table entries. Surfaces the fixed scheduler-tick cost.
//!
//! - `bench_poll_scheduler_tick_idle_with_timers` — wheel pre-populated
//!   with 256 non-firing timers (scheduled at `u64::MAX / 2`) so
//!   `advance` walks a real bucket chain but never fires anything.
//!
//! Unblocked in T2.5 (replacing the earlier clock::now_ns() proxy).
//! Now exercises real code via dpdk-net-core's bench-internals
//! feature gate (spec §4.3).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use std::time::Duration;

fn bench_poll_scheduler_tick_empty(c: &mut Criterion) {
    c.bench_function("bench_poll_scheduler_tick_empty", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        b.iter(|| {
            h.poll_once();
            black_box(&h);
        });
    });
}

fn bench_poll_scheduler_tick_idle_with_timers(c: &mut Criterion) {
    c.bench_function("bench_poll_scheduler_tick_idle_with_timers", |b| {
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
    targets = bench_poll_scheduler_tick_empty, bench_poll_scheduler_tick_idle_with_timers
}
criterion_main!(benches);
