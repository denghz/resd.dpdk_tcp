//! bench-micro::tsc_read — spec §11.2 targets 3 + 4.
//!
//! Three targets, ordered by descending FFI/abstraction cost:
//!
//! - `bench_tsc_read_ffi` — calls `dpdk_net_now_ns` across the C ABI.
//!   Measures FFI hop + rdtsc + scaled-multiply ns conversion. This is
//!   what C-side consumers actually pay.
//!
//! - `bench_now_ns` — calls `dpdk_net_core::clock::now_ns()` directly
//!   in Rust. Same work as the FFI variant (rdtsc + ns conversion) but
//!   without the C ABI boundary. Renamed from the older
//!   `bench_tsc_read_inline` for honesty — the original name implied
//!   raw inline rdtsc, but the function performs a ns conversion.
//!
//! - `bench_rdtsc_raw` — calls `dpdk_net_core::clock::rdtsc()`. This
//!   is the truly minimum-cost variant: a single `_rdtsc` intrinsic
//!   with no conversion. Returns raw TSC ticks, not nanoseconds.
//!
//! # Note on the §11.2 §4 spec target
//!
//! The spec lists `dpdk_net_now_ns_inline` (header-inline FFI). No
//! such C-header-inline FFI exists today — `dpdk_net_now_ns` is the
//! only public entry point for TSC reads. `bench_rdtsc_raw` is the
//! closest pure-rdtsc proxy that a future `dpdk_net_now_ns_inline`
//! would wrap before the ns conversion; `bench_now_ns` is the
//! closest no-FFI-boundary analog that performs the full ns conversion.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::time::{Duration, Instant};

// Batching factor for `iter_custom`. At sub-10ns workloads, criterion's
// per-iter closure-call + sample-bookkeeping overhead can dominate the
// measured cost. Calling the workload BATCH times inside one closure
// invocation, then dividing the total elapsed by BATCH before returning,
// amortizes that fixed cost. With BATCH=128 the per-call accuracy is
// limited only by Instant::now() resolution (~1-3 ns on this host) +
// loop-overhead (~1 cycle), both << 10 ns observed cost.
const BATCH: u64 = 128;

fn bench_tsc_read_ffi(c: &mut Criterion) {
    c.bench_function("bench_tsc_read_ffi", |b| {
        // `dpdk_net_now_ns` accepts a nullable `*mut dpdk_net_engine` and
        // ignores it (the clock is process-global). Passing null exercises
        // the full FFI call overhead without any Engine state.
        //
        // `iter_custom`: we measure BATCH calls per closure invocation and
        // return `elapsed / BATCH`. Criterion treats the returned Duration
        // as the "per-iter" cost, so the reported median IS per-call cost.
        // XOR-fold the results into a single accumulator and `black_box`
        // it once at end-of-batch so the per-call cost is the FFI hop +
        // rdtsc + scaled-multiply, not BATCH stack roundtrips.
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    acc ^= unsafe {
                        dpdk_net::dpdk_net_now_ns(black_box(std::ptr::null_mut()))
                    };
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

fn bench_now_ns(c: &mut Criterion) {
    c.bench_function("bench_now_ns", |b| {
        // `clock::now_ns` is a pure Rust call — no FFI boundary, no
        // function-pointer indirection. Performs `rdtsc` + the scaled-
        // multiply ns conversion (see `clock.rs::now_ns`). Renamed from
        // the older `bench_tsc_read_inline` because that name implied a
        // raw TSC read; this target measures full ns conversion.
        //
        // `iter_custom`: see `bench_tsc_read_ffi` for batching rationale.
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    acc ^= dpdk_net_core::clock::now_ns();
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

fn bench_rdtsc_raw(c: &mut Criterion) {
    c.bench_function("bench_rdtsc_raw", |b| {
        // `clock::rdtsc` is `#[inline(always)]` and wraps the
        // `core::arch::x86_64::_rdtsc` intrinsic — no conversion, no
        // FFI. Returns raw TSC ticks, not nanoseconds; downstream code
        // that needs ns pays the additional scaled-multiply in `now_ns`.
        //
        // Caveat on absolute number: with `iter_custom` + `Instant::now()`
        // + BATCH=128 loop overhead, the reported ns/call is
        // harness-influenced — the underlying `_rdtsc` intrinsic is
        // ~25-30 cycles on modern x86_64, but `Instant::now()` resolution
        // (~1-3 ns on this host) plus loop-overhead inflate the observed
        // number to ~15 ns. The bench bounds the per-call cost and lets
        // us compare `bench_rdtsc_raw` vs `bench_now_ns` vs
        // `bench_tsc_read_ffi`; it is NOT a tight floor on raw _rdtsc.
        //
        // `iter_custom`: see `bench_tsc_read_ffi` for batching rationale.
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    acc ^= dpdk_net_core::clock::rdtsc();
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets = bench_tsc_read_ffi, bench_now_ns, bench_rdtsc_raw
}
criterion_main!(benches);
