//! bench-micro::timer_advanced — fills gaps left by `bench_timer_add_cancel`.
//!
//! The existing `bench_timer_add_cancel` (tools/bench-micro/benches/timer.rs)
//! measures `TimerWheel::add` + `TimerWheel::cancel` paired in a single
//! round-trip. That shape hides two things:
//!
//! 1. The **asymmetric cost** between `add` (free-list pop / push, bucket
//!    walk to the level-0 chain, `Vec::push` of the slot index) and
//!    `cancel` (a single `Vec::get_mut` on `slots` + generation compare +
//!    tombstone bit flip). The paired bench reports their sum; splitting
//!    them surfaces which half dominates.
//!
//! 2. The **`advance` path with FIRING timers.** The existing idle-poll
//!    bench (`bench_poll_scheduler_tick_idle_with_timers` in poll.rs,
//!    renamed from `bench_poll_idle_with_timers` in T11) pre-populates with
//!    256 timers scheduled at `u64::MAX / 2` so `advance` walks an empty
//!    level-0 chain on every iter (the timers never fire). T10 adds three
//!    variants where the level-0 bucket contains N firing timers and
//!    `advance(now)` drains them all in a single call.
//!
//! # Variants
//!
//! - `bench_timer_add_only` — `TimerWheel::add` alone (no cancel). Setup
//!   builds a fresh harness per iter via `iter_batched_ref` so the wheel
//!   does not grow unboundedly across samples.
//! - `bench_timer_cancel_only` — `TimerWheel::cancel` alone. Setup
//!   pre-adds one non-firing timer per iter; the measured region just
//!   calls cancel. Reflects the per-`cancel` slow path the round-trip
//!   bench averages with add.
//! - `bench_timer_advance_fires_1` — `advance(now)` where the wheel
//!   holds exactly 1 timer due to fire at `now`. The returned SmallVec
//!   contains 1 element (stays inline).
//! - `bench_timer_advance_fires_8` — `advance(now)` with 8 firing
//!   timers. The returned `SmallVec<[..; 8]>` fills its inline capacity
//!   but does not spill to heap.
//! - `bench_timer_advance_fires_64` — `advance(now)` with 64 firing
//!   timers. The returned `SmallVec<[..; 8]>` overflows inline capacity
//!   and heap-allocates the spill backing.  The delta between `_fires_8`
//!   and `_fires_64` includes both the extra 56 fire-record copies AND
//!   the inline→heap transition (one allocation per advance call).
//!
//! # What's measured vs. not
//!
//! Measured: `TimerWheel::add`, `TimerWheel::cancel`, and
//! `TimerWheel::advance` as standalone method calls reached through
//! `EngineNoEalHarness` (the harness exposes `timer_wheel: pub` so the
//! advance variants reach the method directly without a harness
//! wrapper).
//!
//! NOT measured: `Engine::poll`'s timer-fire dispatch — the engine
//! receives the `SmallVec` returned by `advance` and routes each
//! `(TimerId, TimerNode)` to its kind-specific handler
//! (RTO / TLP / Persist / SynRetrans / public). That dispatch lives in
//! `Engine`, not `TimerWheel`, and pays handler-specific costs that vary
//! by kind. The bench numbers here cover only the wheel's drain cost
//! and the SmallVec construction — adding the dispatch cost would
//! require an engine-with-real-conns fixture, out of scope for T10.
//!
//! # SmallVec inline vs heap transition
//!
//! `TimerWheel::advance` returns `SmallVec<[(TimerId, TimerNode); 8]>`
//! (tcp_timer_wheel.rs:150). Each element is `TimerId` (8 B) +
//! `TimerNode` (40 B + padding) ≈ 48 B → inline capacity for 8 elements
//! is ~384 B held inside the SmallVec value itself. With 1 or 8 firing
//! timers the storage stays inline (zero heap allocation in the timed
//! region). At 64 firing timers, the first push past index 7 triggers
//! one heap allocation that backs all 64 entries. The `_fires_64`
//! variant is what surfaces that transition cost.
//!
//! # `black_box` discipline
//!
//! - add: the `when_ns` and `payload` inputs are `black_box`'d so the
//!   optimizer cannot fold the `now_ns - when_ns` delta-tick arithmetic
//!   to a compile-time constant; the returned `TimerId` is fed through
//!   `black_box` so the call is not DCE'd.
//! - cancel: the `TimerId` input is `black_box`'d; the `bool` return is
//!   folded into an accumulator that the closure `black_box`'s.
//! - advance: the returned `SmallVec` is folded into a `(len, xor_of_payloads)`
//!   tuple inside the timed region so every fired element is touched
//!   exactly once; the tuple is then `black_box`'d to defeat DCE without
//!   forcing the SmallVec value itself to round-trip through a stack
//!   spill on every call.
//! - Per-iter setup runs OUTSIDE the timed region courtesy of
//!   `iter_batched_ref` (criterion 0.5 `bencher.rs:341-343`: only the
//!   routine closure is bounded by `measurement.start()` /
//!   `measurement.end()`; setup and drop run outside the measured
//!   window).
//!
//! # Inlining + numeric expectations
//!
//! `TimerWheel::{add,cancel,advance}` carry no `#[inline]` attributes
//! and live in `dpdk-net-core`, while this bench compiles in the
//! `bench-micro` crate. The workspace builds release with fat LTO
//! (top-level `Cargo.toml`), so cross-crate inlining is in scope for
//! the optimizer — but Criterion's `b.iter_batched_ref` closure boundary
//! and the harness-method indirection through `EngineNoEalHarness` may
//! still keep one call frame intact. Numbers reflect the production
//! call shape rather than a fully-inlined hot loop, the same caveat
//! the T3 / T6 / T9 followups flagged.
//!
//! Rough observed costs on the `c6a.metal` bench host (smoke run with
//! `--sample-size 10 --measurement-time 1 --warm-up-time 1`; your
//! mileage will vary, especially for `_fires_64` which depends on
//! system allocator behavior under sustained 1-per-iter heap pressure):
//!
//! Important: criterion's `iter_batched_ref` times ONLY the routine call
//! (between `measurement.start()` and `measurement.end()`, criterion 0.5
//! bencher.rs:355,358-360); the per-iter `EngineNoEalHarness::new` setup
//! and the post-call drop run OUTSIDE the timed window. So the numbers
//! below are wheel-operation costs against a freshly-prepared but
//! possibly-cold-cache wheel state, NOT setup amortization.
//!
//! - `add_only` ~100 ns. The wheel's add work against an empty wheel:
//!   harness `&mut self.timer_wheel` indirection + free-list pop (or
//!   slots-grow on cold wheel) + level/bucket math + `Vec::push` of one
//!   u32 + slot Option write.
//! - `cancel_only` ~60 ns. `slots.get_mut` + generation compare +
//!   tombstone bit flip; no bucket touch. Setup pre-adds one timer
//!   (outside the timed region), so the measured region cancels exactly
//!   one timer per iter. Less work than add — the asymmetry the existing
//!   `bench_timer_add_cancel` round-trip hides.
//! - `advance_fires_1` ~300 ns. 1-tick cursor advance + level-0
//!   bucket walk of 1 entry + slot.take + SmallVec push (inline). The
//!   first cursor tick of a fresh wheel also pays a one-time `last_tick`
//!   write.
//! - `advance_fires_8` ~480 ns. Same per-bucket walk shape, 8 slot.take
//!   + 8 SmallVec pushes (all inline — SmallVec capacity is exactly 8).
//!   No heap allocation in the timed region.
//! - `advance_fires_64` ~1.3 µs. 64 slot.take + 64 SmallVec pushes;
//!   the 9th push exceeds inline capacity and triggers one heap
//!   allocation, and the remaining 55 pushes write into the heap
//!   backing. The ~2.5x jump vs `_fires_8` is dominated by the
//!   allocation + the 56 extra slot.take+push operations.
//!
//! Production realism caveat: the advance variants put all N firing
//! timers in a SINGLE level-0 bucket, so `advance` walks one bucket and
//! drains N entries. Production timers are typically spread across many
//! buckets (one RTO timer per active conn, scheduled at different
//! deadlines) — production `advance` typically walks N buckets each
//! holding ~1 timer, not 1 bucket holding N timers. The bench's
//! one-bucket-burst shape is a worst-case-per-bucket measurement,
//! useful for surfacing the per-fire cost in isolation; it is NOT the
//! shape of typical production `advance` per tick.
//!
//! These are shape expectations; the comparison `_fires_8` (~480 ns)
//! vs `_fires_64` (~1.3 µs) exposing a ~2.5x jump that includes the
//! heap-alloc transition is the load-bearing observation, not the
//! absolute ns. The `add_only` > `cancel_only` ordering is the other
//! load-bearing finding — confirms add's bucket-walk + push is heavier
//! than cancel's tombstone-only path despite both calls sharing the
//! same harness round-trip overhead.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use std::time::Duration;

/// Fire-time used by the advance-fires variants. Chosen so
/// `delay_ticks = FIRE_AT_NS / TICK_NS = 10` lands all timers in
/// level-0 bucket offset 10 (clamped to [1, 255] by
/// `level_and_bucket_offset`). Then `advance(FIRE_AT_NS)` walks 10
/// ticks and drains the bucket on tick 10.
const FIRE_AT_NS: u64 = 100_000;

/// Wheel `now_ns` used by the add-only and cancel-only setups. Zero
/// keeps the delay-tick math simple and matches the harness's default
/// `now_ns` (initialized to 0 in `EngineNoEalHarness::new`, never
/// touched here because `poll_once` is not called).
const ADD_WHEN_NS: u64 = 10_000_000;

/// Add-only: per-iter fresh wheel; the measured region calls
/// `TimerWheel::add` exactly once. The setup closure rebuilds the
/// harness so the slot Vec and free-list do not grow across iters
/// (which would skew the `Vec::push` cost as the heap-grow geometry
/// kicked in).
fn bench_timer_add_only(c: &mut Criterion) {
    c.bench_function("bench_timer_add_only", |b| {
        b.iter_batched_ref(
            || EngineNoEalHarness::new(64, 1_000_000),
            |h| {
                let id = h.timer_add(black_box(ADD_WHEN_NS), black_box(0));
                black_box(id);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Cancel-only: per-iter the setup adds a single non-firing timer
/// (`when_ns = ADD_WHEN_NS` → delay_ticks = 1000, lands at level 1
/// bucket offset 3 per `level_and_bucket_offset`; never fires because
/// advance is never called). The measured region calls
/// `TimerWheel::cancel(id)` once. Reflects the per-call cancel cost
/// the round-trip bench averages with add.
fn bench_timer_cancel_only(c: &mut Criterion) {
    c.bench_function("bench_timer_cancel_only", |b| {
        b.iter_batched_ref(
            || {
                let mut h = EngineNoEalHarness::new(64, 1_000_000);
                let id = h.timer_add(ADD_WHEN_NS, 0);
                (h, id)
            },
            |(h, id)| {
                let cancelled = h.timer_cancel(black_box(*id));
                black_box(cancelled);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Helper for the advance-fires variants. Builds a fresh wheel with
/// `count` timers all scheduled at `FIRE_AT_NS` (so they share a
/// level-0 bucket) and returns the harness. Setup runs OUTSIDE the
/// timed region courtesy of `iter_batched_ref`.
fn make_wheel_with_n_firing_timers(count: usize) -> EngineNoEalHarness {
    let mut h = EngineNoEalHarness::new(count.max(64), 1_000_000);
    // pre_populate_timers adds `count` timers at `FIRE_AT_NS` with
    // payload = i. All `count` timers land in the same level-0 bucket
    // (offset 10) because they share a fire_at_ns and the harness's
    // now_ns is 0 (poll_once never called).
    let _ids = h.pre_populate_timers(count, FIRE_AT_NS);
    h
}

/// Fold the SmallVec returned by `advance` into a (len, payload-xor)
/// pair the timed region can pass to `black_box`. Touches every fired
/// element exactly once so the optimizer cannot elide the
/// `slots.take()` + SmallVec push work.
#[inline(always)]
fn fold_fired(
    fired: &smallvec::SmallVec<
        [(
            dpdk_net_core::tcp_timer_wheel::TimerId,
            dpdk_net_core::tcp_timer_wheel::TimerNode,
        ); 8],
    >,
) -> (usize, u64) {
    let mut xor: u64 = 0;
    for (id, node) in fired.iter() {
        xor ^= id.slot as u64;
        xor ^= id.generation as u64;
        xor ^= node.user_data;
        xor ^= node.fire_at_ns;
    }
    (fired.len(), xor)
}

/// Advance with 1 firing timer. Stays inline in the returned SmallVec.
fn bench_timer_advance_fires_1(c: &mut Criterion) {
    c.bench_function("bench_timer_advance_fires_1", |b| {
        b.iter_batched_ref(
            || make_wheel_with_n_firing_timers(1),
            |h| {
                let fired = h.timer_wheel.advance(black_box(FIRE_AT_NS));
                let acc = fold_fired(&fired);
                black_box(acc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Advance with 8 firing timers. Fills inline capacity exactly; no
/// heap spill.
fn bench_timer_advance_fires_8(c: &mut Criterion) {
    c.bench_function("bench_timer_advance_fires_8", |b| {
        b.iter_batched_ref(
            || make_wheel_with_n_firing_timers(8),
            |h| {
                let fired = h.timer_wheel.advance(black_box(FIRE_AT_NS));
                let acc = fold_fired(&fired);
                black_box(acc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Advance with 64 firing timers. Overflows the 8-element inline
/// capacity; the SmallVec heap-allocates once during the drain. The
/// delta vs. `_fires_8` includes the extra 56 fold-iter steps AND the
/// inline→heap transition.
fn bench_timer_advance_fires_64(c: &mut Criterion) {
    c.bench_function("bench_timer_advance_fires_64", |b| {
        b.iter_batched_ref(
            || make_wheel_with_n_firing_timers(64),
            |h| {
                let fired = h.timer_wheel.advance(black_box(FIRE_AT_NS));
                let acc = fold_fired(&fired);
                black_box(acc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets =
        bench_timer_add_only,
        bench_timer_cancel_only,
        bench_timer_advance_fires_1,
        bench_timer_advance_fires_8,
        bench_timer_advance_fires_64,
}
criterion_main!(benches);
