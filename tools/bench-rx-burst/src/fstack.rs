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
//! # Regression history — T55 fast-iter 06:09 hang
//!
//! Pre-T55-followup, the state machine had no forward-progress
//! watchdog: a wedged peer (e.g. burst-echo-server stuck in
//! `write()` because a prior bench-rx-burst run got SIGKILLed
//! mid-burst, leaving the peer's accept-side blocked on a dead
//! ESTAB connection) would cause `phase_read_burst` to loop
//! forever on `FF_EAGAIN`. The outer process-level `timeout 300s`
//! eventually SIGKILLed the bench, but the resulting CSV was empty
//! and the peer accumulated yet another wedged connection —
//! cascading the failure into every subsequent fast-iter run.
//!
//! The fix: a per-bucket `last_progress: Instant` watchdog. If no
//! `ff_read` / `ff_write` makes progress for `STALL_TIMEOUT`, the
//! current bucket transitions to `BucketError(stall)` and the
//! state machine advances to the next bucket (closing the wedged
//! fd in the process — peer kernel sees the FIN and unsticks the
//! burst-echo-server's `read_line`). Mirrors the
//! `STALL_TIMEOUT` watchdog in `bench-rx-burst::dpdk::run_one_burst`
//! and the `RTT_TIMEOUT` ceiling in `bench-rtt::fstack::imp`.
//!
//! Structurally the bucket grid was already wired correctly (one
//! `ff_run`, threaded `bucket_idx`) — the hang was a missing
//! liveness check, not a one-shot-`ff_run` bug like the bench-rtt
//! `8d40aa3` regression.
//!
//! # Clock anchor
//!
//! `dut_recv_ns` is `CLOCK_REALTIME` ns since the Unix epoch (via
//! `SystemTime::now()`), matching the linux + dpdk arms' anchor and
//! the peer's `peer_send_ns`. NTP-bounded skew (~100 µs same-AZ) is
//! the absolute-correctness floor; distribution shape (p50/p99) is
//! the headline.
//!
//! # Tracing
//!
//! Set `BENCH_FSTACK_TRACE=1` to enable phase-transition + bucket-
//! boundary markers on stderr (zero cost when unset). Level `=2`
//! also emits per-`ff_read` / per-`ff_write` progress (very noisy;
//! only enable for live debugging of a single small bucket).

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

/// Per-bucket forward-progress watchdog. If no `ff_read` / `ff_write`
/// returns >0 bytes for this long, the current bucket transitions to
/// `BucketError` so the rest of the grid can continue. 30s matches
/// `bench-rx-burst::dpdk::STALL_TIMEOUT`/2 — generous enough for the
/// slowest healthy path (max-bucket 256×256 = 64 KiB ≪ 1 s on a
/// healthy peer) but tight enough to fail fast on a wedged peer
/// (otherwise the outer 300 s `timeout` SIGKILLs the bench, leaves
/// the F-Stack connection in ESTAB on the peer side, and cascades
/// the failure into the next fast-iter run).
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Wall-clock ceiling on the non-blocking connect handshake. Matches
/// the linux_kernel arm's `CONNECT_TIMEOUT`. Independent of
/// `STALL_TIMEOUT` because connect uses POLLOUT polling (not ff_read
/// / ff_write progress), and a wedged peer often shows up at connect
/// time as an accept-queue backup that POLLOUT never fires for.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
    // Per-bucket watchdog: `last_progress` is updated on every
    // successful `ff_read` / `ff_write` (n > 0) and on each bucket
    // boundary. `phase_send_cmd` + `phase_read_burst` bail the
    // current bucket to `BucketError` if `last_progress.elapsed()
    // > STALL_TIMEOUT`. `connect_started_at` is the analogous gate
    // for the non-blocking connect handshake (Connect / WaitConnect
    // phases don't make ff_read/ff_write progress so they need a
    // separate ceiling).
    last_progress: Instant,
    connect_started_at: Instant,
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

    trace_event(&format!(
        "run_grid: entered, grid_len={} warmup={} measure={}",
        grid.len(),
        warmup_bursts,
        measure_bursts
    ));

    let now = Instant::now();
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
        last_progress: now,
        connect_started_at: now,
    };

    trace_event("run_grid: calling ff_run");
    // SAFETY: ff_run is synchronous; it blocks until the callback calls
    // ff_stop_run and the inner poll loop unwinds. The stack frame of
    // `state` lives for the entire duration of this unsafe block, so
    // the raw pointer remains valid for every callback invocation.
    unsafe {
        let arg = &mut state as *mut State<'_> as *mut c_void;
        ff_run(grid_callback, arg);
    }
    trace_event(&format!(
        "run_grid: ff_run returned, results_len={}",
        state.results.len()
    ));

    state.results
}

/// Internal tracing helper — emits `bench-rx-burst: fstack <msg>` to
/// stderr when the `BENCH_FSTACK_TRACE` env var is set. Used to
/// diagnose where the F-Stack state machine is hanging without
/// flooding stderr on production runs (e.g. fast-iter-suite).
///
/// `BENCH_FSTACK_TRACE=1` — coarse markers (phase transitions, bucket
/// boundaries, FFI errors).
/// `BENCH_FSTACK_TRACE=2` — fine-grained markers (per-callback entry,
/// per-ff_read/ff_write progress). VERY noisy.
fn trace_event(msg: &str) {
    if std::env::var_os("BENCH_FSTACK_TRACE").is_some() {
        eprintln!("bench-rx-burst: fstack {}", msg);
    }
}

/// Verbose trace — only emitted at TRACE level >= 2. Used for
/// per-ff_read / per-ff_write markers that would flood the log on a
/// large bucket grid.
fn trace_verbose(msg: &str) {
    if std::env::var("BENCH_FSTACK_TRACE").ok().as_deref() == Some("2") {
        eprintln!("bench-rx-burst: fstack {}", msg);
    }
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
    trace_event(&format!(
        "phase_connect: bucket_idx={} W={} N={}",
        state.bucket_idx,
        state.grid[state.bucket_idx].segment_size,
        state.grid[state.bucket_idx].burst_count
    ));
    // Reset the watchdog clocks at every bucket boundary so a slow
    // previous bucket doesn't blow the next bucket's watchdog before
    // it even starts its handshake.
    let now = Instant::now();
    state.connect_started_at = now;
    state.last_progress = now;

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
        trace_event("phase_connect: synchronous connect, starting first burst");
        // Synchronous-completion fast path.
        start_first_burst(state);
        return Step::Continue;
    }
    let errno = fstack_errno();
    if errno == FF_EINPROGRESS {
        trace_event("phase_connect: connect EINPROGRESS, transitioning to WaitConnect");
        state.phase = Phase::WaitConnect;
        return Step::Yield;
    }
    state.phase = Phase::BucketError(format!(
        "ff_connect failed: rc={rc} errno={errno} (expected FF_EINPROGRESS={FF_EINPROGRESS})"
    ));
    Step::Continue
}

fn phase_wait_connect(state: &mut State<'_>) -> Step {
    if state.connect_started_at.elapsed() > CONNECT_TIMEOUT {
        state.phase = Phase::BucketError(format!(
            "fstack_rx_burst: connect handshake stalled after {:?} (peer accept queue likely backed up by a wedged earlier connection)",
            CONNECT_TIMEOUT
        ));
        return Step::Continue;
    }
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
        trace_event("phase_wait_connect: connect complete, starting first burst");
        // Reset the data-path watchdog: connect succeeded, but it
        // didn't push any payload bytes yet, so `last_progress`
        // should anchor on the moment we cut over to the data path
        // (not the moment we opened the socket).
        state.last_progress = Instant::now();
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
    trace_event(&format!(
        "start_first_burst: bucket_idx={} samples_cap={}",
        state.bucket_idx,
        state.samples.capacity()
    ));
    queue_next_burst(state, 0, true);
}

/// Build the BURST command for the next burst in this bucket and
/// transition to SendCmd. `is_warmup=true` for the warmup phase.
fn queue_next_burst(state: &mut State<'_>, burst_idx: u64, is_warmup: bool) {
    let bucket = state.grid[state.bucket_idx];
    let cmd = format!("BURST {} {}\n", bucket.burst_count, bucket.segment_size);
    trace_event(&format!(
        "queue_next_burst: bucket_idx={} burst_idx={} warmup={} cmd=`{}`",
        state.bucket_idx,
        burst_idx,
        is_warmup,
        cmd.trim_end()
    ));
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
    // Watchdog: if no forward progress (ff_write n>0 OR ff_read n>0)
    // for STALL_TIMEOUT, the bucket is wedged. Bail with a visible
    // error so the rest of the grid keeps going and the outer
    // process-level timeout doesn't have to SIGKILL us.
    if state.last_progress.elapsed() > STALL_TIMEOUT {
        state.phase = Phase::BucketError(format!(
            "fstack_rx_burst: send-cmd stalled after {:?} on burst {} (warmup={}, sent={}/{})",
            STALL_TIMEOUT, burst_idx, is_warmup, sent, cmd_buf.len()
        ));
        return Step::Continue;
    }
    let mut made_progress = false;
    let entry_sent = sent;
    let cmd_len = cmd_buf.len();
    loop {
        if sent >= cmd_buf.len() {
            trace_event(&format!(
                "phase_send_cmd: sent done bucket_idx={} burst_idx={} warmup={}",
                state.bucket_idx, burst_idx, is_warmup
            ));
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
            state.last_progress = Instant::now();
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
                    trace_verbose(&format!(
                        "phase_send_cmd: EAGAIN partial sent={}/{} -> Continue",
                        sent, cmd_len
                    ));
                    return Step::Continue;
                }
                trace_verbose(&format!(
                    "phase_send_cmd: EAGAIN no-progress sent={}/{} entry={} -> Yield",
                    sent, cmd_len, entry_sent
                ));
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
        trace_verbose(&format!(
            "phase_send_cmd: n=0 sent={}/{} -> Yield",
            sent, cmd_len
        ));
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
        trace_event(&format!(
            "phase_read_burst: already drained bucket_idx={} burst_idx={} total={}",
            state.bucket_idx, burst_idx, total
        ));
        // Already drained — advance to next burst / next bucket.
        return advance_post_burst(state, burst_idx, is_warmup);
    }

    // Watchdog: same forward-progress contract as phase_send_cmd —
    // a wedged peer (typical cause: burst-echo-server blocked in
    // `write()` to a dead earlier ESTAB connection from a SIGKILLed
    // prior bench run) shows up here as `ff_read` -> EAGAIN forever.
    // Bail the bucket so the rest of the grid still runs.
    if state.last_progress.elapsed() > STALL_TIMEOUT {
        state.phase = Phase::BucketError(format!(
            "fstack_rx_burst: read-burst stalled after {:?} on burst {} (warmup={}, recvd={}/{} bytes; peer likely wedged from a prior SIGKILLed run)",
            STALL_TIMEOUT, burst_idx, is_warmup, recv_buf.len(), total
        ));
        return Step::Continue;
    }

    let entry_recv = recv_buf.len();
    let mut made_progress = false;
    loop {
        if recv_buf.len() >= total {
            trace_event(&format!(
                "phase_read_burst: drained bucket_idx={} burst_idx={} total={}",
                state.bucket_idx, burst_idx, total
            ));
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
            state.last_progress = Instant::now();

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
                    trace_verbose("phase_read_burst: EAGAIN partial -> Continue");
                    return Step::Continue;
                }
                trace_verbose(&format!(
                    "phase_read_burst: EAGAIN no-progress entry={} total={} -> Yield",
                    entry_recv, total
                ));
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
            trace_event(&format!(
                "advance_post_burst: warmup done bucket_idx={}",
                state.bucket_idx
            ));
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
    trace_event(&format!(
        "phase_bucket_done: bucket_idx={} samples_len={}",
        state.bucket_idx,
        samples.len()
    ));
    state.results.push(GridResult {
        bucket_id: bucket.bucket_id,
        result: Ok(FstackRxBurstRun { samples }),
    });
    finish_bucket_and_advance(state);
    Step::Continue
}

fn phase_bucket_error(state: &mut State<'_>, msg: String) -> Step {
    let bucket = state.grid[state.bucket_idx];
    trace_event(&format!(
        "phase_bucket_error: bucket_idx={} err={}",
        state.bucket_idx, msg
    ));
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
        trace_event(&format!(
            "finish_bucket_and_advance: advancing to bucket_idx={}",
            state.bucket_idx
        ));
        state.phase = Phase::Connect;
    } else {
        trace_event("finish_bucket_and_advance: grid done, transitioning to Done");
        state.phase = Phase::Done;
    }
}

fn phase_done(state: &mut State<'_>) -> Step {
    if !state.stopped {
        trace_event("phase_done: calling ff_stop_run");
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
