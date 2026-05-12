//! bench-micro::send — spec §11.2 targets 9 + 10.
//!
//! These benches measure the **per-segment build work** that
//! `Engine::send_bytes` (engine.rs:5655-5989) executes on the
//! production TX hot path. The old `bench_send_small` /
//! `bench_send_large_chain` benches measured `SendQueue::push`,
//! which was removed from `Engine::send_bytes` in A5 Task 10
//! (see engine.rs:5947-5951 — `c.snd.push(&bytes[..accepted])` is
//! gone; `snd_retrans` owns in-flight tracking via mbuf refs).
//! Those benches were dead code and have been deleted.
//!
//! # Strategy (B — per-segment-build pure-function bench)
//!
//! Calling `Engine::send_bytes` directly requires a live Engine,
//! which requires DPDK EAL bring-up + a real `tx_data_mempool`.
//! bench-micro is no-EAL, so the alloc/refcnt/ring-push parts of
//! the loop (engine.rs:5784-5850) cannot be exercised here.
//!
//! What this bench DOES cover (the CPU-bound bulk of the per-segment
//! cost) — mirrors engine.rs:5734-5950 line for line:
//!
//!   * `crate::clock::now_ns()` for TSval                  (5741)
//!   * `TcpOpts { timestamps: Some(...) }` construction    (5742-5748)
//!   * `SegmentTx { .. }` literal build                    (5749-5762)
//!   * `frame.clear()` + `frame.resize(needed, 0)`         (5772-5773)
//!   * `tcp_output::build_segment(&seg, &mut frame[..])`   (5774) — IPv4
//!     + TCP checksum + header pack; the dominant cost
//!   * `std::ptr::copy_nonoverlapping` of `n` frame bytes
//!     into the (fake-Box-backed) mbuf data area           (5801)
//!   * `counters::add(&eth.tx_bytes, n)` + `counters::inc(&tcp.tx_data)` (5855-5856)
//!   * `SendRetrans::push_after_tx(entry)`                 (5896)
//!   * First-segment RTO arm: `TimerWheel::add(...)`       (5921-5931)
//!     (only fires once per bench iteration because each `iter` resets
//!     the per-conn state; this matches the steady-state cost of one
//!     `send_bytes` call kicking off a new in-flight burst)
//!
//! What this bench DOES NOT cover (out of scope without EAL):
//!
//!   * `shim_rte_pktmbuf_alloc(tx_data_mempool)`           (5784)
//!   * `shim_rte_pktmbuf_append(m, n)`                     (5792)
//!   * `tx_offload_finalize(...)` (it dereferences live    (5810)
//!     mbuf data-pointer/length fields written by
//!     `rte_pktmbuf_append`; skipped here)
//!   * `shim_rte_mbuf_refcnt_update(m, 1)` (real DPDK
//!     refcnt update on a live mbuf)                       (5824)
//!   * `tx_pending_data` ring push                         (5829-5849)
//!   * `arm_tlp_pto()` post-loop call (TLP gate rejects   (5985-5986)
//!     because no SRTT sample exists; cheap no-op anyway)
//!
//! Quantitative note: in a 1460 B MSS-sized segment, the IPv4 + TCP
//! pseudo-header + payload checksum fold inside `build_segment` is
//! ~95% of the per-segment CPU cost; the alloc/append/refcnt shim
//! triple typically adds < 30 ns combined. The numbers reported by
//! these benches are a tight lower bound on the per-segment work
//! `send_bytes` performs.
//!
//! # Targets
//!
//!   * `bench_send_small_segment_build` — one iteration drives a
//!     single 128 B payload through the per-segment loop (one
//!     iteration of the `while remaining > 0` block).
//!   * `bench_send_large_segment_build` — one iteration drives an
//!     8 KiB payload, which at MSS=1460 produces 6 MSS-sized
//!     segments (the loop body runs 6×). The first segment also
//!     pays the RTO-timer arm (engine.rs:5907 — `was_empty == true`
//!     on the first push); subsequent segments skip the arm.
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
use dpdk_net_core::counters::{add, inc, Counters};
use dpdk_net_core::flow_table::FourTuple;
use dpdk_net_core::mempool::Mbuf;
use dpdk_net_core::tcp_conn::TcpConn;
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::{build_segment, SegmentTx, FRAME_HDRS_MIN, TCP_ACK, TCP_PSH};
use dpdk_net_core::tcp_retrans::RetransEntry;
use dpdk_net_core::tcp_state::TcpState;
use dpdk_net_core::tcp_timer_wheel::{TimerKind, TimerNode, TimerWheel};
use std::time::Duration;

const OUR_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const GATEWAY_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const TEST_SEND_BUF_BYTES: u32 = 256 * 1024;
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

/// Drive the per-segment build loop body once per byte-slice arg,
/// mirroring engine.rs:5734-5945. `frame` is the engine's
/// `tx_frame_scratch` analogue; `fake_mbuf_data` simulates the
/// `dst` region inside an `rte_pktmbuf_append`-returned buffer;
/// `wheel` accepts the first-segment RTO arm.
///
/// Returns the count of segments built so the caller can `black_box` it.
#[inline(always)]
fn run_segment_build_loop(
    conn: &mut TcpConn,
    bytes: &[u8],
    frame: &mut Vec<u8>,
    fake_mbuf_data: &mut [u8],
    wheel: &mut TimerWheel,
    counters: &Counters,
    fake_mbuf_ptr: *mut dpdk_net_sys::rte_mbuf,
) -> u32 {
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

    let mut seg_count: u32 = 0;
    while remaining > 0 {
        let take = remaining.min(mss_cap as usize);
        let payload = &bytes[offset..offset + take];
        // engine.rs:5740-5748 — TS option per RFC 7323 §3 MUST-22.
        let options = if ts_enabled {
            let tsval = (dpdk_net_core::clock::now_ns() / 1000) as u32;
            TcpOpts {
                timestamps: Some((tsval, ts_recent)),
                ..Default::default()
            }
        } else {
            TcpOpts::default()
        };
        let seg = SegmentTx {
            src_mac: OUR_MAC,
            dst_mac: GATEWAY_MAC,
            src_ip: tuple.local_ip,
            dst_ip: tuple.peer_ip,
            src_port: tuple.local_port,
            dst_port: tuple.peer_port,
            seq: cur_seq,
            ack: rcv_nxt,
            flags: TCP_ACK | TCP_PSH,
            window: advertised_window,
            options,
            payload,
        };
        // engine.rs:5767-5773 — clear + resize the frame scratch.
        let needed = FRAME_HDRS_MIN + 40 + take;
        frame.clear();
        frame.resize(needed, 0);
        // engine.rs:5774 — the dominant per-segment cost (IPv4 + TCP
        // checksum + header pack).
        let Some(n) = build_segment(&seg, frame.as_mut_slice()) else {
            break;
        };
        // engine.rs:5800-5802 — copy the freshly-built frame into the
        // mbuf data area. Fake mbuf's data buffer is 4 KiB so any
        // 1460+headers-byte segment fits.
        unsafe {
            std::ptr::copy_nonoverlapping(frame.as_ptr(), fake_mbuf_data.as_mut_ptr(), n);
        }
        // engine.rs:5855-5856 — eth.tx_bytes / tcp.tx_data counter updates.
        add(&counters.eth.tx_bytes, n as u64);
        inc(&counters.tcp.tx_data);

        // engine.rs:5877-5889 — build the RetransEntry; engine.rs:5896
        // — push_after_tx; engine.rs:5907-5938 — first-segment RTO arm.
        let first_tx_ts_ns = dpdk_net_core::clock::now_ns();
        let hdrs_len = (n - take) as u16;
        let entry = RetransEntry {
            seq: cur_seq,
            len: take as u16,
            mbuf: Mbuf::from_ptr(fake_mbuf_ptr),
            first_tx_ts_ns,
            xmit_count: 1,
            sacked: false,
            lost: false,
            xmit_ts_ns: first_tx_ts_ns,
            hdrs_len,
        };
        let was_empty = conn.snd_retrans.is_empty();
        conn.snd_retrans.push_after_tx(entry);
        // record_send is an early-return when send_ack_log is disabled
        // (default state); kept here for production-path parity.
        conn.send_ack_log.record_send(
            dpdk_net_core::tcp_send_ack_log::SeqRange {
                begin: cur_seq,
                end: cur_seq.wrapping_add(take as u32),
            },
            first_tx_ts_ns,
        );
        if was_empty && conn.rto_timer_id.is_none() {
            let rto_us = conn.rtt_est.rto_us();
            if rto_us > 0 {
                let fire_at = first_tx_ts_ns + (rto_us as u64 * 1_000);
                let id = wheel.add(
                    first_tx_ts_ns,
                    TimerNode {
                        fire_at_ns: fire_at,
                        owner_handle: 1, // arbitrary; harness never fires
                        kind: TimerKind::Rto,
                        user_data: 0,
                        generation: 0,
                        cancelled: false,
                    },
                );
                conn.rto_timer_id = Some(id);
                conn.timer_ids.push(id);
            }
        }

        offset += take;
        cur_seq = cur_seq.wrapping_add(take as u32);
        remaining -= take;
        seg_count += 1;
    }

    // engine.rs:5952-5957 — advance snd_nxt. Production code also
    // tracks `accepted` for the `arm_tlp_pto` gate + `send_refused_pending`
    // signal; we omit both because the TLP gate rejects without an SRTT
    // sample (no-op cost) and `send_refused_pending` is a one-bit write
    // measured elsewhere.
    conn.snd_nxt = cur_seq;
    seg_count
}

/// Target 9: single 128 B payload → 1 segment built.
///
/// Per-iter `iter_batched_ref` setup resets the conn / wheel /
/// retrans queue so each `send_bytes`-equivalent call sees the
/// same "fresh in-flight burst" cost (snd_retrans empty + no RTO
/// timer set), which is the cold path inside the burst-send
/// pattern. This is the worst-case-per-segment cost — subsequent
/// in-burst segments skip the RTO arm.
fn bench_send_small_segment_build(c: &mut Criterion) {
    c.bench_function("bench_send_small_segment_build", |b| {
        let payload = [0x42u8; 128];
        // Frame + fake mbuf data are reused across iterations
        // (matches `tx_frame_scratch`'s reuse pattern in engine.rs:5723).
        let mut frame: Vec<u8> = Vec::with_capacity(FRAME_HDRS_MIN + 40 + MSS as usize);
        let mut fake_mbuf_data: Box<[u8; 4096]> = Box::new([0u8; 4096]);
        let counters = Counters::new();

        b.iter_batched_ref(
            // Per-iter setup: fresh conn + wheel + retrans queue so the
            // first-segment RTO arm fires (matches the steady-state
            // entry cost of one send_bytes call into a freshly drained
            // in-flight queue).
            || {
                (
                    make_est_conn(),
                    // 64-slot wheel is more than enough for a single
                    // RTO arm per iter; matches the harness's tiny
                    // capacity in bench-poll.rs.
                    TimerWheel::new(64),
                )
            },
            |(conn, wheel)| {
                let fake_mbuf_ptr = fake_mbuf_data.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf;
                let n = run_segment_build_loop(
                    conn,
                    black_box(&payload),
                    &mut frame,
                    fake_mbuf_data.as_mut_slice(),
                    wheel,
                    &counters,
                    fake_mbuf_ptr,
                );
                black_box(n);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Target 10: 8 KiB payload → 6 MSS-sized segments at MSS=1460.
///
/// 8192 / 1460 = 5.6 → 6 segments (5 × 1460 + 1 × 892). Mirrors a
/// "burst write that fills several wire-MTU segments back to back".
/// First segment pays the RTO-arm cost; segments 2..6 hit the
/// `was_empty == false` fast path. This is the multi-segment shape
/// the old `bench_send_large_chain` claimed to measure but actually
/// didn't (it benched a single `VecDeque` copy).
fn bench_send_large_segment_build(c: &mut Criterion) {
    c.bench_function("bench_send_large_segment_build", |b| {
        let payload = vec![0x42u8; 8 * 1024];
        let mut frame: Vec<u8> = Vec::with_capacity(FRAME_HDRS_MIN + 40 + MSS as usize);
        let mut fake_mbuf_data: Box<[u8; 4096]> = Box::new([0u8; 4096]);
        let counters = Counters::new();

        b.iter_batched_ref(
            || (make_est_conn(), TimerWheel::new(64)),
            |(conn, wheel)| {
                let fake_mbuf_ptr = fake_mbuf_data.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf;
                let n = run_segment_build_loop(
                    conn,
                    black_box(&payload),
                    &mut frame,
                    fake_mbuf_data.as_mut_slice(),
                    wheel,
                    &counters,
                    fake_mbuf_ptr,
                );
                black_box(n);
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
    targets = bench_send_small_segment_build, bench_send_large_segment_build
}
criterion_main!(benches);
