//! F-Stack max-sustained-throughput runner — comparator arm for spec §11.2.
//!
//! Drives the W × C grid against a live F-Stack peer
//! (`/opt/f-stack-peer/bench-peer` on the baked AMI, port 10003) using
//! `C` persistent F-Stack connections.
//!
//! # Why ff_run-driven state machine
//!
//! F-Stack's BSD-shaped API is NOT usable outside the `ff_run`
//! callback: DPDK packet processing only runs inside ff_run's poll
//! loop, so a call sequence outside it never makes wire progress.
//! Additionally, ff_run calls `rte_eal_cleanup()` on exit, which can
//! only be invoked once per process. Together, these constraints
//! force the entire W × C measurement grid to complete inside a
//! SINGLE ff_run invocation, driven by a state machine that the
//! per-iteration callback advances.
//!
//! # Why F-Stack
//!
//! mTCP upstream is dormant; F-Stack is actively maintained and
//! builds against DPDK 23.11. See `fstack_burst.rs` module docs and
//! `docs/superpowers/reports/perf-a10-mtcp-rebuild-investigation.md`
//! Phase 4 for the full rationale.
//!
//! # Measurement contract
//!
//! Wire goodput = `bytes_echoed_in_window / window_duration_ns`, in bps.
//! `bytes_echoed_in_window` is the running sum of bytes received back
//! from the echo peer during the measurement window — bytes the peer
//! received on the wire and sent back, analogous to dpdk_net's snd_una
//! delta. This is wire-rate comparable across stacks.
//! `bytes_written_in_window` (ff_write bytes) is also recorded for
//! reference; above ~2.5 Gbps it exceeds ENA link capacity and is a
//! buffer-fill artifact, not wire delivery.
//!
//! `pps` is left at 0 — same rationale as `linux_maxtp.rs` (no
//! socket-level segments-out probe). Bench-report can filter F-Stack
//! rows out of pps pivots via `dimensions_json.tx_ts_mode = "n/a"`.
//!
//! # Per-bucket close + drain (parity with dpdk_maxtp)
//!
//! Each bucket opens C fresh sockets and closes them at the end so
//! handles don't leak across buckets — same hygiene the dpdk_maxtp
//! arm does to avoid `InvalidConnHandle` on later buckets.

use std::os::raw::{c_int, c_uint, c_void};
use std::time::{Duration, Instant};

use crate::dpdk_maxtp::TxTsMode;
use crate::fstack_ffi::{
    ff_close, ff_connect, ff_getsockopt, ff_ioctl, ff_poll, ff_read, ff_run, ff_socket,
    ff_stop_run, ff_write, fstack_errno, make_linux_sockaddr_in, AF_INET, FF_EINPROGRESS, FIONBIO,
    POLLOUT, SOCK_STREAM, SO_ERROR, SOL_SOCKET,
};
use crate::fstack_ffi::PollFd;
use crate::maxtp::{Bucket, MaxtpSample};

/// One bucket's raw measurement product. Mirrors `linux_maxtp::BucketRun`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketRun {
    /// `sustained_goodput_bps` is computed from `bytes_echoed_in_window`
    /// (bytes the peer sent back = bytes it received on the wire).
    /// This is wire-rate comparable to dpdk_net's snd_una delta.
    pub sample: MaxtpSample,
    /// Bytes accepted by `ff_write` (buffer-fill rate — may exceed wire rate).
    pub bytes_written_in_window: u64,
    /// Bytes received back from the echo peer (wire-confirmed delivery).
    pub bytes_echoed_in_window: u64,
    pub tx_ts_mode: TxTsMode,
}

/// Outcome of a single bucket inside the grid run. Per-bucket soft-fail.
pub struct MaxtpGridResult {
    pub bucket: Bucket,
    pub result: Result<BucketRun, String>,
}

// ---------------------------------------------------------------------------
// State machine.
// ---------------------------------------------------------------------------

enum Phase {
    /// Open all C sockets, set non-blocking, issue non-blocking connects.
    ConnectAll,
    /// Wait for every connect to complete (poll SO_ERROR per fd).
    /// `deadline` caps how long we wait before failing the bucket — prevents
    /// indefinite hang if DPDK/ARP state is dirty and SYNs never complete.
    WaitConnectAll { checked: usize, deadline: Instant },
    /// Pump writes round-robin during the warmup window.
    Warmup { warmup_deadline: Instant },
    /// Pump writes round-robin during the measurement window.
    /// `bytes_written` and `bytes_echoed` accumulate across callback invocations.
    Measure {
        t_start: Instant,
        measure_deadline: Instant,
        bytes_written: u64,
        bytes_echoed: u64,
    },
    /// Close every fd; advance to next bucket when done.
    CloseAll {
        idx: usize,
        result: Result<BucketRun, String>,
    },
    /// Bucket failed before connections fully opened.
    BucketError(String),
    /// All buckets done — call ff_stop_run() and return.
    Done,
}

struct MaxtpGridState<'a> {
    grid: &'a [Bucket],
    bucket_idx: usize,
    /// Current bucket's connection set. Empty when no bucket is in flight.
    fds: Vec<c_int>,
    phase: Phase,
    /// Output — one entry pushed per finished bucket.
    results: Vec<MaxtpGridResult>,
    /// Static run-wide config.
    warmup: Duration,
    duration: Duration,
    peer_ip_host_order: u32,
    peer_port: u16,
    tx_ts_mode: TxTsMode,
    /// Pre-allocated payload for the CURRENT bucket (resized on bucket entry).
    payload: Vec<u8>,
    /// Shared discard buffer for inbound drain (echo-server replies).
    discard: Vec<u8>,
    /// Set true once ff_stop_run has been called.
    stopped: bool,
}

/// Drive the entire W × C maxtp grid inside a single ff_run invocation.
///
/// `ff_run` is one-shot per process (it calls `rte_eal_cleanup` on
/// exit), so ALL buckets must complete before `ff_stop_run` fires.
pub fn run_maxtp_grid(
    grid: &[Bucket],
    warmup: Duration,
    duration: Duration,
    peer_ip_host_order: u32,
    peer_port: u16,
    tx_ts_mode: TxTsMode,
) -> Vec<MaxtpGridResult> {
    if grid.is_empty() {
        return Vec::new();
    }

    // Initial payload size = first bucket's write_bytes; resized on
    // bucket entry (CloseAll → next ConnectAll transition).
    let initial_payload = vec![0u8; grid[0].write_bytes as usize];

    let mut state = MaxtpGridState {
        grid,
        bucket_idx: 0,
        fds: Vec::new(),
        phase: Phase::ConnectAll,
        results: Vec::with_capacity(grid.len()),
        warmup,
        duration,
        peer_ip_host_order,
        peer_port,
        tx_ts_mode,
        payload: initial_payload,
        discard: vec![0u8; 65536],
        stopped: false,
    };

    // SAFETY: ff_run is synchronous. The stack frame of `state` lives
    // for the entire duration of this unsafe block.
    unsafe {
        let arg = &mut state as *mut MaxtpGridState<'_> as *mut c_void;
        ff_run(maxtp_grid_callback, arg);
    }

    state.results
}

extern "C" fn maxtp_grid_callback(arg: *mut c_void) -> c_int {
    // SAFETY: `arg` came from `&mut MaxtpGridState as *mut _ as *mut c_void`.
    let state = unsafe { &mut *(arg as *mut MaxtpGridState<'_>) };

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

fn advance(state: &mut MaxtpGridState<'_>) -> Step {
    let phase = std::mem::replace(&mut state.phase, Phase::Done);

    match phase {
        Phase::ConnectAll => phase_connect_all(state),
        Phase::WaitConnectAll { checked, deadline } => phase_wait_connect_all(state, checked, deadline),
        Phase::Warmup { warmup_deadline } => phase_warmup(state, warmup_deadline),
        Phase::Measure {
            t_start,
            measure_deadline,
            bytes_written,
            bytes_echoed,
        } => phase_measure(state, t_start, measure_deadline, bytes_written, bytes_echoed),
        Phase::CloseAll { idx, result } => phase_close_all(state, idx, result),
        Phase::BucketError(msg) => phase_bucket_error(state, msg),
        Phase::Done => phase_done(state),
    }
}

fn phase_connect_all(state: &mut MaxtpGridState<'_>) -> Step {
    let bucket = state.grid[state.bucket_idx];

    // Resize per-bucket payload for the current write_bytes.
    state.payload.clear();
    state.payload.resize(bucket.write_bytes as usize, 0);

    // Build C fds. On any failure mid-loop, close what we've already
    // opened and route to BucketError.
    let mut fds: Vec<c_int> = Vec::with_capacity(bucket.conn_count as usize);
    for i in 0..bucket.conn_count {
        let fd = unsafe { ff_socket(AF_INET as c_int, SOCK_STREAM, 0) };
        if fd < 0 {
            let errno = fstack_errno();
            for &fd in &fds {
                let _ = unsafe { ff_close(fd) };
            }
            state.phase =
                Phase::BucketError(format!("ff_socket on conn {i} failed: errno={errno}"));
            return Step::Continue;
        }
        // Non-blocking BEFORE connect so the handshake can be polled
        // through ff_run rather than blocking the callback.
        let on: c_int = 1;
        let rc = unsafe { ff_ioctl(fd, FIONBIO, &on as *const c_int) };
        if rc != 0 {
            let errno = fstack_errno();
            let _ = unsafe { ff_close(fd) };
            for &fd in &fds {
                let _ = unsafe { ff_close(fd) };
            }
            state.phase = Phase::BucketError(format!(
                "ff_ioctl(FIONBIO) on conn {i} failed: rc={rc} errno={errno}"
            ));
            return Step::Continue;
        }
        let sa = make_linux_sockaddr_in(state.peer_ip_host_order, state.peer_port);
        let rc = unsafe { ff_connect(fd, &sa, std::mem::size_of_val(&sa) as u32) };
        if rc != 0 {
            let errno = fstack_errno();
            if errno != FF_EINPROGRESS {
                let _ = unsafe { ff_close(fd) };
                for &fd in &fds {
                    let _ = unsafe { ff_close(fd) };
                }
                state.phase = Phase::BucketError(format!(
                    "ff_connect on conn {i} failed: rc={rc} errno={errno} \
                     (expected FF_EINPROGRESS={FF_EINPROGRESS})"
                ));
                return Step::Continue;
            }
        }
        fds.push(fd);
    }
    state.fds = fds;
    state.phase = Phase::WaitConnectAll {
        checked: 0,
        deadline: Instant::now() + Duration::from_secs(30),
    };
    // Yield once so the kernel has a chance to advance the handshakes
    // before we start polling SO_ERROR.
    Step::Yield
}

fn phase_wait_connect_all(state: &mut MaxtpGridState<'_>, mut checked: usize, deadline: Instant) -> Step {
    if Instant::now() > deadline {
        state.phase = Phase::BucketError(
            "connect timeout: connections did not complete within 30 s \
             (DPDK/ARP state may be dirty — clean /run/dpdk/rte/ and retry)"
                .into(),
        );
        return Step::Continue;
    }
    while checked < state.fds.len() {
        let fd = state.fds[checked];
        // Poll POLLOUT to detect handshake completion — SO_ERROR alone
        // returns 0 for both "still connecting" and "connected".
        let mut pfd = PollFd { fd, events: POLLOUT, revents: 0 };
        let n = unsafe { ff_poll(&mut pfd, 1, 0) };
        if n < 0 {
            let errno = fstack_errno();
            state.phase = Phase::BucketError(format!("ff_poll failed: errno={errno}"));
            return Step::Continue;
        }
        if n == 0 || (pfd.revents & POLLOUT) == 0 {
            // This connection not ready yet; yield and retry all from here.
            state.phase = Phase::WaitConnectAll { checked, deadline };
            return Step::Yield;
        }
        // POLLOUT: check SO_ERROR for outcome.
        let mut sock_err: c_int = 0;
        let mut len: c_uint = std::mem::size_of::<c_int>() as c_uint;
        let rc = unsafe {
            ff_getsockopt(
                fd,
                SOL_SOCKET,
                SO_ERROR,
                &mut sock_err as *mut c_int as *mut c_void,
                &mut len as *mut c_uint,
            )
        };
        if rc != 0 {
            let errno = fstack_errno();
            state.phase = Phase::BucketError(format!(
                "ff_getsockopt(SO_ERROR) on conn {checked} failed: rc={rc} errno={errno}"
            ));
            return Step::Continue;
        }
        if sock_err == 0 {
            checked += 1;
            continue;
        }
        state.phase = Phase::BucketError(format!(
            "connect SO_ERROR={sock_err} on conn {checked}"
        ));
        return Step::Continue;
    }
    // All connections established.
    state.phase = Phase::Warmup {
        warmup_deadline: Instant::now() + state.warmup,
    };
    Step::Continue
}

fn phase_warmup(state: &mut MaxtpGridState<'_>, warmup_deadline: Instant) -> Step {
    // Pump for a single sweep across all fds, then re-check the deadline.
    let _ = pump_round_robin_once(&state.fds, &state.payload, &mut state.discard);
    if Instant::now() >= warmup_deadline {
        let now = Instant::now();
        state.phase = Phase::Measure {
            t_start: now,
            measure_deadline: now + state.duration,
            bytes_written: 0,
            bytes_echoed: 0,
        };
        return Step::Continue;
    }
    state.phase = Phase::Warmup { warmup_deadline };
    // Yield to let the kernel drain ACKs / fill recv buffer between sweeps.
    Step::Yield
}

fn phase_measure(
    state: &mut MaxtpGridState<'_>,
    t_start: Instant,
    measure_deadline: Instant,
    mut bytes_written: u64,
    mut bytes_echoed: u64,
) -> Step {
    let (written, echoed) = pump_round_robin_once(&state.fds, &state.payload, &mut state.discard);
    bytes_written = bytes_written.saturating_add(written);
    bytes_echoed = bytes_echoed.saturating_add(echoed);
    if Instant::now() >= measure_deadline {
        let elapsed_ns = Instant::now()
            .saturating_duration_since(t_start)
            .as_nanos() as u64;
        let elapsed_ns = elapsed_ns.max(1);
        // Wire-rate goodput: bytes the peer echoed back = bytes it received on wire.
        // Comparable to dpdk_net's snd_una delta over the same window.
        let sample = MaxtpSample::from_window(bytes_echoed, 0, elapsed_ns);
        let run = BucketRun {
            sample,
            bytes_written_in_window: bytes_written,
            bytes_echoed_in_window: bytes_echoed,
            tx_ts_mode: state.tx_ts_mode,
        };
        state.phase = Phase::CloseAll {
            idx: 0,
            result: Ok(run),
        };
        return Step::Continue;
    }
    state.phase = Phase::Measure {
        t_start,
        measure_deadline,
        bytes_written,
        bytes_echoed,
    };
    Step::Yield
}

fn phase_close_all(
    state: &mut MaxtpGridState<'_>,
    mut idx: usize,
    result: Result<BucketRun, String>,
) -> Step {
    while idx < state.fds.len() {
        let _ = unsafe { ff_close(state.fds[idx]) };
        idx += 1;
    }
    let bucket = state.grid[state.bucket_idx];
    state.fds.clear();
    state.results.push(MaxtpGridResult { bucket, result });
    state.bucket_idx += 1;
    if state.bucket_idx < state.grid.len() {
        state.phase = Phase::ConnectAll;
    } else {
        state.phase = Phase::Done;
    }
    Step::Continue
}

fn phase_bucket_error(state: &mut MaxtpGridState<'_>, msg: String) -> Step {
    // ConnectAll already closed any partial fds before raising
    // BucketError, so state.fds is either fully populated (failure
    // detected mid-handshake) or empty (failure during socket open).
    for &fd in &state.fds {
        let _ = unsafe { ff_close(fd) };
    }
    state.fds.clear();
    let bucket = state.grid[state.bucket_idx];
    state.results.push(MaxtpGridResult {
        bucket,
        result: Err(msg),
    });
    state.bucket_idx += 1;
    if state.bucket_idx < state.grid.len() {
        state.phase = Phase::ConnectAll;
    } else {
        state.phase = Phase::Done;
    }
    Step::Continue
}

fn phase_done(state: &mut MaxtpGridState<'_>) -> Step {
    if !state.stopped {
        unsafe { ff_stop_run() };
        state.stopped = true;
    }
    state.phase = Phase::Done;
    Step::Stopped
}

/// One round-robin sweep across every connection: drain inbound first,
/// then issue a single non-blocking write attempt. Returns
/// `(bytes_written, bytes_echoed)` where `bytes_echoed` is the count
/// of echo bytes received from the peer — wire-confirmed delivery,
/// comparable to dpdk_net's snd_una delta.
fn pump_round_robin_once(fds: &[c_int], payload: &[u8], discard: &mut [u8]) -> (u64, u64) {
    if fds.is_empty() || payload.is_empty() {
        return (0, 0);
    }
    let mut total_written: u64 = 0;
    let mut total_echoed: u64 = 0;
    for &fd in fds {
        // Drain inbound (non-blocking). Cap at 8 reads per conn per
        // sweep so a fast peer cannot starve other conns this callback.
        // Break only on EAGAIN (n ≤ 0); a partial read does NOT indicate
        // the recv buffer is drained.
        for _ in 0..8 {
            let n = unsafe { ff_read(fd, discard.as_mut_ptr() as *mut c_void, discard.len()) };
            if n <= 0 {
                break; // EAGAIN or EOF
            }
            total_echoed = total_echoed.saturating_add(n as u64);
        }
        // Single non-blocking write attempt.
        let n = unsafe { ff_write(fd, payload.as_ptr() as *const c_void, payload.len()) };
        if n > 0 {
            total_written = total_written.saturating_add(n as u64);
        }
        // n <= 0 → EAGAIN or per-conn error; round-robin continues.
    }
    (total_written, total_echoed)
}

// ---------------------------------------------------------------------------
// Helpers retained from the old API.
// ---------------------------------------------------------------------------

/// Format an IP host-order u32 as dotted-quad for log messages.
#[allow(dead_code)]
fn format_ip_host_order(ip: u32) -> String {
    let b = ip.to_be_bytes();
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `format_ip_host_order` produces dotted-quad — verify the byte
    /// order matches `Ipv4Addr::to_string()`.
    #[test]
    fn format_ip_host_order_dotted_quad() {
        assert_eq!(format_ip_host_order(0x0A_00_00_2A), "10.0.0.42");
        assert_eq!(format_ip_host_order(0xC0_A8_01_0A), "192.168.1.10");
    }
}
