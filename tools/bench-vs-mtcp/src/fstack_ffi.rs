//! F-Stack FFI bindings — minimal subset for bench-vs-mtcp burst + maxtp.
//!
//! F-Stack (https://github.com/F-Stack/f-stack, Tencent) is a FreeBSD
//! TCP/IP stack ported to userspace on DPDK. Unlike mTCP (last
//! meaningful upstream commit 2021, dormant) F-Stack is actively
//! maintained and builds against DPDK 23.11 directly.
//!
//! This module is gated behind the `fstack` cargo feature so default
//! builds don't require libfstack.a (which only exists on the
//! bench-pair AMI — see image-builder component
//! `04b-install-f-stack.yaml`). The Rust workspace builds cleanly on
//! dev hosts that don't have F-Stack installed.
//!
//! # API surface
//!
//! F-Stack exposes a BSD-socket-shaped API prefixed `ff_*`:
//! `ff_init`, `ff_socket`, `ff_bind`, `ff_listen`, `ff_connect`,
//! `ff_accept`, `ff_read`, `ff_write`, `ff_close`. We bind a tight
//! subset — just enough to drive the burst + maxtp comparators
//! against the same `bench-peer-fstack` listener that the AMI
//! component installs at `/opt/f-stack-peer/bench-peer`.
//!
//! # Sockaddr shape — `linux_sockaddr` not `sockaddr_in`
//!
//! F-Stack uses an internal `linux_sockaddr` struct (see
//! `lib/ff_api.h`) because internally it converts between
//! Linux-style + FreeBSD-style sockaddr layouts. Wire-format-wise
//! the bytes match what `libc::sockaddr_in` produces for AF_INET; we
//! use a `#[repr(C)]` mirror of `linux_sockaddr` here so the FFI
//! signature lines up exactly.
//!
//! # Lifetime + thread-safety
//!
//! `ff_init` MUST be called exactly once per process and from the
//! lcore F-Stack pinned. Bench-vs-mtcp drives a single lcore so this
//! is naturally one-shot. Caller (`fstack_burst`/`fstack_maxtp`) owns
//! the `ff_init` call site.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};

/// F-Stack's internal sockaddr shape — BSD-style on the wire for AF_INET,
/// but the API is named `linux_sockaddr` because F-Stack converts between
/// Linux + FreeBSD layouts internally. For AF_INET this is byte-identical
/// to `libc::sockaddr_in` minus the trailing 8-byte padding (16 B vs 16 B
/// — the trailing `sa_data[14]` covers everything but the first 2 B of
/// family). See `lib/ff_api.h::linux_sockaddr`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LinuxSockaddr {
    pub sa_family: i16,
    pub sa_data: [u8; 14],
}

unsafe extern "C" {
    /// Initialise F-Stack (parses argv for EAL + F-Stack config). Must
    /// be called exactly once per process. Returns 0 on success.
    pub fn ff_init(argc: c_int, argv: *const *const c_char) -> c_int;

    /// Open a socket; returns fd ≥ 0 on success, -1 on error.
    pub fn ff_socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;

    pub fn ff_bind(s: c_int, addr: *const LinuxSockaddr, addrlen: c_uint) -> c_int;

    pub fn ff_listen(s: c_int, backlog: c_int) -> c_int;

    pub fn ff_connect(s: c_int, addr: *const LinuxSockaddr, addrlen: c_uint) -> c_int;

    pub fn ff_accept(s: c_int, addr: *mut LinuxSockaddr, addrlen: *mut c_uint) -> c_int;

    pub fn ff_close(fd: c_int) -> c_int;

    pub fn ff_read(fd: c_int, buf: *mut c_void, nbytes: usize) -> isize;

    pub fn ff_write(fd: c_int, buf: *const c_void, nbytes: usize) -> isize;

    pub fn ff_ioctl(fd: c_int, request: usize, ...) -> c_int;

    /// F-Stack's main event loop — calls `loop_fn(arg)` once per poll
    /// iteration. All socket operations (ff_socket, ff_connect,
    /// ff_write, ff_read, ff_getsockopt) MUST be called from inside
    /// this callback; calling them outside ff_run is a no-op because
    /// DPDK packet processing only runs inside ff_run's poll loop.
    ///
    /// ff_run calls rte_eal_cleanup() when the loop exits, so it may
    /// be invoked AT MOST ONCE per process. The entire measurement grid
    /// (all buckets) must complete inside a single ff_run invocation.
    pub fn ff_run(loop_fn: extern "C" fn(*mut c_void) -> c_int, arg: *mut c_void);

    /// Signal ff_run to stop after the current callback returns.
    /// Safe to call from inside the callback; ff_run returns after the
    /// callback completes. rte_eal_cleanup() fires on ff_run return.
    pub fn ff_stop_run();

    /// Get a socket option. Uses FreeBSD-namespace level + optname
    /// constants (SOL_SOCKET=0xffff, SO_ERROR=0x1007, etc.) — NOT
    /// Linux values, even though F-Stack runs on Linux.
    pub fn ff_getsockopt(
        s: c_int,
        level: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut c_uint,
    ) -> c_int;
}

/// AF_INET (IPv4) — Linux-style value used by F-Stack's
/// `linux_sockaddr.sa_family`.
pub const AF_INET: i16 = 2;

/// SOCK_STREAM (TCP).
pub const SOCK_STREAM: c_int = 1;

/// FIONBIO ioctl request (set non-blocking). Linux value mirrored —
/// F-Stack accepts the Linux constant via its compat layer.
pub const FIONBIO: usize = 0x5421;

/// Socket-level option identifier — FreeBSD value.
/// Linux uses SOL_SOCKET=1; F-Stack uses the FreeBSD value 0xffff.
pub const SOL_SOCKET: c_int = 0xffff_u32 as c_int;

/// Socket option: pending connect error (getsockopt after EINPROGRESS).
/// FreeBSD value 0x1007; Linux SO_ERROR=4.
pub const SO_ERROR: c_int = 0x1007_u32 as c_int;

/// errno values F-Stack writes to system errno after each call.
///
/// Despite running on Linux, F-Stack translates its internal FreeBSD
/// errno through `__errno_location` — but the translation produces
/// Linux-namespace values in system errno (confirmed by T50: ff_connect
/// returned errno=115, not FreeBSD 36). Compare `fstack_errno()` against
/// these Linux values.
pub const FF_EAGAIN: i32 = 11;       // Linux EAGAIN
pub const FF_EINPROGRESS: i32 = 115; // Linux EINPROGRESS

/// Read the system errno set by the last F-Stack call.
/// F-Stack writes Linux-namespace errno values; compare against
/// `FF_EAGAIN`, `FF_EINPROGRESS`, etc.
#[inline]
pub fn fstack_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Build a `LinuxSockaddr` for AF_INET targeting `(ip_host_order, port)`.
///
/// The tuple is laid out as `{port_be_u16, ip_be_u32, [u8; 8] zero}`
/// inside the 14-byte `sa_data` slot, matching `sockaddr_in`'s wire
/// format. `port` is host byte order on input; we convert to
/// network byte order inline.
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

/// Initialise F-Stack with a vector of CLI args. F-Stack consumes the
/// EAL flags + its own `--conf` flag from this argv. Calls
/// `ff_init(argc, argv)` exactly once. Returns Ok(()) on success.
///
/// Caller is responsible for ensuring this is called from the
/// dedicated lcore F-Stack will run on (typically lcore 0/2 — same
/// shape as the dpdk_net engine bring-up).
pub fn ff_init_from_args(args: &[String]) -> Result<(), String> {
    let cstrings: Vec<CString> = args
        .iter()
        .map(|s| CString::new(s.as_str()).map_err(|e| format!("ff_init: invalid arg `{s}`: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let argv: Vec<*const c_char> = cstrings.iter().map(|s| s.as_ptr()).collect();
    let argc = argv.len() as c_int;
    let rc = unsafe { ff_init(argc, argv.as_ptr()) };
    if rc != 0 {
        return Err(format!("ff_init returned {rc}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the sockaddr layout produces correct wire format for
    /// AF_INET — port + IP in network byte order at the right
    /// offsets.
    #[test]
    fn make_linux_sockaddr_in_layout_matches_sockaddr_in() {
        // 192.168.1.10 host order = 0xC0_A8_01_0A
        let ip_host: u32 = 0xC0_A8_01_0A;
        let port: u16 = 10003;
        let sa = make_linux_sockaddr_in(ip_host, port);
        assert_eq!(sa.sa_family, AF_INET);
        // Port 10003 = 0x2713 host -> 0x13 0x27 wire (big-endian).
        assert_eq!(sa.sa_data[0], 0x27);
        assert_eq!(sa.sa_data[1], 0x13);
        // IP wire bytes (network order, MSB first).
        assert_eq!(sa.sa_data[2], 0xC0);
        assert_eq!(sa.sa_data[3], 0xA8);
        assert_eq!(sa.sa_data[4], 0x01);
        assert_eq!(sa.sa_data[5], 0x0A);
        // Padding stays zero.
        for i in 6..14 {
            assert_eq!(sa.sa_data[i], 0);
        }
    }

    #[test]
    fn linux_sockaddr_size_is_16_bytes() {
        // sa_family (2) + sa_data (14) = 16 bytes — wire-compatible
        // with sockaddr_in (16 bytes on Linux/FreeBSD).
        assert_eq!(std::mem::size_of::<LinuxSockaddr>(), 16);
    }
}
