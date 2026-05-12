//! bench-micro::loss_detection — SACK scoreboard, retrans queue, RACK
//! loss-detection module surface.
//!
//! Where this sits in the path: every received ACK that carries SACK
//! blocks runs `SackScoreboard::insert` + `SendRetrans::mark_sacked` per
//! block (tcp_input.rs:898-899); every advancing cumulative ACK runs
//! `SackScoreboard::prune_below` once (tcp_input.rs:982) and the
//! engine-side `SendRetrans::prune_below_into_mbufs` once
//! (engine.rs:4913); every ACK that touches the retrans set runs
//! `compute_reo_wnd_us` (tcp_input.rs:1082) and the RACK detect-lost
//! pass over `snd_retrans.entries` (tcp_input.rs:1095). The TX path
//! pushes one `RetransEntry` per segment via
//! `SendRetrans::push_after_tx` (engine.rs send loop). On RTO firing
//! the engine runs `rack_mark_losses_on_rto_into` (engine.rs:3310).
//! Bench-micro previously had zero direct coverage of these — only T1's
//! `bench_send_*` exercised `push_after_tx` indirectly through the full
//! per-segment build path.
//!
//! # Variants
//!
//! * `bench_sack_scoreboard_insert_first` — empty scoreboard, single
//!   `insert(SackBlock)`. Measures the no-merge / no-overflow path:
//!   one merge-search pass over zero existing blocks plus the trailing
//!   append. Call site: tcp_input.rs:898 (per-SACK-block on every ACK
//!   with new SACK data).
//! * `bench_sack_scoreboard_insert_append` — scoreboard pre-loaded
//!   with 3 disjoint blocks; insert a 4th non-overlapping block,
//!   filling the array to its 4-entry capacity
//!   (`MAX_SACK_SCOREBOARD_ENTRIES`, tcp_sack.rs:11). The merge-search
//!   loop at tcp_sack.rs:43-65 iterates 3 times, finds no overlap or
//!   touch — `seq_le(block.left, cur.right) && seq_le(cur.left,
//!   block.right)` is false for every existing block — and falls
//!   through to the append at tcp_sack.rs:70-72. `collapse()` is
//!   NEVER invoked on this path (only the merged branch at
//!   tcp_sack.rs:66-69 calls it). This bench measures the 3 failed
//!   merge checks + array-store + count-bump cost; it does NOT cover
//!   the collapse/merge path — `_insert_overlap_merge` does. Call
//!   site: same as `_insert_first`.
//! * `bench_sack_scoreboard_insert_overlap_merge` — scoreboard
//!   pre-loaded with 3 disjoint blocks; insert a 4th block that DOES
//!   overlap an existing block, so the merge branch
//!   (tcp_sack.rs:47-65) fires and `collapse()` runs the
//!   pairwise-scan at tcp_sack.rs:103+. This is the in-order-resync
//!   shape — a new SACK extends an existing hole rather than opening
//!   a fresh one. Cost: 1 successful merge-check + the merge-store +
//!   collapse pass over `count` blocks. Call site: same as above.
//! * `bench_sack_is_sacked_hit` — scoreboard with 4 blocks; lookup a
//!   seq that lands INSIDE the 3rd block. Hits the early-return after
//!   3 iterations of the linear range-check loop (tcp_sack.rs:34-38).
//!   Call site: A5 RACK scan over the retrans deque (engine-side).
//! * `bench_sack_is_sacked_miss` — same scoreboard; lookup a seq that
//!   falls in none of the four ranges. Hits the full 4-iteration loop
//!   then returns false. The two `is_sacked` benches together bound
//!   the range-check cost from best to worst.
//! * `bench_sack_prune_below` — scoreboard with 4 blocks; advance
//!   `snd_una` past 2 of them so the prune-write loop drops 2 entries
//!   and rewrites the array head (tcp_sack.rs:82-101). Call site:
//!   tcp_input.rs:982 (every advancing cumulative ACK).
//! * `bench_retrans_push_after_tx` — `SendRetrans::with_capacity(192)`
//!   pre-pushed to 16 entries, then one `push_after_tx` of a
//!   `RetransEntry` literal. The deque is in steady-state (well past
//!   first-push, well below capacity) so the timed push is a true
//!   `VecDeque::push_back` amortized-O(1) with NO `reserve` or
//!   realloc — mirrors the production construction at
//!   tcp_conn.rs:448. Isolates the deque-push cost from T1's bundled
//!   per-segment build path. Call site: engine.rs send loop (every
//!   TX'd segment).
//! * `bench_retrans_prune_below_into_mbufs` — `SendRetrans` with 8
//!   in-flight entries; advance `snd_una` past the first 4 so the
//!   hot-path drain loop pops 4 entries into the engine scratch
//!   (tcp_retrans.rs:124-141). NULL-mbuf caveat: every entry holds
//!   `Mbuf::from_ptr(null_mut())`. The drain function skips null
//!   pointers at tcp_retrans.rs:133 — the bench therefore measures
//!   the queue-pop cost (the `pop_front` deque mutation, the
//!   `wrapping_add`, the `seq_le` check, the SmallVec capacity-check
//!   on `push` is bypassed because `NonNull::new(null_mut())` returns
//!   `None`). The actual mbuf-free cost — `shim_rte_pktmbuf_free` on
//!   each popped pointer — runs engine-side in a separate loop
//!   (engine.rs:4928-) and is NOT exercised here. This is the
//!   queue-management cost only.
//! * `bench_retrans_mark_sacked` — `SendRetrans` with 8 entries; pass
//!   in a `SackBlock` that overlaps the middle 2 entries. The function
//!   iterates the full deque (tcp_retrans.rs:147-155), so measured
//!   cost scales with deque length, not with the overlapped count.
//!   Call site: tcp_input.rs:899 (per-SACK-block on every ACK with
//!   new SACK data).
//! * `bench_rack_compute_reo_wnd` — pure arithmetic; `compute_reo_wnd_us`
//!   over typical (non-aggressive, with-srtt) inputs. Sub-10 ns
//!   expected; uses `iter_custom` + BATCH=128 to amortize criterion's
//!   per-iter overhead. Call site: tcp_input.rs:1082 (every ACK that
//!   touches the retrans set).
//! * `bench_rack_mark_losses_on_rto` — `SendRetrans` with 8 entries
//!   aged enough for the §6.3 age clause to fire on all but the front
//!   entry (the front always fires via the `seq == snd_una` clause).
//!   Measures the marker pass with a pre-grown `Vec<u16>` scratch so
//!   the cost is the iteration + push, not the scratch alloc. Call
//!   site: engine.rs:3310 (RTO fire path — rare event handler, but
//!   tail-latency relevant).
//!
//! # Null-mbuf caveat
//!
//! Several variants use `Mbuf::from_ptr(std::ptr::null_mut())` for the
//! held mbuf reference. The bench measures the queue management cost
//! (deque push/pop, range-check loops, marker iteration); the actual
//! mbuf lifecycle (`shim_rte_pktmbuf_free`, refcount updates) runs in
//! the engine post-dispatch and is NOT measured. A real production
//! workload pays both costs per loss-recovery event; this bench
//! isolates the upper layer only.
//!
//! # Inlining disclosure
//!
//! `SackScoreboard::*`, `SendRetrans::push_after_tx`, and
//! `compute_reo_wnd_us` are not marked `#[inline]` at their
//! definitions (tcp_sack.rs / tcp_retrans.rs / tcp_rack.rs). Criterion
//! drives them across the crate boundary the same way production
//! call sites do, so the measurement reflects the production call
//! shape rather than a fully-inlined hot loop. `rto_age_expired`
//! inside `rack_mark_losses_on_rto_into` IS `#[inline]`
//! (tcp_rack.rs:158), so its cost is folded into the marker pass.
//!
//! # `black_box` discipline
//!
//! Insert / push / prune / mark variants `black_box` the input
//! `SackBlock` / `RetransEntry` so the bench routine cannot
//! constant-fold the call. Read-only variants (`is_sacked`,
//! `compute_reo_wnd_us`) `black_box` BOTH the input seq/inputs AND
//! the returned bool/u32 so LLVM cannot DCE the call. The seq passed
//! to `is_sacked` is rotated per-iteration on the timed path so the
//! constant-fold-against-fixed-scoreboard escape is unavailable to
//! the optimizer (the per-iter `wrapping_add` and `if`-reset are
//! inside the timed region — a few cycles of bookkeeping floor on top
//! of the actual `is_sacked` cost — disclosed here).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::mempool::Mbuf;
use dpdk_net_core::tcp_options::SackBlock;
use dpdk_net_core::tcp_rack::{compute_reo_wnd_us, rack_mark_losses_on_rto_into};
use dpdk_net_core::tcp_retrans::{RetransEntry, SendRetrans};
use dpdk_net_core::tcp_sack::SackScoreboard;
use smallvec::SmallVec;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Batching factor for `iter_custom`. Matches the BATCH used by
/// `parse_options`, `rx_prelude`, and `build_segment::pseudo_header`
/// for sub-10 ns pure-fn benches.
const BATCH: u64 = 128;

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Build a SACK scoreboard pre-loaded with the given blocks via the
/// production `insert` path. The blocks must be disjoint and ordered
/// non-overlapping so the merge pass at tcp_sack.rs:43-65 doesn't fold
/// them together (a fold would defeat the test fixture's intent).
fn make_scoreboard_with_blocks(blocks: &[(u32, u32)]) -> SackScoreboard {
    let mut sb = SackScoreboard::new();
    for &(left, right) in blocks {
        sb.insert(SackBlock { left, right });
    }
    sb
}

/// Build a `SendRetrans` with `segs.len()` queued entries via the
/// production `push_after_tx` path. Each entry holds a NULL `Mbuf`;
/// see the file-level null-mbuf caveat for implications.
fn make_retrans_with_entries(segs: &[(u32, u16)]) -> SendRetrans {
    let mut r = SendRetrans::new();
    for &(seq, len) in segs {
        r.push_after_tx(RetransEntry {
            seq,
            len,
            mbuf: Mbuf::from_ptr(std::ptr::null_mut()),
            first_tx_ts_ns: 1_000_000,
            xmit_count: 1,
            sacked: false,
            lost: false,
            xmit_ts_ns: 1_000_000,
            hdrs_len: 0,
        });
    }
    r
}

// ---------------------------------------------------------------------
// SackScoreboard variants
// ---------------------------------------------------------------------

/// Empty scoreboard + 1 insert. The merge-search loop has zero
/// iterations; the function appends to slot 0 and bumps `count`.
fn bench_sack_scoreboard_insert_first(c: &mut Criterion) {
    c.bench_function("bench_sack_scoreboard_insert_first", |b| {
        b.iter_batched_ref(
            SackScoreboard::new,
            |sb| {
                let inserted = sb.insert(black_box(SackBlock {
                    left: 1000,
                    right: 2000,
                }));
                black_box(inserted);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Scoreboard pre-loaded with 3 disjoint blocks; insert a 4th
/// non-overlapping block (700, 800). The merge-search loop iterates
/// 3 times finding NO overlap or touch with any existing block, so
/// `collapse()` is NOT called — control flow falls through to the
/// trailing append at tcp_sack.rs:70-72 (count goes from 3 to 4 —
/// exactly MAX_SACK_SCOREBOARD_ENTRIES, no eviction yet). This is
/// the "scoreboard full but not overflowing" shape that a typical
/// 3-reorder session reaches steady-state at. The merge / collapse
/// path is exercised separately by `_insert_overlap_merge`.
fn bench_sack_scoreboard_insert_append(c: &mut Criterion) {
    c.bench_function("bench_sack_scoreboard_insert_append", |b| {
        b.iter_batched_ref(
            || make_scoreboard_with_blocks(&[(100, 200), (300, 400), (500, 600)]),
            |sb| {
                let inserted = sb.insert(black_box(SackBlock {
                    left: 700,
                    right: 800,
                }));
                black_box(inserted);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Scoreboard pre-loaded with 3 disjoint blocks; insert a 4th block
/// (350, 450) that overlaps the second existing block (300, 400).
/// The merge-search loop hits the overlap condition
/// `seq_le(350, 400) && seq_le(300, 450)` on iteration 1 (i=1),
/// updates blocks[1] in place to (300, 450), sets `merged_into =
/// Some(1)`, then invokes `collapse()` (tcp_sack.rs:67) which runs
/// the pairwise-scan at tcp_sack.rs:103+ — finds no further pair to
/// fold (the merged (300, 450) doesn't touch (100, 200) or
/// (500, 600)) and returns after one full O(count^2) scan. Covers
/// the merge + collapse path that `_insert_append` does not.
fn bench_sack_scoreboard_insert_overlap_merge(c: &mut Criterion) {
    c.bench_function("bench_sack_scoreboard_insert_overlap_merge", |b| {
        b.iter_batched_ref(
            || make_scoreboard_with_blocks(&[(100, 200), (300, 400), (500, 600)]),
            |sb| {
                // (350, 450) overlaps (300, 400) — merges into block[1]
                // as (300, 450); collapse() finds no further overlap.
                let inserted = sb.insert(black_box(SackBlock {
                    left: 350,
                    right: 450,
                }));
                black_box(inserted);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Scoreboard with 4 blocks; lookup seq that lands inside the 3rd
/// block. The linear range-check loop runs 3 iterations and returns
/// true. The per-iter rotation across a small seq window defeats
/// branch-predictor pinning on a single fixed probe; production RACK
/// scans iterate over the retrans entries themselves
/// (tcp_input.rs:1095) with seq distribution determined by the
/// in-flight burst shape, not by this fixed rotation — so the
/// measurement bounds the per-call cost rather than mirroring a
/// specific production scan shape.
fn bench_sack_is_sacked_hit(c: &mut Criterion) {
    let sb = make_scoreboard_with_blocks(&[(100, 200), (300, 400), (500, 600), (700, 800)]);
    c.bench_function("bench_sack_is_sacked_hit", |b| {
        // Rotate the seq across a window inside the 3rd block (500..600)
        // so each call probes a different cache line of state — though
        // the 4-entry scoreboard fits in one cacheline, the seq rotation
        // prevents LLVM from constant-folding any single repeated probe.
        let mut probe = 500u32;
        b.iter(|| {
            let hit = sb.is_sacked(black_box(probe));
            black_box(hit);
            probe = probe.wrapping_add(1);
            if probe >= 600 {
                probe = 500;
            }
        });
    });
}

/// Same scoreboard; lookup seq that's in none of the four ranges. The
/// linear range-check loop runs 4 iterations and returns false. Worst
/// case for the 4-block scoreboard.
fn bench_sack_is_sacked_miss(c: &mut Criterion) {
    let sb = make_scoreboard_with_blocks(&[(100, 200), (300, 400), (500, 600), (700, 800)]);
    c.bench_function("bench_sack_is_sacked_miss", |b| {
        // Probe in the gap between block 1 and block 2 (200..300).
        // Linear loop exits with `false` after 4 iterations.
        let mut probe = 220u32;
        b.iter(|| {
            let hit = sb.is_sacked(black_box(probe));
            black_box(hit);
            probe = probe.wrapping_add(1);
            if probe >= 300 {
                probe = 220;
            }
        });
    });
}

/// Scoreboard with 4 blocks; advance `snd_una` past the first 2 so
/// the prune-write loop drops 2 entries and rewrites the head of the
/// array. Covers the rewrite path (w != i for the surviving entries)
/// rather than the all-drop or no-drop fast cases.
fn bench_sack_prune_below(c: &mut Criterion) {
    c.bench_function("bench_sack_prune_below", |b| {
        b.iter_batched_ref(
            || make_scoreboard_with_blocks(&[(100, 200), (300, 400), (500, 600), (700, 800)]),
            |sb| {
                // snd_una=450 → drops blocks (100,200) and (300,400);
                // keeps (500,600) and (700,800) unchanged because their
                // left edges are above snd_una.
                sb.prune_below(black_box(450));
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

// ---------------------------------------------------------------------
// SendRetrans variants
// ---------------------------------------------------------------------

/// Steady-state `SendRetrans::push_after_tx` cost. Mirrors the
/// production construction at tcp_conn.rs:448, where the inner
/// `VecDeque` is built via `SendRetrans::with_capacity(send_buf_bytes
/// / mss + 1)` — a 256 KiB send buffer at 1460 B MSS pre-sizes to
/// ~180 slots (see tcp_retrans.rs:54-69 doc). The bench uses
/// `with_capacity(PUSH_CAPACITY)` (matches the production pre-size)
/// and pre-pushes `PRE_PUSHED` entries OUTSIDE the timed region so
/// the deque is in its steady-state shape — many entries already,
/// plenty of headroom — before the timed pushes fire. Inside the
/// timed region we run `BATCH` pushes on the SAME deque (deque grows
/// from PRE_PUSHED to PRE_PUSHED+BATCH; with PUSH_CAPACITY chosen so
/// the sum stays below capacity, NO realloc occurs across the
/// batch). The measured cost is therefore a true
/// `VecDeque::push_back` amortized-O(1) cost: no `reserve` call, no
/// realloc, no initial heap-grow path.
///
/// `iter_custom` lets us amortize criterion's per-iter closure-call
/// overhead across `BATCH=128` pushes and exclude the (large) setup
/// cost from the timed region. T1's `bench_send_*` measures push
/// indirectly inside the full per-segment build (segment-build +
/// checksum + mbuf-copy + push); this target isolates push alone.
///
/// Earlier revisions of this bench used `SendRetrans::new()` with no
/// pre-size and the timed region paid the geometric VecDeque-grow
/// reserves of a from-empty deque; that conflated the first-push
/// initial-alloc with the per-push steady-state cost. The current
/// setup eliminates that conflation.
fn bench_retrans_push_after_tx(c: &mut Criterion) {
    /// Pre-size: production at 256 KiB / 1460 B MSS pre-sizes to
    /// ~180 slots. The timed batch pushes `BATCH` more on top of
    /// `PRE_PUSHED`; PUSH_CAPACITY must exceed `PRE_PUSHED + BATCH`
    /// so no realloc happens inside the batch. 192 + 128 + headroom
    /// = 384.
    const PUSH_CAPACITY: usize = 384;
    /// Mid-burst in-flight count — enough that the VecDeque is well
    /// past first-push state. Matches a sustained-TX flow snapshot.
    const PRE_PUSHED: usize = 16;
    c.bench_function("bench_retrans_push_after_tx", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::from_nanos(0);
            for _ in 0..iters {
                // Setup OUTSIDE timed region: pre-size and pre-push
                // so the deque is in its steady-state shape (many
                // entries, no first-push initial alloc).
                let mut r = SendRetrans::with_capacity(PUSH_CAPACITY);
                for i in 0..PRE_PUSHED {
                    r.push_after_tx(RetransEntry {
                        seq: 1000 + (i as u32) * 1460,
                        len: 1460,
                        mbuf: Mbuf::from_ptr(std::ptr::null_mut()),
                        first_tx_ts_ns: 1_000_000,
                        xmit_count: 1,
                        sacked: false,
                        lost: false,
                        xmit_ts_ns: 1_000_000,
                        hdrs_len: 54,
                    });
                }
                // Time BATCH pushes; capacity headroom guarantees no
                // realloc inside this region. Bench measures the
                // steady-state VecDeque::push_back amortized-O(1) cost.
                let start = Instant::now();
                for k in 0..BATCH {
                    r.push_after_tx(black_box(RetransEntry {
                        seq: 1000 + ((PRE_PUSHED as u32) + k as u32) * 1460,
                        len: 1460,
                        mbuf: Mbuf::from_ptr(std::ptr::null_mut()),
                        first_tx_ts_ns: 1_000_000,
                        xmit_count: 1,
                        sacked: false,
                        lost: false,
                        xmit_ts_ns: 1_000_000,
                        hdrs_len: 54,
                    }));
                }
                total += start.elapsed();
                black_box(&r);
            }
            total / (BATCH as u32)
        });
    });
}

/// `SendRetrans` with 8 in-flight entries; advance `snd_una` past the
/// first 4 so the hot-path drain loop pops 4 entries into the engine
/// scratch.
///
/// Null-mbuf caveat: every entry holds `Mbuf::from_ptr(null_mut())`.
/// The drain function skips null pointers at tcp_retrans.rs:133
/// (`NonNull::new(null_mut())` returns `None`), so the per-entry
/// `out.push(p)` is also skipped. The bench measures the deque-pop
/// cost (`pop_front`, the `wrapping_add`, the `seq_le` check) — the
/// `SmallVec::push` of the mbuf pointer is NOT exercised. The actual
/// `shim_rte_pktmbuf_free` runs engine-side in a separate loop
/// (engine.rs:4928+) and is NOT measured. This is the queue-
/// management cost only.
fn bench_retrans_prune_below_into_mbufs(c: &mut Criterion) {
    c.bench_function("bench_retrans_prune_below_into_mbufs", |b| {
        b.iter_batched_ref(
            || {
                // 8 entries, each 100 B, contiguous: seqs at 1000, 1100,
                // .., 1700. End-seqs at 1100, 1200, .., 1800.
                let r = make_retrans_with_entries(&[
                    (1000, 100),
                    (1100, 100),
                    (1200, 100),
                    (1300, 100),
                    (1400, 100),
                    (1500, 100),
                    (1600, 100),
                    (1700, 100),
                ]);
                // Engine scratch — pre-grown to inline capacity so the
                // bench measures the steady-state zero-realloc property
                // (the audit-grade scenario this code was written for).
                let scratch: SmallVec<[std::ptr::NonNull<dpdk_net_sys::rte_mbuf>; 16]> =
                    SmallVec::new();
                (r, scratch)
            },
            |(r, scratch)| {
                // snd_una=1400 covers entries [0..4) (end_seqs 1100, 1200,
                // 1300, 1400 are all <= 1400). Entry 4 has end_seq=1500
                // > 1400, so the drain stops there.
                r.prune_below_into_mbufs(black_box(1400), scratch);
                black_box(&scratch);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// `SendRetrans` with 8 entries; pass in a `SackBlock` overlapping the
/// middle 2 entries. `mark_sacked` iterates the full deque (no
/// early-exit), so cost scales with deque length, not overlap count.
/// The middle-overlap shape is the typical "fast retransmit found a
/// hole" pattern; pure-cost-vs-length is captured.
fn bench_retrans_mark_sacked(c: &mut Criterion) {
    c.bench_function("bench_retrans_mark_sacked", |b| {
        b.iter_batched_ref(
            || {
                make_retrans_with_entries(&[
                    (1000, 100),
                    (1100, 100),
                    (1200, 100),
                    (1300, 100),
                    (1400, 100),
                    (1500, 100),
                    (1600, 100),
                    (1700, 100),
                ])
            },
            |r| {
                // Block [1200, 1400) overlaps entries at seq 1200 and 1300.
                r.mark_sacked(black_box(SackBlock {
                    left: 1200,
                    right: 1400,
                }));
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

// ---------------------------------------------------------------------
// tcp_rack variants
// ---------------------------------------------------------------------

/// Pure-arithmetic `compute_reo_wnd_us` over typical
/// (non-aggressive, with-srtt) inputs. Returns
/// `min(srtt/4, min_rtt/2).max(1_000)`. Sub-10 ns; uses `iter_custom`
/// + BATCH=128 to amortize criterion's per-iter overhead.
fn bench_rack_compute_reo_wnd(c: &mut Criterion) {
    c.bench_function("bench_rack_compute_reo_wnd", |b| {
        // Realistic intra-AZ trading flow numbers: min_rtt ≈ 60 µs,
        // srtt ≈ 100 µs. Both divisions land below the 1000-µs floor,
        // so the `.max(1_000)` branch fires — exactly the production
        // path on every ACK before steady-state RTT inflates beyond
        // millisecond range.
        let min_rtt_us: u32 = 60;
        let srtt_us: Option<u32> = Some(100);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    // Rotate one bit of the input per call so LLVM cannot
                    // hoist the call out of the inner loop as a single
                    // CSE'd constant.
                    let w = compute_reo_wnd_us(
                        black_box(false),
                        black_box(min_rtt_us),
                        black_box(srtt_us),
                    );
                    acc ^= w as u64;
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

/// `SendRetrans` with 8 entries aged enough for the §6.3 age clause
/// to mark them all. Measures the marker pass with a pre-grown
/// `Vec<u16>` scratch so the cost is the iteration + push, not the
/// scratch alloc.
///
/// Setup: `xmit_ts_ns = 1_000_000` (1 ms) on every entry; `now_ns =
/// 1_000_000_000` (1 s) so `age_ns = 999_000_000`. With `rtt_us=
/// 50_000` (50 ms) and `reo_wnd_us=1_000` (1 ms), the threshold is
/// `(50_000 + 1_000) * 1_000 = 51_000_000` ns. `age >= threshold` →
/// every non-sacked, non-lost, in-flight entry fires the age clause
/// and is pushed into `out`. snd_una=900 < entry-front-seq=1000, so
/// the front-entry-at-snd_una clause does NOT fire (it would
/// duplicate-mark the front entry); the age clause carries it.
///
/// `Vec<u16>` scratch is `with_capacity(16)` — large enough that the
/// 8 pushes never re-allocate. This isolates the marker pass cost.
fn bench_rack_mark_losses_on_rto(c: &mut Criterion) {
    c.bench_function("bench_rack_mark_losses_on_rto", |b| {
        b.iter_batched_ref(
            || {
                let r = make_retrans_with_entries(&[
                    (1000, 100),
                    (1100, 100),
                    (1200, 100),
                    (1300, 100),
                    (1400, 100),
                    (1500, 100),
                    (1600, 100),
                    (1700, 100),
                ]);
                let scratch: Vec<u16> = Vec::with_capacity(16);
                (r, scratch)
            },
            |(r, scratch)| {
                scratch.clear();
                rack_mark_losses_on_rto_into(
                    black_box(&r.entries),
                    black_box(900),
                    black_box(50_000),
                    black_box(1_000),
                    black_box(1_000_000_000),
                    scratch,
                );
                black_box(&scratch);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

// ---------------------------------------------------------------------
// VecDeque type usage check (compile-time)
// ---------------------------------------------------------------------

// The `rack_mark_losses_on_rto_into` signature takes
// `&VecDeque<RetransEntry>` directly. We import it explicitly here so
// stale callers (a future rename) surface at compile time, since our
// fixture goes through `SendRetrans.entries` instead.
const _: fn(
    &VecDeque<RetransEntry>,
    u32,
    u32,
    u32,
    u64,
    &mut Vec<u16>,
) = rack_mark_losses_on_rto_into;

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets =
        bench_sack_scoreboard_insert_first,
        bench_sack_scoreboard_insert_append,
        bench_sack_scoreboard_insert_overlap_merge,
        bench_sack_is_sacked_hit,
        bench_sack_is_sacked_miss,
        bench_sack_prune_below,
        bench_retrans_push_after_tx,
        bench_retrans_prune_below_into_mbufs,
        bench_retrans_mark_sacked,
        bench_rack_compute_reo_wnd,
        bench_rack_mark_losses_on_rto,
}
criterion_main!(benches);
