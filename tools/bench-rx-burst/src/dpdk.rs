//! DUT-side dpdk_net RX-burst measurement.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. Drives the peer's
//! `burst-echo-server` over a single dpdk_net TCP connection: per
//! bucket (W, N) sends `BURST N W\n`, reads N segments of W bytes
//! back-to-back, captures `wall_ns()` (CLOCK_REALTIME) per chunk
//! arrival; coalesced segments share a chunk's recv timestamp.
//! Computes `latency_ns = dut_recv_ns - peer_send_ns`.
//!
//! # Threading
//!
//! Single thread. Reuses the engine's main `poll_once()` loop driven
//! synchronously by this module — no dedicated poll thread, mirroring
//! bench-rtt and bench-tx-burst's dpdk arms.
//!
//! # Header parsing
//!
//! Each Readable event reports a `total_len` covering 1+ segments
//! (the engine coalesces in-order data into one event). We copy the
//! actual bytes out of the conn's `readable_scratch_iovecs` per event
//! and append to a per-burst `recv_buf`, then drain in W-byte chunks
//! and parse the header from each. See [`segment::parse_burst_chunk`].
//!
//! # Engine event/scratch model
//!
//! The engine emits one `Readable` event per `deliver_readable` call,
//! and `deliver_readable` is invoked at most once per RX-frame dispatch
//! that advances `rcv_nxt`. The per-conn `readable_scratch_iovecs` is
//! cleared at the start of every `deliver_readable` invocation AND at
//! the top of every `poll_once`, so any pre-existing scratch state is
//! invalidated before the next emit. Two consequences:
//!
//! 1. If the bench calls `engine.poll_once()` while a `Readable` event
//!    from a prior poll is still in the event queue, that event's
//!    `total_len` no longer corresponds to anything in the conn's
//!    scratch — the prior poll's iovec entries were cleared at the top
//!    of the new poll. The bytes are still "delivered" from TCP's
//!    perspective (rcv_nxt has advanced) but no longer accessible via
//!    the scratch. This module therefore drains events immediately
//!    after every `poll_once` to avoid leaving stale Readables in the
//!    queue across a poll boundary.
//!
//! 2. If two `deliver_readable` calls fire inside the same poll (e.g.
//!    two TCP segments arrive in one RX burst), only the LAST call's
//!    iovecs survive in the scratch when the bench reads it — the
//!    first call's scratch was cleared by the second's prelude. The
//!    bench reports both events' `total_len` in the engine-delivered
//!    accumulator but only the latest event's bytes are accessible via
//!    `readable_scratch_iovecs`. Missing bytes mean missing segments
//!    in the recorded CDF; the per-bucket sample count reflects the
//!    truth (`run.samples.len()` ≤ `burst_count × measure_bursts`).
//!
//! # Clock anchor
//!
//! Both `peer_send_ns` (segment header) and `dut_recv_ns` (this
//! module) are `CLOCK_REALTIME` ns since the Unix epoch — anchored on
//! the same wall clock. NTP offset (~100 µs on AWS same-AZ) bounds
//! the absolute cross-host latency reading; the distribution shape
//! (p50/p99 spread) is what we report.
//!
//! We deliberately do NOT use `dpdk_net_core::clock::now_ns()` here —
//! that's TSC-based, anchored on calibration start, and not
//! comparable to the peer's wall clock. Phase 9 c7i HW-TS will
//! tighten the cross-host bound below the NTP floor.

use std::time::{Duration, Instant};

use anyhow::Context;

use bench_rtt::workload::open_connection;
use dpdk_net_core::engine::Engine;
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

use crate::segment::{parse_burst_chunk, SegmentRecord};

/// Forward-progress watchdog for one burst's RX phase. If no segment
/// is received for this long, fail the bucket. 60s is generous on a
/// healthy peer (per-burst at 100 Gbps drains in <1 ms); the deadline
/// is here for the wedged-peer case where the operator wants a
/// visible failure.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// One bucket's run-time configuration. Caller owns `engine` + `conn`.
pub struct DpdkRxBurstCfg<'a> {
    pub engine: &'a Engine,
    pub conn: ConnHandle,
    /// Bucket index — propagates into `SegmentRecord.bucket_id`.
    pub bucket_id: u32,
    /// Bytes per segment (must be ≥ 16 to fit the header).
    pub segment_size: usize,
    /// Segments per burst.
    pub burst_count: usize,
    /// Warmup bursts (discarded).
    pub warmup_bursts: u64,
    /// Measurement bursts (recorded).
    pub measure_bursts: u64,
}

/// One bucket's measurement product. One `SegmentRecord` per
/// (burst, segment) tuple over `measure_bursts`. Each measurement
/// burst contributes `burst_count` records, so total len is
/// `measure_bursts * burst_count` (warmup excluded).
pub struct DpdkRxBurstRun {
    pub samples: Vec<SegmentRecord>,
}

/// Output of one drain pass: the bytes pulled out of the conn's
/// `readable_scratch_iovecs` plus the `Readable` events' aggregate
/// `total_len` since the last drain.
///
/// `engine_delivered` is the engine's count of bytes delivered to this
/// conn (rcv_nxt advance) summed across all `Readable` events popped
/// in this call. `bytes` is what we can actually read out of the
/// current scratch state. In the well-behaved case (one Readable per
/// poll, drain immediately after), `engine_delivered == bytes.len()`.
/// They diverge when multiple `deliver_readable` calls fired inside a
/// single poll (only the last call's iovecs survive in the scratch),
/// or when a `Readable` from a prior poll's queue had its scratch
/// invalidated by a subsequent `poll_once`'s top-of-poll cleanup. In
/// both cases the bytes are TCP-delivered (engine-counted) but no
/// longer accessible to the consumer; bench-rx-burst tracks
/// `engine_delivered` for forward-progress accounting and parses what
/// it has in `bytes` — missing bytes become missing segments in the
/// recorded CDF.
#[derive(Debug, Default)]
struct DrainOutcome {
    bytes: Vec<u8>,
    engine_delivered: u64,
}

/// Drive one (W, N) bucket. Sends `warmup_bursts + measure_bursts`
/// `BURST N W\n` commands; the peer ships each burst's segments
/// back-to-back; we drain Readable events, parse headers, and
/// timestamp on delivery.
pub fn run_bucket(cfg: &DpdkRxBurstCfg<'_>) -> anyhow::Result<DpdkRxBurstRun> {
    if cfg.segment_size < 16 {
        anyhow::bail!(
            "dpdk_rx_burst: segment_size ({}) must be ≥ 16 (header size)",
            cfg.segment_size
        );
    }
    if cfg.burst_count == 0 {
        anyhow::bail!("dpdk_rx_burst: burst_count must be ≥ 1");
    }

    let mut samples: Vec<SegmentRecord> =
        Vec::with_capacity((cfg.measure_bursts as usize) * cfg.burst_count);

    // Warmup — discard records.
    for i in 0..cfg.warmup_bursts {
        let _records = run_one_burst(cfg, /* burst_idx */ i, /* record */ false)
            .with_context(|| format!("warmup burst {i}"))?;
    }

    // Measurement — record per-segment latency.
    for i in 0..cfg.measure_bursts {
        let records = run_one_burst(cfg, i, true)
            .with_context(|| format!("measurement burst {i}"))?;
        samples.extend(records);
    }

    Ok(DpdkRxBurstRun { samples })
}

/// Send one `BURST N W\n` command and drain `N * W` bytes of
/// response. Returns the per-segment records when `record == true`,
/// else an empty Vec.
fn run_one_burst(
    cfg: &DpdkRxBurstCfg<'_>,
    burst_idx: u64,
    record: bool,
) -> anyhow::Result<Vec<SegmentRecord>> {
    let cmd = format!("BURST {} {}\n", cfg.burst_count, cfg.segment_size);
    // `send_command` returns any `Readable` bytes that arrived during
    // its trailing `poll_once` (peer-responded-fast case). Those bytes
    // would otherwise be orphaned by the next `poll_once`'s top-of-
    // poll scratch cleanup — see the module docstring's "engine
    // event/scratch model" section.
    let early = send_command(cfg.engine, cfg.conn, cmd.as_bytes())?;

    // Drain N*W bytes; for each chunk that completes a multiple of
    // W bytes, parse headers and stamp `dut_recv_ns`.
    let total = cfg.burst_count * cfg.segment_size;
    let mut recv_buf: Vec<u8> = Vec::with_capacity(total);
    let mut records: Vec<SegmentRecord> = if record {
        Vec::with_capacity(cfg.burst_count)
    } else {
        Vec::new()
    };
    let mut next_seg_idx: u64 = 0;
    let mut last_progress = Instant::now();
    // Track engine-reported delivered-bytes total (sum of `total_len`
    // across all popped `Readable` events). The loop exits when this
    // reaches `total` — using only `recv_buf.len()` would stall the
    // bench whenever a multi-`deliver_readable`-per-poll event lost
    // its earlier scratch state (the bytes are still TCP-delivered;
    // the bench just can't read them).
    let mut engine_delivered_total: u64 = 0;

    // Process the trailing bytes drained inside `send_command` first
    // — they're already accounted for as engine-delivered, so the
    // loop's exit condition only needs the new chunks added below.
    if !early.bytes.is_empty() || early.engine_delivered > 0 {
        consume_chunk_into_buf(
            &early,
            wall_ns(),
            total,
            cfg.segment_size,
            cfg.bucket_id,
            burst_idx,
            record,
            &mut recv_buf,
            &mut records,
            &mut next_seg_idx,
            &mut engine_delivered_total,
        );
        if engine_delivered_total > 0 {
            last_progress = Instant::now();
        }
    }

    while engine_delivered_total < total as u64 {
        cfg.engine.poll_once();
        let chunk = drain_readable_bytes(cfg.engine, cfg.conn)?;
        if chunk.bytes.is_empty() && chunk.engine_delivered == 0 {
            if last_progress.elapsed() >= STALL_TIMEOUT {
                anyhow::bail!(
                    "dpdk_rx_burst: burst {} stalled at {}/{} bytes received \
                     (no forward progress in {:?})",
                    burst_idx,
                    engine_delivered_total,
                    total,
                    STALL_TIMEOUT
                );
            }
            continue;
        }

        let dut_recv_ns = wall_ns();
        last_progress = Instant::now();

        consume_chunk_into_buf(
            &chunk,
            dut_recv_ns,
            total,
            cfg.segment_size,
            cfg.bucket_id,
            burst_idx,
            record,
            &mut recv_buf,
            &mut records,
            &mut next_seg_idx,
            &mut engine_delivered_total,
        );
    }

    Ok(records)
}

/// Append `chunk.bytes` into `recv_buf` (capped at `total`), update
/// the engine-delivered accumulator, and (when `record` is true) parse
/// any newly-complete W-byte segments into per-segment records.
///
/// Pulled out of `run_one_burst` so the bytes returned by
/// `send_command`'s trailing drain can share the same processing pipe
/// as the bytes drained inside the main `poll_once` loop. Pure
/// w.r.t. the engine; the test suite exercises it with synthetic
/// `DrainOutcome` inputs.
#[allow(clippy::too_many_arguments)]
fn consume_chunk_into_buf(
    chunk: &DrainOutcome,
    dut_recv_ns: u64,
    total: usize,
    segment_size: usize,
    bucket_id: u32,
    burst_idx: u64,
    record: bool,
    recv_buf: &mut Vec<u8>,
    records: &mut Vec<SegmentRecord>,
    next_seg_idx: &mut u64,
    engine_delivered_total: &mut u64,
) {
    *engine_delivered_total = engine_delivered_total.saturating_add(chunk.engine_delivered);

    // Append fresh bytes; bound recv_buf to `total` (extra bytes
    // shouldn't occur in this protocol, but defensive).
    let want = total.saturating_sub(recv_buf.len());
    let take = chunk.bytes.len().min(want);
    recv_buf.extend_from_slice(&chunk.bytes[..take]);

    if !record {
        return;
    }

    // Parse complete W-byte segments out of the buffer prefix.
    // We track `next_seg_idx` (vs. parsing once at end) so the
    // record's `dut_recv_ns` reflects the moment we observed the
    // segment, not the moment the burst completed.
    //
    // `seq_idx` comes from the segment header, NOT from
    // `next_seg_idx`'s positional count — so when the engine loses
    // bytes mid-burst (multi-event-per-poll, see module docstring),
    // the records we DO emit still carry the correct sender-side
    // sequence index. `next_seg_idx` just bounds the parse cursor
    // forward in `recv_buf` to avoid re-emitting the same record on
    // each iteration.
    let parsed = parse_burst_chunk(recv_buf, segment_size);
    while *next_seg_idx < parsed.len() as u64 {
        let (seq_idx, peer_send_ns) = parsed[*next_seg_idx as usize];
        records.push(SegmentRecord::new(
            bucket_id,
            burst_idx,
            seq_idx,
            peer_send_ns,
            dut_recv_ns,
        ));
        *next_seg_idx += 1;
    }
}

/// Send a small ASCII command (e.g. `BURST 16 64\n`) over the
/// established connection. Loops on partial accept; drives `poll_once`
/// + drains Readable events between attempts so back-pressure clears.
///
/// Returns any bytes drained during the trailing `poll_once`. The peer
/// may begin shipping the burst before this function returns (small
/// commands fit in one TCP segment + TCP_NODELAY on the peer means
/// it acks-and-replies immediately), so the trailing poll often
/// surfaces a `Readable` event for the conn. Those bytes are returned
/// to the caller rather than discarded so `run_one_burst` can fold
/// them into `recv_buf` before the next `poll_once` invalidates the
/// scratch.
fn send_command(
    engine: &Engine,
    conn: ConnHandle,
    bytes: &[u8],
) -> anyhow::Result<DrainOutcome> {
    let mut sent: usize = 0;
    let deadline = Instant::now() + STALL_TIMEOUT;
    let mut early = DrainOutcome::default();
    while sent < bytes.len() {
        let remaining = &bytes[sent..];
        match engine.send_bytes(conn, remaining) {
            Ok(n) => sent += n as usize,
            Err(e) => anyhow::bail!("send_bytes failed for control command: {e:?}"),
        }
        if sent < bytes.len() {
            engine.poll_once();
            // Capture any incoming bytes that arrived while waiting —
            // shouldn't happen mid-command but the engine's scratch
            // is per-conn and unconditional, so if we leave them in
            // the queue the next `poll_once` will invalidate their
            // backing iovecs.
            let mid = drain_readable_bytes(engine, conn)?;
            early.bytes.extend_from_slice(&mid.bytes);
            early.engine_delivered = early
                .engine_delivered
                .saturating_add(mid.engine_delivered);
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "control-command send stalled at {}/{} bytes",
                    sent,
                    bytes.len()
                );
            }
        }
    }
    // One final poll to push the command onto the TX ring. Drain any
    // `Readable` events emitted during that poll BEFORE returning, so
    // the caller's next `poll_once` (top-of-poll scratch cleanup)
    // doesn't invalidate the iovecs the queued events point into.
    // The captured bytes flow back to `run_one_burst` via the
    // `DrainOutcome` return.
    engine.poll_once();
    let tail = drain_readable_bytes(engine, conn)?;
    early.bytes.extend_from_slice(&tail.bytes);
    early.engine_delivered = early
        .engine_delivered
        .saturating_add(tail.engine_delivered);
    Ok(early)
}

/// Pop all pending events for `conn`; for each `Readable`, copy the
/// referenced bytes out of the conn's `readable_scratch_iovecs` and
/// concatenate. Surfaces `Closed` / `Error` events as anyhow errors.
///
/// Returns `(bytes_from_scratch, engine_delivered_total)`. Under the
/// well-behaved one-Readable-per-poll path the two match exactly;
/// when they diverge (multi-`deliver_readable`-per-poll or stale
/// queued events whose scratch was invalidated by an interleaved
/// `poll_once`), `engine_delivered_total` reports the engine's
/// rcv_nxt advance and `bytes_from_scratch` reports what's actually
/// readable. The caller uses the former for forward-progress
/// accounting and the latter for header parsing — see the module
/// docstring's "engine event/scratch model" section.
fn drain_readable_bytes(engine: &Engine, conn: ConnHandle) -> anyhow::Result<DrainOutcome> {
    let mut events = engine.events();
    let mut engine_delivered: u64 = 0;
    while let Some(ev) = events.pop() {
        match ev {
            InternalEvent::Readable {
                conn: ch,
                total_len,
                ..
            } if ch == conn => {
                engine_delivered = engine_delivered.saturating_add(total_len as u64);
            }
            InternalEvent::Error { conn: ch, err, .. } if ch == conn => {
                anyhow::bail!("tcp error during recv: errno={err}");
            }
            InternalEvent::Closed { conn: ch, err, .. } if ch == conn => {
                anyhow::bail!("connection closed during recv: err={err}");
            }
            _ => {
                // Unrelated — drop.
            }
        }
    }
    drop(events);

    if engine_delivered == 0 {
        return Ok(DrainOutcome::default());
    }

    // Copy iovec bytes out of the conn's readable_scratch_iovecs. The
    // scratch holds the segments for the LATEST `deliver_readable`
    // call on this conn; earlier emits in the same poll, or in any
    // prior poll, had their scratch overwritten before we reached
    // this drain.
    //
    // The scratch lives until the NEXT `poll_once`, so reading it
    // here (between poll calls) is safe.
    let mut out: Vec<u8> = Vec::new();
    let ft = engine.flow_table();
    if let Some(c) = ft.get(conn) {
        for iv in &c.readable_scratch_iovecs {
            if iv.base.is_null() || iv.len == 0 {
                continue;
            }
            // SAFETY: the iovec's `base` points into an mbuf that
            // dpdk_net pinned for the duration of this poll cycle.
            // We read it before the next `poll_once` clears the
            // scratch.
            let slice = unsafe { std::slice::from_raw_parts(iv.base, iv.len as usize) };
            out.extend_from_slice(slice);
        }
    }
    drop(ft);

    Ok(DrainOutcome {
        bytes: out,
        engine_delivered,
    })
}

/// Open a single dpdk_net connection to the peer's burst-echo-server
/// control port. Wraps `bench_rtt::workload::open_connection` so the
/// call site doesn't have to know about that dependency shape.
pub fn open_control_connection(
    engine: &Engine,
    peer_ip_host_order: u32,
    peer_control_port: u16,
) -> anyhow::Result<ConnHandle> {
    open_connection(engine, peer_ip_host_order, peer_control_port)
        .context("dpdk_rx_burst open_connection")
}

/// CLOCK_REALTIME ns reading — the DUT-side wall-clock anchor used
/// to subtract `peer_send_ns` (also CLOCK_REALTIME) from. Same shape
/// as the linux arm's `wall_ns` so cross-stack rows in the same CSV
/// share a clock namespace.
fn wall_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Vec<u8>` representing one W-byte segment with the
    /// `[be64 seq_idx | be64 peer_send_ns]` header in the first 16
    /// bytes and zero-pad in the remainder. Mirrors the wire shape
    /// `burst-echo-server.c::send_burst` emits.
    fn build_segment(seq_idx: u64, peer_send_ns: u64, w: usize) -> Vec<u8> {
        assert!(w >= 16);
        let mut out = vec![0u8; w];
        out[..8].copy_from_slice(&seq_idx.to_be_bytes());
        out[8..16].copy_from_slice(&peer_send_ns.to_be_bytes());
        out
    }

    /// Concat `n` segments at indices `start..start+n` into one Vec.
    fn build_segments(start: u64, n: u64, w: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity((n as usize) * w);
        for i in 0..n {
            out.extend_from_slice(&build_segment(start + i, (start + i) * 1_000, w));
        }
        out
    }

    /// Happy path: a single chunk holds the full burst. Verifies the
    /// well-behaved single-Readable-per-poll case still produces one
    /// record per segment with the seq_idx read from the header.
    #[test]
    fn consume_chunk_into_buf_single_full_chunk_records_all_segments() {
        let n: u64 = 16;
        let w: usize = 64;
        let total = (n as usize) * w;
        let chunk = DrainOutcome {
            bytes: build_segments(0, n, w),
            engine_delivered: total as u64,
        };

        let mut recv_buf = Vec::new();
        let mut records = Vec::new();
        let mut next_seg_idx: u64 = 0;
        let mut engine_delivered_total: u64 = 0;

        consume_chunk_into_buf(
            &chunk,
            /* dut_recv_ns */ 12_345,
            total,
            w,
            /* bucket_id */ 0,
            /* burst_idx */ 0,
            /* record */ true,
            &mut recv_buf,
            &mut records,
            &mut next_seg_idx,
            &mut engine_delivered_total,
        );

        assert_eq!(engine_delivered_total, total as u64);
        assert_eq!(recv_buf.len(), total);
        assert_eq!(records.len(), n as usize);
        for (i, rec) in records.iter().enumerate() {
            assert_eq!(rec.seg_idx, i as u64);
            assert_eq!(rec.peer_send_ns, (i as u64) * 1_000);
            assert_eq!(rec.dut_recv_ns, 12_345);
        }
    }

    /// Bug reproduction: simulate the live-verified failure pattern
    /// (sum=960, scratch=832, mismatch=128) directly through the
    /// chunk-processing pipeline.
    ///
    /// Before the fix, `drain_readable_bytes` bailed out with the
    /// "Readable total_len sum (960) does not match scratch bytes
    /// (832)" error whenever two `Readable` events were pending and
    /// the first event's scratch had been clobbered. The bench grid
    /// could never complete a single bucket. After the fix the bench
    /// surfaces `engine_delivered` (the engine's rcv_nxt count) and
    /// `bytes` (what's accessible) separately; the loop's exit
    /// condition uses `engine_delivered_total` so missing-byte
    /// scenarios advance toward completion instead of stalling.
    ///
    /// The test feeds two chunks mirroring the production trace:
    ///   chunk 1: engine_delivered=960, bytes=832  (segs 2..14)
    ///   chunk 2: engine_delivered=64,  bytes=64   (seg 15)
    ///
    /// Total engine-delivered: 1024 (== burst). Recorded records:
    /// 14 (segs 2..15, the ones whose bytes weren't clobbered). The
    /// fact that we record FEWER than 16 reflects engine-level data
    /// loss truthfully — the bench doesn't fabricate latency samples
    /// for segments it never observed.
    #[test]
    fn consume_chunk_into_buf_handles_engine_delivered_exceeding_scratch_bytes() {
        let n_burst: u64 = 16;
        let w: usize = 64;
        let total = (n_burst as usize) * w;

        // The bench observes scratch bytes only for the LATEST emit
        // — so the FIRST 128B (segs 0..1) are lost. Scratch holds
        // segs 2..14 (13 segments × 64B = 832B).
        let lost_bytes = 128u64;
        let chunk1 = DrainOutcome {
            bytes: build_segments(2, 13, w),
            engine_delivered: lost_bytes + 13 * w as u64, // 128 + 832 = 960
        };
        // A second drain pass picks up the 16th segment in a later
        // poll (its scratch is intact, single Readable, no
        // clobbering).
        let chunk2 = DrainOutcome {
            bytes: build_segments(15, 1, w),
            engine_delivered: w as u64,
        };

        let mut recv_buf = Vec::new();
        let mut records = Vec::new();
        let mut next_seg_idx: u64 = 0;
        let mut engine_delivered_total: u64 = 0;

        consume_chunk_into_buf(
            &chunk1,
            /* dut_recv_ns */ 1_000_000,
            total,
            w,
            0,
            0,
            true,
            &mut recv_buf,
            &mut records,
            &mut next_seg_idx,
            &mut engine_delivered_total,
        );
        consume_chunk_into_buf(
            &chunk2,
            /* dut_recv_ns */ 2_000_000,
            total,
            w,
            0,
            0,
            true,
            &mut recv_buf,
            &mut records,
            &mut next_seg_idx,
            &mut engine_delivered_total,
        );

        // Forward-progress accounting reaches `total` even though
        // recv_buf is short by `lost_bytes` — this is what unsticks
        // the bench loop's `while engine_delivered_total < total`.
        assert_eq!(engine_delivered_total, total as u64);
        assert_eq!(recv_buf.len(), total - lost_bytes as usize);

        // Records carry the SENDER-side seq_idx from each header, so
        // the recorded sample set reflects truthfully which segments
        // the bench actually observed.
        assert_eq!(records.len(), 14);
        let observed_seq_idx: Vec<u64> = records.iter().map(|r| r.seg_idx).collect();
        let expected_seq_idx: Vec<u64> = (2..=15).collect();
        assert_eq!(observed_seq_idx, expected_seq_idx);

        // The first 13 records share `dut_recv_ns` (chunk 1's
        // observation moment); the 14th carries chunk 2's
        // timestamp. Mirrors the linux arm's "coalesced read shares
        // a recv timestamp" semantics — coalescing here happens at
        // the engine layer instead of at the read syscall.
        for rec in &records[..13] {
            assert_eq!(rec.dut_recv_ns, 1_000_000);
        }
        assert_eq!(records[13].dut_recv_ns, 2_000_000);
    }

    /// Forward-progress under pure data loss: an entire poll's worth
    /// of bytes are engine-delivered but with zero accessible scratch
    /// (e.g. a Readable event sat in the queue across a `poll_once`
    /// boundary that cleared its iovecs without any fresh emit on the
    /// next poll). Without `engine_delivered` accounting, the loop's
    /// `recv_buf.len() < total` exit condition would never advance
    /// and the bench would stall for 60s on STALL_TIMEOUT.
    #[test]
    fn consume_chunk_into_buf_advances_progress_when_bytes_empty_but_delivered_nonzero() {
        let w: usize = 64;
        let total: usize = 4 * w;
        // 128B engine-delivered, zero scratch bytes accessible.
        let stale = DrainOutcome {
            bytes: Vec::new(),
            engine_delivered: 2 * w as u64,
        };
        let mut recv_buf = Vec::new();
        let mut records = Vec::new();
        let mut next_seg_idx: u64 = 0;
        let mut engine_delivered_total: u64 = 0;

        consume_chunk_into_buf(
            &stale,
            /* dut_recv_ns */ 0,
            total,
            w,
            0,
            0,
            /* record */ true,
            &mut recv_buf,
            &mut records,
            &mut next_seg_idx,
            &mut engine_delivered_total,
        );

        assert_eq!(engine_delivered_total, 2 * w as u64);
        assert!(recv_buf.is_empty());
        assert!(records.is_empty());
    }

    /// Non-record (warmup) path still advances the engine-delivered
    /// counter so warmup bursts terminate. Mirrors the measurement-
    /// path test above; `record == false` means no SegmentRecords get
    /// produced but the loop's exit condition must still fire.
    #[test]
    fn consume_chunk_into_buf_advances_progress_in_warmup_record_off_path() {
        let n: u64 = 4;
        let w: usize = 64;
        let total = (n as usize) * w;
        let chunk = DrainOutcome {
            bytes: build_segments(0, n, w),
            engine_delivered: total as u64,
        };
        let mut recv_buf = Vec::new();
        let mut records = Vec::new();
        let mut next_seg_idx: u64 = 0;
        let mut engine_delivered_total: u64 = 0;

        consume_chunk_into_buf(
            &chunk,
            /* dut_recv_ns */ 0,
            total,
            w,
            0,
            0,
            /* record */ false,
            &mut recv_buf,
            &mut records,
            &mut next_seg_idx,
            &mut engine_delivered_total,
        );

        assert_eq!(engine_delivered_total, total as u64);
        assert_eq!(recv_buf.len(), total);
        assert!(
            records.is_empty(),
            "warmup path must not emit SegmentRecords"
        );
        assert_eq!(
            next_seg_idx, 0,
            "next_seg_idx only advances on the record=true path"
        );
    }
}
