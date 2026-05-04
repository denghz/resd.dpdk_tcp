//! F-Stack RTT path for mode A (spec §8) — request/response loop
//! mirroring `linux_kernel.rs`'s shape but driving the F-Stack BSD-
//! socket-style API (`ff_socket`, `ff_connect`, `ff_write`, `ff_read`,
//! `ff_close`).
//!
//! # Why F-Stack
//!
//! F-Stack (https://github.com/F-Stack/f-stack, Tencent) is a FreeBSD
//! TCP/IP stack ported to userspace on DPDK. Unlike mTCP (dormant,
//! DPDK 18.05/19.08 only) F-Stack is actively maintained and builds
//! against DPDK 23.11. See
//! `docs/superpowers/reports/perf-a10-mtcp-rebuild-investigation.md`
//! Phase 4 for the alternative-stack survey.
//!
//! # Feature gating
//!
//! Compiled only with `--features fstack`; default builds omit this
//! module so the workspace compiles on dev hosts without
//! libfstack.a. The bench-pair AMI installs libfstack.a at
//! `/opt/f-stack/lib/libfstack.a` (image-builder component
//! `04b-install-f-stack.yaml`).
//!
//! # FFI re-use
//!
//! F-Stack FFI bindings live in the bench-vs-mtcp crate
//! (`bench_vs_mtcp::fstack_ffi`) so we don't duplicate them here.
//! Pulling in a path-dep on bench-vs-mtcp's lib gives bench-vs-linux
//! access to the same `ff_*` extern "C" surface the burst + maxtp
//! arms use.

use std::os::raw::c_int;
use std::time::{Duration, Instant};

use bench_vs_mtcp::fstack_ffi::{
    ff_close, ff_connect, ff_ioctl, ff_read, ff_socket, ff_write, make_linux_sockaddr_in,
    AF_INET, FIONBIO, SOCK_STREAM,
};

/// Connect deadline mirroring `linux_kernel.rs::CONNECT_TIMEOUT`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-iteration RTT ceiling against wedge.
const RTT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect to the peer via F-Stack. Returns the F-Stack socket fd.
///
/// Sequence:
///   1. `ff_socket(AF_INET, SOCK_STREAM, 0)` — open BSD-shape socket.
///   2. Blocking `ff_connect` to the peer (handshake completes inline).
///   3. Set non-blocking via `ff_ioctl(FIONBIO, &on)` so subsequent
///      `ff_write` calls behave per F-Stack header notes.
pub fn connect(peer_ip_host_order: u32, peer_port: u16) -> anyhow::Result<c_int> {
    let _ = CONNECT_TIMEOUT; // F-Stack ff_connect doesn't expose a timeout knob
    let fd = unsafe { ff_socket(AF_INET as c_int, SOCK_STREAM, 0) };
    if fd < 0 {
        anyhow::bail!("ff_socket returned {fd}");
    }
    let sa = make_linux_sockaddr_in(peer_ip_host_order, peer_port);
    let rc = unsafe { ff_connect(fd, &sa, std::mem::size_of_val(&sa) as u32) };
    if rc != 0 {
        unsafe { ff_close(fd) };
        anyhow::bail!("ff_connect returned {rc}");
    }
    let on: c_int = 1;
    let rc = unsafe { ff_ioctl(fd, FIONBIO, &on as *const c_int) };
    if rc != 0 {
        unsafe { ff_close(fd) };
        anyhow::bail!("ff_ioctl(FIONBIO) returned {rc}");
    }
    Ok(fd)
}

/// One measured request-response RTT over an F-Stack socket. Returns
/// the round-trip time in nanoseconds.
///
/// Mirrors `linux_kernel::request_response_once` shape but uses
/// `ff_write` + `ff_read`. Loops on partial-write / partial-read
/// (F-Stack returns less than nbytes if the send buffer fills mid-
/// request).
pub fn request_response_once(
    fd: c_int,
    request: &[u8],
    response_bytes: usize,
) -> anyhow::Result<u64> {
    let t0 = Instant::now();

    // Write request — loop on EAGAIN / partial-accept.
    let mut sent: usize = 0;
    while sent < request.len() {
        let n =
            unsafe { ff_write(fd, request[sent..].as_ptr() as *const _, request.len() - sent) };
        if n > 0 {
            sent += n as usize;
        } else if n < 0 {
            if t0.elapsed() >= RTT_TIMEOUT {
                anyhow::bail!("ff_write stalled at {sent}/{} bytes", request.len());
            }
            std::thread::yield_now();
        }
    }

    // Read response — same loop shape.
    let mut buf = vec![0u8; response_bytes];
    let mut got: usize = 0;
    while got < response_bytes {
        let n = unsafe {
            ff_read(
                fd,
                buf[got..].as_mut_ptr() as *mut _,
                response_bytes - got,
            )
        };
        if n > 0 {
            got += n as usize;
        } else if n == 0 {
            anyhow::bail!("ff_read EOF at {got}/{response_bytes} bytes");
        } else {
            // EAGAIN — retry.
            if t0.elapsed() >= RTT_TIMEOUT {
                anyhow::bail!("ff_read stalled at {got}/{response_bytes} bytes");
            }
            std::thread::yield_now();
        }
    }

    Ok(t0.elapsed().as_nanos() as u64)
}

/// Run `warmup + iterations` request/response round-trips. Returns
/// the raw RTT samples in nanoseconds (warmup discarded). Mirrors
/// `linux_kernel::run_rtt_workload`.
pub fn run_rtt_workload(
    fd: c_int,
    request_bytes: usize,
    response_bytes: usize,
    warmup: u64,
    iterations: u64,
) -> anyhow::Result<Vec<f64>> {
    let request = vec![0u8; request_bytes];

    for i in 0..warmup {
        request_response_once(fd, &request, response_bytes)
            .map_err(|e| anyhow::anyhow!("fstack warmup iter {i}: {e}"))?;
    }

    let mut samples: Vec<f64> = Vec::with_capacity(iterations as usize);
    for i in 0..iterations {
        let rtt_ns = request_response_once(fd, &request, response_bytes)
            .map_err(|e| anyhow::anyhow!("fstack measurement iter {i}: {e}"))?;
        samples.push(rtt_ns as f64);
    }
    Ok(samples)
}

/// Close the F-Stack socket. Soft-fail on per-fd error.
pub fn close(fd: c_int) {
    let rc = unsafe { ff_close(fd) };
    if rc != 0 {
        eprintln!("bench-vs-linux fstack: ff_close({fd}) returned {rc}; continuing");
    }
}
