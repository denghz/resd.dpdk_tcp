//! bench-micro::parse_options — RX TCP-options decode cost.
//!
//! `tcp_options::parse_options` (crates/dpdk-net-core/src/tcp_options.rs:270)
//! decodes the TCP-options byte region on every received TCP segment that
//! carries options. On the steady-state RX data path this is called from
//! `tcp_input::dispatch_established` at tcp_input.rs:803 (and again from
//! the SYN-handling and post-handshake paths at engine.rs:4599,
//! tcp_input.rs:531 / 609); the steady-state data path is the high-frequency
//! call site.
//!
//! These benches measure the pure-function parse cost across three
//! production-representative option blobs. Blobs are constructed via
//! `TcpOpts::encode` at setup time (outside the timed region) so they are
//! byte-perfect to what the wire encoder emits.
//!
//! # Blob construction
//!
//! All three blobs are built via `TcpOpts::encode` at setup time (outside
//! the timed region). For TS-only and TS+SACK the encoder emits the
//! Linux-canonical NOP+NOP+TS leading shape (per RFC 7323 Appendix A);
//! for SYN-shape the encoder emits MSS + NOP+WS + SACKP + TS — the
//! corpus-compat WSCALE-second order our stack deliberately picks per
//! `AD-A8.5-tx-wscale-position` (modern Linux emits WSCALE last instead,
//! see tcp_options.rs:5-12). Encoder source is at tcp_options.rs:148-228.
//!
//! Note: `parse_options` has three straight-line fast-paths. PO2 added
//! two 12-byte TS-only ones: the NOP-first
//! `[NOP, NOP, OPT_TIMESTAMP, 10, tsval4, tsecr4]` (RFC 7323 Appendix A's
//! recommended layout, the shape Linux peers and our own encoder emit
//! on steady-state data ACKs, so `bench_parse_options_ts_only` hits it)
//! and the TS-first `[OPT_TIMESTAMP, 10, tsval4, tsecr4, NOP, NOP]`
//! (covers peers that emit TS before the NOP padding).
//!
//! PO5 added the NOP-first TS+SACK fast-path for lengths 24/32/40 —
//! shape `[NOP, NOP, OPT_TIMESTAMP, 10, tsval4, tsecr4, NOP, NOP,
//! OPT_SACK, 2+8*N, ...N×(left4, right4)...]` for N ∈ {1, 2, 3} SACK
//! blocks — which is the Linux-canonical ACK shape during loss recovery /
//! reordering. `bench_parse_options_ts_sack` (32 bytes, N=2) now hits
//! that fast-path's straight-line decode and the bench number drops
//! accordingly. The SYN-shape bench still measures the general
//! state-machine loop (20 bytes, non-canonical option mix).
//!
//! # Variants
//!
//! * `bench_parse_options_ts_only` — 12-byte canonical TS-only ACK
//!   buffer as `NOP, NOP, OPT_TIMESTAMP, 10, tsval4, tsecr4`. Hits the
//!   PO2 NOP-first fast-path in `parse_options` (a length + 4-byte
//!   prefix check, then a straight-line two-`u32::from_be_bytes` decode,
//!   no state-machine loop).
//! * `bench_parse_options_ts_sack` — TS option + a 2-block SACK option,
//!   the shape the encoder emits when an ACK reports a single reorder:
//!   `NOP+NOP+TS(12) + NOP+NOP+SACK_hdr+2*8` = 32 bytes (see the
//!   tcp_options.rs:441-443 round-trip test). Exercises the TS + SACK
//!   branches (tcp_options.rs:357-386) including 2 `u32::from_be_bytes`
//!   decodes per SACK block.
//! * `bench_parse_options_mss_ws_ts_sackperm` — full SYN/SYN-ACK options
//!   shape: MSS + WS + SACK-permitted + TS, encoded canonically as
//!   `MSS(4) + NOP+WS(4) + SACKP(2) + TS(10)` = 20 bytes (see
//!   tcp_options.rs:412-413 round-trip test). Exercises every supported
//!   option-kind branch of the state-machine loop on a single call.
//!
//! # Scope caveats
//!
//! * Pure-function bench. The production call sites
//!   (tcp_input.rs:531 / 609 / 803, engine.rs:4599) pay
//!   `ParsedSegment::options` slicing + caller-side `match` on the
//!   `Result` + downstream PAWS/SACK use of the parsed values on top of
//!   the parse cost; none of that is measured here.
//! * `parse_options` is `#[inline]` (tcp_options.rs:269); when invoked
//!   from the bench's outer closure the inlining boundary is the closure,
//!   not the production `tcp_input::dispatch_*` caller — register
//!   allocation, branch layout, and surrounding-code interleaving may
//!   differ from the inlined production hotspot.
//! * Numbers are CPU / turbo-state / power-governor dependent. Reported
//!   as "what this host measured for this option blob", not as a
//!   universal cost.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::tcp_options::{parse_options, SackBlock, TcpOpts};
use std::time::{Duration, Instant};

/// Batching factor for `iter_custom`. The smallest variant (TS-only
/// fast-path) is expected to be well under 30 ns/call, where criterion's
/// per-iter closure-call + sample-bookkeeping overhead can dominate. Calling
/// the workload `BATCH` times inside one closure invocation, then dividing
/// the total elapsed by `BATCH` before returning, amortizes that fixed
/// cost. Mirrors the BATCH=128 choice in `bench_tsc_read_*`.
const BATCH: u64 = 128;

/// Run `parse_options(blob)` `BATCH` times per closure invocation, XOR-folding
/// the parsed Timestamps + every decoded SACK block's `left`/`right` seqs +
/// MSS/WS into a single accumulator so LLVM cannot DCE the parse. `black_box`
/// is applied to the input slice on every call (preventing constant-folding
/// the parse output) and to the accumulator at end-of-batch (preventing
/// dead-store elimination of the accumulated parse results).
///
/// Why per-block fold (not just `sack_block_count`): `parse_options` decodes
/// each block's `u32 left` + `u32 right` via `u32::from_be_bytes`
/// (tcp_options.rs:371-383) and writes them into `opts.sack_blocks[..]`. The
/// function is `#[inline]` (tcp_options.rs:269), so LLVM can inline it into
/// this batch loop; if we fold only `sack_block_count`, the optimizer can
/// prove the per-block seq values are never read and elide the block-decode
/// work, under-reporting the real RX-path cost. Folding every block's
/// seqs forces the decode loop to actually be observed.
fn bench_parse_blob(c: &mut Criterion, name: &str, blob: &[u8]) {
    c.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    // black_box(blob) prevents the optimizer from
                    // partially-evaluating `parse_options` against the
                    // setup-time-known blob bytes.
                    let r = parse_options(black_box(blob));
                    match r {
                        Ok(opts) => {
                            // Fold the parse result into `acc`. Folding
                            // every decoded SACK block's `left`/`right`
                            // forces LLVM to keep the per-block decode
                            // work (tcp_options.rs:371-383); folding
                            // `timestamps` + `mss` + `wscale` covers the
                            // other branches of the state-machine loop.
                            if let Some((tsval, tsecr)) = opts.timestamps {
                                acc ^= (tsval as u64) ^ ((tsecr as u64) << 32);
                            }
                            let n = opts.sack_block_count as usize;
                            acc ^= n as u64;
                            for b in &opts.sack_blocks[..n] {
                                acc ^= (b.left as u64) ^ ((b.right as u64) << 32);
                            }
                            acc ^= opts.mss.unwrap_or(0) as u64;
                            acc ^= opts.wscale.unwrap_or(0) as u64;
                        }
                        Err(_) => {
                            // Production-shape blobs must parse cleanly;
                            // any error path here is a bench setup bug.
                            // Fold a sentinel so the panic-on-error doesn't
                            // get DCE'd into the success path either.
                            panic!("parse_options failed on production-shape blob");
                        }
                    }
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

fn bench_parse_options_ts_only(c: &mut Criterion) {
    // Linux-canonical TS-only ACK buffer: encoder emits NOP+NOP+TS(10)
    // = 12 bytes when only `timestamps` is set (tcp_options.rs:124-127).
    // This is the RFC 7323 Appendix A recommendation (leading NOPs for
    // 4-byte alignment) and is what every modern Linux peer emits on
    // steady-state data ACKs.
    //
    // This shape hits the PO2 NOP-first fast-path in `parse_options`:
    // `opts.len() == 12 && opts[0..4] == [NOP, NOP, OPT_TIMESTAMP, 10]`
    // → straight-line decode of tsval/tsecr, no state-machine loop. (The
    // other 12-byte fast-path, `[OPT_TIMESTAMP, 10, ..., NOP, NOP]`,
    // covers TS-first peers and is not exercised here.) This bench
    // therefore measures the fast-path cost on the canonical Linux-peer /
    // own-encoder TS-only shape.
    let opts = TcpOpts {
        timestamps: Some((0x0000_1000, 0xCAFE_BABE)),
        ..Default::default()
    };
    let mut buf = [0u8; 40];
    let n = opts.encode(&mut buf).expect("encode TS-only");
    assert_eq!(n, 12, "TS-only canonical blob must be 12 bytes");
    bench_parse_blob(c, "bench_parse_options_ts_only", &buf[..n]);
}

fn bench_parse_options_ts_sack(c: &mut Criterion) {
    // TS + 2 SACK blocks: encoder emits NOP+NOP+TS(10) + NOP+NOP+SACK_hdr+2*8
    // = 12 + 4 + 16 = 32 bytes (see tcp_options.rs:441-443). Per PO5, this
    // 32-byte NOP-first shape now hits the TS+SACK straight-line fast-path
    // in `parse_options` (a length + 8-byte prefix check, then a straight-
    // line three-pair u32 decode for tsval/tsecr + both SACK blocks, no
    // state-machine loop).
    let mut opts = TcpOpts {
        timestamps: Some((0x1122_3344, 0x5566_7788)),
        ..Default::default()
    };
    opts.push_sack_block(SackBlock { left: 1000, right: 2000 });
    opts.push_sack_block(SackBlock { left: 3000, right: 4000 });
    let mut buf = [0u8; 40];
    let n = opts.encode(&mut buf).expect("encode TS + 2-block SACK");
    assert_eq!(n, 32, "TS + 2-block SACK blob must be 32 bytes");
    bench_parse_blob(c, "bench_parse_options_ts_sack", &buf[..n]);
}

fn bench_parse_options_mss_ws_ts_sackperm(c: &mut Criterion) {
    // Our encoder's SYN-shape (MSS + WS + SACK-permitted + TS) encoded as
    // MSS(4) + NOP+WS(4) + SACKP(2) + TS(10) = 20 bytes. Note: this is the
    // corpus-compat WSCALE-second order our stack deliberately emits (see
    // tcp_options.rs:5-12); modern Linux `tcp_options_write` emits
    // MSS + SACK_PERMITTED + TS + NOP+WS (WSCALE last) instead. Both
    // orderings are RFC 9293 §3.2 compliant; we choose WSCALE-second to
    // match the shivansh + ligurio packetdrill corpus byte-for-byte
    // (tcp_options.rs:412-413 unit test).
    //
    // Why this bench measures comparably to (or faster than) the TS+SACK
    // variant despite having more options: SYN-shape decodes are all
    // fixed-size single-value arms (MSS/WS/SACK_PERMITTED/TS at
    // tcp_options.rs:310-355), while the TS+SACK bench's SACK arm runs a
    // per-block u32-pair decode loop (tcp_options.rs:357-385) over 2
    // blocks plus a NOP+NOP pre-pad. Per-byte the SACK loop costs more
    // cycles than the SYN-shape's straight-line single-value branches,
    // so the SYN-shape's larger byte count does not necessarily translate
    // into a higher parse cost.
    let opts = TcpOpts {
        mss: Some(1460),
        wscale: Some(7),
        sack_permitted: true,
        timestamps: Some((0xDEAD_BEEF, 0x0000_0000)),
        ..Default::default()
    };
    let mut buf = [0u8; 40];
    let n = opts.encode(&mut buf).expect("encode full SYN-shape");
    assert_eq!(n, 20, "SYN-shape blob must be 20 bytes");
    bench_parse_blob(c, "bench_parse_options_mss_ws_ts_sackperm", &buf[..n]);
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets =
        bench_parse_options_ts_only,
        bench_parse_options_ts_sack,
        bench_parse_options_mss_ws_ts_sackperm
}
criterion_main!(benches);
