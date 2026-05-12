//! bench-micro::flow_scale — FlowTable behavior at scale.
//!
//! The existing `bench_flow_lookup_hot` (flow_lookup.rs:48) measures
//! `FlowTable::lookup_by_tuple` on a 16-entry table while ALWAYS probing
//! the same 4-tuple. That keeps one bucket + one slot entry pinned in L1
//! and lets the branch predictor cache the comparison, so it reports
//! the **best-case** lookup cost — production RX, which cycles through
//! 4-tuples from different live flows, doesn't see that shape.
//!
//! This bench adds three different shapes the single-tuple hot bench
//! hides:
//!
//! 1. **Varied lookups across N flows.** Walks every populated tuple in
//!    round-robin so the branch predictor + cache cannot pin a single
//!    entry. Three sizes (16 / 256 / 4096) bracket L1-resident, L1-spill,
//!    and L2-spill working sets.
//!
//! 2. **Miss path.** Looking up a tuple that is NOT in the table forces
//!    the underlying `std::collections::HashMap` to walk its bucket to
//!    confirm absence. This is a real production case (stale-segment
//!    arrival after FIN; reused 4-tuple before TIME-WAIT decays). True
//!    SipHash collisions in `std::collections::hash_map::RandomState`
//!    require cryptographic-grade effort to craft offline — the miss
//!    path is the closest natural proxy for the worst-case probe walk
//!    that production sees.
//!
//! 3. **Insert O(N) scan.** `FlowTable::insert`
//!    (crates/dpdk-net-core/src/flow_table.rs:131) does
//!    `self.slots.iter().position(|s| s.is_none())` — a linear scan
//!    through the `Vec<Option<TcpConn>>` for the first empty slot.
//!    Every `accept()` and every outbound `connect()` pays this; on a
//!    half-full table it walks ~N/2 slots before finding free space.
//!    The empty-table case finds slot 0 immediately and serves as the
//!    "scan floor" baseline. The half-full case at 4096 capacity walks
//!    2048 `Option::is_none` checks before falling through.
//!
//! # What this bench measures vs. what it doesn't
//!
//! Measured surface:
//!   - `FlowTable::lookup_by_tuple` — the HashMap probe + slot
//!     translation. Both hit (varied) and miss (absent tuple) shapes.
//!   - `FlowTable::insert` — the O(N) empty-slot scan + HashMap insert
//!     + slot store + handle bump.
//!
//! NOT measured:
//!   - The full Engine RX path (`Engine::handle_rx`) which calls
//!     `lookup_by_hash` via the RSS-or-siphash classifier at
//!     engine.rs:4765. That's a different fork with its own bucket-hash
//!     selection ahead of the HashMap probe — measured indirectly by
//!     `bench_rx_prelude_*`, not here.
//!   - `TcpConn` construction cost (paid in setup, OUTSIDE timed region).
//!   - `accept()` end-to-end (which calls `insert` plus SYN-cookie /
//!     state-init work).
//!
//! # `RandomState` and run-to-run variance
//!
//! `FlowTable::by_tuple` is a `std::collections::HashMap<FourTuple, u32>`
//! built with the std default `RandomState`. Bucket assignment depends
//! on the per-process random seed, so the same tuples land in different
//! buckets across runs — absolute numbers can vary by 5-15 ns from run
//! to run, especially for the miss-path variant which depends on
//! whether the absent tuple's probe walks 1 / 2 / 3+ chained entries.
//! The shape comparison (hot vs. varied vs. miss vs. insert-empty vs.
//! insert-half-full) is stable within a run.
//!
//! # `black_box` discipline
//!
//! - Lookups: `black_box(&tuple)` on input prevents the optimizer from
//!   constant-folding the HashMap probe against a setup-time-known key;
//!   `black_box(result)` on the returned `Option<ConnHandle>` prevents
//!   DCE of the call. The accumulator XOR-fold inside `iter_custom`
//!   batches also forces the per-lookup result to be observed.
//! - Inserts: `iter_batched_ref` rebuilds the table state per iter; the
//!   inserted `TcpConn` and `black_box` on the table reference defeat
//!   constant-folding of the `iter().position` scan.
//!
//! # Inlining + numeric expectations
//!
//! Neither `lookup_by_tuple` (flow_table.rs:153) nor `insert`
//! (flow_table.rs:126) carry `#[inline]` attributes. Criterion drives
//! them across the `dpdk-net-core` crate boundary the same way the
//! engine does, so the measurement reflects the production call shape
//! rather than a fully-inlined hot loop.
//!
//! Rough expected costs on a tuned Zen4 host (observed on the
//! `c6a.metal` bench host — your mileage will vary):
//!
//! - `varied_16` ~15-25 ns. 16 entries are still L1-resident; the
//!   rotation defeats branch prediction but the HashMap probe is
//!   uniform cost per call regardless, so the delta vs. hot is small.
//! - `varied_256` ~15-20 ns. Still effectively L1; HashMap probe
//!   touches one bucket + one slot entry, both small per-call working
//!   sets.
//! - `varied_4k` ~15-30 ns. L2-spill-eligible by total HashMap size,
//!   but each call touches only one bucket — cache-miss cost only
//!   fires when the rotation index hits a cold bucket.
//! - `miss` ~10-30 ns. HashMap bucket walk for absent key; std's
//!   RandomState typically keeps the probe chain short.
//! - `insert_empty` ~0.5-2 µs. One-slot scan + HashMap insert + the
//!   per-iter FlowTable drop (64-slot Vec + HashMap) — the
//!   `iter_batched_ref` setup+drop pair runs within the measurement
//!   window per sample.
//! - `insert_half_full_4k` ~2-10 µs. 2048-slot `Option::is_none` scan
//!   + HashMap insert + the half-full table's drop.
//!
//! The exact numbers vary across runs because std `RandomState` re-seeds
//! the HashMap per process. The shape comparison (varied_16 ≈ hot at
//! L1-resident sizes; insert_half_full > insert_empty by 2-10x because
//! of the O(N) scan) is the load-bearing observation, not the absolute
//! ns.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::flow_table::{FlowTable, FourTuple};
use dpdk_net_core::tcp_conn::TcpConn;
use std::time::{Duration, Instant};

/// Batching factor for `iter_custom` on the lookup variants. Mirrors the
/// BATCH=128 used by `bench_parse_options` / `bench_rx_prelude_*` /
/// `bench_loss_detection`'s sub-30 ns paths to amortize criterion's
/// per-iter closure-call overhead. Each varied lookup is expected at
/// 30-150 ns, comfortably above the threshold where iter_custom is
/// strictly needed; using BATCH here is for cross-bench consistency
/// and to keep the inner-loop modular-index rotation defeat branch
/// prediction the same way the production RX burst does.
const BATCH: u64 = 128;

/// Build a `FlowTable` of `capacity` slots with `n_populated` of them
/// filled by unique 4-tuples. Returns the table + the populated tuples
/// in insertion order. Setup runs OUTSIDE the timed region.
///
/// Tuple construction: `local_port` and `peer_port` are perturbed per
/// entry so every tuple is unique, and `local_ip` is perturbed via the
/// upper octets every 65 535 entries so an `i > u16::MAX` test stays
/// unique without u32 overflow on `peer_port + i`.
fn populate_table(capacity: usize, n_populated: usize) -> (FlowTable, Vec<FourTuple>) {
    assert!(n_populated <= capacity, "n_populated must fit capacity");
    let mut ft = FlowTable::new(capacity as u32);
    let mut tuples = Vec::with_capacity(n_populated);
    for i in 0..n_populated {
        let t = make_tuple(i);
        // Same knobs as the in-tree TcpConn unit tests — pure-Rust
        // constructor; no DPDK state touched.
        let c = TcpConn::new_client(t, 1_000 + i as u32, 1460, 1024, 2048, 5000, 5000, 1_000_000);
        ft.insert(c).expect("slot available during populate");
        tuples.push(t);
    }
    (ft, tuples)
}

/// Deterministic unique 4-tuple for index `i`. The `local_ip` upper
/// octet rotates every 65 536 entries so even at capacity = 4 096 we
/// stay well clear of `u16` wrap of `local_port` / `peer_port`.
fn make_tuple(i: usize) -> FourTuple {
    let high = (i >> 16) as u32;
    FourTuple {
        local_ip: 0x0a_00_00_02 | (high << 24),
        local_port: 40_000u16.wrapping_add(i as u16),
        peer_ip: 0x0a_00_00_01,
        peer_port: 5_000u16.wrapping_add(i as u16),
    }
}

// ---------------------------------------------------------------------
// Varied lookups: cycle through every populated tuple per call so the
// branch predictor + L1 cache cannot pin a single entry's bucket and
// slot. Three sizes bracket L1-resident, L1-spill, L2-spill working
// sets respectively.
// ---------------------------------------------------------------------

/// Run a varied-lookup bench over `tuples` against `ft`. Each inner
/// iter probes a different tuple (modular index across BATCH) so LLVM
/// cannot CSE the call against a single fixed key, and the branch
/// predictor cannot lock onto a single bucket walk.
fn bench_varied_lookup(c: &mut Criterion, name: &str, ft: &FlowTable, tuples: &[FourTuple]) {
    c.bench_function(name, |b| {
        let mut idx = 0usize;
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    // Rotate across all populated tuples. The modular
                    // index defeats branch prediction on the HashMap
                    // bucket probe and ensures every populated bucket
                    // is touched in steady state.
                    let t = &tuples[idx % tuples.len()];
                    let h = ft.lookup_by_tuple(black_box(t));
                    acc ^= h.unwrap_or(0) as u64;
                    idx = idx.wrapping_add(1);
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

/// 16 populated entries — same population count as `bench_flow_lookup_hot`,
/// but rotates across all 16 tuples per call instead of pinning one.
/// Surfaces the "branch-pred defeats" delta vs. the hot bench.
fn bench_flow_lookup_varied_16(c: &mut Criterion) {
    let (ft, tuples) = populate_table(64, 16);
    bench_varied_lookup(c, "bench_flow_lookup_varied_16", &ft, &tuples);
}

/// 256 populated entries. At ~40 B per HashMap entry + ~16 B per
/// `Option<TcpConn>` slot pointer surface, 256 entries spill out of L1d
/// on most server cores (Zen4 L1d = 32 KiB). Still L2-resident.
fn bench_flow_lookup_varied_256(c: &mut Criterion) {
    let (ft, tuples) = populate_table(512, 256);
    bench_varied_lookup(c, "bench_flow_lookup_varied_256", &ft, &tuples);
}

/// 4 096 populated entries. The HashMap backing store + slot Vec
/// together overflow L2 on most cores (Zen4 L2 = 1 MiB) — bucket probes
/// pay an L2-miss-to-L3 fetch on most accesses. The 65 535-slot ceiling
/// is the actual production max; 4 096 is a conservative midpoint that
/// still exercises the L2-spill regime without dragging setup to many
/// seconds.
fn bench_flow_lookup_varied_4k(c: &mut Criterion) {
    let (ft, tuples) = populate_table(8192, 4096);
    bench_varied_lookup(c, "bench_flow_lookup_varied_4k", &ft, &tuples);
}

// ---------------------------------------------------------------------
// Miss path: lookup_by_tuple on a tuple that's NOT in the table. The
// HashMap walks its bucket to confirm absence. This is the closest
// natural proxy for the worst-case probe walk; crafting real SipHash
// collisions against `std::collections::hash_map::RandomState`
// requires breaking a cryptographic seed, which is out of scope here.
// ---------------------------------------------------------------------

/// Miss-path lookup. Table has 256 entries; probe tuple is constructed
/// to be guaranteed absent (port range disjoint from populated tuples).
/// The `lookup_by_tuple` call walks the HashMap bucket and returns
/// `None`. Production analog: stale segment arrival after FIN, or a
/// reused 4-tuple probe before TIME-WAIT decays. Per-process
/// `RandomState` makes the absolute number run-dependent (5-15 ns
/// run-to-run variance is normal); the shape is stable.
fn bench_flow_lookup_miss(c: &mut Criterion) {
    let (ft, _tuples) = populate_table(512, 256);
    // Rotate across a set of guaranteed-absent tuples so the optimizer
    // cannot CSE the probe to a single fixed key — the absent set must
    // be disjoint from populated tuples. Populated peer_port range is
    // 5000..5256; populated local_port range is 40000..40256. We use
    // peer_port range 60000..60000+ABSENT_COUNT, local_port 50000+ —
    // disjoint on both ports.
    const ABSENT_COUNT: usize = 16;
    let absent: Vec<FourTuple> = (0..ABSENT_COUNT)
        .map(|i| FourTuple {
            local_ip: 0x0a_00_00_02,
            local_port: 50_000u16.wrapping_add(i as u16),
            peer_ip: 0x0a_00_00_01,
            peer_port: 60_000u16.wrapping_add(i as u16),
        })
        .collect();
    c.bench_function("bench_flow_lookup_miss", |b| {
        let mut idx = 0usize;
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    let t = &absent[idx % absent.len()];
                    let h = ft.lookup_by_tuple(black_box(t));
                    // `unwrap_or(0)` keeps the XOR-fold defined when
                    // the lookup misses (the expected case). `acc`
                    // ends up XOR'd with zero on every iter; the
                    // black_box at end-of-batch is what prevents DCE
                    // of the call.
                    acc ^= h.unwrap_or(0) as u64;
                    idx = idx.wrapping_add(1);
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

// ---------------------------------------------------------------------
// Insert O(N) scan: the cost hidden by today's benches. `insert`
// (flow_table.rs:131) does `self.slots.iter().position(|s| s.is_none())`
// — a linear walk of `Vec<Option<TcpConn>>` until it finds free space.
// On an empty table that's 1 check; on a half-full N=4096 table that's
// ~2048 checks before falling through.
// ---------------------------------------------------------------------

/// Insert into a fresh `FlowTable::new(64)`. Slot 0 is immediately
/// empty so the `iter().position` returns at index 0 — measures the
/// "scan floor" cost (one `is_none` check + HashMap insert + slot
/// store + handle bump). The `with_capacity(64)` here matches the
/// pre-T9 `flow_lookup.rs` populate fixture so the comparison vs. that
/// bench's costs is apples-to-apples on table layout.
fn bench_flow_table_insert_empty(c: &mut Criterion) {
    c.bench_function("bench_flow_table_insert_empty", |b| {
        b.iter_batched_ref(
            || FlowTable::new(64),
            |ft| {
                // Insert one entry; slot scan finds index 0 immediately.
                let t = make_tuple(0);
                let conn =
                    TcpConn::new_client(t, 1_000, 1460, 1024, 2048, 5000, 5000, 1_000_000);
                let h = ft.insert(black_box(conn));
                black_box(h);
                black_box(&ft);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Insert into a half-full 4 096-slot table. Setup pre-populates slots
/// `[0..2048)` so the next `insert` walks 2 048 `Option::is_none`
/// checks before falling through to slot 2 048. This is the O(N) cost
/// that today's benches hide.
///
/// `iter_batched_ref` rebuilds the half-full table state per timed
/// iter via `with_setup`, so the timed region pays ONLY the scan +
/// HashMap insert + slot write — not the 2 048 setup inserts. The
/// setup cost is real (each setup insert itself does an O(i) scan,
/// quadratic total) but it is excluded from criterion's measurement
/// window via the iter-batched setup closure.
///
/// Why 4 096 / 2 048 (half-full): the production max is 65 535 slots
/// (FlowTable cap = u32 capacity). At 65 535 the setup-per-iter cost
/// scales as O(N^2) = ~2 billion ops per setup → bench setup time
/// would dominate wall-clock. 4 096 / 2 048 is the largest power-of-2
/// half-full point where setup stays well under 10 ms / iter and the
/// scan cost is already 100x the empty-insert floor — enough to surface
/// the asymmetry without making the bench unrunnable.
fn bench_flow_table_insert_half_full_4k(c: &mut Criterion) {
    const CAP: usize = 4096;
    const PREFILL: usize = 2048;
    c.bench_function("bench_flow_table_insert_half_full_4k", |b| {
        b.iter_batched_ref(
            || populate_table(CAP, PREFILL).0,
            |ft| {
                // Insert one more; scan walks PREFILL slots before
                // finding empty slot at index PREFILL. Tuple index is
                // PREFILL (== next-free) so it cannot collide with
                // any populated tuple.
                let t = make_tuple(PREFILL);
                let conn = TcpConn::new_client(
                    t,
                    1_000 + PREFILL as u32,
                    1460,
                    1024,
                    2048,
                    5000,
                    5000,
                    1_000_000,
                );
                let h = ft.insert(black_box(conn));
                black_box(h);
                black_box(&ft);
            },
            // LargeInput: the prefill setup is non-trivial (2 048
            // inserts, themselves O(i) each, ~2M `is_none` checks
            // total). LargeInput tells criterion to amortize setup
            // across many samples and reuse the prepared input where
            // possible — except `iter_batched_ref` always pays setup
            // per iter to enforce freshness, so this is essentially
            // an annotation that the per-iter overhead is non-trivial.
            // We accept slow bench wall-clock here; the per-sample
            // measurement is what we care about.
            criterion::BatchSize::LargeInput,
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
        bench_flow_lookup_varied_16,
        bench_flow_lookup_varied_256,
        bench_flow_lookup_varied_4k,
        bench_flow_lookup_miss,
        bench_flow_table_insert_empty,
        bench_flow_table_insert_half_full_4k,
}
criterion_main!(benches);
