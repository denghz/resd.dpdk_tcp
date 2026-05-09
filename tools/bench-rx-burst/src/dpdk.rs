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
    send_command(cfg.engine, cfg.conn, cmd.as_bytes())?;

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

    while recv_buf.len() < total {
        cfg.engine.poll_once();
        let chunk = drain_readable_bytes(cfg.engine, cfg.conn)?;
        if chunk.is_empty() {
            if last_progress.elapsed() >= STALL_TIMEOUT {
                anyhow::bail!(
                    "dpdk_rx_burst: burst {} stalled at {}/{} bytes received \
                     (no forward progress in {:?})",
                    burst_idx,
                    recv_buf.len(),
                    total,
                    STALL_TIMEOUT
                );
            }
            continue;
        }

        let dut_recv_ns = wall_ns();
        last_progress = Instant::now();

        // Append fresh bytes; bound recv_buf to `total` (extra bytes
        // shouldn't occur in this protocol, but defensive).
        let want = total - recv_buf.len();
        let take = chunk.len().min(want);
        recv_buf.extend_from_slice(&chunk[..take]);

        if !record {
            continue;
        }

        // Parse complete W-byte segments out of the buffer prefix.
        // We track `next_seg_idx` (vs. parsing once at end) so the
        // record's `dut_recv_ns` reflects the moment we observed the
        // segment, not the moment the burst completed.
        let parsed = parse_burst_chunk(&recv_buf, cfg.segment_size);
        while next_seg_idx < parsed.len() as u64 {
            let (seq_idx, peer_send_ns) = parsed[next_seg_idx as usize];
            records.push(SegmentRecord::new(
                cfg.bucket_id,
                burst_idx,
                seq_idx,
                peer_send_ns,
                dut_recv_ns,
            ));
            next_seg_idx += 1;
        }
    }

    Ok(records)
}

/// Send a small ASCII command (e.g. `BURST 16 64\n`) over the
/// established connection. Loops on partial accept; drives `poll_once`
/// + drains Readable events between attempts so back-pressure clears.
fn send_command(engine: &Engine, conn: ConnHandle, bytes: &[u8]) -> anyhow::Result<()> {
    let mut sent: usize = 0;
    let deadline = Instant::now() + STALL_TIMEOUT;
    while sent < bytes.len() {
        let remaining = &bytes[sent..];
        match engine.send_bytes(conn, remaining) {
            Ok(n) => sent += n as usize,
            Err(e) => anyhow::bail!("send_bytes failed for control command: {e:?}"),
        }
        if sent < bytes.len() {
            engine.poll_once();
            // Drop any incoming bytes that arrived while waiting —
            // shouldn't happen mid-command, but defensive.
            let _ = drain_readable_bytes(engine, conn)?;
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "control-command send stalled at {}/{} bytes",
                    sent,
                    bytes.len()
                );
            }
        }
    }
    // One final poll to push the command onto the TX ring.
    engine.poll_once();
    Ok(())
}

/// Pop all pending events for `conn`; for each `Readable`, copy the
/// referenced bytes out of the conn's `readable_scratch_iovecs` and
/// concatenate. Surfaces `Closed` / `Error` events as anyhow errors.
fn drain_readable_bytes(engine: &Engine, conn: ConnHandle) -> anyhow::Result<Vec<u8>> {
    let mut events = engine.events();
    let mut readable_lens: Vec<u32> = Vec::new();
    while let Some(ev) = events.pop() {
        match ev {
            InternalEvent::Readable {
                conn: ch,
                total_len,
                ..
            } if ch == conn => {
                readable_lens.push(total_len);
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

    if readable_lens.is_empty() {
        return Ok(Vec::new());
    }

    // Copy iovec bytes out of the conn's readable_scratch_iovecs. The
    // scratch holds the segments for the LATEST emit on this conn;
    // top-of-next-poll clears it. Multiple Readable events in one
    // drain pass would, in principle, be ambiguous — but in practice
    // dpdk_net emits ONE Readable per `deliver_readable` call and
    // fresh delivery clears + repopulates the scratch, so a single
    // poll_once produces at most one scratch state for the conn.
    // We collapse all `total_len` we observed into one read of the
    // current scratch contents.
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

    // Cross-check: the sum of `total_len` across observed Readables
    // should match the bytes we pulled out of the scratch. If they
    // diverge, the engine's coalescing model has changed; fail loud.
    let sum: u64 = readable_lens.iter().map(|&l| l as u64).sum();
    if sum as usize != out.len() {
        anyhow::bail!(
            "Readable total_len sum ({}) does not match scratch bytes ({}); \
             engine event/scratch model may have changed",
            sum,
            out.len()
        );
    }

    Ok(out)
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
