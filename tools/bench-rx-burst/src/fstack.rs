//! F-Stack RX-burst arm.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. Drives the peer's
//! `burst-echo-server` over a single F-Stack TCP connection inside a
//! SINGLE `ff_run` invocation: per bucket (W, N) sends `BURST N W\n`,
//! reads N×W bytes back-to-back, captures CLOCK_REALTIME on each
//! `ff_read` return, parses headers, records per-segment latency.
//!
//! # Why ff_run-driven state machine
//!
//! F-Stack's BSD-shaped API is NOT usable outside the `ff_run`
//! callback: DPDK packet processing only runs inside ff_run's poll
//! loop. `ff_run` calls `rte_eal_cleanup()` on exit, so it's
//! one-shot per process. The entire bucket grid + warmup +
//! measurement therefore complete inside one ff_run invocation,
//! driven by a state machine that the per-iteration callback advances.
//!
//! Mirrors `bench-tx-burst::fstack`'s overall shape; the differences
//! are RX-side (read-loop instead of write-loop) and per-segment
//! header parsing on the drained bytes.
//!
//! # Clock anchor
//!
//! `dut_recv_ns` is `CLOCK_REALTIME` ns since the Unix epoch (via
//! `SystemTime::now()`), matching the linux + dpdk arms' anchor and
//! the peer's `peer_send_ns`. NTP-bounded skew (~100 µs same-AZ) is
//! the absolute-correctness floor; distribution shape (p50/p99) is
//! the headline.

#![cfg(feature = "fstack")]

use std::os::raw::{c_int, c_uint, c_void};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::fstack_ffi::{
    ff_close, ff_connect, ff_getsockopt, ff_ioctl, ff_poll, ff_read, ff_run, ff_socket,
    ff_stop_run, ff_write, fstack_errno, make_linux_sockaddr_in, AF_INET, FF_EAGAIN,
    FF_EINPROGRESS, FIONBIO, POLLOUT, SOCK_STREAM, SO_ERROR, SOL_SOCKET,
};
use crate::fstack_ffi::PollFd;
use crate::segment::{parse_burst_chunk, SegmentRecord};

/// One bucket's runtime configuration. The grid is fed all at once
/// to `run_grid` so the entire sweep completes inside a single
/// `ff_run` invocation.
#[derive(Clone, Copy, Debug)]
pub struct FstackBucketCfg {
    pub bucket_id: u32,
    pub segment_size: usize,
    pub burst_count: usize,
}

/// Per-bucket measurement product.
pub struct FstackRxBurstRun {
    pub samples: Vec<SegmentRecord>,
}

/// Outcome of a single bucket inside the grid run. Per-bucket soft-fail:
/// `Err(message)` does NOT abort siblings.
pub struct GridResult {
    pub bucket_id: u32,
    pub result: Result<FstackRxBurstRun, String>,
}

/// Phases the state machine cycles through. Each callback invocation
/// performs one or more transitions until either:
///   - all bytes for the current substep have been issued (advance), or
///   - the F-Stack send/recv buffer would-blocks (return so the next
///     ff_run iteration drains ACKs / RX before retrying).
enum Phase {
    /// Open the socket + ioctl(FIONBIO) + start non-blocking connect.
    Connect,
    /// Non-blocking connect issued; poll POLLOUT until writable, then
    /// check SO_ERROR.
    WaitConnect,
    /// Send the BURST command for the current burst.
    SendCmd {
        burst_idx: u64,
        is_warmup: bool,
        sent: usize,
        cmd_buf: Vec<u8>,
    },
    /// Read bytes back; parse headers as W-byte chunks complete.
    ReadBurst {
        burst_idx: u64,
        is_warmup: bool,
        recv_buf: Vec<u8>,
        next_seg_idx: u64,
    },
    /// Bucket finished cleanly.
    BucketDone,
    /// Bucket failed; record error and move to next bucket.
    BucketError(String),
    /// All buckets done — call `ff_stop_run` and return.
    Done,
}

struct State<'a> {
    grid: &'a [FstackBucketCfg],
    bucket_idx: usize,
    fd: c_int,
    phase: Phase,
    // Per-bucket scratch for measurement records.
    samples: Vec<SegmentRecord>,
    // Run-wide knobs.
    warmup_bursts: u64,
    measure_bursts: u64,
    peer_ip_host_order: u32,
    peer_control_port: u16,
    // Output.
    results: Vec<GridResult>,
    stopped: bool,
}

/// Drive the entire bucket grid inside a single ff_run invocation.
/// Returns one `GridResult` per bucket in input order. Per-bucket
/// failures (connect refused, send wedge, etc.) are captured as
/// `Err(String)` and do NOT abort the rest of the grid.
pub fn run_grid(
    grid: &[FstackBucketCfg],
    warmup_bursts: u64,
    measure_bursts: u64,
    peer_ip_host_order: u32,
    peer_control_port: u16,
) -> Vec<GridResult> {
    if grid.is_empty() {
        return Vec::new();
    }

    let mut state = State {
        grid,
        bucket_idx: 0,
        fd: -1,
        phase: Phase::Connect,
        samples: Vec::new(),
        warmup_bursts,
        measure_bursts,
        peer_ip_host_order,
        peer_control_port,
        results: Vec::with_capacity(grid.len()),
        stopped: false,
    };

    // SAFETY: ff_run is synchronous; it blocks until the callback calls
    // ff_stop_run and the inner poll loop unwinds. The stack frame of
    // `state` lives for the entire duration of this unsafe block, so
    // the raw pointer remains valid for every callback invocation.
    unsafe {
        let arg = &mut state as *mut State<'_> as *mut c_void;
        ff_run(grid_callback, arg);
    }

    state.results
}

extern "C" fn grid_callback(arg: *mut c_void) -> c_int {
    // SAFETY: `arg` came from `&mut State as *mut _ as *mut c_void` in
    // `run_grid`. ff_run is synchronous so the stack frame is alive;
    // nothing else aliases the pointer.
    let state = unsafe { &mut *(arg as *mut State<'_>) };

    if state.stopped {
        return 0;
    }

    loop {
        match advance(state) {
            Step::Continue => continue,
            Step::Yield => return 0,
            Step::Stopped => return 0,
        }
    }
}

enum Step {
    Continue,
    Yield,
    Stopped,
}

fn advance(state: &mut State<'_>) -> Step {
    let phase = std::mem::replace(&mut state.phase, Phase::Done);
    match phase {
        Phase::Connect => phase_connect(state),
        Phase::WaitConnect => phase_wait_connect(state),
        Phase::SendCmd {
            burst_idx,
            is_warmup,
            sent,
            cmd_buf,
        } => phase_send_cmd(state, burst_idx, is_warmup, sent, cmd_buf),
        Phase::ReadBurst {
            burst_idx,
            is_warmup,
            recv_buf,
            next_seg_idx,
        } => phase_read_burst(state, burst_idx, is_warmup, recv_buf, next_seg_idx),
        Phase::BucketDone => phase_bucket_done(state),
        Phase::BucketError(msg) => phase_bucket_error(state, msg),
        Phase::Done => phase_done(state),
    }
}

fn phase_connect(state: &mut State<'_>) -> Step {
    let fd = unsafe { ff_socket(AF_INET as c_int, SOCK_STREAM, 0) };
    if fd < 0 {
        let errno = fstack_errno();
        state.phase = Phase::BucketError(format!("ff_socket failed: errno={errno}"));
        return Step::Continue;
    }
    state.fd = fd;

    let on: c_int = 1;
    let rc = unsafe { ff_ioctl(fd, FIONBIO, &on as *const c_int) };
    if rc != 0 {
        let errno = fstack_errno();
        state.phase =
            Phase::BucketError(format!("ff_ioctl(FIONBIO) failed: rc={rc} errno={errno}"));
        return Step::Continue;
    }

    let sa = make_linux_sockaddr_in(state.peer_ip_host_order, state.peer_control_port);
    let rc = unsafe { ff_connect(fd, &sa, std::mem::size_of_val(&sa) as u32) };
    if rc == 0 {
        // Synchronous-completion fast path.
        start_first_burst(state);
        return Step::Continue;
    }
    let errno = fstack_errno();
    if errno == FF_EINPROGRESS {
        state.phase = Phase::WaitConnect;
        return Step::Yield;
    }
    state.phase = Phase::BucketError(format!(
        "ff_connect failed: rc={rc} errno={errno} (expected FF_EINPROGRESS={FF_EINPROGRESS})"
    ));
    Step::Continue
}

fn phase_wait_connect(state: &mut State<'_>) -> Step {
    let mut pfd = PollFd {
        fd: state.fd,
        events: POLLOUT,
        revents: 0,
    };
    let n = unsafe { ff_poll(&mut pfd, 1, 0) };
    if n < 0 {
        let errno = fstack_errno();
        state.phase = Phase::BucketError(format!("ff_poll failed: errno={errno}"));
        return Step::Continue;
    }
    if n == 0 || (pfd.revents & POLLOUT) == 0 {
        state.phase = Phase::WaitConnect;
        return Step::Yield;
    }
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
        start_first_burst(state);
        return Step::Continue;
    }
    state.phase = Phase::BucketError(format!("connect SO_ERROR={sock_err}"));
    Step::Continue
}

/// Initialise per-bucket scratch and queue the first burst's BURST
/// command. Called once after Connect succeeds and once between
/// buckets via `start_next_bucket`.
fn start_first_burst(state: &mut State<'_>) {
    let bucket = state.grid[state.bucket_idx];
    state.samples = Vec::with_capacity(
        (state.measure_bursts as usize) * bucket.burst_count,
    );
    queue_next_burst(state, 0, true);
}

/// Build the BURST command for the next burst in this bucket and
/// transition to SendCmd. `is_warmup=true` for the warmup phase.
fn queue_next_burst(state: &mut State<'_>, burst_idx: u64, is_warmup: bool) {
    let bucket = state.grid[state.bucket_idx];
    let cmd = format!("BURST {} {}\n", bucket.burst_count, bucket.segment_size);
    state.phase = Phase::SendCmd {
        burst_idx,
        is_warmup,
        sent: 0,
        cmd_buf: cmd.into_bytes(),
    };
}

fn phase_send_cmd(
    state: &mut State<'_>,
    burst_idx: u64,
    is_warmup: bool,
    mut sent: usize,
    cmd_buf: Vec<u8>,
) -> Step {
    let mut made_progress = false;
    loop {
        if sent >= cmd_buf.len() {
            // Command fully sent — switch to read phase.
            state.phase = Phase::ReadBurst {
                burst_idx,
                is_warmup,
                recv_buf: Vec::new(),
                next_seg_idx: 0,
            };
            return Step::Continue;
        }
        let remaining = &cmd_buf[sent..];
        let n = unsafe {
            ff_write(
                state.fd,
                remaining.as_ptr() as *const c_void,
                remaining.len(),
            )
        };
        if n > 0 {
            sent += n as usize;
            made_progress = true;
            continue;
        }
        if n < 0 {
            let errno = fstack_errno();
            if errno == FF_EAGAIN {
                state.phase = Phase::SendCmd {
                    burst_idx,
                    is_warmup,
                    sent,
                    cmd_buf,
                };
                if made_progress {
                    return Step::Continue;
                }
                return Step::Yield;
            }
            state.phase =
                Phase::BucketError(format!("ff_write(BURST cmd) failed: errno={errno}"));
            return Step::Continue;
        }
        // n == 0 — transient.
        state.phase = Phase::SendCmd {
            burst_idx,
            is_warmup,
            sent,
            cmd_buf,
        };
        return Step::Yield;
    }
}

fn phase_read_burst(
    state: &mut State<'_>,
    burst_idx: u64,
    is_warmup: bool,
    mut recv_buf: Vec<u8>,
    mut next_seg_idx: u64,
) -> Step {
    let bucket = state.grid[state.bucket_idx];
    let total = bucket.burst_count * bucket.segment_size;
    let mut scratch = [0u8; 4096];

    if recv_buf.len() >= total {
        // Already drained — advance to next burst / next bucket.
        return advance_post_burst(state, burst_idx, is_warmup);
    }

    let mut made_progress = false;
    loop {
        if recv_buf.len() >= total {
            return advance_post_burst(state, burst_idx, is_warmup);
        }
        let want = (total - recv_buf.len()).min(scratch.len());
        let n = unsafe {
            ff_read(
                state.fd,
                scratch.as_mut_ptr() as *mut c_void,
                want,
            )
        };
        if n > 0 {
            let dut_recv_ns = wall_ns();
            let n = n as usize;
            recv_buf.extend_from_slice(&scratch[..n]);
            made_progress = true;

            if !is_warmup {
                let parsed = parse_burst_chunk(&recv_buf, bucket.segment_size);
                while next_seg_idx < parsed.len() as u64 {
                    let (seq_idx, peer_send_ns) = parsed[next_seg_idx as usize];
                    state.samples.push(SegmentRecord::new(
                        bucket.bucket_id,
                        burst_idx,
                        seq_idx,
                        peer_send_ns,
                        dut_recv_ns,
                    ));
                    next_seg_idx += 1;
                }
            }
            continue;
        }
        if n < 0 {
            let errno = fstack_errno();
            if errno == FF_EAGAIN {
                state.phase = Phase::ReadBurst {
                    burst_idx,
                    is_warmup,
                    recv_buf,
                    next_seg_idx,
                };
                if made_progress {
                    return Step::Continue;
                }
                return Step::Yield;
            }
            state.phase = Phase::BucketError(format!("ff_read failed: errno={errno}"));
            return Step::Continue;
        }
        // n == 0 — peer closed mid-burst.
        state.phase = Phase::BucketError(format!(
            "fstack_rx_burst: peer closed connection mid-burst {} \
             ({}/{} bytes read)",
            burst_idx,
            recv_buf.len(),
            total
        ));
        return Step::Continue;
    }
}

/// Move to the next burst in the bucket, or finish the bucket if the
/// last measurement burst just completed.
fn advance_post_burst(state: &mut State<'_>, burst_idx: u64, is_warmup: bool) -> Step {
    let next_idx = burst_idx + 1;
    if is_warmup {
        if next_idx >= state.warmup_bursts {
            // Warmup done; start measurement.
            queue_next_burst(state, 0, false);
            return Step::Continue;
        }
        queue_next_burst(state, next_idx, true);
        return Step::Continue;
    }
    if next_idx >= state.measure_bursts {
        // Measurement done; finish bucket.
        state.phase = Phase::BucketDone;
        return Step::Continue;
    }
    queue_next_burst(state, next_idx, false);
    Step::Continue
}

fn phase_bucket_done(state: &mut State<'_>) -> Step {
    let bucket = state.grid[state.bucket_idx];
    let samples = std::mem::take(&mut state.samples);
    state.results.push(GridResult {
        bucket_id: bucket.bucket_id,
        result: Ok(FstackRxBurstRun { samples }),
    });
    finish_bucket_and_advance(state);
    Step::Continue
}

fn phase_bucket_error(state: &mut State<'_>, msg: String) -> Step {
    let bucket = state.grid[state.bucket_idx];
    state.results.push(GridResult {
        bucket_id: bucket.bucket_id,
        result: Err(msg),
    });
    state.samples.clear();
    finish_bucket_and_advance(state);
    Step::Continue
}

/// Close the per-bucket fd and either start the next bucket
/// (re-Connect) or move to the Done phase.
fn finish_bucket_and_advance(state: &mut State<'_>) {
    if state.fd >= 0 {
        unsafe {
            let _ = ff_close(state.fd);
        }
        state.fd = -1;
    }
    state.bucket_idx += 1;
    if state.bucket_idx < state.grid.len() {
        state.phase = Phase::Connect;
    } else {
        state.phase = Phase::Done;
    }
}

fn phase_done(state: &mut State<'_>) -> Step {
    if !state.stopped {
        unsafe { ff_stop_run() };
        state.stopped = true;
    }
    state.phase = Phase::Done;
    Step::Stopped
}

/// CLOCK_REALTIME ns reading — same shape as the linux + dpdk arms.
fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// Silence unused-import warnings on the F-Stack-only timing helpers
// when the feature is on but we don't reference them directly.
#[allow(dead_code)]
fn _suppress_unused_import_warnings() {
    let _ = Duration::from_secs(0);
    let _ = Instant::now();
}
