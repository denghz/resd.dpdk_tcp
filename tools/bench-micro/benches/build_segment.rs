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
//! # T1/T3 numerical bridge
//!
//! T3's `bench_build_segment_data_mss` (~833 ns observed) measures
//! `build_segment` alone for a 1526-byte MSS frame. T1's
//! `bench_send_large_segment_build_warm` (~5.4 µs / 6 segments
//! ≈ 900 ns/seg observed) bundles the same `build_segment` call plus
//! the per-segment overheads listed below. The ~50-150 ns gap between
//! T3-alone and T1-per-segment is consistent with:
//!   * `SendRetrans::push_after_tx` entry construction + Vec push
//!     (engine.rs ~6155-6172).
//!   * `Counters` atomic bumps for `eth.tx_bytes` and `tcp.tx_data`
//!     (engine.rs ~6131-6132).
//!   * `FlowTable::get_mut(handle)` lookup mirroring the production
//!     `flow_table.borrow_mut() + get_mut(handle)` pattern
//!     (engine.rs ~6167-6170) — ~30-50 ns at sub-128-flow scale.
//!   * `SegmentTx` struct construction with a fresh `TcpOpts` each
//!     segment (see "What this bench measures vs. what production
//!     callers pay" below for the encode-on-every-call note).
//!
//! So when looking at T3 alone vs. T1 large-warm, attribute the
//! delta to the bundled retrans/counter/flow-table overhead, not to
//! a discrepancy in the underlying build_segment cost.
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
//!   compute, no allocation. Measured ~7-8 ns on this host (~35-40
//!   cycles at 5 GHz). The work is: build a 12-byte stack
//!   pseudo-header buffer (src_ip || dst_ip || zero || proto ||
//!   tcp_seg_len) then call `internet_checksum` to fold it — a
//!   multi-load + 16-bit ones-complement fold loop, not a 5-instruction
//!   sequence. Measured with `iter_custom` + BATCH=128 (same pattern as
//!   `bench_tsc_read_*` and `bench_parse_options`) since the workload
//!   is sub-10 ns per call and criterion's per-iter overhead would
//!   otherwise dominate.
//!
//! # What this bench measures vs. what production callers pay
//!
//! This bench measures `build_segment` in isolation: a pre-built
//! `SegmentTx` literal is reused across iterations and the function is
//! invoked against a stack/heap `out` buffer. Note that `SegmentTx`
//! owns a `TcpOpts` struct (NOT a pre-encoded options blob); the
//! options are reconstructed by setup, but `build_segment` calls
//! `seg.options.encode(...)` into the on-the-wire bytes on every
//! invocation (tcp_output.rs:157). This is faithful to production,
//! where each segment in `Engine::send_bytes` builds a fresh
//! `SegmentTx` literal that bundles a freshly-constructed `TcpOpts`
//! and `build_segment` runs its `encode` on every segment. So this
//! bench DOES pay the per-call options encode; what it does NOT
//! exercise are the additional per-segment costs production pays:
//!
//!   * `SegmentTx { .. }` struct construction (per-segment
//!     `seq`, `ack`, `payload` slice + a freshly-constructed
//!     `TcpOpts` value built from the conn's TS state).
//!   * Downstream the mbuf alloc, `shim_rte_pktmbuf_append` (whose
//!     returned `dst` is now the `&mut [u8]` build_segment writes
//!     into directly post-PO10), refcount bump, and TX-burst submission.
//!
//! T1's `bench_send_*` measures the whole bundle; this bench peels off
//! the `build_segment` step on its own. T2's `bench_parse_options`
//! measures the inverse RX-decode side. Neither this bench nor T2
//! exercises the production-call-site inlining boundary — `build_segment`
//! is invoked through a `Vec`/`Box`-backed `out` buffer here, not the
//! mbuf-data `&mut [u8]` returned by `rte_pktmbuf_append` (PO10), so
//! register allocation and surrounding-code interleaving may differ from
//! the production hotspot.
//!
//! Concretely: `build_segment` is declared `pub fn` with NO `#[inline]`
//! attribute (tcp_output.rs:43). The production hot-path call site lives
//! in the same crate (`dpdk-net-core`) at engine.rs:6050 inside
//! `Engine::send_bytes`. This bench imports the function cross-crate
//! (see `use dpdk_net_core::tcp_output::build_segment;` below). The
//! cross-crate import means the inlining boundary is fat-LTO-dependent:
//! the workspace root Cargo.toml sets `[profile.release] lto = "fat"`,
//! `codegen-units = 1` (release profile that bench harnesses inherit),
//! which lets LLVM inline across crate boundaries at link time, so the
//! release-bench number should match the production same-crate cost.
//! Without fat LTO (e.g. a debug bench, or thin-LTO release), the
//! production call site may inline `build_segment` while this bench's
//! cross-crate call would not — the numbers reported here assume fat
//! LTO is on (it is, per workspace Cargo.toml).
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
    build_segment, build_segment_offload, tcp_pseudo_header_checksum, SegmentTx, FRAME_HDRS_MIN,
    TCP_ACK, TCP_PSH,
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

/// PO4: same as `build_and_fold` but invokes `build_segment_offload` —
/// the offload-active path skips the full TCP payload fold and writes
/// the pseudo-only TCP cksum + zero IPv4 cksum directly. Used by the
/// `_offload` bench variants to surface the per-byte savings.
#[inline(always)]
fn build_offload_and_fold(seg: &SegmentTx, out: &mut [u8]) -> u64 {
    let n =
        build_segment_offload(black_box(seg), out).expect("bench setup sized out correctly");
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

/// PO4 companion to `bench_build_segment_data_mss`: same SegmentTx
/// shape (PSH+ACK, TS opts, 1460 B payload, total 1526 B frame) but
/// calls `build_segment_offload` — the variant used on the
/// `tx_cksum_offload_active` path in `Engine::send_bytes`. The TCP
/// checksum field is set to the pseudo-header-only fold (no payload
/// fold) and the IPv4 cksum field is set to 0; both fields are
/// idempotently rewritten by `tx_offload_finalize` downstream, so the
/// final wire bytes are bit-identical to the
/// build_segment+tx_offload_finalize sequence (verified by
/// `build_segment_offload_wire_equivalent_*` unit tests in tcp_output.rs).
///
/// Expected delta vs. `bench_build_segment_data_mss`: the per-byte TCP
/// payload fold (~0.5 cycles/byte × 1460 B ≈ 250-400 ns at 5 GHz on
/// Zen4) is gone. The remaining cost is options encode, IPv4 header
/// pack, pseudo-header fold (~7-8 ns), the copy_from_slice of the
/// payload into `out`, and the `build_and_fold` per-byte XOR
/// accumulator (uniform across variants).
fn bench_build_segment_data_mss_offload(c: &mut Criterion) {
    c.bench_function("bench_build_segment_data_mss_offload", |b| {
        let opts = ts_opts();
        assert_eq!(opts.encoded_len(), 12, "TS-only options must be 12 bytes");
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
        let _ = build_segment_offload(&seg, &mut out).expect("bench setup must succeed");

        b.iter(|| {
            let acc = build_offload_and_fold(&seg, &mut out);
            black_box(acc);
        });
    });
}

/// Pseudo-header-only checksum: the 12-byte fold over src_ip + dst_ip +
/// proto + tcp_seg_len. Used by the A-HW TX-offload path
/// (tcp_output.rs:223) where the PMD folds in the TCP header + payload
/// at wire time. Pure compute, no allocation; observed sub-10 ns per
/// call on this host (~7-8 ns ≈ ~35-40 cycles at 5 GHz, see top-of-file
/// doc-comment for the cost breakdown). "Sub-10 ns" is the measurement,
/// not a target.
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
        bench_build_segment_data_mss_offload,
        bench_pseudo_header_checksum,
}
criterion_main!(benches);
