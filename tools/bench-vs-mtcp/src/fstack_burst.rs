//! F-Stack burst-workload runner — comparator arm for spec §11.1.
//!
//! Drives the K × G grid against a live F-Stack peer
//! (`/opt/f-stack-peer/bench-peer` on the baked AMI, port 10003 by
//! default) using one F-Stack connection per bucket.
//!
//! # Why ff_run-driven state machine
//!
//! F-Stack's BSD-shaped API (`ff_socket`/`ff_connect`/`ff_write`/...) is
//! NOT usable outside the `ff_run` callback: DPDK packet processing
//! only runs inside ff_run's poll loop, so a call sequence outside it
//! never makes wire progress. Additionally, ff_run calls
//! `rte_eal_cleanup()` on exit, which can only be invoked once per
//! process. Together, these constraints force the entire K × G
//! measurement grid to complete inside a SINGLE ff_run invocation,
//! driven by a state machine that the per-iteration callback advances.
//!
//! # Why F-Stack vs mTCP
//!
//! mTCP upstream is dormant (DPDK 18.05/19.08 only, last meaningful
//! commit 2021). F-Stack is actively maintained, ports the FreeBSD
//! 13 TCP stack to userspace on DPDK, and builds against DPDK 23.11.
//! The 2026-04-29 mTCP rebuild investigation
//! (`docs/superpowers/reports/perf-a10-mtcp-rebuild-investigation.md`)
//! flagged F-Stack as the highest-value alternative; this module
//! implements the comparator.
//!
//! # Measurement contract
//!
//! Mirrors `dpdk_burst.rs`'s shape. Per burst:
//! - `t0` = inline TSC pre-first-`ff_write`.
//! - `t_first_wire` = TSC right after the first `ff_write` returns.
//!   F-Stack does not expose HW TX-TS (the DPDK `rte_mbuf::tx_timestamp`
//!   dynfield isn't surfaced through the BSD-socket-shaped API), so
//!   `TxTsMode::TscFallback` is the only available source — same as
//!   the dpdk_net arm on ENA.
//! - `t1` = TSC at end-of-drain when the full K bytes have been
//!   accepted by F-Stack.
//!
//! Throughput per burst = K / (t1 − t0), bps.
//!
//! # Soft-fail per-bucket
//!
//! Each bucket's outcome is captured into [`BurstGridResult`]. A
//! bucket-level failure (connect refused, send wedge, etc.) becomes
//! `Err(String)` for that bucket without aborting the rest of the
//! grid — the next bucket opens a fresh connection.

use std::os::raw::{c_int, c_uint, c_void};
use std::time::{Duration, Instant};

use crate::burst::{Bucket, BurstSample};
use crate::dpdk_burst::TxTsMode;
use crate::fstack_ffi::{
    ff_close, ff_connect, ff_getsockopt, ff_ioctl, ff_poll, ff_run, ff_socket, ff_stop_run,
    ff_write, fstack_errno, make_linux_sockaddr_in, AF_INET, FF_EAGAIN, FF_EINPROGRESS, FIONBIO,
    POLLOUT, SOCK_STREAM, SO_ERROR, SOL_SOCKET,
};
use crate::fstack_ffi::PollFd;

/// One bucket's raw measurement product. Mirrors the dpdk_burst shape.
pub struct BucketRun {
    pub samples: Vec<BurstSample>,
    /// Sum of `bucket.burst_bytes` across measurement bursts (warmup
    /// excluded). Caller does not use this for sanity-invariant
    /// (F-Stack doesn't expose a `tx_payload_bytes` counter); kept
    /// for symmetry with dpdk_burst::BucketRun.
    pub sum_over_bursts_bytes: u64,
    pub tx_ts_mode: TxTsMode,
}

/// Outcome of a single bucket inside the grid run. Per-bucket soft-fail:
/// `Err(message)` does not abort siblings.
pub struct BurstGridResult {
    pub bucket: Bucket,
    pub result: Result<BucketRun, String>,
}

// ---------------------------------------------------------------------------
// State machine — driven from ff_run's per-iteration callback.
// ---------------------------------------------------------------------------

/// Phases the state machine cycles through. Each callback invocation
/// performs one or more state transitions until either:
///   - all bytes for the current substep have been issued (advance), or
///   - the F-Stack send buffer would-blocks (return so the next ff_run
///     iteration drains ACKs before retrying).
enum Phase {
    /// Open the socket + ioctl(FIONBIO) + start non-blocking connect.
    Connect,
    /// Non-blocking connect issued; poll SO_ERROR until 0 or non-EAGAIN error.
    WaitConnect,
    /// Pump warmup bursts; no samples recorded.
    Warmup { bursts_done: u64, sent: usize },
    /// Inter-burst gap during warmup.
    WarmupGap { bursts_done: u64, gap_until: Instant },
    /// Capture t0 and start a measurement burst.
    MeasureStart {
        bursts_done: u64,
        samples: Vec<BurstSample>,
        sum: u64,
    },
    /// Mid-measurement burst — accumulate sent bytes, capture t_first_wire
    /// on the first byte sent, capture t1 when done.
    MeasureWrite {
        bursts_done: u64,
        sent: usize,
        t0_tsc: u64,
        t_first_wire: Option<u64>,
        samples: Vec<BurstSample>,
        sum: u64,
    },
    /// Inter-burst gap during measurement.
    MeasureGap {
        bursts_done: u64,
        gap_until: Instant,
        samples: Vec<BurstSample>,
        sum: u64,
    },
    /// Bucket finished cleanly; close fd, push result, advance to next bucket.
    CloseAndNext {
        samples: Vec<BurstSample>,
        sum: u64,
    },
    /// Bucket failed; close fd, push Err, advance to next bucket.
    BucketError(String),
    /// All buckets done — call ff_stop_run() and return.
    Done,
}

/// Mutable state owned by the grid driver and threaded through ff_run via
/// a `*mut c_void`. The state lives on the calling thread's stack for the
/// entire duration of `run_burst_grid`'s `ff_run` invocation.
struct BurstGridState<'a> {
    grid: &'a [Bucket],
    bucket_idx: usize,
    fd: c_int,
    phase: Phase,
    /// Output — one entry pushed per finished bucket.
    results: Vec<BurstGridResult>,
    /// Per-bucket payload (one Vec<u8> per unique burst_bytes value).
    payload_for_bucket: Vec<&'a [u8]>,
    /// Static run-wide config.
    warmup_bursts: u64,
    measure_bursts: u64,
    tsc_hz: u64,
    peer_ip_host_order: u32,
    peer_port: u16,
    tx_ts_mode: TxTsMode,
    /// Set true once ff_stop_run has been called so we don't call it twice.
    stopped: bool,
}

/// Drive the entire K × G burst grid inside a single ff_run invocation.
///
/// `ff_run` is one-shot per process (it calls `rte_eal_cleanup` on
/// exit), so ALL buckets must complete before `ff_stop_run` fires. The
/// callback advances a state machine; control returns to ff_run on
/// EAGAIN so DPDK can drain ACKs before the next attempt.
///
/// Returns one [`BurstGridResult`] per bucket (in the same order). A
/// per-bucket failure (connect refused, send wedge, etc.) is captured
/// as `Err(String)` and does not abort the rest of the grid.
pub fn run_burst_grid(
    grid: &[Bucket],
    warmup: u64,
    bursts: u64,
    tsc_hz: u64,
    peer_ip_host_order: u32,
    peer_port: u16,
    tx_ts_mode: TxTsMode,
) -> Vec<BurstGridResult> {
    if grid.is_empty() {
        return Vec::new();
    }

    // Pre-allocate one zero-filled payload per unique burst_bytes value.
    // The state machine references these by index into `payload_for_bucket`,
    // which is the same length as the grid (so each bucket index maps
    // directly to its payload via `grid[bucket_idx]`'s burst_bytes).
    let mut unique_sizes: Vec<u64> = grid.iter().map(|b| b.burst_bytes).collect();
    unique_sizes.sort_unstable();
    unique_sizes.dedup();
    let payload_storage: Vec<Vec<u8>> = unique_sizes
        .iter()
        .map(|&n| vec![0u8; n as usize])
        .collect();
    // Map bucket index → which storage slot holds its payload.
    let payload_for_bucket: Vec<&[u8]> = grid
        .iter()
        .map(|b| {
            let idx = unique_sizes
                .iter()
                .position(|&n| n == b.burst_bytes)
                .expect("dedup invariant: every bucket size is present");
            payload_storage[idx].as_slice()
        })
        .collect();

    let mut state = BurstGridState {
        grid,
        bucket_idx: 0,
        fd: -1,
        phase: Phase::Connect,
        results: Vec::with_capacity(grid.len()),
        payload_for_bucket,
        warmup_bursts: warmup,
        measure_bursts: bursts,
        tsc_hz,
        peer_ip_host_order,
        peer_port,
        tx_ts_mode,
        stopped: false,
    };

    // SAFETY: ff_run is synchronous; it blocks until the callback calls
    // ff_stop_run and the inner poll loop unwinds. The stack frame of
    // `state` therefore lives for the entire duration of this unsafe
    // block, so the raw pointer remains valid for every callback
    // invocation.
    unsafe {
        let arg = &mut state as *mut BurstGridState<'_> as *mut c_void;
        ff_run(burst_grid_callback, arg);
    }

    state.results
}

// ---------------------------------------------------------------------------
// Callback — entered once per ff_run poll iteration.
// ---------------------------------------------------------------------------

/// `ff_run` invokes this once per poll iteration. The function is
/// `extern "C" fn` (safe pointer); the unsafe blocks are inside the body
/// for the FFI calls themselves.
extern "C" fn burst_grid_callback(arg: *mut c_void) -> c_int {
    // SAFETY: `arg` came from `&mut BurstGridState as *mut _ as *mut c_void`
    // in `run_burst_grid`. ff_run is synchronous so the stack frame is
    // still alive; nothing else aliases the pointer.
    let state = unsafe { &mut *(arg as *mut BurstGridState<'_>) };

    // Once we've called ff_stop_run, ff_run may invoke us once or twice
    // more before unwinding. Short-circuit in that window.
    if state.stopped {
        return 0;
    }

    // Drive the state machine. Many transitions can happen in a single
    // callback invocation (e.g. Connect → WaitConnect → Warmup all in
    // one go if connect completes synchronously). We only `return 0`
    // when we hit EAGAIN or a wait-gate (gap_until / WaitConnect retry).
    loop {
        match advance(state) {
            Step::Continue => continue,
            Step::Yield => return 0,
            Step::Stopped => return 0,
        }
    }
}

/// Single-step result for the inner state-machine advance.
enum Step {
    /// Made progress; call `advance` again in the same callback invocation.
    Continue,
    /// Would-block / wait-gate; return from the callback.
    Yield,
    /// `ff_stop_run` was called; return from the callback.
    Stopped,
}

/// Take ONE state-machine step. Many transitions can be chained in a
/// single callback by looping on `Continue`.
fn advance(state: &mut BurstGridState<'_>) -> Step {
    // We need to swap the phase out of the struct so we can match-and-replace
    // by value (avoiding the pattern of cloning Vec<BurstSample> per step).
    // The phase is restored at the end of each match arm.
    let phase = std::mem::replace(&mut state.phase, Phase::Done);

    match phase {
        Phase::Connect => phase_connect(state),
        Phase::WaitConnect => phase_wait_connect(state),
        Phase::Warmup { bursts_done, sent } => phase_warmup(state, bursts_done, sent),
        Phase::WarmupGap {
            bursts_done,
            gap_until,
        } => phase_warmup_gap(state, bursts_done, gap_until),
        Phase::MeasureStart {
            bursts_done,
            samples,
            sum,
        } => phase_measure_start(state, bursts_done, samples, sum),
        Phase::MeasureWrite {
            bursts_done,
            sent,
            t0_tsc,
            t_first_wire,
            samples,
            sum,
        } => phase_measure_write(state, bursts_done, sent, t0_tsc, t_first_wire, samples, sum),
        Phase::MeasureGap {
            bursts_done,
            gap_until,
            samples,
            sum,
        } => phase_measure_gap(state, bursts_done, gap_until, samples, sum),
        Phase::CloseAndNext { samples, sum } => phase_close_and_next(state, samples, sum),
        Phase::BucketError(msg) => phase_bucket_error(state, msg),
        Phase::Done => phase_done(state),
    }
}

fn phase_connect(state: &mut BurstGridState<'_>) -> Step {
    let fd = unsafe { ff_socket(AF_INET as c_int, SOCK_STREAM, 0) };
    if fd < 0 {
        let errno = fstack_errno();
        state.phase = Phase::BucketError(format!("ff_socket failed: errno={errno}"));
        return Step::Continue;
    }
    state.fd = fd;

    // Set non-blocking BEFORE connect so we can drive the handshake
    // asynchronously inside ff_run's poll loop.
    let on: c_int = 1;
    let rc = unsafe { ff_ioctl(fd, FIONBIO, &on as *const c_int) };
    if rc != 0 {
        let errno = fstack_errno();
        state.phase =
            Phase::BucketError(format!("ff_ioctl(FIONBIO) failed: rc={rc} errno={errno}"));
        return Step::Continue;
    }

    // Issue the non-blocking connect.
    let sa = make_linux_sockaddr_in(state.peer_ip_host_order, state.peer_port);
    let rc = unsafe { ff_connect(fd, &sa, std::mem::size_of_val(&sa) as u32) };
    if rc == 0 {
        // Synchronous-completion fast path: connect already done.
        state.phase = Phase::Warmup {
            bursts_done: 0,
            sent: 0,
        };
        return Step::Continue;
    }
    let errno = fstack_errno();
    if errno == FF_EINPROGRESS {
        state.phase = Phase::WaitConnect;
        // No data to send yet; yield so the kernel can advance the handshake.
        return Step::Yield;
    }
    state.phase = Phase::BucketError(format!(
        "ff_connect failed: rc={rc} errno={errno} (expected FF_EINPROGRESS={FF_EINPROGRESS})"
    ));
    Step::Continue
}

fn phase_wait_connect(state: &mut BurstGridState<'_>) -> Step {
    // Use ff_poll(POLLOUT, timeout=0) to detect when the non-blocking
    // connect completes. SO_ERROR alone is unreliable: it returns 0 both
    // while the handshake is in flight (SYN_SENT) and after it succeeds,
    // making the two states indistinguishable without a writability check.
    let mut pfd = PollFd { fd: state.fd, events: POLLOUT, revents: 0 };
    let n = unsafe { ff_poll(&mut pfd, 1, 0) };
    if n < 0 {
        let errno = fstack_errno();
        state.phase = Phase::BucketError(format!("ff_poll failed: errno={errno}"));
        return Step::Continue;
    }
    if n == 0 || (pfd.revents & POLLOUT) == 0 {
        // Not writable yet — handshake still in flight.
        state.phase = Phase::WaitConnect;
        return Step::Yield;
    }
    // POLLOUT fired: handshake complete or failed. Check SO_ERROR.
    let mut sock_err: c_int = 0;
    let mut len: c_uint = std::mem::size_of::<c_int>() as c_uint;
    let rc = unsafe {
        ff_getsockopt(
            state.fd,
            SOL_SOCKET,
            SO_ERROR,
            &mut sock_err as *mut c_int as *mut c_void,
            &mut len as *mut c_uint,
        )
    };
    if rc != 0 {
        let errno = fstack_errno();
        state.phase =
            Phase::BucketError(format!("ff_getsockopt(SO_ERROR) failed: rc={rc} errno={errno}"));
        return Step::Continue;
    }
    if sock_err == 0 {
        state.phase = Phase::Warmup { bursts_done: 0, sent: 0 };
        return Step::Continue;
    }
    state.phase = Phase::BucketError(format!("connect SO_ERROR={sock_err}"));
    Step::Continue
}

fn phase_warmup(state: &mut BurstGridState<'_>, mut bursts_done: u64, mut sent: usize) -> Step {
    let payload = state.payload_for_bucket[state.bucket_idx];
    if payload.is_empty() {
        // 0-byte burst is meaningless; treat as already done.
        bursts_done = state.warmup_bursts;
    }

    // Try to push as much of the current burst as possible in this callback.
    while bursts_done < state.warmup_bursts {
        let made_progress_or_eagain = pump_one_burst(state.fd, payload, &mut sent, &mut None);
        match made_progress_or_eagain {
            PumpStep::Progress => {
                if sent == payload.len() {
                    // Burst complete. Advance to next warmup burst.
                    bursts_done = bursts_done.saturating_add(1);
                    sent = 0;
                    if bursts_done == state.warmup_bursts {
                        break;
                    }
                    let gap_ms = state.grid[state.bucket_idx].gap_ms;
                    if gap_ms > 0 {
                        state.phase = Phase::WarmupGap {
                            bursts_done,
                            gap_until: Instant::now() + Duration::from_millis(gap_ms),
                        };
                        return Step::Yield;
                    }
                    // gap=0: continue immediately.
                    continue;
                }
                // Partial burst — loop to try the remainder right away.
                continue;
            }
            PumpStep::WouldBlock => {
                // EAGAIN — yield to let the kernel drain.
                state.phase = Phase::Warmup { bursts_done, sent };
                return Step::Yield;
            }
            PumpStep::Error(msg) => {
                state.phase = Phase::BucketError(msg);
                return Step::Continue;
            }
        }
    }

    // Warmup complete; transition to measurement.
    state.phase = Phase::MeasureStart {
        bursts_done: 0,
        samples: Vec::with_capacity(state.measure_bursts as usize),
        sum: 0,
    };
    Step::Continue
}

fn phase_warmup_gap(state: &mut BurstGridState<'_>, bursts_done: u64, gap_until: Instant) -> Step {
    if Instant::now() >= gap_until {
        state.phase = Phase::Warmup {
            bursts_done,
            sent: 0,
        };
        return Step::Continue;
    }
    state.phase = Phase::WarmupGap {
        bursts_done,
        gap_until,
    };
    Step::Yield
}

fn phase_measure_start(
    state: &mut BurstGridState<'_>,
    bursts_done: u64,
    samples: Vec<BurstSample>,
    sum: u64,
) -> Step {
    let t0_tsc = dpdk_net_core::clock::rdtsc();
    state.phase = Phase::MeasureWrite {
        bursts_done,
        sent: 0,
        t0_tsc,
        t_first_wire: None,
        samples,
        sum,
    };
    Step::Continue
}

#[allow(clippy::too_many_arguments)]
fn phase_measure_write(
    state: &mut BurstGridState<'_>,
    bursts_done: u64,
    mut sent: usize,
    t0_tsc: u64,
    mut t_first_wire: Option<u64>,
    mut samples: Vec<BurstSample>,
    mut sum: u64,
) -> Step {
    let bucket = state.grid[state.bucket_idx];
    let payload = state.payload_for_bucket[state.bucket_idx];

    if payload.is_empty() {
        // Cannot meaningfully measure a 0-byte burst — skip the bucket.
        state.phase = Phase::BucketError("burst_bytes=0 cannot be measured".to_string());
        return Step::Continue;
    }

    // Drive remaining bytes for this burst.
    let pump = pump_one_burst(state.fd, payload, &mut sent, &mut t_first_wire);
    match pump {
        PumpStep::Progress => {
            if sent < payload.len() {
                // Partial — loop and retry the remainder this same callback.
                state.phase = Phase::MeasureWrite {
                    bursts_done,
                    sent,
                    t0_tsc,
                    t_first_wire,
                    samples,
                    sum,
                };
                return Step::Continue;
            }
            // Burst complete.
            let t1_tsc = dpdk_net_core::clock::rdtsc();
            // t_first_wire MUST be Some — pump_one_burst sets it on the
            // first successful write. Defensive fallback: if for any
            // reason it isn't, treat as t0 to keep the row monotonic.
            let t_first_wire_tsc = t_first_wire.unwrap_or(t0_tsc);

            let t0_ns = tsc_to_absolute_ns(t0_tsc, state.tsc_hz);
            let t_first_wire_ns = tsc_to_absolute_ns(t_first_wire_tsc, state.tsc_hz);
            let t1_ns = tsc_to_absolute_ns(t1_tsc, state.tsc_hz);

            if t1_ns <= t0_ns || t_first_wire_ns < t0_ns || t1_ns < t_first_wire_ns {
                eprintln!(
                    "fstack_burst: WARN dropping burst (bursts_done={bursts_done}) — non-monotonic TSC \
                     (t0={t0_ns} t_first_wire={t_first_wire_ns} t1={t1_ns})"
                );
            } else {
                let sample =
                    BurstSample::from_timestamps(bucket.burst_bytes, t0_ns, t_first_wire_ns, t1_ns);
                samples.push(sample);
                sum = sum.saturating_add(bucket.burst_bytes);
            }

            let bursts_done_next = bursts_done.saturating_add(1);
            if bursts_done_next == state.measure_bursts {
                state.phase = Phase::CloseAndNext { samples, sum };
                return Step::Continue;
            }
            if bucket.gap_ms > 0 {
                state.phase = Phase::MeasureGap {
                    bursts_done: bursts_done_next,
                    gap_until: Instant::now() + Duration::from_millis(bucket.gap_ms),
                    samples,
                    sum,
                };
                return Step::Yield;
            }
            // gap=0 → next burst right away.
            state.phase = Phase::MeasureStart {
                bursts_done: bursts_done_next,
                samples,
                sum,
            };
            Step::Continue
        }
        PumpStep::WouldBlock => {
            state.phase = Phase::MeasureWrite {
                bursts_done,
                sent,
                t0_tsc,
                t_first_wire,
                samples,
                sum,
            };
            Step::Yield
        }
        PumpStep::Error(msg) => {
            state.phase = Phase::BucketError(msg);
            Step::Continue
        }
    }
}

fn phase_measure_gap(
    state: &mut BurstGridState<'_>,
    bursts_done: u64,
    gap_until: Instant,
    samples: Vec<BurstSample>,
    sum: u64,
) -> Step {
    if Instant::now() >= gap_until {
        state.phase = Phase::MeasureStart {
            bursts_done,
            samples,
            sum,
        };
        return Step::Continue;
    }
    state.phase = Phase::MeasureGap {
        bursts_done,
        gap_until,
        samples,
        sum,
    };
    Step::Yield
}

fn phase_close_and_next(
    state: &mut BurstGridState<'_>,
    samples: Vec<BurstSample>,
    sum: u64,
) -> Step {
    let bucket = state.grid[state.bucket_idx];
    if state.fd >= 0 {
        let _ = unsafe { ff_close(state.fd) };
        state.fd = -1;
    }
    state.results.push(BurstGridResult {
        bucket,
        result: Ok(BucketRun {
            samples,
            sum_over_bursts_bytes: sum,
            tx_ts_mode: state.tx_ts_mode,
        }),
    });
    state.bucket_idx += 1;
    if state.bucket_idx < state.grid.len() {
        state.phase = Phase::Connect;
    } else {
        state.phase = Phase::Done;
    }
    Step::Continue
}

fn phase_bucket_error(state: &mut BurstGridState<'_>, msg: String) -> Step {
    let bucket = state.grid[state.bucket_idx];
    if state.fd >= 0 {
        let _ = unsafe { ff_close(state.fd) };
        state.fd = -1;
    }
    state.results.push(BurstGridResult {
        bucket,
        result: Err(msg),
    });
    state.bucket_idx += 1;
    if state.bucket_idx < state.grid.len() {
        state.phase = Phase::Connect;
    } else {
        state.phase = Phase::Done;
    }
    Step::Continue
}

fn phase_done(state: &mut BurstGridState<'_>) -> Step {
    if !state.stopped {
        unsafe { ff_stop_run() };
        state.stopped = true;
    }
    // Restore Done so subsequent callback invocations remain a no-op.
    state.phase = Phase::Done;
    Step::Stopped
}

// ---------------------------------------------------------------------------
// Inner pump — one tight-loop attempt at filling the rest of a burst.
// ---------------------------------------------------------------------------

enum PumpStep {
    /// Made some progress (at least one byte written, or already done).
    Progress,
    /// EAGAIN — send buffer full; caller must yield.
    WouldBlock,
    /// Hard error from F-Stack; bucket is dead.
    Error(String),
}

/// Tight-loop on `ff_write` until the burst is complete OR we hit
/// EAGAIN. `t_first_wire`, when supplied as `&mut Option<u64>`, is set
/// to `rdtsc()` on the first successful write.
fn pump_one_burst(
    fd: c_int,
    payload: &[u8],
    sent: &mut usize,
    t_first_wire: &mut Option<u64>,
) -> PumpStep {
    if *sent >= payload.len() {
        return PumpStep::Progress;
    }
    let mut made_progress = false;
    loop {
        let remaining = &payload[*sent..];
        let n = unsafe { ff_write(fd, remaining.as_ptr() as *const c_void, remaining.len()) };
        if n > 0 {
            if t_first_wire.is_none() {
                *t_first_wire = Some(dpdk_net_core::clock::rdtsc());
            }
            *sent += n as usize;
            made_progress = true;
            if *sent == payload.len() {
                return PumpStep::Progress;
            }
            continue;
        }
        if n < 0 {
            let errno = fstack_errno();
            if errno == FF_EAGAIN {
                // Send buffer full — yield to let DPDK drain ACKs.
                if made_progress {
                    return PumpStep::Progress;
                }
                return PumpStep::WouldBlock;
            }
            return PumpStep::Error(format!("ff_write failed: errno={errno}"));
        }
        // n == 0: unexpected for a TCP socket; treat as transient.
        if made_progress {
            return PumpStep::Progress;
        }
        return PumpStep::WouldBlock;
    }
}

// ---------------------------------------------------------------------------
// Helpers retained from the old API.
// ---------------------------------------------------------------------------

/// TSC → absolute ns; same shape as dpdk_burst::tsc_to_absolute_ns.
fn tsc_to_absolute_ns(tsc: u64, tsc_hz: u64) -> u64 {
    bench_e2e::workload::tsc_delta_to_ns(0, tsc, tsc_hz)
}

/// Format an IP host-order u32 as dotted-quad for log messages.
#[allow(dead_code)]
fn format_ip_host_order(ip: u32) -> String {
    let b = ip.to_be_bytes();
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

/// Close every fd in the slice, ignoring per-fd errors.
#[allow(dead_code)]
fn close_all(conns: &[c_int]) {
    for &fd in conns {
        let _ = unsafe { ff_close(fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tsc_to_absolute_ns` shape matches dpdk_burst (round-trip).
    #[test]
    fn tsc_to_absolute_ns_monotonic() {
        let a = tsc_to_absolute_ns(1_000_000_000, 3_000_000_000);
        let b = tsc_to_absolute_ns(2_000_000_000, 3_000_000_000);
        assert!(b > a);
    }

    #[test]
    fn format_ip_host_order_dotted_quad() {
        assert_eq!(format_ip_host_order(0x0A_00_00_2A), "10.0.0.42");
        assert_eq!(format_ip_host_order(0xC0_A8_01_0A), "192.168.1.10");
    }
}
