//! Reusable request-response RTT workload.
//!
//! Extracted from `main.rs` in the A10 T7 bench-stress reshape so
//! `bench-stress` can drive the same inner loop under netem + FaultInjector
//! scenarios (spec §7). The binary `main.rs` now re-routes through this
//! module; external consumers (`bench-stress`) depend on `bench-e2e` as a
//! library and pull these pub functions directly.
//!
//! The public surface is deliberately narrow:
//!
//! - [`RTT_TIMEOUT`], [`CONNECT_TIMEOUT`] — per-iter + handshake ceilings.
//! - [`IterRecord`] — the per-iteration measurement product carrying RTT +
//!   attribution buckets + `rx_hw_ts_ns`.
//! - [`open_connection`] — retry-on-PeerUnreachable + drive poll_once
//!   until `Connected` fires.
//! - [`request_response_attributed`] — one measured round-trip with
//!   attribution composed in either HW-TS or TSC-fallback mode.
//! - [`tsc_delta_to_ns`] — u128-intermediate TSC→ns conversion.
//!
//! The helpers are intentionally not methods on a struct — each bench
//! binary pairs them with its own precondition / CSV / counter-delta
//! plumbing, and a free-function shape keeps the call sites flat.

use anyhow::Context;

use dpdk_net_core::engine::Engine;
use dpdk_net_core::error::Error;
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

use crate::attribution::{AttributionMode, HwTsBuckets, TscFallbackBuckets};

/// Timeout for each request-response round-trip. Tens of microseconds on
/// a healthy host; 10 s is the floor against wedge.
pub const RTT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for the initial three-way handshake. Matches RTT ceiling.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Per-iteration measurement product.
// ---------------------------------------------------------------------------

/// The per-iteration measurement product. `mode` selects which bucket
/// variant is populated; the unpopulated variant is `None`. `rx_hw_ts_ns`
/// is the raw value from the Readable event (0 on ENA).
#[derive(Debug, Clone, Copy)]
pub struct IterRecord {
    pub rtt_ns: u64,
    pub rx_hw_ts_ns: u64,
    pub mode: AttributionMode,
    pub hw_buckets: Option<HwTsBuckets>,
    pub tsc_buckets: Option<TscFallbackBuckets>,
}

// ---------------------------------------------------------------------------
// One measured round-trip with attribution buckets.
// ---------------------------------------------------------------------------

/// One measured round-trip with attribution buckets.
///
/// Mirrors bench-ab-runner's `request_response_once` (carry-forward
/// accumulator for partial-accept safety) and adds the timestamp captures
/// needed to compose either the 5-bucket HW-TS variant or the 3-bucket
/// TSC-fallback variant. See `main.rs` bucket-derivation notes for the
/// HW-TS composition; the TSC-fallback path is straightforward.
pub fn request_response_attributed(
    engine: &Engine,
    conn: ConnHandle,
    request: &[u8],
    response_bytes: usize,
    tsc_hz: u64,
    carry_forward: &mut usize,
) -> anyhow::Result<IterRecord> {
    // Wall-clock entry.
    let t_user_send = dpdk_net_core::clock::rdtsc();

    // --- Send phase ---
    let send_deadline = std::time::Instant::now() + RTT_TIMEOUT;
    let mut sent: usize = 0;
    while sent < request.len() {
        let remaining = &request[sent..];
        let accepted = match engine.send_bytes(conn, remaining) {
            Ok(n) => n,
            Err(Error::InvalidConnHandle(_)) => {
                anyhow::bail!(
                    "peer closed connection mid-iteration \
                     (InvalidConnHandle from send_bytes after {sent}/{} bytes)",
                    request.len()
                );
            }
            Err(e) => anyhow::bail!("send_bytes failed: {e:?}"),
        };
        sent += accepted as usize;
        if sent < request.len() {
            engine.poll_once();
            *carry_forward = carry_forward
                .saturating_add(drain_and_accumulate_readable(engine, conn, &mut None)?);
            if std::time::Instant::now() >= send_deadline {
                anyhow::bail!(
                    "send timeout ({}/{} bytes accepted)",
                    sent,
                    request.len()
                );
            }
        }
    }
    let t_tx_sched = dpdk_net_core::clock::rdtsc();

    // --- Receive phase ---
    let recv_deadline = std::time::Instant::now() + RTT_TIMEOUT;
    let mut got: usize = *carry_forward;
    *carry_forward = 0;
    // Latest Readable event's rx_hw_ts_ns, captured mid-drain.
    let mut last_rx_hw_ts_ns: Option<u64> = None;
    while got < response_bytes {
        engine.poll_once();
        got += drain_and_accumulate_readable(engine, conn, &mut last_rx_hw_ts_ns)?;
        if got < response_bytes && std::time::Instant::now() >= recv_deadline {
            anyhow::bail!("recv timeout ({}/{} bytes)", got, response_bytes);
        }
    }
    if got > response_bytes {
        *carry_forward = got - response_bytes;
    }
    let t_enqueued = dpdk_net_core::clock::rdtsc();

    // --- Wall-clock exit ---
    let t_user_return = dpdk_net_core::clock::rdtsc();

    let rtt_ns = tsc_delta_to_ns(t_user_send, t_user_return, tsc_hz);
    let rx_hw_ts_ns = last_rx_hw_ts_ns.unwrap_or(0);
    let mode = AttributionMode::from_rx_hw_ts(rx_hw_ts_ns);

    // Compose buckets such that `total_ns()` == rtt_ns exactly.
    let (hw_buckets, tsc_buckets) = match mode {
        AttributionMode::Hw => {
            let host_span_ns = tsc_delta_to_ns(t_tx_sched, t_enqueued, tsc_hz);
            let bucket_a = tsc_delta_to_ns(t_user_send, t_tx_sched, tsc_hz);
            let bucket_e = tsc_delta_to_ns(t_enqueued, t_user_return, tsc_hz);
            (
                Some(HwTsBuckets {
                    user_send_to_tx_sched_ns: bucket_a,
                    tx_sched_to_nic_tx_wire_ns: 0,
                    nic_tx_wire_to_nic_rx_ns: host_span_ns,
                    nic_rx_to_enqueued_ns: 0,
                    enqueued_to_user_return_ns: bucket_e,
                }),
                None,
            )
        }
        AttributionMode::Tsc => {
            let bucket_a = tsc_delta_to_ns(t_user_send, t_tx_sched, tsc_hz);
            let bucket_b = tsc_delta_to_ns(t_tx_sched, t_enqueued, tsc_hz);
            let bucket_c = tsc_delta_to_ns(t_enqueued, t_user_return, tsc_hz);
            (
                None,
                Some(TscFallbackBuckets {
                    user_send_to_tx_sched_ns: bucket_a,
                    tx_sched_to_enqueued_ns: bucket_b,
                    enqueued_to_user_return_ns: bucket_c,
                }),
            )
        }
    };

    Ok(IterRecord {
        rtt_ns,
        rx_hw_ts_ns,
        mode,
        hw_buckets,
        tsc_buckets,
    })
}

/// Drain events from the engine, accumulating Readable-payload bytes on
/// `conn`. On each Readable event observed, writes the carried
/// `rx_hw_ts_ns` into `last_rx_hw_ts_ns` (overwriting; we keep the last
/// one seen in this drain). Fails on Error/Closed for `conn`.
pub fn drain_and_accumulate_readable(
    engine: &Engine,
    conn: ConnHandle,
    last_rx_hw_ts_ns: &mut Option<u64>,
) -> anyhow::Result<usize> {
    let mut events = engine.events();
    let mut bytes: usize = 0;
    while let Some(ev) = events.pop() {
        match ev {
            InternalEvent::Readable {
                conn: ch,
                total_len,
                rx_hw_ts_ns,
                ..
            } if ch == conn => {
                bytes = bytes.saturating_add(total_len as usize);
                *last_rx_hw_ts_ns = Some(rx_hw_ts_ns);
            }
            InternalEvent::Error { conn: ch, err, .. } if ch == conn => {
                anyhow::bail!("tcp error during recv: errno={err}");
            }
            InternalEvent::Closed { conn: ch, err, .. } if ch == conn => {
                anyhow::bail!("connection closed during recv: err={err}");
            }
            _ => {
                // Unrelated event kinds — drop.
            }
        }
    }
    Ok(bytes)
}

/// Convert a TSC-cycle delta to nanoseconds. u128 intermediate to avoid
/// overflow at realistic durations.
pub fn tsc_delta_to_ns(t0: u64, t1: u64, tsc_hz: u64) -> u64 {
    let delta = t1.wrapping_sub(t0);
    ((delta as u128).saturating_mul(1_000_000_000u128) / tsc_hz as u128) as u64
}

// ---------------------------------------------------------------------------
// Connection bring-up — retry-on-PeerUnreachable until gateway ARP
// resolves, then drive poll_once until Connected is observed.
// ---------------------------------------------------------------------------

/// Open a TCP connection to the peer. Retry `connect` on
/// `PeerUnreachable` (gateway MAC not yet learned), then drive
/// `poll_once` until the `Connected` event arrives.
pub fn open_connection(
    engine: &Engine,
    peer_ip: u32,
    peer_port: u16,
) -> anyhow::Result<ConnHandle> {
    let handle = retry_on_peer_unreachable(
        CONNECT_TIMEOUT,
        std::time::Duration::from_millis(10),
        || engine.connect(peer_ip, peer_port, 0),
        || {
            engine.poll_once();
        },
    )?;

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        engine.poll_once();
        if drain_until_connected_or_error(engine, handle)? {
            return Ok(handle);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("connect timeout after {:?}", CONNECT_TIMEOUT);
        }
    }
}

fn retry_on_peer_unreachable<T, F, B>(
    timeout: std::time::Duration,
    sleep_dur: std::time::Duration,
    mut op: F,
    mut between: B,
) -> anyhow::Result<T>
where
    F: FnMut() -> Result<T, Error>,
    B: FnMut(),
{
    let start = std::time::Instant::now();
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(Error::PeerUnreachable(_)) => {
                between();
                if start.elapsed() > timeout {
                    anyhow::bail!("gateway ARP did not resolve within {:?}", timeout);
                }
                std::thread::sleep(sleep_dur);
            }
            Err(e) => anyhow::bail!("engine.connect failed: {e:?}"),
        }
    }
}

fn drain_until_connected_or_error(
    engine: &Engine,
    handle: ConnHandle,
) -> anyhow::Result<bool> {
    let mut events = engine.events();
    while let Some(ev) = events.pop() {
        match ev {
            InternalEvent::Connected { conn, .. } if conn == handle => return Ok(true),
            InternalEvent::Error { conn, err, .. } if conn == handle => {
                anyhow::bail!("connect error: errno={err}");
            }
            InternalEvent::Closed { conn, err, .. } if conn == handle => {
                anyhow::bail!("connection closed during handshake: err={err}");
            }
            _ => {
                // Ignore state-change / writable / other-handle events.
            }
        }
    }
    Ok(false)
}

/// Run `iterations` measured request-response round-trips and return
/// the raw RTT samples in nanoseconds plus the count of iterations
/// that hit a per-iter timeout (and were skipped rather than aborting
/// the bucket). `warmup` iterations are discarded; a warmup failure
/// still aborts the run because the bucket isn't yet primed.
///
/// Closes C-D3: previously a single per-iter `?` propagation killed
/// the entire scenario, dropping all earlier samples. Now we count
/// failed iters into a column and only abort if more than 50% of the
/// measurement iterations fail (the bucket is genuinely wedged).
///
/// The outer harness (bench-rtt main, bench-stress) owns the
/// connection lifetime + CSV emit + counter-delta plumbing; this
/// helper is the pure workload inner loop.
pub fn run_rtt_workload(
    engine: &Engine,
    conn: ConnHandle,
    request_bytes: usize,
    response_bytes: usize,
    tsc_hz: u64,
    warmup: u64,
    iterations: u64,
) -> anyhow::Result<(Vec<f64>, u64)> {
    let request = vec![0u8; request_bytes];
    let mut carry_forward: usize = 0;

    for i in 0..warmup {
        request_response_attributed(
            engine,
            conn,
            &request,
            response_bytes,
            tsc_hz,
            &mut carry_forward,
        )
        .with_context(|| format!("warmup iteration {i}"))?;
    }

    run_rtt_measurement_loop(iterations, |i| {
        request_response_attributed(
            engine,
            conn,
            &request,
            response_bytes,
            tsc_hz,
            &mut carry_forward,
        )
        .with_context(|| format!("measurement iteration {i}"))
        .map(|rec| rec.rtt_ns as f64)
    })
}

/// Pure measurement-loop that counts per-iter failures into a u64 and
/// only bails if more than 50% of `iterations` failed. Shared between
/// `run_rtt_workload` and any future cross-stack call site.
///
/// The closure receives the iter index and returns either the RTT in
/// ns (as f64) or an error. Errors are logged at eprintln and counted;
/// success values are pushed into the returned sample vec.
///
/// This shape is what makes the "fail-count semantics" testable
/// without a live DPDK engine — the unit test below feeds a closure
/// that fails on a deterministic schedule and asserts the
/// (samples_len, failed_count) pair. See C-D3 in the plan.
pub fn run_rtt_measurement_loop<F>(
    iterations: u64,
    mut step: F,
) -> anyhow::Result<(Vec<f64>, u64)>
where
    F: FnMut(u64) -> anyhow::Result<f64>,
{
    let mut samples: Vec<f64> = Vec::with_capacity(iterations as usize);
    let mut failed: u64 = 0;
    for i in 0..iterations {
        match step(i) {
            Ok(rtt_ns) => samples.push(rtt_ns),
            Err(e) => {
                eprintln!("bench-rtt: iter {i} failed: {e:#}");
                failed += 1;
                if failed > iterations / 2 {
                    anyhow::bail!(
                        "more than 50% of iterations failed ({failed}/{iterations}); \
                         aborting scenario (last error: {e:#})"
                    );
                }
            }
        }
    }
    Ok((samples, failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsc_delta_to_ns_basic() {
        assert_eq!(tsc_delta_to_ns(0, 3_000, 3_000_000_000), 1_000);
        assert_eq!(tsc_delta_to_ns(42, 42, 3_000_000_000), 0);
    }

    #[test]
    fn tsc_delta_to_ns_handles_wrap() {
        let t0 = u64::MAX - 999;
        let t1 = t0.wrapping_add(3_000);
        assert_eq!(tsc_delta_to_ns(t0, t1, 3_000_000_000), 1_000);
    }

    /// C-D3 regression guard: per-iter failures must NOT propagate
    /// out via `?` — they must be counted into a separate `failed`
    /// counter and only abort if more than 50% fail. The 7 success
    /// + 3 failure case here is the canonical "kept what you had"
    /// scenario from the plan.
    #[test]
    fn run_rtt_measurement_loop_keeps_samples_on_per_iter_failure() {
        let mut iter_idx = 0;
        let result = run_rtt_measurement_loop(10, |i| {
            iter_idx = i;
            // Fail iters {2, 5, 8} — same set the plan calls out.
            if i == 2 || i == 5 || i == 8 {
                anyhow::bail!("synthetic per-iter failure at iter={i}")
            }
            Ok(1_000.0)
        });
        let (samples, failed) = result.expect("loop should not bail at 30% failure rate");
        assert_eq!(samples.len(), 7, "successful iters must survive");
        assert_eq!(failed, 3, "failed counter must increment per failure");
    }

    /// Spec gate: more than 50% failures aborts the scenario with a
    /// diagnostic. Boundary case: 6 failures out of 10 is > 5.
    #[test]
    fn run_rtt_measurement_loop_bails_above_50pct_failures() {
        let result = run_rtt_measurement_loop(10, |i| {
            if i < 6 {
                anyhow::bail!("synthetic failure at iter={i}")
            }
            Ok(2_000.0)
        });
        let err = result.expect_err("should bail above 50% failure rate");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("more than 50% of iterations failed"),
            "abort message must reference the threshold: {msg}"
        );
    }

    /// All-success path returns failed=0 and full sample count.
    #[test]
    fn run_rtt_measurement_loop_all_success() {
        let (samples, failed) = run_rtt_measurement_loop(5, |_| Ok(3_000.0)).unwrap();
        assert_eq!(samples.len(), 5);
        assert_eq!(failed, 0);
    }
}
