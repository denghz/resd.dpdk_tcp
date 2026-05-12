//! bench-micro::deliver_readable — application-visible read latency.
//!
//! These benches measure the **iovec materialization step** from
//! `Engine::deliver_readable` (engine.rs:5585-5724). That body is the
//! last hop in the receive path: `tcp_input::dispatch` enqueues
//! mbuf-backed `InOrderSegment`s into `conn.recv.bytes`, then the
//! engine calls `deliver_readable` which pops them into
//! `delivered_segments`, materializes a `DpdkNetIovec` array in
//! `readable_scratch_iovecs`, and emits a single READABLE event the
//! application reads through the public `dpdk_net_poll` API.
//!
//! The existing micro-benches stop at `tcp_input::dispatch`
//! (`bench_tcp_input_*` measure the dispatch path, which only
//! enqueues into `recv.bytes`). This bench isolates the cost of the
//! iovec-emission step that follows — the work the trading
//! application actually observes as "receive latency from the
//! moment the segment is buffered to the moment the user can read
//! the bytes".
//!
//! # Helper + drift-guard caveat (codex C3 / N1 carryover from T1)
//!
//! Each iteration drives the canonical per-call helper,
//! `dpdk_net_core::engine::test_support::EngineNoEalHarness::
//! deliver_readable_step`, which lives behind the `bench-internals`
//! feature gate inside `dpdk-net-core` and is covered by
//! `deliver_readable_step_drift_guard` unit tests in `engine.rs`.
//! The helper mirrors the body of `Engine::deliver_readable` at
//! engine.rs:5585-5724.
//!
//! The drift-guard tests assert **helper-internal** shape, NOT
//! production-vs-helper equivalence — production
//! `Engine::deliver_readable` requires DPDK EAL bring-up which
//! `bench-micro` cannot do. If production changes shape without
//! updating the helper, those tests will still pass; the protection
//! is procedural (helper lives next to the production body in
//! engine.rs so reviewers see both at once), not enforced.
//! Constructing a real `Engine` to assert equivalence would require
//! EAL and is out of scope for this no-EAL bench.
//!
//! # What this bench COVERS
//!
//! The helper drives the following observable side effects, each
//! mirrored from `Engine::deliver_readable` (line numbers in parens):
//!
//!   * Per-call `flow_table.get_mut(handle)` lookup, mirroring the
//!     production `borrow_mut() + get_mut(handle)` pattern (5593-5601).
//!     The Stage-1 harness uses `&mut FlowTable` directly, omitting
//!     `RefCell::borrow_mut` tracking that production pays.
//!   * `conn.delivered_segments.clear()` (5607)
//!   * Pop loop from `conn.recv.bytes` into `delivered_segments`
//!     under a `total_delivered` byte budget (5609-5654) — full-pop
//!     branch only for these two targets (no split).
//!   * `conn.readable_scratch_iovecs.clear()` (5660)
//!   * The iovec-materialization loop (5662-5669): for each segment,
//!     push a `DpdkNetIovec { base: seg.data_ptr(), len, _pad: 0 }`
//!     onto the scratch + accumulate `total_len`. `data_ptr()` reads
//!     `buf_addr + data_off` through the `shim_rte_pktmbuf_data` shim
//!     on a fake-mbuf-backed segment (see below).
//!   * `recv_buf_delivered` cumulative byte-throughput counter bump
//!     (5723).
//!
//! # What this bench DELIBERATELY SKIPS
//!
//! Three pieces of the production body are out of scope for the
//! application-visible read-latency bench, each with a one-line reason:
//!
//!   * `events.push(InternalEvent::Readable { .. })` (5704-5715) —
//!     the event-queue push and its per-event observability counter
//!     bumps (`rx_iovec_segs_total`, `rx_multi_seg_events`) are gated
//!     by `obs-none` in production and represent the observability
//!     surface, not the application-visible delivery primitive.
//!   * `crate::clock::now_ns()` for `emitted_ts_ns` (5711) — same
//!     `obs-none` gate, ~7 ns at 5 GHz on Zen4 with cached TSC.
//!   * `dpdk_net_poll` API marshalling — the public translation from
//!     `InternalEvent::Readable` to the C-ABI `dpdk_net_event_t` plus
//!     its iovec-slice publish (`dpdk-net/src/lib.rs:495-576`) is a
//!     separate FFI surface, exercised by `bench-poll` not here.
//!
//! Actual mbuf-data-pointer dereferences are also not exercised:
//! the helper writes a `seg.data_ptr()` pointer into each iovec, but
//! the bench's fake mbufs have zeroed `buf_addr` (NULL) — the
//! resulting iovec base is NULL, which is fine because the bench
//! never reads through it. The cost being measured is the *pointer
//! arithmetic* (`buf_addr + data_off`), not a cache-line load from
//! the user's payload.
//!
//! # Cross-crate inlining boundary (codex carryover from T1/T3)
//!
//! Production calls `deliver_readable` from inside `Engine::poll`'s
//! body — same crate, so LLVM can inline the helper across the call
//! site under the workspace's `lto = "fat"` release profile. This
//! bench calls `deliver_readable_step` from `tools/bench-micro`
//! across a crate boundary; release LTO is fat per the workspace
//! `Cargo.toml`, so the cross-crate inline should land. If LTO is
//! ever weakened, this bench's numbers gain a function-call frame
//! that production does not pay — verify before reporting headline
//! perf claims.
//!
//! # `black_box` discipline (codex C1 lessons)
//!
//! The bench folds the helper's observable side effects into an
//! accumulator that is `black_box`-ed at end of iter:
//!
//!   * Conn input handle is `black_box`-ed pre-call (forces a re-
//!     read on each iter so LLVM cannot hoist `flow_table.get_mut`).
//!   * Outcome fields (`seg_count`, `total_len`, `partial_split`) +
//!     the resulting `iovec.len` sum are XOR-folded into an
//!     accumulator that is `black_box`-ed at end of iter. This
//!     keeps the iovec push observable: a 3-field fold lets LLVM
//!     reduce per-iovec writes to nothing if it sees no downstream
//!     consumer, so we fold every produced iovec's `len` (the
//!     `_pad` and `base` fields are constant per fixture and
//!     contribute zero entropy).
//!
//! # Fake-mbuf setup
//!
//! Each `InOrderSegment` holds a `MbufHandle` pointing at a
//! `Box<[u8; 256]>` cast to `*mut rte_mbuf`. The helper's
//! `seg.data_ptr()` reads `buf_addr + data_off` (both zero in our
//! fake — `data_ptr() == NULL`); no DPDK code reads through the
//! returned pointer. `MbufHandle::Drop` calls
//! `shim_rte_pktmbuf_free_seg`, which guards on `m->pool == NULL`
//! and falls back to a refcnt-only decrement for fake-mbuf storage
//! (shim.c:122-130) — safe for our use. We bump the refcount once
//! at construction so the Drop's pre-dec read is non-zero (avoids
//! the leak-diagnostic counter bump). Same convention as
//! `bench_tcp_input_data_segment` (tcp_input.rs:78-86) and the
//! `deliver_readable_step` drift-guard tests in engine.rs.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::counters::Counters;
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use dpdk_net_core::flow_table::{ConnHandle, FlowTable, FourTuple};
use dpdk_net_core::mempool::MbufHandle;
use dpdk_net_core::tcp_conn::{InOrderSegment, TcpConn};
use dpdk_net_core::tcp_state::TcpState;
use std::ptr::NonNull;
use std::time::Duration;

/// Per-segment payload length used for both bench targets. 128 B is
/// representative of a small trading message (post-TCP-header). The
/// per-iovec cost is dominated by the `data_ptr()` pointer arithmetic
/// plus the iovec push; segment length only feeds the `total_len`
/// accumulator and the `recv_buf_delivered` counter, so payload size
/// has marginal effect at this bench's resolution.
const SEG_LEN: u16 = 128;

/// Build an ESTABLISHED-state TcpConn. Same shape as the
/// `deliver_readable_step` drift-guard tests in engine.rs.
fn make_est_conn() -> TcpConn {
    let t = FourTuple {
        local_ip: 0x0a_00_00_02,
        local_port: 40_000,
        peer_ip: 0x0a_00_00_01,
        peer_port: 5_000,
    };
    let mut c = TcpConn::new_client(t, 1000, 1460, 64 * 1024, 64 * 1024, 5_000, 5_000, 1_000_000);
    c.state = TcpState::Established;
    c
}

/// One fake rte_mbuf backing buffer. 256 B covers the first cacheline
/// of `struct rte_mbuf` where `buf_addr`, `data_off`, `refcnt`, `pool`
/// live; the helper never reads past that.
type FakeMbuf = Box<[u8; 256]>;

/// Seed `conn.recv.bytes` with `count` `InOrderSegment`s, one per
/// caller-supplied fake mbuf slot. Each segment carries `SEG_LEN` bytes
/// at offset 54 (the TCP-payload offset shape mirroring the
/// `bench_tcp_input_data_segment` fixture).
///
/// SAFETY: each `MbufHandle::from_raw` takes one refcount unit; we
/// bump refcount immediately before construction so the handle's
/// `Drop` pre-dec read is non-zero (otherwise the
/// `mbuf_refcnt_drop_unexpected` leak diagnostic fires).
fn seed_recv_bytes(conn: &mut TcpConn, fake_mbufs: &mut [FakeMbuf]) {
    for slot in fake_mbufs.iter_mut() {
        // SAFETY: slot is a live Box<[u8; 256]>; pointer is non-null.
        let nn: NonNull<dpdk_net_sys::rte_mbuf> = unsafe {
            NonNull::new_unchecked(slot.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf)
        };
        // Bump refcount so MbufHandle::Drop's pre-dec read is non-zero.
        // SAFETY: nn points at a Box<[u8; 256]>; the refcnt field lies
        // within the first cacheline (16 B in for x86_64), well within
        // the 256-byte allocation.
        unsafe {
            dpdk_net_sys::shim_rte_mbuf_refcnt_update(nn.as_ptr(), 1);
        }
        // SAFETY: refcount bump above transfers one ref to this handle.
        let mbuf = unsafe { MbufHandle::from_raw(nn) };
        conn.recv.bytes.push_back(InOrderSegment {
            mbuf,
            offset: 54,
            len: SEG_LEN,
        });
    }
}

/// Build a fresh `(FlowTable, handle, fake_mbufs)` with the requested
/// number of in-order segments queued. The `fake_mbufs` Vec must be
/// returned to the caller and kept alive across the helper call —
/// the `MbufHandle`s inside the conn dereference these pointers in
/// `data_ptr()` and on Drop.
fn make_seeded_state(seg_count: usize) -> (FlowTable, ConnHandle, Vec<FakeMbuf>) {
    let mut ft = FlowTable::new(4);
    let handle = ft
        .insert(make_est_conn())
        .expect("FlowTable::insert into a fresh 4-slot table cannot fail");
    let mut fake_mbufs: Vec<FakeMbuf> = (0..seg_count).map(|_| Box::new([0u8; 256])).collect();
    let conn = ft.get_mut(handle).expect("fresh handle valid");
    seed_recv_bytes(conn, &mut fake_mbufs);
    (ft, handle, fake_mbufs)
}

/// Drive the helper once and fold its observable side effects into
/// an accumulator. The accumulator is the caller's responsibility to
/// `black_box` outside the timed region.
///
/// We fold:
///   - `seg_count`, `total_len`, `partial_split` from the outcome
///     (forces LLVM to materialize the helper's return).
///   - Every produced iovec's `len` field (forces the iovec push to
///     have an observable consumer, so LLVM cannot collapse the
///     push loop's writes).
#[inline(always)]
fn run_deliver_readable_step(
    flow_table: &mut FlowTable,
    handle: ConnHandle,
    total_delivered: u32,
    counters: &Counters,
) -> u64 {
    let outcome = EngineNoEalHarness::deliver_readable_step(
        flow_table,
        handle,
        total_delivered,
        counters,
    );
    let mut acc: u64 = outcome.seg_count as u64;
    acc ^= outcome.total_len as u64;
    acc ^= outcome.partial_split as u64;
    // Fold every iovec's len so the per-segment push has an
    // observable consumer. The iovecs live on the conn's scratch
    // Vec; we walk them after the helper returns. `_pad` is always
    // zero per the helper contract, and `base` is constant per
    // fixture (zeroed fake mbuf), so folding `len` is the field
    // that carries the per-iter signal.
    if let Some(conn) = flow_table.get(handle) {
        for iov in &conn.readable_scratch_iovecs {
            acc ^= iov.len as u64;
        }
    }
    acc
}

/// Target: one segment in `recv.bytes`, deliver materializes 1 iovec.
/// Mirrors the steady-state "small single-segment receive" shape — a
/// 128 B trading message arrives in one mbuf, the engine pops it and
/// publishes a one-element iovec slice.
fn bench_deliver_readable_1_iovec(c: &mut Criterion) {
    c.bench_function("bench_deliver_readable_1_iovec", |b| {
        let counters = Counters::new();
        b.iter_batched_ref(
            // Per-iter setup: fresh flow table + freshly seeded
            // single segment. Outside the timed region.
            || make_seeded_state(1),
            |(flow_table, handle, _fake_mbufs)| {
                let acc = run_deliver_readable_step(
                    flow_table,
                    black_box(*handle),
                    SEG_LEN as u32,
                    &counters,
                );
                black_box(acc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Target: four segments in `recv.bytes`, deliver materializes 4
/// iovecs. Mirrors a multi-segment receive (e.g. a 4-MTU burst from
/// the peer arrived together and got dispatched into separate
/// `InOrderSegment`s; the deliver step publishes them in one
/// READABLE event with a 4-element iovec slice). The bench's reported
/// number minus the 1-iovec number approximates the per-iovec
/// scaling cost (pop + push + `data_ptr` arithmetic + `total_len`
/// fold).
fn bench_deliver_readable_4_iovec(c: &mut Criterion) {
    c.bench_function("bench_deliver_readable_4_iovec", |b| {
        let counters = Counters::new();
        b.iter_batched_ref(
            || make_seeded_state(4),
            |(flow_table, handle, _fake_mbufs)| {
                let acc = run_deliver_readable_step(
                    flow_table,
                    black_box(*handle),
                    (SEG_LEN as u32) * 4,
                    &counters,
                );
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
        bench_deliver_readable_1_iovec,
        bench_deliver_readable_4_iovec,
}
criterion_main!(benches);
