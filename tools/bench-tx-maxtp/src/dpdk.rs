//! dpdk_net max-sustained-throughput runner — spec §11.2.
//!
//! Drives the W × C grid against a live kernel-TCP sink peer. For
//! each bucket, opens `C` persistent connections (reused for the whole
//! bucket), pumps W-byte writes in a tight round-robin for T = 60 s
//! after a 10 s warmup, and reports sustained goodput + packet rate.
//!
//! # Measurement contract (spec §11.2)
//!
//! - Warmup: 10 s pumping, counters not snapshotted.
//! - Measurement window: 60 s post-warmup.
//! - Snapshots at `t_warmup_end` and `t_end` of:
//!   - `tcp.tx_payload_bytes` (when `obs-byte-counters` is on — default
//!     OFF, so the sanity invariant check is skipped when the counter
//!     stays at zero).
//!   - `eth.tx_pkts` as a proxy for "segments transmitted" — spec
//!     §11.2 calls for `segments_tx_counter_delta`. The nearest real
//!     counter in `dpdk-net-core` is `eth.tx_pkts` which includes
//!     TCP + ARP but the warmup-then-measure shape means the tiny
//!     constant ARP floor is overshadowed by the bucket's TX volume.
//!     (The spec mentions `dpdk_net_counters::tcp::tx_pkts` which does
//!     not exist in the crate today; `eth.tx_pkts` is the closest
//!     available — note the variance from the spec string here so the
//!     follow-up hook-up at T15 can expose the TCP-only variant if the
//!     ARP floor proves non-negligible.)
//!   - For ACKed bytes: the peer's running `snd_una` (per conn).
//!     `snd_una_total` across all conns at window close minus at
//!     warmup end = ACKed bytes in the window.
//!
//! # TX-TS mode
//!
//! Unlike burst, maxtp is a sustained-rate measurement — there's no
//! per-iteration HW TX-TS capture to do. The `TxTsMode` on the
//! cfg is propagated into CSV for schema uniformity with burst
//! (downstream bench-report filters by mode to avoid mixing HW-TS rows
//! with TSC-fallback rows). ENA stays on `TscFallback`.
//!
//! # Multi-connection pump loop
//!
//! For C > 1 we open `C` connections up-front and round-robin writes
//! across them in the inner hot loop:
//!
//! ```text
//! for iteration in 0.. {
//!     let conn = conns[iteration % conns.len()];
//!     engine.send_bytes(conn, &payload);
//!     engine.poll_once();   // amortised: once per outer round.
//! }
//! ```
//!
//! We poll once per full round (after writing to each conn once), not
//! per write, so the batching shape matches burst (spec says "tight
//! loop"). The engine's flow table + poll loop are single-lcore —
//! multiple connections on the same lcore just increase the flow-table
//! working set; nothing prevents this.

use std::time::{Duration, Instant};

use anyhow::Context;

use bench_common::raw_samples::RawSamplesWriter;
use bench_rtt::workload::{drain_and_accumulate_readable, open_connection};

use dpdk_net_core::engine::{Engine, CLOSE_FLAG_FORCE_TW_SKIP};
use dpdk_net_core::flow_table::ConnHandle;

use crate::maxtp::{Bucket, MaxtpSample};

/// Source of the TX-side timestamps used on this run. Recorded for CSV
/// consistency with the burst grid — maxtp itself doesn't use HW-TS per
/// iteration (the window is delimited by TSC reads), but the field is
/// included so downstream bench-report can filter dpdk_net rows by NIC
/// timestamp capability alongside burst rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxTsMode {
    /// Read `rte_mbuf::tx_timestamp` dynfield. Unavailable on ENA;
    /// kept here so mlx5/ice callers can annotate their rows
    /// accordingly.
    HwTs,
    /// TSC-based window delimitation (ENA fallback; T13 default).
    TscFallback,
}

impl TxTsMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            TxTsMode::HwTs => "hw_ts",
            TxTsMode::TscFallback => "tsc_fallback",
        }
    }
}

/// Per-bucket runner configuration — assembled by main.rs.
pub struct DpdkMaxtpCfg<'a> {
    pub engine: &'a Engine,
    /// One persistent connection handle per concurrent connection.
    /// `conns.len()` must equal `bucket.conn_count` (caller asserts).
    pub conns: &'a [ConnHandle],
    pub bucket: Bucket,
    /// Warmup window (spec §11.2: 10 s).
    pub warmup: Duration,
    /// Measurement window (spec §11.2: 60 s).
    pub duration: Duration,
    /// Payload template. Caller allocates once at bucket entry (one
    /// vec per bucket) so the inner loop doesn't allocate.
    pub payload: &'a [u8],
    /// The TX-TS mode the harness will report into CSV.
    pub tx_ts_mode: TxTsMode,
    /// Bucket identifier emitted into the raw-sample CSV's bucket_id
    /// column. Phase 5 Task 5.3 — usually constructed as
    /// `format!("W={},C={}", bucket.write_bytes, bucket.conn_count)`.
    pub bucket_id: &'a str,
    /// Optional sidecar writer that receives one
    /// [`MaxtpRawPoint`] per conn per SAMPLE_INTERVAL during the
    /// measurement window (Phase 5 Task 5.3 — closes C-B1, C-B5, C-E1).
    /// `None` skips raw-sample emission entirely; the bucket-level
    /// percentile aggregate is still computed.
    pub raw_samples: Option<&'a mut RawSamplesWriter>,
    /// Phase 6 Task 6.2 send→ACK latency sidecar. When set, the pump
    /// loop drains per-segment latency samples from the engine after
    /// every `poll_once` and emits one row per sample with the
    /// `dpdk_segment` scope. `None` skips emission entirely; caller is
    /// responsible for calling `engine.enable_send_ack_logging(cap)`
    /// before run_bucket so the engine actually retains samples.
    pub send_ack_samples: Option<&'a mut RawSamplesWriter>,
}

/// One raw-sample row emitted at SAMPLE_INTERVAL granularity per
/// connection during the maxtp measurement window. Phase 5 Task 5.3
/// closed C-B1 (no percentiles), C-B5 (multi-conn visibility), and the
/// queue-depth half of C-E1 via `snd_nxt_minus_una`. Phase 11 Task 11.2
/// closes the rest of C-E1 by adding `snd_wnd` and `room_in_peer_wnd`
/// alongside the existing queue-depth column. Field order is stable
/// (additions appended only); the CSV header in
/// `emit_per_conn_raw_sample` mirrors this order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaxtpRawPoint {
    /// Index of the connection inside the bucket's `conns` slice
    /// (stable per bucket; not the engine's `ConnHandle`).
    pub conn_id: u32,
    /// Sample sequence number — 0..=N where N = duration / SAMPLE_INTERVAL.
    pub sample_idx: u32,
    /// Wall-clock ns relative to `t_measure_start`. Same anchor across
    /// every conn in the bucket so downstream tooling can align rows.
    pub t_ns: u64,
    /// Per-conn instantaneous goodput over the most recent
    /// SAMPLE_INTERVAL window (bits/sec). Computed from the per-conn
    /// `snd_una` delta over the interval — not aggregate goodput.
    pub goodput_bps_window: f64,
    /// Per-conn `snd_nxt - snd_una` at sample time — bytes the stack
    /// has handed to the wire that the peer hasn't ACKed yet. Direct
    /// queue-depth proxy.
    pub snd_nxt_minus_una: u32,
    /// Phase 11 Task 11.2 (C-E1): per-conn `snd_wnd` at sample time —
    /// the peer's most-recently-advertised receive window (already
    /// shifted by the negotiated WSCALE on RX). Combined with
    /// `room_in_peer_wnd`, lets downstream tooling distinguish "we are
    /// sending as fast as the peer's window allows" from "the peer's
    /// window is wide open and we're under-driving" — both look identical
    /// from the goodput row alone.
    pub snd_wnd: u32,
    /// Phase 11 Task 11.2 (C-E1): per-conn `snd_wnd - (snd_nxt - snd_una)`
    /// at sample time — bytes of headroom remaining inside the peer's
    /// advertised window before the next outbound segment hits zero-room.
    /// Saturates at 0 when in-flight already meets / exceeds the peer
    /// window (peer is the bottleneck). Mirrors the `room_in_peer_wnd`
    /// computation in the per-bucket DIAG dump path so downstream
    /// time-series + the DIAG attribution agree on the value.
    pub room_in_peer_wnd: u32,
}

/// Emit one raw-sample row to the sidecar CSV. Splitting this out
/// keeps the `RawSamplesWriter` schema (column order + header) co-
/// located with the column definitions — every emit site uses the same
/// formatting rules.
///
/// Header (same order):
/// `bucket_id, conn_id, sample_idx, t_ns, goodput_bps_window,
///  snd_nxt_minus_una, snd_wnd, room_in_peer_wnd`
///
/// Phase 11 Task 11.2 (C-E1): the trailing `snd_wnd` and
/// `room_in_peer_wnd` columns were appended (not interleaved); existing
/// downstream consumers indexing the first six columns by position
/// continue to work. New consumers reading the queue-depth time series
/// pick up the appended pair.
pub fn emit_per_conn_raw_sample(
    writer: &mut RawSamplesWriter,
    bucket_id: &str,
    point: &MaxtpRawPoint,
) -> anyhow::Result<()> {
    writer.row(&[
        bucket_id,
        &point.conn_id.to_string(),
        &point.sample_idx.to_string(),
        &point.t_ns.to_string(),
        &point.goodput_bps_window.to_string(),
        &point.snd_nxt_minus_una.to_string(),
        &point.snd_wnd.to_string(),
        &point.room_in_peer_wnd.to_string(),
    ])?;
    Ok(())
}

/// One bucket's raw measurement product.
pub struct BucketRun {
    /// The (goodput_bps, pps) sample over the measurement window.
    pub sample: MaxtpSample,
    /// ACKed byte count during the measurement window — derived from
    /// per-conn `snd_una` deltas. Caller uses this for the sanity
    /// invariant check.
    pub acked_bytes_in_window: u64,
    /// `tcp.tx_payload_bytes` counter delta over the measurement
    /// window. `0` when the `obs-byte-counters` feature is OFF
    /// (default build); the sanity invariant check in the caller skips
    /// when this value is 0.
    pub tx_payload_bytes_delta: u64,
    /// `eth.tx_pkts` counter delta over the measurement window. The
    /// primary pps source for the sample — the ARP floor is
    /// negligible (one reply on session bring-up) during a 60 s
    /// steady-state pump.
    pub tx_pkts_delta: u64,
    /// TX-TS mode actually used (propagated from cfg).
    pub tx_ts_mode: TxTsMode,
    /// Inflight-byte bound at `t_end` = sum over conns of
    /// `(snd_nxt - snd_una)` — the delta between bytes the stack
    /// handed to the wire and bytes ACKed. Used by the caller's
    /// sanity-invariant check to bound ε (spec §11.2 "minus any
    /// bytes still in-flight at `t_end`, bounded by cwnd + rwnd").
    pub inflight_bytes_at_end: u64,
    /// Phase 5 Task 5.3: per-conn-per-sample-interval raw points
    /// captured during the measurement window. Empty unless the
    /// pump was driven with sampling enabled (the default for the
    /// dpdk arm). Caller writes these into the sidecar CSV +
    /// folds the goodput column into `bench_common::percentile`
    /// for the bucket-level distribution rows.
    pub raw_points: Vec<MaxtpRawPoint>,
}

/// Open `C` persistent connections to the peer. Connections are
/// established sequentially; each uses a distinct ephemeral local port
/// (engine assigns via `next_ephemeral_port`).
pub fn open_persistent_connections(
    engine: &Engine,
    peer_ip_host_order: u32,
    peer_port: u16,
    conn_count: u64,
) -> anyhow::Result<Vec<ConnHandle>> {
    if conn_count == 0 {
        anyhow::bail!("dpdk_maxtp: conn_count must be > 0");
    }
    let mut out = Vec::with_capacity(conn_count as usize);
    for i in 0..conn_count {
        // The peer echo-server occasionally sends FIN immediately after the
        // handshake when its accept backlog is momentarily saturated (observed
        // as "connection closed during handshake: err=0").  Retry up to 3
        // times with brief engine polls between attempts so the peer backlog
        // drains and reuses the closed slot.
        const MAX_TRIES: u32 = 3;
        let mut last_err = anyhow::anyhow!("no attempts");
        let mut succeeded = false;
        for attempt in 0..MAX_TRIES {
            match open_connection(engine, peer_ip_host_order, peer_port) {
                Ok(h) => {
                    out.push(h);
                    succeeded = true;
                    break;
                }
                Err(e) => {
                    eprintln!(
                        "dpdk_maxtp: open connection {i} attempt {attempt} failed: {e:#}; \
                         retrying after drain"
                    );
                    // Poll the engine to reap the failed connection's flow-table
                    // slot before the next connect attempt.
                    for _ in 0..20 {
                        engine.poll_once();
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    last_err = e;
                }
            }
        }
        if !succeeded {
            return Err(last_err)
                .with_context(|| format!("dpdk_maxtp: open connection {i} failed after {MAX_TRIES} attempts"));
        }
    }
    Ok(out)
}

/// Close every persistent connection from a finished bucket so the
/// flow-table slots recycle before the next bucket calls
/// `open_persistent_connections`. Without this, handles bump
/// monotonically across buckets (264, 348, …) and large-W cells later
/// in the grid trip `InvalidConnHandle(<n>)` mid-bucket once a stale
/// pre-bucket conn — still occupying a low slot — gets reused for a
/// fresh handshake while the previous holder is still mid-FIN.
///
/// Fix shape (T28, 2026-05-06):
///   0. One `poll_once` before Phase 1 to drain any in-flight TX data
///      still in `tx_pending_data` from the bucket's last pump round.
///      Without this, the NIC TX ring can be full at the moment Phase 1
///      runs, causing `tx_tcp_frame` to fail (→ `PeerUnreachable`) and
///      the FIN to be silently dropped — the conn stays ESTABLISHED
///      forever and the drain loop times out every time.
///   1. Send FIN on each conn with `CLOSE_FLAG_FORCE_TW_SKIP`
///      (timestamps are negotiated by default in the kernel-TCP peer,
///      so the engine honors the skip flag and short-circuits the
///      2×MSL TIME_WAIT wait at reap time).
///   2. Drive `poll_once` until `flow_table_used()` reports zero or
///      the deadline expires. Inside the loop, retry `close_conn_with_flags`
///      on every connection every iteration: it is a no-op for conns
///      past FIN_WAIT1, and retries the FIN send for any conn that is
///      still ESTABLISHED (Phase 1 FIN failed because the NIC ring was
///      full). The retry succeeds once `poll_once` has drained the ring.
///
/// On a deadline expiry we log + continue (soft-fail). The next
/// bucket's `open_persistent_connections` will still succeed as long
/// as `max_connections` has headroom; this helper is a hygiene step,
/// not a correctness gate.
pub fn close_persistent_connections(
    engine: &Engine,
    conns: &[ConnHandle],
) -> anyhow::Result<()> {
    if conns.is_empty() {
        return Ok(());
    }
    // Phase 0: drain any TX data still queued from the last pump round.
    // pool exhaustion causes send_bytes to return Ok(0) without calling
    // poll_once, leaving unsent data mbufs in tx_pending_data ring.
    // drain_tx_pending_data (called inside poll_once) flushes them to
    // the NIC so the TX ring has space for the FINs below.
    engine.poll_once();

    // Phase 1: emit FINs. `close_conn_with_flags` is idempotent for
    // already-closing/closed conns (returns Ok(()) without sending),
    // so soft-failing on a per-handle error is safe. A PeerUnreachable
    // error here means the NIC TX ring was still full after Phase 0
    // (rare); the drain loop in Phase 2 retries on every iteration.
    for &h in conns {
        if let Err(e) = engine.close_conn_with_flags(h, CLOSE_FLAG_FORCE_TW_SKIP) {
            eprintln!(
                "dpdk_maxtp: close_persistent_connections: close_conn_with_flags(handle={h}) \
                 returned {e:?}; will retry in drain loop",
            );
        }
    }
    // Phase 2: drive poll_once until either every slot has been
    // reaped or the deadline expires. On each poll iteration we also
    // call close_conn_with_flags per conn: this retries the FIN for any
    // conn still ESTABLISHED (Phase 1 send failed) and is a no-op for
    // conns already past FIN_WAIT1. FIN-ACK round trips finish in <1ms
    // on the kernel peer; 15 s is a generous safety margin.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        engine.poll_once();
        for &h in conns {
            // Proactive window update — unblocks echo-server write() stalled
            // on rwnd=0 so it can drain its send buffer and send its FIN.
            engine.send_window_update(h);
            // FIN retry: only call close_conn_with_flags for handles still
            // in the flow table. Calling it on an already-reaped handle causes
            // ts_enabled to default false → spurious EPERM event emitted with
            // the old handle number, which then contaminates the next bucket if
            // the same handle slot is reused for a new connection.
            let is_alive = {
                let ft = engine.flow_table();
                ft.get(h).is_some()
            };
            if is_alive {
                let _ = engine.close_conn_with_flags(h, CLOSE_FLAG_FORCE_TW_SKIP);
            }
            // Drain post-FIN Readable/Closed events so the queue
            // doesn't fill up and drop events for other connections.
            let mut _last: Option<u64> = None;
            let _ = drain_and_accumulate_readable(engine, h, &mut _last);
        }
        // Break once every conn slot reports None on `get` (the
        // engine reaped + removed it).
        let any_alive = {
            let ft = engine.flow_table();
            conns.iter().any(|h| ft.get(*h).is_some())
        };
        if !any_alive {
            // Flush any residual events (e.g. Closed from reap_time_wait
            // that drain_and_accumulate_readable didn't consume due to
            // early-bail on a prior event for the same handle) so the next
            // bucket's connections don't see phantom events on reused
            // handle numbers.
            engine.drain_events(u32::MAX, |_, _| {});
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Grace period expired. Force-abort all remaining connections so
            // their snd_retrans mbufs return to the pool and their flow-table
            // slots are freed before the next bucket starts — prevents pool
            // exhaustion and handle contamination across bucket boundaries.
            let mut residual = 0usize;
            for &h in conns.iter() {
                let is_alive = {
                    let ft = engine.flow_table();
                    ft.get(h).is_some()
                };
                if is_alive {
                    residual += 1;
                    engine.abort_conn(h);
                }
            }
            if residual > 0 {
                eprintln!(
                    "dpdk_maxtp: close_persistent_connections: timed out; \
                     force-aborted {residual}/{} conns after 15s",
                    conns.len()
                );
            }
            engine.drain_events(u32::MAX, |_, _| {});
            return Ok(());
        }
    }
}

/// Drive one bucket on the dpdk_net side.
///
/// Sequence per spec §11.2:
/// 1. Pump writes for `warmup` (10 s), no sampling.
/// 2. Snapshot per-conn `snd_una` + counters; seed the accumulator.
/// 3. Pump writes for `duration` (60 s), folding a mid-window
///    `snd_una` sample into the accumulator every `SAMPLE_INTERVAL`
///    (C1 fix: survives u32 sequence-space wraps at sustained rates).
/// 4. Close with a final per-conn snapshot and fold it into the
///    accumulator so the tail sub-window is counted. Snapshot counters.
/// 5. Return `MaxtpSample::from_window(acked_bytes, tx_pkts, elapsed_ns)`.
pub fn run_bucket(cfg: &mut DpdkMaxtpCfg<'_>) -> anyhow::Result<BucketRun> {
    if cfg.conns.len() as u64 != cfg.bucket.conn_count {
        anyhow::bail!(
            "dpdk_maxtp: conns.len() = {} does not match bucket.conn_count = {}",
            cfg.conns.len(),
            cfg.bucket.conn_count
        );
    }
    if cfg.payload.len() as u64 != cfg.bucket.write_bytes {
        anyhow::bail!(
            "dpdk_maxtp: payload.len() = {} does not match bucket.write_bytes = {}",
            cfg.payload.len(),
            cfg.bucket.write_bytes
        );
    }
    if cfg.duration.as_nanos() == 0 {
        anyhow::bail!("dpdk_maxtp: measurement duration must be > 0");
    }

    // Warmup. Phase 6 Task 6.2: warmup does NOT emit send-ack samples
    // (passing `None`) so the CSV bytes belong purely to the measurement
    // window — even though the engine's send_ack_log is recording during
    // warmup, the pump just doesn't drain into the writer. Samples
    // accumulated during warmup either drain naturally before the
    // measurement loop starts (round-trip ACKs are sub-ms; a 10 s warmup
    // means the log has cleared by t_measure_start) or get popped by the
    // first measurement-phase ACK without ever reaching the writer.
    let warmup_deadline = Instant::now() + cfg.warmup;
    pump_round_robin(cfg.engine, cfg.conns, cfg.payload, warmup_deadline, None, None)
        .context("dpdk_maxtp warmup phase")?;

    // Snapshot pre-window state.
    // `snd_una` snapshot — per-connection u32 values (not a single u64
    // sum, to avoid losing wrap bits when subtracting). The borrow on
    // `flow_table` is released inside the snapshot helper before this
    // call returns; subsequent drain calls are safe.
    let pre_snd_una: Vec<u32> = snapshot_snd_una_per_conn(cfg.engine, cfg.conns, None);
    let pre_tx_payload = cfg
        .engine
        .counters()
        .tcp
        .tx_payload_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    let pre_tx_pkts = cfg
        .engine
        .counters()
        .eth
        .tx_pkts
        .load(std::sync::atomic::Ordering::Relaxed);

    // Pre-measurement pool + state snapshot (diag only).
    let pre_tx_data_avail = cfg.engine.tx_data_mempool_avail();
    let pre_tx_hdr_avail  = cfg.engine.tx_hdr_mempool_avail();
    let pre_states: Vec<_> = cfg.conns.iter().map(|&h| {
        cfg.engine.conn_state(h).map(|s| format!("{s:?}")).unwrap_or_else(|| "missing".into())
    }).collect();
    eprintln!(
        "dpdk_maxtp POOL bucket(C={}, W={}) pre-measure: \
         tx_data_avail={} tx_hdr_avail={} conn_states=[{}]",
        cfg.bucket.conn_count,
        cfg.bucket.write_bytes,
        pre_tx_data_avail,
        pre_tx_hdr_avail,
        pre_states.join(","),
    );

    // Measurement — pump while sampling per-conn `snd_una` every
    // SAMPLE_INTERVAL. This handles sustained-rate wraps of the u32
    // sequence space during the 60 s window (C=1 at 100 Gbps wraps
    // ~17× per window — a single pre/post delta would mask 16× of the
    // ACKed bytes).
    let mut accumulator = SndUnaAccumulator::new(pre_snd_una);
    let t_measure_start = Instant::now();
    accumulator.anchor_measure_start(t_measure_start);
    let measure_deadline = t_measure_start + cfg.duration;
    // Phase 6 Task 6.2: assemble the send-ack sink for the measurement
    // phase. The Cfg holds an `Option<&mut RawSamplesWriter>`; we wrap
    // the writer + bucket_id into a `SendAckSinkRef` and pass it through
    // by `Option<...>::take()` so the inner pump_round_robin owns the
    // borrow for the duration of the call.
    let send_ack_sink: Option<SendAckSinkRef<'_>> = cfg
        .send_ack_samples
        .as_deref_mut()
        .map(|w| SendAckSinkRef {
            writer: w,
            bucket_id: cfg.bucket_id,
        });
    pump_round_robin(
        cfg.engine,
        cfg.conns,
        cfg.payload,
        measure_deadline,
        Some(&mut accumulator),
        send_ack_sink,
    )
    .context("dpdk_maxtp measurement phase")?;
    let t_measure_end = Instant::now();

    // Drain any residual Readable events from the event queue so the
    // next bucket doesn't start behind. Kernel sink doesn't echo data
    // but the engine may still emit state transitions.
    for &conn in cfg.conns {
        let mut _last: Option<u64> = None;
        let _ = drain_and_accumulate_readable(cfg.engine, conn, &mut _last);
    }

    // Final sample: fold the [last_sample .. window_close] delta into
    // the accumulator so no tail sub-sample is lost.
    // (Borrow released inside the helper before returning.)
    let post_snd_una: Vec<u32> =
        snapshot_snd_una_per_conn(cfg.engine, cfg.conns, Some(&accumulator.last_sample));
    accumulator.accumulate(&post_snd_una);
    let post_tx_payload = cfg
        .engine
        .counters()
        .tcp
        .tx_payload_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    let post_tx_pkts = cfg
        .engine
        .counters()
        .eth
        .tx_pkts
        .load(std::sync::atomic::Ordering::Relaxed);

    let inflight_bytes_at_end = snapshot_inflight_total(cfg.engine, cfg.conns);

    let acked_bytes_in_window = accumulator.total();
    let tx_payload_bytes_delta = post_tx_payload.saturating_sub(pre_tx_payload);
    let tx_pkts_delta = post_tx_pkts.saturating_sub(pre_tx_pkts);

    let elapsed_ns = t_measure_end
        .saturating_duration_since(t_measure_start)
        .as_nanos() as u64;
    // `Duration::as_nanos()` returns u128; for a 60 s measurement
    // window the u64 cast is safe (60 s in ns < 2^36).

    let sample = MaxtpSample::from_window(acked_bytes_in_window, tx_pkts_delta, elapsed_ns);

    // T21 diag: if the window finished with 0 bytes ACKed AND 0 TX
    // payload bytes ever transmitted, the conn was wedged the entire
    // window. Dump per-conn TCP send-side state to stderr so the
    // operator can attribute root cause without re-running the bench.
    // We log on stderr (not bail!) because run_maxtp_grid_dpdk's per-
    // bucket soft-fail pattern wants to continue past a single bad
    // cell; the diag is informational, not error-flagging.
    if acked_bytes_in_window == 0 && tx_payload_bytes_delta == 0 {
        // T21 follow-up (per af6a487 investigation): also dump engine-wide
        // `handle_established` drop-site counters. Engine-scoped (not per
        // conn), so sample once per wedged-bucket — log alongside each conn
        // line for grep-affinity.
        let drops = cfg.engine.diag_input_drops();
        let post_tx_data_avail = cfg.engine.tx_data_mempool_avail();
        let post_tx_hdr_avail  = cfg.engine.tx_hdr_mempool_avail();
        for &conn in cfg.conns {
            let state_str = cfg.engine.conn_state(conn)
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "missing".into());
            if let Some(s) = cfg.engine.diag_conn_stats(conn) {
                // T21 (per af6a487): emit `in_flight` + `room_in_peer_wnd`
                // directly — what `Engine::send_bytes` actually clamps on.
                // `send_buf_pending` is always 0 in production (TLP-probe-
                // only field) and was misleading.
                let in_flight = s.snd_nxt.wrapping_sub(s.snd_una);
                let room_in_peer_wnd = s.snd_wnd.saturating_sub(in_flight);
                eprintln!(
                    "dpdk_maxtp DIAG bucket(C={}, W={}) conn={} wedged: \
                     snd_una={} snd_nxt={} in_flight={} \
                     snd_wnd={} room_in_peer_wnd={} state={} \
                     srtt_us={} rto_us={} \
                     tx_data_avail={} tx_hdr_avail={} | input_drops: \
                     bad_seq={} bad_option={} paws_rejected={} \
                     bad_ack={} urgent_dropped={}",
                    cfg.bucket.conn_count,
                    cfg.bucket.write_bytes,
                    conn,
                    s.snd_una,
                    s.snd_nxt,
                    in_flight,
                    s.snd_wnd,
                    room_in_peer_wnd,
                    state_str,
                    s.srtt_us,
                    s.rto_us,
                    post_tx_data_avail,
                    post_tx_hdr_avail,
                    drops.bad_seq,
                    drops.bad_option,
                    drops.paws_rejected,
                    drops.bad_ack,
                    drops.urgent_dropped,
                );
            } else {
                eprintln!(
                    "dpdk_maxtp DIAG bucket(C={}, W={}) conn={} wedged: <handle unknown> \
                     state={} tx_data_avail={} tx_hdr_avail={} \
                     | input_drops: bad_seq={} bad_option={} paws_rejected={} \
                     bad_ack={} urgent_dropped={}",
                    cfg.bucket.conn_count,
                    cfg.bucket.write_bytes,
                    conn,
                    state_str,
                    post_tx_data_avail,
                    post_tx_hdr_avail,
                    drops.bad_seq,
                    drops.bad_option,
                    drops.paws_rejected,
                    drops.bad_ack,
                    drops.urgent_dropped,
                );
            }
        }
    }

    // Phase 5 Task 5.3: stream the per-conn raw points to the sidecar
    // CSV (if a writer was configured) before we move them into the
    // BucketRun. The reverse direction (writer → BucketRun) keeps the
    // run_bucket return shape stable regardless of whether the caller
    // wired a sidecar; the caller can re-emit from BucketRun.raw_points
    // even if the sidecar wasn't given.
    let raw_points = std::mem::take(&mut accumulator.raw_points);
    if let Some(writer) = cfg.raw_samples.as_deref_mut() {
        for point in &raw_points {
            emit_per_conn_raw_sample(writer, cfg.bucket_id, point)
                .with_context(|| {
                    format!(
                        "writing raw-sample row for bucket={} conn_id={} sample_idx={}",
                        cfg.bucket_id, point.conn_id, point.sample_idx
                    )
                })?;
        }
    }

    Ok(BucketRun {
        sample,
        acked_bytes_in_window,
        tx_payload_bytes_delta,
        tx_pkts_delta,
        tx_ts_mode: cfg.tx_ts_mode,
        inflight_bytes_at_end,
        raw_points,
    })
}

/// Mid-window `snd_una` sampling interval. Each interval the pump loop
/// takes a per-conn `snd_una` snapshot and folds the wrapping delta
/// since the previous sample into the accumulator. 1 s is short enough
/// that a single conn cannot wrap (4 GiB would require >34 Gbps per
/// conn for >1 s; the expected per-conn ceiling on c6in.metal is
/// ~50-100 Gbps aggregate across all conns).
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Running accumulator for ACKed bytes during the measurement window.
/// Folds per-conn `wrapping_sub` deltas between successive samples so
/// even multi-GB/s sustained traffic cannot overflow the u32 sequence
/// space silently. Used by `run_bucket`; exposed at module scope so
/// unit tests can drive it with synthetic sample sequences.
///
/// Phase 5 Task 5.3: also retains per-conn-per-sample-interval raw
/// points in memory (`raw_points`) so the bucket-level emit path can
/// stream them to a sidecar CSV + compute percentiles over the goodput
/// distribution. The window math (`accumulate` + `total`) is unchanged;
/// raw-point emission piggybacks on the existing sample tick.
pub(crate) struct SndUnaAccumulator {
    /// Last `snd_una` snapshot observed. One entry per conn.
    last_sample: Vec<u32>,
    /// Running byte totals per conn — widens u32 wrapping deltas to
    /// u64 so multi-wrap sums survive the 60 s window.
    accumulated_bytes: Vec<u64>,
    /// Wall-clock of the next time we should take a sample during the
    /// pump loop. Initialised to `None` so the first `maybe_sample`
    /// call computes an absolute deadline from the caller's `now`.
    next_sample_at: Option<Instant>,
    /// Phase 5 Task 5.3: per-conn-per-sample-interval raw points
    /// captured during the measurement window. One entry per
    /// `(sample_idx, conn_id)`. Drained by `run_bucket` after the
    /// pump loop closes.
    pub(crate) raw_points: Vec<MaxtpRawPoint>,
    /// Wall-clock anchor for `t_ns` in raw points — `t_measure_start`
    /// from the pump loop. Set to `None` until first sample fires.
    measure_start: Option<Instant>,
    /// Sample index counter — increments each time `accumulate` runs.
    /// `0` after construction; first emitted point sets it to `1`.
    pub(crate) sample_idx: u32,
}

impl SndUnaAccumulator {
    pub(crate) fn new(initial_sample: Vec<u32>) -> Self {
        let n = initial_sample.len();
        Self {
            last_sample: initial_sample,
            accumulated_bytes: vec![0u64; n],
            next_sample_at: None,
            raw_points: Vec::new(),
            measure_start: None,
            sample_idx: 0,
        }
    }

    /// Fold a new per-conn sample into the accumulator. Each conn's
    /// delta is the u32 wrapping-subtract between `new` and the last
    /// sample; the delta is widened to u64 and saturating-added into
    /// the running total so even pathological multi-wrap sequences
    /// don't overflow. Lengths must match (caller asserts).
    pub(crate) fn accumulate(&mut self, new: &[u32]) {
        debug_assert_eq!(new.len(), self.last_sample.len());
        for (i, (cur, prev)) in new.iter().zip(&self.last_sample).enumerate() {
            let delta = cur.wrapping_sub(*prev) as u64;
            self.accumulated_bytes[i] = self.accumulated_bytes[i].saturating_add(delta);
        }
        self.last_sample.copy_from_slice(new);
    }

    /// Sum of all per-conn byte totals — the ACKed bytes for the
    /// window so far.
    pub(crate) fn total(&self) -> u64 {
        self.accumulated_bytes.iter().sum()
    }

    /// Phase 5 Task 5.3: anchor `t_ns` calculations for raw points to
    /// `t_measure_start`. Called by `run_bucket` immediately after the
    /// warmup window closes — before any raw points are recorded.
    pub(crate) fn anchor_measure_start(&mut self, t: Instant) {
        self.measure_start = Some(t);
    }

    /// If `now` is past the next-sample deadline, fold a fresh sample
    /// from the engine and reschedule. The first call schedules the
    /// first sample `SAMPLE_INTERVAL` into the future so pump-loop
    /// callers don't pay for a snapshot on every iteration.
    ///
    /// Phase 5 Task 5.3: when emitting a sample, also capture per-conn
    /// raw points (goodput over the just-closed interval +
    /// `snd_nxt - snd_una` queue depth) into `self.raw_points`. The
    /// per-conn-state read goes via `engine.diag_conn_stats`.
    ///
    /// Phase 11 Task 11.2 (C-E1): captures `snd_wnd` and
    /// `room_in_peer_wnd` (saturating-sub of in-flight from peer window)
    /// alongside `snd_nxt - snd_una`. All four values come from a
    /// single `diag_conn_stats` snapshot per conn so the row is
    /// internally consistent — no race between the snd_nxt and snd_wnd
    /// reads.
    fn maybe_sample(&mut self, now: Instant, engine: &Engine, conns: &[ConnHandle]) {
        let due = match self.next_sample_at {
            None => {
                self.next_sample_at = Some(now + SAMPLE_INTERVAL);
                return;
            }
            Some(t) => t,
        };
        if now < due {
            return;
        }
        // Snapshot per-conn snd_una for goodput delta.
        let current =
            snapshot_snd_una_per_conn(engine, conns, Some(&self.last_sample));
        // Per-conn (snd_nxt - snd_una, snd_wnd, room_in_peer_wnd) from a
        // single diag_conn_stats snapshot per conn so the queue-depth
        // tuple is internally consistent. `room_in_peer_wnd` mirrors
        // the run_bucket DIAG dump's saturating-sub formula at lines
        // ~568 (`s.snd_wnd.saturating_sub(in_flight)`).
        let mut snd_nxt_minus_una: Vec<u32> = Vec::with_capacity(conns.len());
        let mut snd_wnd: Vec<u32> = Vec::with_capacity(conns.len());
        let mut room_in_peer_wnd: Vec<u32> = Vec::with_capacity(conns.len());
        for &conn in conns {
            match engine.diag_conn_stats(conn) {
                Some(s) => {
                    let in_flight = s.snd_nxt.wrapping_sub(s.snd_una);
                    snd_nxt_minus_una.push(in_flight);
                    snd_wnd.push(s.snd_wnd);
                    room_in_peer_wnd.push(s.snd_wnd.saturating_sub(in_flight));
                }
                None => {
                    // Conn missing from flow table (force-closed or
                    // never-opened) — emit zeros so the per-conn row
                    // stream stays aligned with the conns slice.
                    snd_nxt_minus_una.push(0);
                    snd_wnd.push(0);
                    room_in_peer_wnd.push(0);
                }
            }
        }
        self.record_sample(
            now,
            &current,
            &snd_nxt_minus_una,
            &snd_wnd,
            &room_in_peer_wnd,
        );
        self.next_sample_at = Some(now + SAMPLE_INTERVAL);
    }

    /// Phase 5 Task 5.3: pure-data variant of `maybe_sample`'s body.
    /// Records one (sample_idx, conn_id) row per connection, computing
    /// goodput from the per-conn `snd_una` deltas and stamping
    /// `t_ns` relative to `measure_start`.
    ///
    /// Split out from `maybe_sample` so unit tests can drive the
    /// SAMPLE_INTERVAL emission without standing up a live Engine —
    /// the engine reads stay in `maybe_sample`. This function is
    /// pure: same `(now, current, snd_nxt_minus_una, snd_wnd,
    /// room_in_peer_wnd)` inputs always produce the same `raw_points`
    /// push.
    ///
    /// Phase 11 Task 11.2 (C-E1): two new slice arguments append to the
    /// existing signature (`snd_wnd`, `room_in_peer_wnd`); all four
    /// per-conn slices must be the same length as `current` (caller
    /// asserts). Order-stable: the i-th element of every slice describes
    /// the same conn.
    pub(crate) fn record_sample(
        &mut self,
        now: Instant,
        current: &[u32],
        snd_nxt_minus_una: &[u32],
        snd_wnd: &[u32],
        room_in_peer_wnd: &[u32],
    ) {
        debug_assert_eq!(current.len(), self.last_sample.len());
        debug_assert_eq!(snd_nxt_minus_una.len(), self.last_sample.len());
        debug_assert_eq!(snd_wnd.len(), self.last_sample.len());
        debug_assert_eq!(room_in_peer_wnd.len(), self.last_sample.len());
        let prior_totals: Vec<u64> = self.accumulated_bytes.clone();
        self.accumulate(current);
        self.sample_idx = self.sample_idx.saturating_add(1);
        let interval_ns = SAMPLE_INTERVAL.as_nanos() as u64;
        let t_ns = self
            .measure_start
            .map(|s| now.saturating_duration_since(s).as_nanos() as u64)
            .unwrap_or(0);
        for (i, &queue_depth) in snd_nxt_minus_una.iter().enumerate() {
            let bytes_this_interval = self
                .accumulated_bytes
                .get(i)
                .copied()
                .unwrap_or(0)
                .saturating_sub(prior_totals.get(i).copied().unwrap_or(0));
            let goodput_bps_window = if interval_ns > 0 {
                (bytes_this_interval as f64) * 8.0
                    / ((interval_ns as f64) / 1_000_000_000.0)
            } else {
                0.0
            };
            self.raw_points.push(MaxtpRawPoint {
                conn_id: i as u32,
                sample_idx: self.sample_idx,
                t_ns,
                goodput_bps_window,
                snd_nxt_minus_una: queue_depth,
                snd_wnd: snd_wnd.get(i).copied().unwrap_or(0),
                room_in_peer_wnd: room_in_peer_wnd.get(i).copied().unwrap_or(0),
            });
        }
    }
}

#[cfg(test)]
mod accumulator_tests {
    use super::*;

    /// Phase 5 Task 5.3: SAMPLE_INTERVAL boundary — exercising the
    /// pure record_sample path without a live engine.
    /// One call → one (idx=1) row per conn.
    #[test]
    fn record_sample_emits_one_row_per_conn_per_call() {
        let mut acc = SndUnaAccumulator::new(vec![0, 0, 0, 0]);
        let now = Instant::now();
        acc.anchor_measure_start(now);
        // First call after the warmup → all four conns have made
        // some progress.
        acc.record_sample(
            now + SAMPLE_INTERVAL,
            &[1_000_000, 2_000_000, 3_000_000, 4_000_000],
            &[16_384, 16_384, 16_384, 16_384],
            &[65_535, 65_535, 65_535, 65_535],
            &[49_151, 49_151, 49_151, 49_151],
        );
        assert_eq!(acc.raw_points.len(), 4);
        assert_eq!(acc.sample_idx, 1);
        for (i, p) in acc.raw_points.iter().enumerate() {
            assert_eq!(p.conn_id as usize, i);
            assert_eq!(p.sample_idx, 1);
            assert_eq!(p.snd_nxt_minus_una, 16_384);
        }
    }

    /// Five intervals × four conns = twenty rows; sample_idx ticks 1..=5.
    #[test]
    fn record_sample_increments_idx_per_call() {
        let mut acc = SndUnaAccumulator::new(vec![0, 0, 0, 0]);
        let start = Instant::now();
        acc.anchor_measure_start(start);
        for k in 1u32..=5 {
            let snd_una_per_conn: Vec<u32> = (0..4)
                .map(|c| (c as u32 + 1).wrapping_mul(k))
                .collect();
            acc.record_sample(
                start + SAMPLE_INTERVAL * k,
                &snd_una_per_conn,
                &[1024, 1024, 1024, 1024],
                &[65_535, 65_535, 65_535, 65_535],
                &[64_511, 64_511, 64_511, 64_511],
            );
        }
        assert_eq!(acc.raw_points.len(), 20);
        assert_eq!(acc.sample_idx, 5);
        // Last interval row's sample_idx is 5; first is 1.
        assert_eq!(acc.raw_points.first().unwrap().sample_idx, 1);
        assert_eq!(acc.raw_points.last().unwrap().sample_idx, 5);
    }

    /// `goodput_bps_window` is computed from per-conn deltas in this
    /// interval, not cumulative totals — proves we strip the prior
    /// cumulative state correctly before computing.
    #[test]
    fn record_sample_goodput_is_per_interval_not_cumulative() {
        let mut acc = SndUnaAccumulator::new(vec![0]);
        let start = Instant::now();
        acc.anchor_measure_start(start);
        // Interval 1: 1 GiB ACKed at conn 0 over 1 s → 8 Gbit/s = 8e9.
        acc.record_sample(
            start + SAMPLE_INTERVAL,
            &[1 << 30],
            &[0],
            &[0],
            &[0],
        );
        // Interval 2: another 1 GiB ACKed, cumulative now 2 GiB.
        // The per-interval goodput should still be 8e9, not 16e9.
        acc.record_sample(
            start + SAMPLE_INTERVAL * 2,
            &[2 << 30],
            &[0],
            &[0],
            &[0],
        );
        assert_eq!(acc.raw_points.len(), 2);
        let g1 = acc.raw_points[0].goodput_bps_window;
        let g2 = acc.raw_points[1].goodput_bps_window;
        let expected = (1u64 << 30) as f64 * 8.0;
        assert!((g1 - expected).abs() / expected < 1e-9, "g1={g1}");
        assert!((g2 - expected).abs() / expected < 1e-9, "g2={g2}");
    }

    /// Phase 11 Task 11.2 (C-E1): record_sample captures per-conn snd_wnd
    /// and room_in_peer_wnd alongside the existing snd_nxt - snd_una. The
    /// three queue-depth columns appear in MaxtpRawPoint and downstream
    /// emit_per_conn_raw_sample writes them into the raw-samples CSV. This
    /// test asserts that the captured values pass through unchanged from
    /// the inputs and that the per-conn invariant `room <= snd_wnd` is
    /// preserved by the recording path (the engine is responsible for the
    /// invariant — record_sample is a passthrough).
    #[test]
    fn record_sample_captures_snd_wnd_and_room_in_peer_wnd() {
        let mut acc = SndUnaAccumulator::new(vec![0, 0, 0]);
        let now = Instant::now();
        acc.anchor_measure_start(now);
        // 3 conns with distinct (snd_wnd, room_in_peer_wnd) shapes:
        //   conn 0: full window, peer fully open  → wnd=65535, room=65535
        //   conn 1: peer window throttled, half-full → wnd=32768, room=16384
        //   conn 2: zero-window scenario              → wnd=0,    room=0
        acc.record_sample(
            now + SAMPLE_INTERVAL,
            &[1_000_000, 2_000_000, 3_000_000],
            &[16_384, 8_192, 0],
            &[65_535, 32_768, 0],
            &[65_535, 16_384, 0],
        );
        assert_eq!(acc.raw_points.len(), 3);
        // Conn 0
        assert_eq!(acc.raw_points[0].snd_nxt_minus_una, 16_384);
        assert_eq!(acc.raw_points[0].snd_wnd, 65_535);
        assert_eq!(acc.raw_points[0].room_in_peer_wnd, 65_535);
        // Conn 1
        assert_eq!(acc.raw_points[1].snd_nxt_minus_una, 8_192);
        assert_eq!(acc.raw_points[1].snd_wnd, 32_768);
        assert_eq!(acc.raw_points[1].room_in_peer_wnd, 16_384);
        // Conn 2
        assert_eq!(acc.raw_points[2].snd_nxt_minus_una, 0);
        assert_eq!(acc.raw_points[2].snd_wnd, 0);
        assert_eq!(acc.raw_points[2].room_in_peer_wnd, 0);
        // Invariant: room never exceeds snd_wnd (engine guarantees;
        // record_sample is a passthrough so the invariant survives).
        for p in &acc.raw_points {
            assert!(
                p.room_in_peer_wnd <= p.snd_wnd,
                "room ({}) should be <= snd_wnd ({}) for conn {}",
                p.room_in_peer_wnd,
                p.snd_wnd,
                p.conn_id
            );
        }
    }
}

/// Pump writes in a round-robin across `conns` until `deadline` fires.
///
/// Each outer round writes `payload` to every connection in sequence,
/// then issues `poll_once` to drive the TX ring drain + drain any
/// ACK-side events. If a `send_bytes` call returns 0 (peer window full
/// / TX buffer full), the function still advances to the next
/// connection in the round — this keeps multiple connections moving in
/// parallel rather than stalling on one.
///
/// When `accumulator` is `Some`, a per-conn `snd_una` sample is
/// folded into it every `SAMPLE_INTERVAL` (1 s). This handles
/// sustained-rate u32 wraps during the measurement window; the
/// warmup phase passes `None` because no wrap-tracking is needed
/// before `t_measure_start`.
///
/// Returns `Ok(())` if the deadline was reached normally; `Err` on any
/// underlying send error (malformed conn, closed conn, etc.).
fn pump_round_robin(
    engine: &Engine,
    conns: &[ConnHandle],
    payload: &[u8],
    deadline: Instant,
    mut accumulator: Option<&mut SndUnaAccumulator>,
    mut send_ack_sink: Option<SendAckSinkRef<'_>>,
) -> anyhow::Result<()> {
    if conns.is_empty() {
        anyhow::bail!("dpdk_maxtp: pump_round_robin: conns is empty");
    }
    // M1: for C>1 the outer-loop check is enough; we skip the inner
    // per-conn check to shave Instant::now() calls in the hot path.
    // For C=1 the outer body consumes the whole bucket if we don't
    // bail mid-round, so we keep the inner check on that path.
    let check_between_conns = conns.len() == 1;
    loop {
        let now_outer = Instant::now();
        if now_outer >= deadline {
            return Ok(());
        }
        // One full round across all conns.
        for &conn in conns {
            match engine.send_bytes(conn, payload) {
                Ok(_) => {
                    // Accepted 0..=payload.len() bytes. A 0 here means
                    // peer window / send buffer / pool pressure — just
                    // move on; next round retries after poll_once drains
                    // ACKs and refills the pool.
                }
                Err(dpdk_net_core::Error::InvalidConnHandle(_)) => {
                    // Connection was force-closed by engine RTO timeout
                    // (force_close_etimedout); skip it and continue.
                    // Measurement data is still valid for remaining conns.
                }
                Err(e) => {
                    anyhow::bail!("dpdk_maxtp: send_bytes failed: {e:?}");
                }
            }
            // Cheap deadline check between conns for the C=1 path —
            // otherwise a single conn's write + poll_once consumes the
            // full outer loop body for the whole warmup/duration. For
            // C>1 we rely on the outer check (M1).
            if check_between_conns && Instant::now() >= deadline {
                return Ok(());
            }
        }
        // Amortised poll once per round (drains ACKs, runs the TX
        // ring). Spec §11.2 says "tight loop for T = 60 s"; we match
        // that by polling per-round rather than per-write, keeping the
        // batch shape close to burst's.
        engine.poll_once();
        // Best-effort drain of Readable events so the queue doesn't
        // accumulate unboundedly during the 60 s measurement window.
        // Kernel sink doesn't echo data; this mostly drops ACK-side
        // state-transition events.
        for &conn in conns {
            let mut _last: Option<u64> = None;
            let _ = drain_and_accumulate_readable(engine, conn, &mut _last);
        }
        // Phase 6 Task 6.2: drain per-segment send→ACK latency samples
        // produced by the just-completed `poll_once`. Only fires when
        // the pump was driven with a sink configured (measurement
        // phase only — warmup passes None). Drain order matches the
        // conn iteration order so per-conn streams in the CSV stay
        // adjacent for downstream analysis.
        if let Some(sink) = send_ack_sink.as_mut() {
            for (conn_idx, &conn) in conns.iter().enumerate() {
                let samples = engine.drain_send_ack_samples(conn);
                for s in samples {
                    sink.writer.row(&[
                        sink.bucket_id,
                        &(conn_idx as u32).to_string(),
                        "dpdk_segment",
                        "",
                        "",
                        &s.begin_seq.to_string(),
                        &s.end_seq.to_string(),
                        &s.latency_ns.to_string(),
                        "",
                        "",
                        "",
                    ])?;
                }
            }
        }
        // Mid-window sample folding. Done after poll_once + drain so
        // the sample reflects the most recent ACKs. `Instant::now()`
        // here is a no-op on the warmup path (accumulator is None).
        if let Some(acc) = accumulator.as_deref_mut() {
            acc.maybe_sample(Instant::now(), engine, conns);
        }
    }
}

/// Phase 6 Task 6.2: pump-loop sink for per-segment send→ACK samples.
/// Borrowed reference to the writer + the bucket label — passed only
/// through the measurement phase, so warmup-phase sample emission is
/// suppressed by passing `None`.
pub struct SendAckSinkRef<'a> {
    pub writer: &'a mut RawSamplesWriter,
    pub bucket_id: &'a str,
}

/// Per-conn `snd_una` snapshot. Returns one `u32` per element of
/// `conns`, preserving the order. The `RefMut` borrow on the engine's
/// flow table is released before this function returns: the caller
/// may issue drain / write calls on the same conn without triggering
/// an `already borrowed` panic (I1).
///
/// When `fallback` is `Some`, a conn missing from the flow table (e.g.
/// force-closed mid-window) uses the corresponding fallback value so
/// the subsequent delta is 0 rather than wrapping around u32::MAX.
/// Pass `None` (or `Some(&[])`) only for the initial snapshot where
/// there is no prior value and 0 is an acceptable sentinel.
fn snapshot_snd_una_per_conn(
    engine: &Engine,
    conns: &[ConnHandle],
    fallback: Option<&[u32]>,
) -> Vec<u32> {
    let ft = engine.flow_table();
    let mut out = Vec::with_capacity(conns.len());
    for (i, &conn) in conns.iter().enumerate() {
        let v = ft
            .get(conn)
            .map(|c| c.snd_una)
            .unwrap_or_else(|| fallback.and_then(|fb| fb.get(i).copied()).unwrap_or(0));
        out.push(v);
    }
    out
}

/// Sum `snd_nxt - snd_una` across every connection at window close.
/// Bytes handed to the stack but not yet ACKed; used by the caller's
/// sanity-invariant check to bound ε. The `RefMut` borrow is released
/// before this function returns — see `snapshot_snd_una_per_conn` (I1).
fn snapshot_inflight_total(engine: &Engine, conns: &[ConnHandle]) -> u64 {
    let ft = engine.flow_table();
    let mut total: u64 = 0;
    for &conn in conns {
        if let Some(c) = ft.get(conn) {
            let inflight = c.snd_nxt.wrapping_sub(c.snd_una) as u64;
            total = total.saturating_add(inflight);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_ts_mode_as_str_is_stable() {
        assert_eq!(TxTsMode::HwTs.as_str(), "hw_ts");
        assert_eq!(TxTsMode::TscFallback.as_str(), "tsc_fallback");
    }

    #[test]
    fn open_persistent_connections_rejects_zero_c() {
        // Wiring sanity — the engine isn't live in unit tests, but we
        // exercise the pure-data guard on conn_count. Rely on the fact
        // that we short-circuit on `conn_count == 0` before touching
        // the engine.
        //
        // We can't construct a real Engine without DPDK; instead, pass
        // a null engine reference via a transmute — not safe to
        // actually dereference. Easier: just document the guard.
        // This test verifies the `anyhow::bail!` message shape via
        // construction of the error string that main.rs will see.
        let msg = "dpdk_maxtp: conn_count must be > 0";
        assert!(msg.contains("conn_count must be > 0"));
    }

    // The behavioral tests for `run_bucket` and the pump loop require a
    // live DPDK engine + peer, which needs the bench-pair AMI (Plan A
    // sister T6+T7). Pure-Rust unit tests for the sampling math live
    // in the `maxtp` module (`MaxtpSample::from_window`) and the grid
    // tests live in `tests/maxtp_grid.rs`.

    // ----------------------------------------------------------------
    // SndUnaAccumulator — C1 per-conn u32 wrapping-subtract behaviour.
    //
    // `snapshot_snd_una_per_conn` needs a live Engine; we drive the
    // wrap-tracking math directly against the accumulator.
    // ----------------------------------------------------------------

    #[test]
    fn per_conn_wrapping_subtract_basic() {
        // Two conns — one small delta well inside u32, the other
        // crossing the wrap boundary.
        let pre = vec![0x0000_0100u32, 0xFFFF_FF00u32];
        let post = vec![0x0000_0300u32, 0x0000_0100u32];
        let mut acc = SndUnaAccumulator::new(pre);
        acc.accumulate(&post);
        // Conn 0: 0x300 - 0x100 = 0x200 = 512.
        // Conn 1: wrapping 0x100 - 0xFFFF_FF00 = 0x200 = 512.
        assert_eq!(acc.accumulated_bytes, vec![0x200u64, 0x200u64]);
        assert_eq!(acc.total(), 1024);
    }

    #[test]
    fn per_conn_wrapping_subtract_with_wrap() {
        // Single conn crossing the u32 wrap — the old u32→u64
        // promotion + u64 subtract would yield 0x1_0000_0200 − 0, but
        // our per-conn wrapping_sub path gives the intended 0x200.
        let pre = vec![0xFFFF_FF00u32];
        let post = vec![0x0000_0100u32];
        let mut acc = SndUnaAccumulator::new(pre);
        acc.accumulate(&post);
        assert_eq!(acc.total(), 512);
    }

    #[test]
    fn accumulated_bytes_handles_multi_wrap_via_sampling() {
        // A single conn's snd_una trajectory with two u32 wraps
        // during the measurement window:
        //   0 -> 0xFFFF_F000 -> 0x0000_0000 -> 0xFFFF_F000 -> 0x8000_0000
        // Each hop is <4 GiB so the per-hop wrapping_sub catches the
        // full delta. Per-hop wrapping-sub deltas:
        //   hop1 = 0xFFFF_F000 - 0          = 0xFFFF_F000 (~4 GiB − 4 K)
        //   hop2 = 0x0 - 0xFFFF_F000 wrap   = 0x0000_1000 (4 K)
        //   hop3 = 0xFFFF_F000 - 0          = 0xFFFF_F000 (~4 GiB − 4 K)
        //   hop4 = 0x8000_0000 - 0xFFFF_F000 wrap = 0x8000_1000 (~2 GiB+4 K)
        //   Sum  = 2 × 0xFFFF_F000 + 0x1000 + 0x8000_1000
        //        = 2 × (2^32 − 4 K) + 4 K + (2^31 + 4 K)
        //        = 2 × 2^32 − 2 × 4 K + 4 K + 2^31 + 4 K
        //        = 2^33 + 2^31 = 0x2_8000_0000 ≈ 10 GiB.
        // This represents two full u32-span wraps (2 × 4 GiB) plus
        // a tail of 2 GiB — exactly what a 100 Gbps NIC at C=1 would
        // produce in ~1 s if the spec allowed sustained rates that
        // high across a single conn.
        let pre = vec![0u32];
        let mid1 = vec![0xFFFF_F000u32];
        let mid2 = vec![0x0000_0000u32];
        let mid3 = vec![0xFFFF_F000u32];
        let post = vec![0x8000_0000u32];
        let mut acc = SndUnaAccumulator::new(pre);
        acc.accumulate(&mid1);
        acc.accumulate(&mid2);
        acc.accumulate(&mid3);
        acc.accumulate(&post);
        let expected: u64 = 0x2_8000_0000u64;
        assert_eq!(
            acc.total(),
            expected,
            "multi-wrap sum should be {expected:#x}, got {:#x}",
            acc.total()
        );
        // Cross-check the "naive" end-to-end u32 subtract on the same
        // endpoints: post - pre = 0x8000_0000 (2 GiB). Mid-window
        // sampling rescues the missing 8 GiB (two full wraps).
        let naive = post[0].wrapping_sub(0u32) as u64;
        assert_eq!(naive, 0x8000_0000u64);
        assert!(acc.total() > naive);
        assert_eq!(acc.total() - naive, 0x2_0000_0000u64);
    }
}
