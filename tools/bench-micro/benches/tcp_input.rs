//! bench-micro::tcp_input — spec §11.2 targets 7 + 8 + T4 (pure-ACK
//! left-edge advance).
//!
//! `bench_tcp_input_data_segment` measures `tcp_input::dispatch` for an
//! in-order data segment with PAWS + SACK enabled. The ACK field on
//! that segment equals `snd_una` (no left-edge advance), so the
//! send-side ACK-processing branches that run on every production
//! pure-ACK are NOT exercised by that target — see
//! `bench_tcp_input_pure_ack` below.
//!
//! `bench_tcp_input_ooo_segment` measures the same for an OOO segment
//! that queues into the reassembly buffer.
//!
//! `bench_tcp_input_pure_ack` measures the pure-ACK left-edge advance
//! path: no payload, `seq == rcv_nxt`, `ack > snd_una`. The dispatch
//! site walks `snd_retrans` twice (RACK update_on_ack + detect_lost),
//! takes a TS-source RTT sample, and runs the WRITABLE hysteresis
//! check. The retrans queue is pre-seeded with a single in-flight
//! entry whose `[seq, seq+len)` lies fully below the incoming ACK so
//! the entry's `cum_acked` branch fires inside `update_on_ack`. The
//! actual deque pop (`prune_below_into_mbufs`) lives engine-side, not
//! inside `dispatch` (engine.rs:4913), so this bench does NOT measure
//! the pop or the per-entry mbuf free. It DOES measure the two
//! `snd_retrans` iterations, the RTT sampler, and the WRITABLE
//! hysteresis arithmetic.
//!
//! # Setup parity with in-tree unit tests
//!
//! These benches use the same TcpConn construction pattern as
//! `crates/dpdk-net-core/src/tcp_input.rs::tests::est_conn` and
//! `established_ooo_segment_queues_into_reassembly`. We cannot import
//! those helpers directly — they live under `#[cfg(test)]` — so the
//! setup is open-coded here but mirrors the shapes.
//!
//! A fake `rte_mbuf` pointer backed by boxed 256-B storage covers the
//! refcount-shim dereferences made by the reassembly path. The shim
//! only touches the first cacheline's `refcnt` field. The pure-ACK
//! target's pre-seeded retrans entries hold a null mbuf pointer — the
//! engine-side `prune_below_into_mbufs` (which would dereference it)
//! does not run inside `dispatch`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::engine::DEFAULT_RTT_HISTOGRAM_EDGES_US;
use dpdk_net_core::flow_table::FourTuple;
use dpdk_net_core::mempool::Mbuf;
use dpdk_net_core::tcp_conn::TcpConn;
use dpdk_net_core::tcp_input::{dispatch, MbufInsertCtx, ParsedSegment};
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::TCP_ACK;
use dpdk_net_core::tcp_retrans::RetransEntry;
use dpdk_net_core::tcp_state::TcpState;
use std::time::Duration;

const TEST_SEND_BUF_BYTES: u32 = 256 * 1024;

/// Shared ESTABLISHED-state conn construction. Mirrors the in-tree
/// `tcp_input::tests::est_conn` shape; TS + SACK are opt-in per caller
/// so the OOO bench (target 8) can measure reassembly-queue enqueue
/// without PAWS early-rejecting segments that arrive without a
/// Timestamps option.
fn make_est_conn(
    iss: u32,
    irs: u32,
    peer_wnd: u16,
    ts: Option<u32>,
    sack: bool,
) -> TcpConn {
    let t = FourTuple {
        local_ip: 0x0a_00_00_02,
        local_port: 40000,
        peer_ip: 0x0a_00_00_01,
        peer_port: 5000,
    };
    let mut c = TcpConn::new_client(t, iss, 1460, 1024, 2048, 5000, 5000, 1_000_000);
    c.state = TcpState::Established;
    c.snd_una = iss.wrapping_add(1);
    c.snd_nxt = iss.wrapping_add(1);
    c.irs = irs;
    c.rcv_nxt = irs.wrapping_add(1);
    c.snd_wnd = peer_wnd as u32;
    if let Some(ts_recent) = ts {
        c.ts_enabled = true;
        c.ts_recent = ts_recent;
    }
    c.sack_enabled = sack;
    c
}

fn bench_tcp_input_data_segment(c: &mut Criterion) {
    c.bench_function("bench_tcp_input_data_segment", |b| {
        // Encode a minimal Timestamps option. SACK_PERMITTED was already
        // exchanged on the handshake; we don't repeat it per-segment.
        let peer_opts = TcpOpts {
            timestamps: Some((0x0000_1000, 0xCAFEBABE)),
            ..Default::default()
        };
        let mut opts_buf = [0u8; 40];
        let opts_len = peer_opts.encode(&mut opts_buf).expect("encode opts");
        let payload = [0xABu8; 64];

        // Fake mbuf storage for the in-order append path's refcount shim.
        let mut fake_storage: Box<[u8; 256]> = Box::new([0u8; 256]);
        let mbuf_ctx = MbufInsertCtx {
            mbuf: unsafe {
                std::ptr::NonNull::new_unchecked(
                    fake_storage.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf
                )
            },
            payload_offset: 54,
        };

        b.iter_batched_ref(
            // Per-iteration setup: fresh conn so each dispatch sees an
            // in-order segment at rcv_nxt (5001), rather than the state
            // advancing after the first iteration.
            || make_est_conn(1000, 5000, 1024, Some(200), true),
            |c| {
                // Increment TSval slightly so PAWS accepts. Using a
                // fresh conn per-iteration means ts_recent == 200 on
                // entry, so TSval=300 is always in-window.
                let seg = ParsedSegment {
                    src_port: 5000,
                    dst_port: 40000,
                    seq: 5001,
                    ack: 1001,
                    flags: TCP_ACK,
                    window: 65535,
                    header_len: 20 + opts_len,
                    payload: &payload,
                    options: &opts_buf[..opts_len],
                };
                let out = dispatch(
                    black_box(c),
                    black_box(&seg),
                    &DEFAULT_RTT_HISTOGRAM_EDGES_US,
                    TEST_SEND_BUF_BYTES,
                    Some(mbuf_ctx),
                );
                black_box(out);
            },
            criterion::BatchSize::SmallInput,
        );

        // Keep fake_storage alive until the TcpConn's (and therefore the
        // ReorderQueue's) Drop observes the refcnt field.
        let _ = &mut fake_storage;
    });
}

fn bench_tcp_input_ooo_segment(c: &mut Criterion) {
    c.bench_function("bench_tcp_input_ooo_segment", |b| {
        // OOO segment: seq > rcv_nxt, so it queues into the reorder
        // buffer. No options payload (matches the in-tree OOO test's
        // minimalism at tcp_input.rs:1866).
        //
        // IMPORTANT: TS stays disabled here. If `conn.ts_enabled` were
        // true, `dispatch` would PAWS-early-reject any segment lacking
        // a Timestamps option (tcp_input.rs:606-614), returning before
        // touching the reassembly queue. The point of this target is
        // to measure reassembly-queue enqueue cost (~200-400 ns per
        // spec §11.2), not PAWS rejection (~27 ns).
        let payload = [0x42u8; 64];

        let mut fake_storage: Box<[u8; 256]> = Box::new([0u8; 256]);
        let mbuf_ctx = MbufInsertCtx {
            mbuf: unsafe {
                std::ptr::NonNull::new_unchecked(
                    fake_storage.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf
                )
            },
            payload_offset: 54,
        };

        b.iter_batched_ref(
            || make_est_conn(1000, 5000, 1024, None, false),
            |c| {
                // seq=5100 > rcv_nxt=5001 → reassembly queue path.
                let seg = ParsedSegment {
                    src_port: 5000,
                    dst_port: 40000,
                    seq: 5100,
                    ack: 1001,
                    flags: TCP_ACK,
                    window: 65535,
                    header_len: 20,
                    payload: &payload,
                    options: &[],
                };
                let out = dispatch(
                    black_box(c),
                    black_box(&seg),
                    &DEFAULT_RTT_HISTOGRAM_EDGES_US,
                    TEST_SEND_BUF_BYTES,
                    Some(mbuf_ctx),
                );
                black_box(out);
            },
            criterion::BatchSize::SmallInput,
        );
        let _ = &mut fake_storage;
    });
}

/// Measures `tcp_input::dispatch` on a pure ACK that advances the
/// send-side left edge.
///
/// What this covers (the path is taken because `seq == rcv_nxt`,
/// `payload.is_empty()`, and `seq_lt(snd_una, ack) && seq_le(ack,
/// snd_nxt)`):
///   - `parse_options` decode of the TS option (single TS, no SACK
///     blocks);
///   - PAWS check against `ts_recent` (incoming `tsval` accepted);
///   - the send-side ACK branch at `tcp_input.rs:915-1005` —
///     `snd_una` advance, TS-source RTT sample via
///     `tcp_rtt::Rtt::sample`, WRITABLE hysteresis arithmetic (with
///     `send_refused_pending = true` pre-seeded so the conditional
///     body runs);
///   - the two `snd_retrans` walks at `tcp_input.rs:1071-1077` and
///     `1095-1109` — `update_on_ack` on the pre-seeded entry (its
///     `end_seq <= new snd_una` so the `cum_acked` arm fires) +
///     the detect-lost iteration (which skips the entry because
///     `end_seq <= snd_una`).
///
/// What this does NOT cover:
///   - `SendRetrans::prune_below_into_mbufs` — runs engine-side at
///     `engine.rs:4913` AFTER `dispatch` returns, so the deque pop
///     and per-entry mbuf free are out of scope;
///   - WRITABLE event delivery — the `Outcome.writable_hysteresis_fired`
///     bit is set inside `dispatch`, but the `InternalEvent::Writable`
///     push lives in the engine's outcome translator (T8 territory);
///   - RTO/TLP timer rearm — also engine-side post-dispatch;
///   - SACK scoreboard interaction — `sack_enabled` is left off here
///     to keep the path focused on the left-edge-advance shape; a
///     pure dup-ACK with SACK blocks would exercise a different
///     branch that this target does not measure.
///
/// Important shape detail: this is a left-edge ADVANCE
/// (`ack > snd_una`). If `ack == snd_una` (duplicate ACK, pure window
/// update, or zero-progress ACK) the dispatch site takes a faster
/// branch (`snd_una_advanced_to` stays `None`, no RTT sample, no
/// WRITABLE check) that this bench does NOT measure. The "every WS
/// ping reply / every REST response confirmation" framing applies to
/// the advance case; pure window updates are a separate, cheaper
/// shape.
///
/// `dispatch` is not `#[inline]`-marked at its definition; criterion
/// drives it across the crate boundary the same way every other
/// caller does, so the measurement reflects the production call
/// shape rather than a fully-inlined hot loop.
fn bench_tcp_input_pure_ack(c: &mut Criterion) {
    c.bench_function("bench_tcp_input_pure_ack", |b| {
        // Encode a Timestamps option. `tsval` must be > the conn's
        // `ts_recent` so PAWS accepts; `tsecr` must be `now_us` minus
        // a small delta so the RTT sample (`now_us - tsecr`) lands
        // inside the (1, 60_000_000) microsecond window that
        // `handle_established` validates at `tcp_input.rs:934`. We
        // sample `now_us` once here — criterion's outer wrapper
        // around `b.iter_batched_ref` typically completes the
        // measurement loop within seconds, well inside the 60-second
        // PAWS-RTT validity window. Initial `ts_recent` on the conn
        // is `200` (set by `make_est_conn`), and we use a `tsval`
        // far above that, so PAWS comfortably accepts.
        let now_us = (dpdk_net_core::clock::now_ns() / 1_000) as u32;
        // Subtract a 1ms offset so `now_us - tsecr` is comfortably
        // above 1us (the validator's lower bound) even if the bench
        // routine fires within the same TSC tick as this read.
        let tsecr = now_us.wrapping_sub(1_000);
        let peer_opts = TcpOpts {
            timestamps: Some((0x0001_0000, tsecr)),
            ..Default::default()
        };
        let mut opts_buf = [0u8; 40];
        let opts_len = peer_opts.encode(&mut opts_buf).expect("encode opts");

        // Pre-seed `snd_retrans` with one entry sized 128 bytes whose
        // `[seq, end_seq)` is [1001, 1129). The incoming ACK advances
        // `snd_una` from 1001 to 1129, so the entry is fully covered
        // (`end_seq == 1129 <= snd_una_new == 1129`) and the
        // `update_on_ack` cum_acked branch fires for it. The entry's
        // mbuf is null — `dispatch` never dereferences it; the
        // engine-side `prune_below_into_mbufs` (which would) runs
        // after `dispatch` returns and is out of scope here.
        const RETRANS_SEQ: u32 = 1001;
        const RETRANS_LEN: u16 = 128;
        const ACK_VALUE: u32 = 1129; // RETRANS_SEQ + RETRANS_LEN
        const SND_NXT: u32 = 1129; // == ACK_VALUE so the ACK is at snd_nxt

        b.iter_batched_ref(
            // Per-iteration setup. Rebuilds the conn AND re-pushes the
            // retrans entry, because the engine-side prune (in
            // production) drops cum-ACKed entries — and even though
            // that prune runs outside `dispatch`, the dispatch site
            // mutates `snd_una`, the RTT estimator, and the WRITABLE
            // hysteresis bit, so the per-iter rebuild gives every
            // measured call the same starting state. Matches the
            // pattern used by `bench_tcp_input_data_segment` above.
            || {
                let mut conn = make_est_conn(1000, 5000, 1024, Some(200), false);
                // Bump snd_nxt past snd_una so the ACK lands in the
                // valid (snd_una, snd_nxt] window. After the ACK
                // advances snd_una to 1129, in_flight goes to 0 —
                // well below the 128 KiB hysteresis threshold.
                conn.snd_nxt = SND_NXT;
                // Latch the WRITABLE-hysteresis bit so the dispatch
                // site's conditional body runs (one boolean read +
                // a `wrapping_sub` + a u32 compare + a store).
                conn.send_refused_pending = true;
                // Pre-seed one in-flight entry. `Mbuf::from_ptr` with
                // a null pointer is sufficient — `dispatch` reads
                // `seq`, `len`, `xmit_ts_ns`, `sacked`, `lost` from
                // the entry but never dereferences `mbuf`. The
                // `prune_below_into_mbufs` that does dereference
                // (via DPDK's pktmbuf_free) runs engine-side after
                // `dispatch` returns.
                conn.snd_retrans.push_after_tx(RetransEntry {
                    seq: RETRANS_SEQ,
                    len: RETRANS_LEN,
                    mbuf: Mbuf::from_ptr(std::ptr::null_mut()),
                    first_tx_ts_ns: 0,
                    xmit_count: 1,
                    sacked: false,
                    lost: false,
                    xmit_ts_ns: 0,
                    hdrs_len: 0,
                });
                conn
            },
            |c| {
                let seg = ParsedSegment {
                    src_port: 5000,
                    dst_port: 40000,
                    seq: 5001, // == rcv_nxt: recv side stays put.
                    ack: ACK_VALUE, // 1129 > snd_una=1001: left edge advances by 128.
                    flags: TCP_ACK,
                    window: 65535,
                    header_len: 20 + opts_len,
                    payload: &[], // pure ACK.
                    options: &opts_buf[..opts_len],
                };
                let out = dispatch(
                    black_box(c),
                    black_box(&seg),
                    &DEFAULT_RTT_HISTOGRAM_EDGES_US,
                    TEST_SEND_BUF_BYTES,
                    // No MbufInsertCtx — pure ACK has no payload to insert.
                    None,
                );
                black_box(out);
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
    targets = bench_tcp_input_data_segment, bench_tcp_input_ooo_segment, bench_tcp_input_pure_ack
}
criterion_main!(benches);
