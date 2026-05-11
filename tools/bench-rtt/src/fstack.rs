//! F-Stack RTT path: ff_write request, ff_poll/ff_read until
//! `response_bytes` are returned, capture wall-clock RTT in ns.
//!
//! Mirrors the linux_kernel arm's blocking shape on top of F-Stack's
//! async ff_run callback. F-Stack's BSD-shaped API (`ff_socket`,
//! `ff_connect`, `ff_write`, `ff_read`, `ff_poll`) is NOT usable
//! outside the `ff_run` callback — DPDK packet processing only runs
//! inside ff_run's poll loop, so a call sequence outside it never
//! makes wire progress. Additionally, `ff_run` calls `rte_eal_cleanup()`
//! on exit, which can only fire once per process. The entire RTT
//! workload (warmup + measurement) therefore completes inside a SINGLE
//! ff_run invocation, driven by a per-iteration callback.
//!
//! # Errno + sockopt namespace
//!
//! F-Stack writes Linux-namespace errno values (`FF_EAGAIN=11`,
//! `FF_EINPROGRESS=115`) and supports Linux-namespace
//! `SOL_SOCKET=1`/`SO_ERROR=4` via `ff_getsockopt` (NOT the
//! `_freebsd` variants). This was confirmed in T50 of the bench-vs-mtcp
//! work; we reuse the same namespace constants here via the shared
//! `bench-fstack-ffi` crate (re-exported as `crate::fstack_ffi` for
//! backwards-compatible internal imports).
//!
//! # Connect detection — `ff_poll(POLLOUT, timeout=0)` not SO_ERROR
//!
//! `SO_ERROR` alone is unreliable for non-blocking connect: it returns
//! 0 both while the handshake is in flight (SYN_SENT) and after it
//! succeeds, making the two states indistinguishable without a
//! writability check. Use `ff_poll(POLLOUT, timeout=0)` to detect
//! when the handshake is complete; check `SO_ERROR` afterward to
//! distinguish success from connect-refused.

#[cfg(feature = "fstack")]
pub mod imp {
    use std::os::raw::{c_int, c_uint, c_void};
    use std::time::{Duration, Instant};

    use crate::fstack_ffi::{
        ff_close, ff_connect, ff_getsockopt, ff_ioctl, ff_poll, ff_read, ff_run, ff_socket,
        ff_stop_run, ff_write, fstack_errno, make_linux_sockaddr_in, AF_INET, FF_EAGAIN,
        FF_EINPROGRESS, FIONBIO, POLLOUT, SOCK_STREAM, SO_ERROR, SOL_SOCKET,
    };
    use crate::fstack_ffi::PollFd;

    /// Connect deadline matches the linux_kernel arm. Currently unused
    /// at the call site (the state machine yields back to ff_run rather
    /// than enforcing a wall-clock connect deadline); kept here for
    /// symmetry + a future explicit-bail wiring.
    #[allow(dead_code)]
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Per-iteration RTT ceiling — fail the iter (not the whole run) if
    /// hit. Matches the linux_kernel arm.
    const RTT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Phases of the per-bucket RTT state machine. Each `ff_run`
    /// callback invocation advances the machine until it hits a
    /// would-block (Yield) or completes (Stopped).
    enum Phase {
        /// Open socket, set non-blocking, issue `ff_connect`.
        Connect,
        /// Wait for connect to complete via `ff_poll(POLLOUT)`.
        WaitConnect,
        /// Pump warmup iterations.
        Warmup {
            iter_done: u64,
            sent: usize,
            recvd: usize,
        },
        /// Inter-iter gap during warmup — currently unused (no gap_ms),
        /// kept for symmetry with the burst grid.
        #[allow(dead_code)]
        WarmupGap { iter_done: u64, gap_until: Instant },
        /// Capture wall-clock entry, run measurement iter.
        MeasureStart { iter_done: u64 },
        /// Mid-measurement send phase.
        MeasureWrite {
            iter_done: u64,
            sent: usize,
            t0: Instant,
        },
        /// Mid-measurement receive phase.
        MeasureRead {
            iter_done: u64,
            recvd: usize,
            t0: Instant,
        },
        /// Bucket finished cleanly; close fd + flag completion.
        CloseAndDone,
        /// Bucket failed; close fd + record error.
        BucketError(String),
        /// All done — call `ff_stop_run`.
        Done,
    }

    enum Step {
        Continue,
        Yield,
        Stopped,
    }

    /// Mutable state owned by the RTT driver and threaded through
    /// `ff_run` via a `*mut c_void`. The state lives on the calling
    /// thread's stack for the entire `ff_run` invocation.
    struct State {
        peer_ip_host_order: u32,
        peer_port: u16,
        request: Vec<u8>,
        response_bytes: usize,
        warmup: u64,
        iterations: u64,
        fd: c_int,
        phase: Phase,
        samples: Vec<f64>,
        error: Option<String>,
        stopped: bool,
    }

    /// Drive one RTT workload (warmup + measurement) inside a single
    /// `ff_run` invocation. `ff_run` is one-shot per process, so the
    /// caller (`bench-rtt::main` Task 4.5) must ensure this is the
    /// only ff_run call for the entire bucket sweep — Phase 4 wires
    /// per-payload buckets as additional state-machine sub-steps.
    pub fn run_rtt_workload(
        peer_ip_host_order: u32,
        peer_port: u16,
        request_bytes: usize,
        response_bytes: usize,
        warmup: u64,
        iterations: u64,
    ) -> anyhow::Result<Vec<f64>> {
        let mut state = State {
            peer_ip_host_order,
            peer_port,
            request: vec![0u8; request_bytes],
            response_bytes,
            warmup,
            iterations,
            fd: -1,
            phase: Phase::Connect,
            samples: Vec::with_capacity(iterations as usize),
            error: None,
            stopped: false,
        };

        // SAFETY: ff_run is synchronous; it blocks until the callback
        // calls ff_stop_run and the inner poll loop unwinds. The
        // stack frame of `state` therefore lives for the entire
        // duration of this unsafe block.
        unsafe {
            let arg = &mut state as *mut State as *mut c_void;
            ff_run(rtt_callback, arg);
        }

        if let Some(err) = state.error {
            anyhow::bail!("fstack RTT workload failed: {err}");
        }
        Ok(state.samples)
    }

    /// `ff_run` invokes this once per poll iteration. Drive the state
    /// machine until we yield (EAGAIN / wait gate) or finish.
    extern "C" fn rtt_callback(arg: *mut c_void) -> c_int {
        // SAFETY: `arg` came from `&mut State as *mut _ as *mut c_void`.
        // ff_run is synchronous so the frame is alive; nothing aliases.
        let state = unsafe { &mut *(arg as *mut State) };

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

    /// One state-machine step. Many transitions can chain in one
    /// callback by looping on `Continue`.
    fn advance(state: &mut State) -> Step {
        let phase = std::mem::replace(&mut state.phase, Phase::Done);
        match phase {
            Phase::Connect => phase_connect(state),
            Phase::WaitConnect => phase_wait_connect(state),
            Phase::Warmup {
                iter_done,
                sent,
                recvd,
            } => phase_warmup(state, iter_done, sent, recvd),
            Phase::WarmupGap {
                iter_done,
                gap_until,
            } => phase_warmup_gap(state, iter_done, gap_until),
            Phase::MeasureStart { iter_done } => phase_measure_start(state, iter_done),
            Phase::MeasureWrite {
                iter_done,
                sent,
                t0,
            } => phase_measure_write(state, iter_done, sent, t0),
            Phase::MeasureRead {
                iter_done,
                recvd,
                t0,
            } => phase_measure_read(state, iter_done, recvd, t0),
            Phase::CloseAndDone => phase_close_and_done(state),
            Phase::BucketError(msg) => phase_bucket_error(state, msg),
            Phase::Done => phase_done(state),
        }
    }

    fn phase_connect(state: &mut State) -> Step {
        let fd = unsafe { ff_socket(AF_INET as c_int, SOCK_STREAM, 0) };
        if fd < 0 {
            let errno = fstack_errno();
            state.phase = Phase::BucketError(format!("ff_socket failed: errno={errno}"));
            return Step::Continue;
        }
        state.fd = fd;

        // Set non-blocking BEFORE connect.
        let on: c_int = 1;
        let rc = unsafe { ff_ioctl(fd, FIONBIO, &on as *const c_int) };
        if rc != 0 {
            let errno = fstack_errno();
            state.phase = Phase::BucketError(format!(
                "ff_ioctl(FIONBIO) failed: rc={rc} errno={errno}"
            ));
            return Step::Continue;
        }

        let sa = make_linux_sockaddr_in(state.peer_ip_host_order, state.peer_port);
        let rc = unsafe { ff_connect(fd, &sa, std::mem::size_of_val(&sa) as u32) };
        if rc == 0 {
            // Synchronous-completion fast path.
            state.phase = Phase::Warmup {
                iter_done: 0,
                sent: 0,
                recvd: 0,
            };
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

    fn phase_wait_connect(state: &mut State) -> Step {
        // Use ff_poll(POLLOUT, timeout=0) — see module-level note on
        // why SO_ERROR alone is ambiguous.
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
        // POLLOUT fired: handshake complete or failed.
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
            state.phase = Phase::BucketError(format!(
                "ff_getsockopt(SO_ERROR) failed: rc={rc} errno={errno}"
            ));
            return Step::Continue;
        }
        if sock_err == 0 {
            state.phase = Phase::Warmup {
                iter_done: 0,
                sent: 0,
                recvd: 0,
            };
            return Step::Continue;
        }
        state.phase = Phase::BucketError(format!("connect SO_ERROR={sock_err}"));
        Step::Continue
    }

    fn phase_warmup(state: &mut State, iter_done: u64, mut sent: usize, mut recvd: usize) -> Step {
        if iter_done >= state.warmup {
            state.phase = Phase::MeasureStart { iter_done: 0 };
            return Step::Continue;
        }

        // Drive send phase if not yet complete.
        if sent < state.request.len() {
            match pump_write(state.fd, &state.request, &mut sent) {
                PumpStep::Progress => {
                    state.phase = Phase::Warmup { iter_done, sent, recvd };
                    return Step::Continue;
                }
                PumpStep::WouldBlock => {
                    state.phase = Phase::Warmup { iter_done, sent, recvd };
                    return Step::Yield;
                }
                PumpStep::Error(msg) => {
                    state.phase = Phase::BucketError(msg);
                    return Step::Continue;
                }
            }
        }

        // Send done — drain echo.
        match pump_drain(state.fd, state.response_bytes, &mut recvd) {
            DrainStep::Done => {
                // One full warmup iter complete.
                state.phase = Phase::Warmup {
                    iter_done: iter_done + 1,
                    sent: 0,
                    recvd: 0,
                };
                Step::Continue
            }
            DrainStep::WouldBlock => {
                state.phase = Phase::Warmup { iter_done, sent, recvd };
                Step::Yield
            }
            DrainStep::Error(msg) => {
                state.phase = Phase::BucketError(msg);
                Step::Continue
            }
        }
    }

    fn phase_warmup_gap(state: &mut State, iter_done: u64, gap_until: Instant) -> Step {
        if Instant::now() >= gap_until {
            state.phase = Phase::Warmup {
                iter_done,
                sent: 0,
                recvd: 0,
            };
            return Step::Continue;
        }
        state.phase = Phase::WarmupGap { iter_done, gap_until };
        Step::Yield
    }

    fn phase_measure_start(state: &mut State, iter_done: u64) -> Step {
        if iter_done >= state.iterations {
            state.phase = Phase::CloseAndDone;
            return Step::Continue;
        }
        let t0 = Instant::now();
        state.phase = Phase::MeasureWrite {
            iter_done,
            sent: 0,
            t0,
        };
        Step::Continue
    }

    fn phase_measure_write(state: &mut State, iter_done: u64, mut sent: usize, t0: Instant) -> Step {
        if t0.elapsed() > RTT_TIMEOUT {
            state.phase = Phase::BucketError(format!(
                "measure iter {iter_done}: send timeout (>{:?})",
                RTT_TIMEOUT
            ));
            return Step::Continue;
        }
        match pump_write(state.fd, &state.request, &mut sent) {
            PumpStep::Progress => {
                if sent >= state.request.len() {
                    state.phase = Phase::MeasureRead {
                        iter_done,
                        recvd: 0,
                        t0,
                    };
                    return Step::Continue;
                }
                state.phase = Phase::MeasureWrite { iter_done, sent, t0 };
                Step::Continue
            }
            PumpStep::WouldBlock => {
                state.phase = Phase::MeasureWrite { iter_done, sent, t0 };
                Step::Yield
            }
            PumpStep::Error(msg) => {
                state.phase = Phase::BucketError(msg);
                Step::Continue
            }
        }
    }

    fn phase_measure_read(state: &mut State, iter_done: u64, mut recvd: usize, t0: Instant) -> Step {
        if t0.elapsed() > RTT_TIMEOUT {
            state.phase = Phase::BucketError(format!(
                "measure iter {iter_done}: recv timeout (>{:?})",
                RTT_TIMEOUT
            ));
            return Step::Continue;
        }
        match pump_drain(state.fd, state.response_bytes, &mut recvd) {
            DrainStep::Done => {
                let rtt_ns = t0.elapsed().as_nanos() as u64;
                state.samples.push(rtt_ns as f64);
                state.phase = Phase::MeasureStart {
                    iter_done: iter_done + 1,
                };
                Step::Continue
            }
            DrainStep::WouldBlock => {
                state.phase = Phase::MeasureRead { iter_done, recvd, t0 };
                Step::Yield
            }
            DrainStep::Error(msg) => {
                state.phase = Phase::BucketError(msg);
                Step::Continue
            }
        }
    }

    fn phase_close_and_done(state: &mut State) -> Step {
        if state.fd >= 0 {
            unsafe {
                let _ = ff_close(state.fd);
            }
            state.fd = -1;
        }
        state.phase = Phase::Done;
        Step::Continue
    }

    fn phase_bucket_error(state: &mut State, msg: String) -> Step {
        if state.fd >= 0 {
            unsafe {
                let _ = ff_close(state.fd);
            }
            state.fd = -1;
        }
        state.error = Some(msg);
        state.phase = Phase::Done;
        Step::Continue
    }

    fn phase_done(state: &mut State) -> Step {
        if !state.stopped {
            unsafe { ff_stop_run() };
            state.stopped = true;
        }
        state.phase = Phase::Done;
        Step::Stopped
    }

    enum PumpStep {
        Progress,
        WouldBlock,
        Error(String),
    }

    fn pump_write(fd: c_int, payload: &[u8], sent: &mut usize) -> PumpStep {
        if *sent >= payload.len() {
            return PumpStep::Progress;
        }
        let mut made_progress = false;
        loop {
            let remaining = &payload[*sent..];
            let n = unsafe { ff_write(fd, remaining.as_ptr() as *const c_void, remaining.len()) };
            if n > 0 {
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
                    if made_progress {
                        return PumpStep::Progress;
                    }
                    return PumpStep::WouldBlock;
                }
                return PumpStep::Error(format!("ff_write failed: errno={errno}"));
            }
            // n == 0: unexpected on TCP socket; treat as transient.
            if made_progress {
                return PumpStep::Progress;
            }
            return PumpStep::WouldBlock;
        }
    }

    enum DrainStep {
        Done,
        WouldBlock,
        Error(String),
    }

    fn pump_drain(fd: c_int, expected: usize, recvd: &mut usize) -> DrainStep {
        if *recvd >= expected {
            return DrainStep::Done;
        }
        let mut buf = [0u8; 4096];
        loop {
            let want = (expected - *recvd).min(buf.len());
            let n = unsafe { ff_read(fd, buf.as_mut_ptr() as *mut c_void, want) };
            if n > 0 {
                *recvd += n as usize;
                if *recvd >= expected {
                    return DrainStep::Done;
                }
                continue;
            }
            if n < 0 {
                let errno = fstack_errno();
                if errno == FF_EAGAIN {
                    return DrainStep::WouldBlock;
                }
                return DrainStep::Error(format!("ff_read failed: errno={errno}"));
            }
            return DrainStep::Error(
                "ff_read returned 0 (connection closed during echo)".to_string(),
            );
        }
    }
}

#[cfg(not(feature = "fstack"))]
pub mod imp {
    pub fn run_rtt_workload(
        _peer_ip_host_order: u32,
        _peer_port: u16,
        _request_bytes: usize,
        _response_bytes: usize,
        _warmup: u64,
        _iterations: u64,
    ) -> anyhow::Result<Vec<f64>> {
        anyhow::bail!(
            "bench-rtt built without fstack feature; rebuild with --features fstack \
             on the AMI where libfstack.a is installed at /opt/f-stack/lib/."
        )
    }
}
