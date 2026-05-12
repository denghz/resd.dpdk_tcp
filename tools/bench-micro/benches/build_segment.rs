//! bench-micro::build_segment — TX header + checksum construction cost.
//!
//! `tcp_output::build_segment` (crates/dpdk-net-core/src/tcp_output.rs:43)
//! constructs the full Ethernet + IPv4 + TCP frame, including the IPv4
//! header checksum and the TCP checksum (pseudo-header + TCP header +
//! payload fold) on every transmitted segment. In production it is invoked
//! once per segment from `Engine::send_bytes` at engine.rs:6050 (verified
//! by T1 round-2 spot-check); it also runs once per control frame from
//! the ACK/RST/SYN/FIN emitters elsewhere in engine.rs. This bench is the
//! TX-side companion to T2's `bench_parse_options` (RX-side TCP-options
//! decode); both functions run on every packet in production.
//!
//! T1's `bench_send_*` benches measure the entire per-segment build-loop
//! body — `build_segment` is one step inside, bundled with mbuf-alloc,
//! frame-scratch resize, retrans push, RTO arm, counter ops, and the
//! `copy_nonoverlapping` into the mbuf data region. This bench isolates
//! `build_segment` itself so the checksum-fold cost is visible as a
//! separate number across three payload shapes plus an isolated
//! pseudo-header-only measurement.
//!
//! # Variants
//!
//! * `bench_build_segment_bare_ack` — ACK only (no payload), TS options
//!   (12 B). Header-only frame, total ~66 B (14 Eth + 20 IPv4 + 32 TCP
//!   incl. 12 B options). Represents a bare ACK or window update.
//! * `bench_build_segment_data_64b` — PSH+ACK with 64 B payload + TS opts.
//!   Total ~130 B. Represents a small REST/WS request payload.
//! * `bench_build_segment_data_mss` — PSH+ACK with 1460 B payload (MSS)
//!   plus TS opts. Total ~1526 B. Surfaces the per-byte cost (TCP
//!   payload checksum fold + the `copy_from_slice` into `out`);
//!   representative of a full-MSS data segment in a bulk burst.
//! * `bench_pseudo_header_checksum` — isolates
//!   `tcp_output::tcp_pseudo_header_checksum` (tcp_output.rs:223). Pure
//!   compute, no allocation; ~5 cycles target. Measured with
//!   `iter_custom` + BATCH=128 (same pattern as `bench_tsc_read_*` and
//!   `bench_parse_options`) since the workload is sub-10 ns per call.
//!
//! # What this bench measures vs. what production callers pay
//!
//! This bench measures `build_segment` in isolation: a pre-built
//! `SegmentTx` literal is reused across iterations and the function is
//! invoked against a stack/heap `out` buffer. Production callers pay
//! additional per-segment costs that are NOT in this number:
//!
//!   * `tx_frame_scratch` `RefCell::borrow_mut` to access the
//!     engine-owned reusable frame buffer.
//!   * `SegmentTx { .. }` struct construction with the per-segment
//!     `seq`, `ack`, `payload` slice and a freshly-encoded
//!     `TcpOpts` value.
//!   * `frame.clear()` + `frame.resize(needed, 0)` to right-size the
//!     scratch.
//!   * Downstream the mbuf alloc, `copy_nonoverlapping`, refcount bump,
//!     and TX-burst submission.
//!
//! T1's `bench_send_*` measures the whole bundle; this bench peels off
//! the `build_segment` step on its own. T2's `bench_parse_options`
//! measures the inverse RX-decode side. Neither this bench nor T2
//! exercises the production-call-site inlining boundary — `build_segment`
//! is invoked through a `Vec`/`Box`-backed `out` buffer here, not the
//! engine-owned `tx_frame_scratch` `RefCell`, so register allocation and
//! surrounding-code interleaving may differ from the production hotspot.
//!
//! # `black_box` discipline
//!
//! For the three frame-building variants we XOR-fold every byte of
//! `out[..n]` into an accumulator after each call (same approach as T1's
//! `run_segment_build_loop`). Without the full-buffer fold, LLVM can
//! observe that the only consumer of `out` is a 3-byte read for the
//! accumulator and reduce the per-byte checksum + memcpy work to just
//! those 3 bytes — which under-reports the dominant cost at MSS-sized
//! payloads. The full-byte fold adds ~0.5 cycles/byte (~140 ns at 5 GHz
//! for the 1526 B MSS frame, ~7 ns for the 66 B bare ACK), and is
//! uniform across variants so the cross-variant delta still tracks the
//! actual `build_segment` cost. The fold is per-frame, so smaller frames
//! pay proportionally less overhead.
//!
//! For `bench_pseudo_header_checksum` we XOR-fold the returned `u16`
//! values across BATCH=128 calls and `black_box` the accumulator once
//! per batch (mirroring `bench_tsc_read_*`).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::{
    build_segment, tcp_pseudo_header_checksum, SegmentTx, FRAME_HDRS_MIN, TCP_ACK, TCP_PSH,
};
use std::time::{Duration, Instant};

/// Batching factor for the pseudo-header-only bench. At sub-10 ns per
/// call, criterion's per-iter closure-call + sample-bookkeeping overhead
/// can dominate the measured cost. Mirrors the `bench_tsc_read_*` and
/// `bench_parse_options` choice of 128.
const BATCH: u64 = 128;

/// MSS used by the data-mss variant — matches the production
/// `EngineConfig::default().tcp_mss` (engine.rs ~line 576) and the
/// `bench-micro::send` constant of the same name. If the production
/// MSS default changes, update here too.
const MSS_PAYLOAD: usize = 1460;

const SRC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const DST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const SRC_IP: u32 = 0x0a_00_00_02;
const DST_IP: u32 = 0x0a_00_00_01;
const SRC_PORT: u16 = 40000;
const DST_PORT: u16 = 5000;
const SEQ: u32 = 1001;
const ACK: u32 = 5001;
const WINDOW: u16 = 1024;

/// Build the canonical TS-only options blob the engine emits on every
/// established-state segment (RFC 7323 §3 MUST-22). Same shape T1's
/// helper uses; ts_recent/ts_val values are arbitrary fixed sentinels.
fn ts_opts() -> TcpOpts {
    TcpOpts {
        timestamps: Some((0x0000_1000, 0xCAFE_BABE)),
        ..Default::default()
    }
}

/// Run `build_segment(&seg, &mut out)` once and XOR-fold every byte of
/// the written region into an accumulator. The accumulator is returned
/// so the caller can `black_box` it; folding every byte (not just a
/// few offsets) keeps the per-byte payload checksum + frame-write work
/// observable to LLVM. Cost is ~0.5 cycles/byte at 5 GHz (~7 ns for
/// the 66 B bare ACK, ~140 ns for the 1526 B MSS frame).
#[inline(always)]
fn build_and_fold(seg: &SegmentTx, out: &mut [u8]) -> u64 {
    let n = build_segment(black_box(seg), out).expect("bench setup sized out correctly");
    let mut acc: u64 = n as u64;
    for &b in &out[..n] {
        acc ^= b as u64;
    }
    acc
}

/// Bare ACK with TS options — no payload. Total frame: 14 B Ethernet,
/// 20 B IPv4, 32 B TCP (20 B header + 12 B TS) = 66 bytes on the wire.
/// Production sees this shape for pure ACKs, window updates, and
/// zero-window probes.
fn bench_build_segment_bare_ack(c: &mut Criterion) {
    c.bench_function("bench_build_segment_bare_ack", |b| {
        let opts = ts_opts();
        // Bench-setup assertion: confirm the encoded options byte count
        // matches the documented 12-byte canonical TS-only blob so the
        // out-buffer sizing below is correct; otherwise build_segment
        // would silently truncate.
        assert_eq!(opts.encoded_len(), 12, "TS-only options must be 12 bytes");
        let payload: &[u8] = &[];
        let seg = SegmentTx {
            src_mac: SRC_MAC,
            dst_mac: DST_MAC,
            src_ip: SRC_IP,
            dst_ip: DST_IP,
            src_port: SRC_PORT,
            dst_port: DST_PORT,
            seq: SEQ,
            ack: ACK,
            flags: TCP_ACK,
            window: WINDOW,
            options: opts,
            payload,
        };
        // FRAME_HDRS_MIN(54) + 40-byte option cushion + 0 payload = 94 B,
        // matching the production scratch sizing at engine.rs:6043.
        let mut out: Vec<u8> = vec![0u8; FRAME_HDRS_MIN + 40 + payload.len()];
        // Pre-flight: invoke once outside the timed region to confirm
        // the bench inputs produce a valid frame (the `expect` in
        // `build_and_fold` would otherwise fire on the first timed iter).
        let _ = build_segment(&seg, &mut out).expect("bench setup must succeed");

        b.iter(|| {
            let acc = build_and_fold(&seg, &mut out);
            black_box(acc);
        });
    });
}

/// PSH+ACK with 64 B payload + TS options. Total frame: 14 B Ethernet,
/// 20 B IPv4, 32 B TCP, 64 B payload = 130 bytes. Representative of a
/// small REST/WS request or an interactive trading-command write.
fn bench_build_segment_data_64b(c: &mut Criterion) {
    c.bench_function("bench_build_segment_data_64b", |b| {
        let opts = ts_opts();
        assert_eq!(opts.encoded_len(), 12, "TS-only options must be 12 bytes");
        let payload_buf = [0x42u8; 64];
        let seg = SegmentTx {
            src_mac: SRC_MAC,
            dst_mac: DST_MAC,
            src_ip: SRC_IP,
            dst_ip: DST_IP,
            src_port: SRC_PORT,
            dst_port: DST_PORT,
            seq: SEQ,
            ack: ACK,
            flags: TCP_PSH | TCP_ACK,
            window: WINDOW,
            options: opts,
            payload: &payload_buf,
        };
        let mut out: Vec<u8> = vec![0u8; FRAME_HDRS_MIN + 40 + payload_buf.len()];
        let _ = build_segment(&seg, &mut out).expect("bench setup must succeed");

        b.iter(|| {
            let acc = build_and_fold(&seg, &mut out);
            black_box(acc);
        });
    });
}

/// PSH+ACK with 1460 B payload (MSS) + TS options. Total frame: 14 B
/// Ethernet, 20 B IPv4, 32 B TCP, 1460 B payload = 1526 bytes. The
/// per-byte work on this shape (TCP checksum fold over the payload,
/// the `copy_from_slice` of payload bytes into `out`, plus the
/// `build_and_fold` accumulator's XOR over `out[..n]`) is what
/// distinguishes its cost from the smaller variants; this bench cannot
/// itself attribute the cost split among those three byte-loops.
fn bench_build_segment_data_mss(c: &mut Criterion) {
    c.bench_function("bench_build_segment_data_mss", |b| {
        let opts = ts_opts();
        assert_eq!(opts.encoded_len(), 12, "TS-only options must be 12 bytes");
        // Non-zero payload bytes so the TCP checksum has actual work to
        // fold (a slice of zeros would fold to zero and the optimizer
        // might shortcut the loop; non-zero bytes match the production
        // shape of trading-message bytes).
        let payload_buf = vec![0x42u8; MSS_PAYLOAD];
        let seg = SegmentTx {
            src_mac: SRC_MAC,
            dst_mac: DST_MAC,
            src_ip: SRC_IP,
            dst_ip: DST_IP,
            src_port: SRC_PORT,
            dst_port: DST_PORT,
            seq: SEQ,
            ack: ACK,
            flags: TCP_PSH | TCP_ACK,
            window: WINDOW,
            options: opts,
            payload: &payload_buf,
        };
        let mut out: Vec<u8> = vec![0u8; FRAME_HDRS_MIN + 40 + payload_buf.len()];
        let _ = build_segment(&seg, &mut out).expect("bench setup must succeed");

        b.iter(|| {
            let acc = build_and_fold(&seg, &mut out);
            black_box(acc);
        });
    });
}

/// Pseudo-header-only checksum: the 12-byte fold over src_ip + dst_ip +
/// proto + tcp_seg_len. Used by the A-HW TX-offload path
/// (tcp_output.rs:223) where the PMD folds in the TCP header + payload
/// at wire time. Pure compute, no allocation; sub-10 ns target.
///
/// `iter_custom` + BATCH=128 amortizes criterion's per-iter overhead
/// (same pattern as `bench_tsc_read_*` and `bench_parse_options`). The
/// `tcp_seg_len` input is `black_box`-fenced inside the loop so LLVM
/// cannot constant-fold the pseudo-header bytes ahead of time. The
/// returned `u16` values are XOR-folded into an accumulator that is
/// `black_box`'d once per batch so the per-call result has an
/// observable consumer.
fn bench_pseudo_header_checksum(c: &mut Criterion) {
    c.bench_function("bench_pseudo_header_checksum", |b| {
        // Pseudo-header tcp_seg_len = TCP header (20) + payload bytes;
        // 1480 corresponds to a 20-byte-header MSS-sized segment, a
        // realistic production input.
        let tcp_seg_len: u32 = 1480;
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    let v = tcp_pseudo_header_checksum(
                        black_box(SRC_IP),
                        black_box(DST_IP),
                        black_box(tcp_seg_len),
                    );
                    acc ^= v as u64;
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
    targets =
        bench_build_segment_bare_ack,
        bench_build_segment_data_64b,
        bench_build_segment_data_mss,
        bench_pseudo_header_checksum,
}
criterion_main!(benches);
