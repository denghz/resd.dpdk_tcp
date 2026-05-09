//! F-Stack FFI bindings — minimal subset for bench-rtt RTT path.
//!
//! Mirrors `tools/bench-vs-mtcp/src/fstack_ffi.rs` constants and
//! signatures verbatim. The contents are duplicated rather than reused
//! through a path-dep because bench-vs-mtcp already depends on
//! bench-rtt (for the workload helpers), and depending back would
//! create a cycle. Phase 5 of the 2026-05-09 bench-suite overhaul
//! lifts the FFI bindings into a shared crate so this duplication
//! goes away.
//!
//! # Errno + sockopt namespace (T50 lessons)
//!
//! F-Stack runs on Linux but its compat layer uses LINUX-namespace
//! constants for both errno (`EAGAIN=11`, `EINPROGRESS=115`) and
//! `getsockopt(SOL_SOCKET=1, SO_ERROR=4, ...)` via the
//! `ff_getsockopt` (NOT `_freebsd`) entry point.
//!
//! # Connect detection — `ff_poll(POLLOUT, timeout=0)`
//!
//! `SO_ERROR` alone is unreliable for non-blocking connect (returns 0
//! both during SYN_SENT and after success). Poll for POLLOUT readiness
//! first; once it fires, check SO_ERROR to distinguish success from
//! refused.

#![cfg(feature = "fstack")]

use std::os::raw::{c_char, c_int, c_uint, c_void};

/// F-Stack's internal sockaddr shape — BSD on the wire for AF_INET,
/// but the API is named `linux_sockaddr` because F-Stack converts
/// between Linux + FreeBSD layouts internally. For AF_INET this is
/// byte-identical to `libc::sockaddr_in`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LinuxSockaddr {
    pub sa_family: i16,
    pub sa_data: [u8; 14],
}

/// `struct pollfd` mirror for `ff_poll`. Wire layout: fd(4) events(2) revents(2) = 8 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

unsafe extern "C" {
    pub fn ff_init(argc: c_int, argv: *const *const c_char) -> c_int;
    pub fn ff_socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    pub fn ff_connect(s: c_int, addr: *const LinuxSockaddr, addrlen: c_uint) -> c_int;
    pub fn ff_close(fd: c_int) -> c_int;
    pub fn ff_read(fd: c_int, buf: *mut c_void, nbytes: usize) -> isize;
    pub fn ff_write(fd: c_int, buf: *const c_void, nbytes: usize) -> isize;
    pub fn ff_ioctl(fd: c_int, request: usize, ...) -> c_int;
    pub fn ff_run(loop_fn: extern "C" fn(*mut c_void) -> c_int, arg: *mut c_void);
    pub fn ff_stop_run();
    pub fn ff_getsockopt(
        s: c_int,
        level: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut c_uint,
    ) -> c_int;
    pub fn ff_poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
}

pub const AF_INET: i16 = 2;
pub const SOCK_STREAM: c_int = 1;
pub const FIONBIO: usize = 0x5421;
pub const POLLOUT: i16 = 0x0004;
pub const SOL_SOCKET: c_int = 1;
pub const SO_ERROR: c_int = 4;
pub const FF_EAGAIN: i32 = 11;
pub const FF_EINPROGRESS: i32 = 115;

#[inline]
pub fn fstack_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub fn make_linux_sockaddr_in(ip_host_order: u32, port: u16) -> LinuxSockaddr {
    let mut sa = LinuxSockaddr {
        sa_family: AF_INET,
        sa_data: [0u8; 14],
    };
    let port_be = port.to_be_bytes();
    let ip_be = ip_host_order.to_be_bytes();
    sa.sa_data[0] = port_be[0];
    sa.sa_data[1] = port_be[1];
    sa.sa_data[2] = ip_be[0];
    sa.sa_data[3] = ip_be[1];
    sa.sa_data[4] = ip_be[2];
    sa.sa_data[5] = ip_be[3];
    sa
}
