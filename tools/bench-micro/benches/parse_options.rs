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
//! All three blobs are built via `TcpOpts::encode` at setup time
//! (outside the timed region). That is the same byte-for-byte shape our
//! own peer (and Linux kernel) emits on the wire: Linux canonical order
//! per `net/ipv4/tcp_output.c::tcp_options_write`. Encoder source is at
//! tcp_options.rs:148-228.
//!
//! Note: the T9-H7 fast-path inside `parse_options`
//! (tcp_options.rs:274-288) matches a different 12-byte shape —
//! `[OPT_TIMESTAMP, 10, tsval4, tsecr4, NOP, NOP]` (TS first, trailing
//! NOPs). The encoder emits `NOP, NOP, OPT_TIMESTAMP, 10, tsval4, tsecr4`
//! (leading NOPs), so the fast-path's `opts[0] == OPT_TIMESTAMP` check
//! does NOT fire on encoder-produced blobs (or on Linux-peer-produced
//! ACKs, which use the same NOP+NOP+TS order). These benches therefore
//! measure the **general state-machine loop**, not the fast-path. If a
//! production peer (e.g. some BSD or appliance stack) emits TS-first
//! with trailing NOPs, the fast-path will apply for that traffic and
//! the cost is lower than what this bench reports; the encoder shape is
//! the right reference for our own peer + Linux interop.
//!
//! # Variants
//!
//! * `bench_parse_options_ts_only` — 12-byte canonical TS-only ACK
//!   buffer as `NOP, NOP, OPT_TIMESTAMP, 10, tsval4, tsecr4`. Exercises
//!   the state-machine loop's NOP-skip path twice plus the TIMESTAMP
//!   branch (see tcp_options.rs:294-298 + tcp_options.rs:339-356).
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
/// the parsed Timestamps + SACK-block-count into a single accumulator so
/// LLVM cannot DCE the parse. `black_box` is applied to the input slice on
/// every call (preventing constant-folding the parse output) and to the
/// accumulator at end-of-batch (preventing dead-store elimination of the
/// accumulated parse results).
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
                            // Fold the parse result into `acc`. Including
                            // `sack_block_count` covers the SACK-bearing
                            // variants; the TS fold covers both TS-bearing
                            // and TS-absent (None → 0) variants.
                            if let Some((tsval, tsecr)) = opts.timestamps {
                                acc ^= (tsval as u64) ^ ((tsecr as u64) << 32);
                            }
                            acc ^= opts.sack_block_count as u64;
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
    // Canonical TS-only ACK buffer: encoder emits NOP+NOP+TS(10) = 12 bytes
    // when only `timestamps` is set (tcp_options.rs:124-127, also the
    // T9-H7 fast-path shape at tcp_options.rs:271-288).
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
    // = 12 + 4 + 16 = 32 bytes (see tcp_options.rs:441-443).
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
    // Full SYN/SYN-ACK shape: MSS + WS + SACK-permitted + TS encoded as
    // MSS(4) + NOP+WS(4) + SACKP(2) + TS(10) = 20 bytes (Linux canonical;
    // see tcp_options.rs:412-413 unit test).
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
