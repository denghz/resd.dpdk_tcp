//! bench-micro::send — spec §11.2 targets 9 + 10.
//!
//! These benches measure the **per-segment build CPU work** that
//! `Engine::send_bytes` (engine.rs:5655-5989) executes on the
//! production TX hot path. The old `bench_send_small` /
//! `bench_send_large_chain` benches measured `SendQueue::push`,
//! which was removed from `Engine::send_bytes` in A5 Task 10
//! (see engine.rs:5947-5951 — `c.snd.push(&bytes[..accepted])` is
//! gone; `snd_retrans` owns in-flight tracking via mbuf refs).
//! Those benches were dead code and have been deleted.
//!
//! # What this bench covers
//!
//! Each iteration drives the **canonical per-segment helper**,
//! `dpdk_net_core::engine::test_support::EngineNoEalHarness::
//! send_bytes_segment_build_step`, which lives behind the
//! `bench-internals` feature gate inside `dpdk-net-core` and is
//! covered by `send_bytes_segment_build_step_drift_guard` unit tests
//! in `engine.rs`. The helper mirrors the body of the production
//! `while remaining > 0` loop at engine.rs:5734-5944; if that loop
//! changes shape without the helper updating in lock-step, the
//! drift-guard test breaks at PR time rather than years later when
//! a bench number silently drifts.
//!
//! The helper covers (engine.rs line ranges in parens):
//!
//!   * TS option per RFC 7323 §3 MUST-22                          (5740-5748)
//!   * `SegmentTx` literal build                                  (5749-5762)
//!   * `frame.clear()` + `frame.resize(needed, 0)`                (5767-5773)
//!   * `tcp_output::build_segment(&seg, &mut frame[..])`          (5774)
//!     — IPv4 + TCP checksum + header pack; the dominant cost
//!   * `std::ptr::copy_nonoverlapping` of `n` frame bytes into
//!     the (fake-Box-backed) mbuf data area                       (5800-5802)
//!   * `counters::add(&eth.tx_bytes, n)` + `counters::inc(&tcp.tx_data)` (5855-5856)
//!   * `RetransEntry { .. }` + `SendRetrans::push_after_tx`       (5879-5896)
//!   * `send_ack_log.record_send(..)` — early-return when disabled (5900-5906)
//!   * First-burst RTO arm: `TimerWheel::add(..)`                 (5907-5938)
//!
//! # What this bench DELIBERATELY skips
//!
//! Four ops in the production per-segment loop require a live DPDK
//! EAL + mempool + port, which bench-micro (no-EAL by design) cannot
//! provide. Each skip carries a one-line reason:
//!
//!   * `shim_rte_pktmbuf_alloc(tx_data_mempool)`            (5784)
//!     — no live DPDK mempool exists in this process.
//!   * `shim_rte_pktmbuf_append(m, n)`                      (5792)
//!     — needs the mbuf returned by `alloc`; the fake-Box `mbuf_data`
//!     slice simulates the returned `dst` region.
//!   * `tx_offload_finalize(m, &seg, ..)`                   (5810)
//!     — reads/writes live mbuf metadata (`ol_flags`, `l2_len`,
//!     `l3_len`, TCP-cksum pseudo-header rewrite) written by
//!     `rte_pktmbuf_append`; cannot exercise without a real mbuf.
//!   * `shim_rte_mbuf_refcnt_update(m, 1)`                  (5824)
//!     — touches a real DPDK refcount field.
//!   * `tx_pending_data` ring push                          (5829-5849)
//!     — the engine-owned `tx_pending_data` `RefCell<Ring>` does
//!     not exist outside an Engine instance.
//!
//! # `arm_tlp_pto` framing
//!
//! `Engine::send_bytes` also calls `arm_tlp_pto(handle)` after the
//! per-segment loop (engine.rs:5985-5986). It is omitted here because
//! the synthetic conn has no SRTT sample, so the gate at engine.rs:6004
//! (and `tlp_arm_gate_passes` downstream) short-circuits with zero
//! cost. **In production with SRTT** the call adds a `flow_table.borrow()`
//! plus a gate evaluation plus potentially a `TimerWheel::add` — that
//! cost is NOT captured by these benches.
//!
//! # Counter-contention caveat
//!
//! Counter ops are measured here as single-thread per-lcore cost on a
//! thread-local `Counters` with zero cross-core contention and ideal
//! cache locality. Production hits the same atomic on the owning
//! lcore (per-engine `Counters`, no contention either), so the ±2 ns
//! delta at 3 GHz between this fixture and a hot-cache production
//! load is below the bench's measurement noise floor. The bench is
//! NOT representative of cross-core contention on a shared atomic
//! (which the Stage 1 single-lcore engine does not have).
//!
//! # Targets
//!
//! Four targets — both sizes × both burst phases:
//!
//!   * `bench_send_small_segment_build_cold` — 128 B payload, fresh
//!     conn each iter → first-burst RTO arm pays the `TimerWheel::add`
//!     cost (engine.rs:5907 — `was_empty == true`).
//!   * `bench_send_small_segment_build_warm` — 128 B payload, conn
//!     pre-seeded with a pending retrans entry and an `rto_timer_id`
//!     so the arm gate short-circuits (engine.rs:5907 — `was_empty ==
//!     false`). Mirrors a steady-state in-burst segment.
//!   * `bench_send_large_segment_build_cold` — 8 KiB payload → 6
//!     MSS-sized segments (1460 B each, last one 892 B). First
//!     segment pays the RTO arm; segments 2..6 hit the warm gate.
//!   * `bench_send_large_segment_build_warm` — same 8 KiB payload
//!     but the conn already has an in-flight entry + rto_timer_id,
//!     so even the first segment of THIS burst skips the arm.
//!
//! Production sees both cold and warm cases. Reporting them
//! separately (rather than averaging) is more honest than the prior
//! single-number-per-size shape.
//!
//! # Fake-mbuf setup
//!
//! A Box-backed `[u8; 4096]` simulates the rte_mbuf data region
//! reachable from `dst` in engine.rs:5800. The bench does not call
//! any DPDK shim that reads/writes mbuf struct fields (refcount,
//! ol_flags, l2/l3/l4_len). The `RetransEntry::mbuf` field stores
//! a raw pointer for retransmit dispatch — we use a `Mbuf::from_ptr`
//! wrapper around the fake-Box pointer, which is safe because the
//! bench never invokes any retrans path that would dereference it.
//! Same convention as `bench_tcp_input_data_segment` (tcp_input.rs:78-86).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::counters::Counters;
use dpdk_net_core::engine::test_support::{
    EngineNoEalHarness, SegmentBuildScratch, SegmentBuildSnapshot,
};
use dpdk_net_core::flow_table::FourTuple;
use dpdk_net_core::mempool::Mbuf;
use dpdk_net_core::tcp_conn::TcpConn;
use dpdk_net_core::tcp_output::FRAME_HDRS_MIN;
use dpdk_net_core::tcp_retrans::RetransEntry;
use dpdk_net_core::tcp_state::TcpState;
use dpdk_net_core::tcp_timer_wheel::{TimerKind, TimerNode, TimerWheel};
use std::time::Duration;

const OUR_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const GATEWAY_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const TEST_SEND_BUF_BYTES: u32 = 256 * 1024;
/// MSS used by the bench. **MUST stay in lock-step with
/// `EngineConfig::default().tcp_mss`** (engine.rs ~line 576). If the
/// production default changes (e.g. jumbo-frame tuning), update this
/// constant here too — otherwise the bench's per-segment chunking
/// drifts from what `send_bytes` actually does on the same conn.
const MSS: u16 = 1460;

/// Construct an ESTABLISHED conn ready for `send_bytes` work — TS
/// negotiated (so the per-segment loop builds the TS option), no
/// SACK, peer window wide enough to admit the whole payload. Mirrors
/// the shape used inside `Engine::send_bytes` (engine.rs:5671-5688
/// snapshot) by setting `peer_mss`, `snd_wnd`, `ts_enabled`,
/// `ts_recent`.
fn make_est_conn() -> TcpConn {
    let t = FourTuple {
        local_ip: 0x0a_00_00_02,
        local_port: 40000,
        peer_ip: 0x0a_00_00_01,
        peer_port: 5000,
    };
    let mut c = TcpConn::new_client(
        t,
        1000,
        MSS,
        1024,
        TEST_SEND_BUF_BYTES,
        5000,
        5000,
        1_000_000,
    );
    c.state = TcpState::Established;
    c.snd_una = 1001;
    c.snd_nxt = 1001;
    c.irs = 5000;
    c.rcv_nxt = 5001;
    // Peer window large enough to admit any test payload without
    // tripping the `room_in_peer_wnd` cap in `send_bytes`.
    c.snd_wnd = 64 * 1024;
    c.ts_enabled = true;
    c.ts_recent = 0xCAFEBABE;
    // peer_mss is already MSS via `new_client`.
    c
}

/// Pre-seed the conn so the helper's first-burst RTO arm gate
/// rejects (engine.rs:5907 — `was_empty == false` || `rto_timer_id`
/// already set). Mirrors a steady-state in-burst segment.
fn warm_up_conn(conn: &mut TcpConn, wheel: &mut TimerWheel) {
    // One pending retrans entry → `snd_retrans.is_empty() == false`.
    conn.snd_retrans.push_after_tx(RetransEntry {
        seq: conn.snd_nxt.wrapping_sub(128),
        len: 128,
        // Null pointer is never derefed by the helper — only stashed.
        mbuf: Mbuf::from_ptr(std::ptr::null_mut()),
        first_tx_ts_ns: 0,
        xmit_count: 1,
        sacked: false,
        lost: false,
        xmit_ts_ns: 0,
        hdrs_len: 0,
    });
    // Pre-set the RTO timer id so even if `was_empty` were true, the
    // second clause of the gate rejects.
    let id = wheel.add(
        0,
        TimerNode {
            fire_at_ns: 1_000_000_000,
            owner_handle: 1,
            kind: TimerKind::Rto,
            user_data: 0,
            generation: 0,
            cancelled: false,
        },
    );
    conn.rto_timer_id = Some(id);
    conn.timer_ids.push(id);
}

/// Drive the per-segment build loop body once per byte-slice arg,
/// delegating to the canonical per-segment helper. Returns the
/// per-segment `frame_len` xor-accumulator so the caller can
/// `black_box` an observable consumer of the produced bytes (codex C1).
#[inline(always)]
fn run_segment_build_loop(
    conn: &mut TcpConn,
    bytes: &[u8],
    frame: &mut Vec<u8>,
    fake_mbuf_data: &mut [u8],
    wheel: &mut TimerWheel,
    counters: &Counters,
    fake_mbuf_ptr: *mut dpdk_net_sys::rte_mbuf,
) -> u64 {
    // Snapshot the per-call fields the production loop reads under a
    // single immutable borrow at engine.rs:5671-5688.
    let tuple = conn.four_tuple();
    let seq_start = conn.snd_nxt;
    let snd_una = conn.snd_una;
    let snd_wnd = conn.snd_wnd;
    let peer_mss = conn.peer_mss;
    let rcv_nxt = conn.rcv_nxt;
    let free_space_total = conn.recv.free_space_total();
    let ws_shift_out = conn.ws_shift_out;
    let ts_enabled = conn.ts_enabled;
    let ts_recent = conn.ts_recent;

    // Mirrors engine.rs:5694-5703 — the room/in-flight/remaining
    // computation that bounds the per-call accepted byte count.
    let mss_cap = (peer_mss as u32).min(MSS as u32).max(1);
    let in_flight = seq_start.wrapping_sub(snd_una);
    let room_in_peer_wnd = snd_wnd.saturating_sub(in_flight);
    let send_buf_room = TEST_SEND_BUF_BYTES.saturating_sub(in_flight);
    let mut remaining = bytes
        .len()
        .min(room_in_peer_wnd as usize)
        .min(send_buf_room as usize);
    let mut offset = 0usize;
    let mut cur_seq = seq_start;

    let advertised_window = (free_space_total >> ws_shift_out).min(u16::MAX as u32) as u16;

    // Mirrors engine.rs:5723-5733 — pre-size the frame scratch.
    let initial_cap_needed = FRAME_HDRS_MIN + 40 + mss_cap as usize;
    let current_cap = frame.capacity();
    if current_cap < initial_cap_needed {
        frame.reserve(initial_cap_needed - current_cap);
    }

    // Snapshot bundled per the helper's contract — read once, threaded
    // unchanged through every per-segment call.
    let snapshot = SegmentBuildSnapshot {
        src_mac: OUR_MAC,
        dst_mac: GATEWAY_MAC,
        src_ip: tuple.local_ip,
        dst_ip: tuple.peer_ip,
        src_port: tuple.local_port,
        dst_port: tuple.peer_port,
        rcv_nxt,
        advertised_window,
        ts_enabled,
        ts_recent,
    };

    // Accumulate an observable side effect of the per-segment writes
    // so LLVM cannot DCE the `copy_nonoverlapping` inside the helper
    // (codex C1). xor of (frame_len, first-byte, mid-byte, last-byte)
    // touches all three pages the production code writes (header,
    // option block, payload tail) and is < 1 ns per segment.
    let mut byte_acc: u64 = 0;

    while remaining > 0 {
        let take = remaining.min(mss_cap as usize);
        let payload = &bytes[offset..offset + take];

        let outcome = EngineNoEalHarness::send_bytes_segment_build_step(
            conn,
            cur_seq,
            payload,
            &snapshot,
            &mut SegmentBuildScratch {
                frame,
                mbuf_data: fake_mbuf_data,
                wheel,
                counters,
                fake_mbuf_ptr,
            },
        );
        let Some(n) = outcome.frame_len else {
            break;
        };
        // C1 (codex): touch the freshly-written mbuf bytes so the
        // upstream `build_segment` + `copy_nonoverlapping` writes have
        // an observable consumer. Pick three offsets across the frame
        // — header (0), option-block midpoint, payload tail — so a
        // DCE pass cannot prove any contiguous range is dead.
        byte_acc ^= n as u64;
        byte_acc ^= fake_mbuf_data[0] as u64;
        if n >= 2 {
            byte_acc ^= fake_mbuf_data[n / 2] as u64;
            byte_acc ^= fake_mbuf_data[n - 1] as u64;
        }

        offset += take;
        cur_seq = cur_seq.wrapping_add(take as u32);
        remaining -= take;
    }

    // engine.rs:5952-5957 — advance snd_nxt. Production code also
    // tracks `accepted` for the `arm_tlp_pto` gate + `send_refused_pending`
    // signal; we omit both because the TLP gate rejects without an SRTT
    // sample in this fixture (see top-of-file `arm_tlp_pto` framing
    // note) and `send_refused_pending` is a one-bit write measured
    // elsewhere.
    conn.snd_nxt = cur_seq;
    byte_acc
}

/// Target 9 (cold): single 128 B payload → 1 segment built.
///
/// Per-iter `iter_batched_ref` setup resets the conn / wheel /
/// retrans queue so each `send_bytes`-equivalent call sees the
/// "fresh in-flight burst" cost (snd_retrans empty + no RTO timer
/// set), which is the path inside the burst-send pattern where the
/// first-burst RTO arm fires. Worst-case-per-segment cost.
fn bench_send_small_segment_build_cold(c: &mut Criterion) {
    c.bench_function("bench_send_small_segment_build_cold", |b| {
        let payload = [0x42u8; 128];
        let mut frame: Vec<u8> = Vec::with_capacity(FRAME_HDRS_MIN + 40 + MSS as usize);
        let mut fake_mbuf_data: Box<[u8; 4096]> = Box::new([0u8; 4096]);
        let counters = Counters::new();

        b.iter_batched_ref(
            || (make_est_conn(), TimerWheel::new(64)),
            |(conn, wheel)| {
                let fake_mbuf_ptr = fake_mbuf_data.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf;
                let acc = run_segment_build_loop(
                    conn,
                    black_box(&payload),
                    &mut frame,
                    fake_mbuf_data.as_mut_slice(),
                    wheel,
                    &counters,
                    fake_mbuf_ptr,
                );
                black_box(acc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Target 9 (warm): single 128 B payload → 1 segment built, conn
/// pre-seeded so the first-burst RTO arm gate rejects. Mirrors the
/// steady-state in-burst segment cost — segments 2..N of any burst,
/// or any single-segment send into a conn that already has in-flight
/// data unACKed. Production sees this case at least as often as the
/// cold case (every multi-segment burst plus every in-flight write).
fn bench_send_small_segment_build_warm(c: &mut Criterion) {
    c.bench_function("bench_send_small_segment_build_warm", |b| {
        let payload = [0x42u8; 128];
        let mut frame: Vec<u8> = Vec::with_capacity(FRAME_HDRS_MIN + 40 + MSS as usize);
        let mut fake_mbuf_data: Box<[u8; 4096]> = Box::new([0u8; 4096]);
        let counters = Counters::new();

        b.iter_batched_ref(
            || {
                let mut conn = make_est_conn();
                let mut wheel = TimerWheel::new(64);
                warm_up_conn(&mut conn, &mut wheel);
                (conn, wheel)
            },
            |(conn, wheel)| {
                let fake_mbuf_ptr = fake_mbuf_data.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf;
                let acc = run_segment_build_loop(
                    conn,
                    black_box(&payload),
                    &mut frame,
                    fake_mbuf_data.as_mut_slice(),
                    wheel,
                    &counters,
                    fake_mbuf_ptr,
                );
                black_box(acc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Target 10 (cold): 8 KiB payload → 6 MSS-sized segments at MSS=1460.
///
/// 8192 / 1460 = 5.6 → 6 segments (5 × 1460 + 1 × 892). Mirrors a
/// "burst write that fills several wire-MTU segments back to back".
/// First segment pays the RTO-arm cost; segments 2..6 hit the
/// `was_empty == false` fast path. This is the multi-segment shape
/// the old `bench_send_large_chain` claimed to measure but actually
/// didn't (it benched a single `VecDeque` copy).
fn bench_send_large_segment_build_cold(c: &mut Criterion) {
    c.bench_function("bench_send_large_segment_build_cold", |b| {
        let payload = vec![0x42u8; 8 * 1024];
        let mut frame: Vec<u8> = Vec::with_capacity(FRAME_HDRS_MIN + 40 + MSS as usize);
        let mut fake_mbuf_data: Box<[u8; 4096]> = Box::new([0u8; 4096]);
        let counters = Counters::new();

        b.iter_batched_ref(
            || (make_est_conn(), TimerWheel::new(64)),
            |(conn, wheel)| {
                let fake_mbuf_ptr = fake_mbuf_data.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf;
                let acc = run_segment_build_loop(
                    conn,
                    black_box(&payload),
                    &mut frame,
                    fake_mbuf_data.as_mut_slice(),
                    wheel,
                    &counters,
                    fake_mbuf_ptr,
                );
                black_box(acc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Target 10 (warm): 8 KiB payload → 6 MSS-sized segments at MSS=1460,
/// conn pre-seeded so even the first segment of this burst skips the
/// RTO arm. Surfaces the pure-loop-body cost — every segment hits the
/// in-burst fast path.
fn bench_send_large_segment_build_warm(c: &mut Criterion) {
    c.bench_function("bench_send_large_segment_build_warm", |b| {
        let payload = vec![0x42u8; 8 * 1024];
        let mut frame: Vec<u8> = Vec::with_capacity(FRAME_HDRS_MIN + 40 + MSS as usize);
        let mut fake_mbuf_data: Box<[u8; 4096]> = Box::new([0u8; 4096]);
        let counters = Counters::new();

        b.iter_batched_ref(
            || {
                let mut conn = make_est_conn();
                let mut wheel = TimerWheel::new(64);
                warm_up_conn(&mut conn, &mut wheel);
                (conn, wheel)
            },
            |(conn, wheel)| {
                let fake_mbuf_ptr = fake_mbuf_data.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf;
                let acc = run_segment_build_loop(
                    conn,
                    black_box(&payload),
                    &mut frame,
                    fake_mbuf_data.as_mut_slice(),
                    wheel,
                    &counters,
                    fake_mbuf_ptr,
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
        bench_send_small_segment_build_cold,
        bench_send_small_segment_build_warm,
        bench_send_large_segment_build_cold,
        bench_send_large_segment_build_warm,
}
criterion_main!(benches);
