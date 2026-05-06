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
//! # Errno surface
//!
//! F-Stack syscalls return -1 on error and set the **host (Linux)**
//! `errno` via `ff_os_errno` (see
//! `/opt/src/f-stack/lib/ff_host_interface.c`). The translation is
//! BSD → Linux internally, so callers compare against Linux libc
//! errno values — `EAGAIN = 11`, `EINPROGRESS = 115` — NOT the
//! FreeBSD numbers (35 / 36) that `ff_errno.h` documents as the
//! pre-translation values. The [`ff_errno`] helper here reads
//! `*__errno_location()` which is what F-Stack actually wrote to.
//!
//! # Non-blocking connect (`EINPROGRESS`) flow
//!
//! With `FIONBIO` set, `ff_connect` returns `-1` with `errno =
//! EINPROGRESS` while the SYN is in flight. Callers must:
//! 1. Detect `EINPROGRESS` (not bail).
//! 2. Wait for the socket to become writable via `ff_select`.
//! 3. Confirm via `ff_getsockopt(SO_ERROR)` (a deferred error like
//!    `ECONNREFUSED` would otherwise go unnoticed until the next
//!    `ff_write`).
//!
//! [`connect_nonblocking`] wraps that flow.
//!
//! # Lifetime + thread-safety
//!
//! `ff_init` MUST be called exactly once per process and from the
//! lcore F-Stack pinned. Bench-vs-mtcp drives a single lcore so this
//! is naturally one-shot. Caller (`fstack_burst`/`fstack_maxtp`) owns
//! the `ff_init` call site.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::time::{Duration, Instant};

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

/// `fd_set` — same layout as Linux/FreeBSD: 1024 bits stored as 16 × u64.
/// F-Stack's `kern_select` accepts the standard fd_set bit layout (see
/// `lib/ff_syscall_wrapper.c::ff_select`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FdSet {
    pub fds_bits: [u64; 16], // FD_SETSIZE / 64 = 1024 / 64
}

impl FdSet {
    pub fn zero() -> Self {
        FdSet { fds_bits: [0u64; 16] }
    }

    pub fn set(&mut self, fd: c_int) {
        let i = (fd as usize) / 64;
        let bit = (fd as usize) % 64;
        if i < self.fds_bits.len() {
            self.fds_bits[i] |= 1u64 << bit;
        }
    }

    pub fn is_set(&self, fd: c_int) -> bool {
        let i = (fd as usize) / 64;
        let bit = (fd as usize) % 64;
        if i < self.fds_bits.len() {
            (self.fds_bits[i] & (1u64 << bit)) != 0
        } else {
            false
        }
    }
}

/// `struct timeval` — Linux/FreeBSD layout (8 B sec + 8 B usec on
/// 64-bit). F-Stack's `ff_select` reads this directly, see
/// `lib/ff_syscall_wrapper.c`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
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

    /// `ff_select(nfds, readfds, writefds, exceptfds, timeout)` — same
    /// signature as POSIX `select`. F-Stack accepts the standard
    /// fd_set bit layout; on -1 errno is set via `ff_os_errno` (Linux
    /// libc errno values). See `lib/ff_syscall_wrapper.c::ff_select`.
    pub fn ff_select(
        nfds: c_int,
        readfds: *mut FdSet,
        writefds: *mut FdSet,
        exceptfds: *mut FdSet,
        timeout: *mut Timeval,
    ) -> c_int;

    /// `ff_getsockopt(s, level, optname, optval, optlen)` — POSIX
    /// `getsockopt` shape. We use this with `(SOL_SOCKET, SO_ERROR)` to
    /// confirm a non-blocking connect actually succeeded after the
    /// socket becomes writable. See `lib/ff_syscall_wrapper.c`.
    pub fn ff_getsockopt(
        s: c_int,
        level: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut c_uint,
    ) -> c_int;

    /// F-Stack's main loop driver — calls the user-provided callback in
    /// a loop. Bench-vs-mtcp does NOT use this entry point; it pumps
    /// directly via `ff_socket` / `ff_write` from the main thread (the
    /// `ff_init` call already brings F-Stack's internal poll loop up
    /// in lcore-pinned mode). We bind the symbol for completeness but
    /// the burst + maxtp arms call `ff_run`-free code paths.
    #[allow(dead_code)]
    pub fn ff_run(loop_fn: extern "C" fn(*mut c_void) -> c_int, arg: *mut c_void);
}

unsafe extern "C" {
    /// glibc's thread-local errno accessor. F-Stack's syscall wrappers
    /// set the host (Linux) `errno` via `ff_os_errno`, so reading
    /// `*__errno_location()` is the canonical way to retrieve the
    /// last F-Stack syscall's errno. See
    /// `/opt/src/f-stack/lib/ff_host_interface.c::ff_os_errno`.
    fn __errno_location() -> *mut c_int;
}

/// Read the host `errno` set by the most recent F-Stack syscall.
///
/// F-Stack's `ff_os_errno` translates its FreeBSD-style internal
/// errnos into Linux libc errno values before returning. So we compare
/// against Linux constants (`EAGAIN = 11`, `EINPROGRESS = 115`), NOT
/// FreeBSD values (35 / 36). The `FF_*` constants below are the
/// **Linux** values that F-Stack actually surfaces.
pub fn ff_errno() -> c_int {
    unsafe { *__errno_location() }
}

/// AF_INET (IPv4) — Linux-style value used by F-Stack's
/// `linux_sockaddr.sa_family`.
pub const AF_INET: i16 = 2;

/// SOCK_STREAM (TCP).
pub const SOCK_STREAM: c_int = 1;

/// FIONBIO ioctl request (set non-blocking). Linux value mirrored —
/// F-Stack accepts the Linux constant via its compat layer.
pub const FIONBIO: usize = 0x5421;

/// Linux `SOL_SOCKET` value — F-Stack's getsockopt accepts both Linux
/// and FreeBSD level numbers (see `lib/ff_syscall_wrapper.c`,
/// `LINUX_SOL_SOCKET = 1` is mapped to FreeBSD `SOL_SOCKET`).
pub const SOL_SOCKET: c_int = 1;

/// Linux `SO_ERROR` value — used after non-blocking connect completes
/// to confirm the connection succeeded.
pub const SO_ERROR: c_int = 4;

/// Linux libc errno values. F-Stack's `ff_os_errno` translates its
/// BSD-style internal errnos into Linux values before they reach the
/// caller, so these are the values we compare against — NOT the
/// FreeBSD numbers (`ff_EAGAIN = 35`, `ff_EINPROGRESS = 36`) that
/// `ff_errno.h` documents as the internal mapping.
///
/// The `FF_` prefix is kept on `FF_EAGAIN` for API stability — earlier
/// code referenced the FreeBSD value via the same name; the value has
/// been corrected to the Linux constant that F-Stack actually surfaces
/// at runtime through `*__errno_location()`.
pub const FF_EAGAIN: c_int = 11;

/// Linux `EWOULDBLOCK` — same as `EAGAIN` on Linux. Documented
/// separately because both names appear in BSD source.
pub const FF_EWOULDBLOCK: c_int = 11;

/// Linux `EINPROGRESS` — set on a non-blocking `ff_connect` that has
/// initiated the SYN but not yet completed the handshake. Caller must
/// `ff_select` for writable, then `ff_getsockopt(SO_ERROR)` to confirm.
pub const FF_EINPROGRESS: c_int = 115;

/// Linux `EINTR` — used to retry `ff_select` on signal interruption.
pub const FF_EINTR: c_int = 4;

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

/// Block on a non-blocking `ff_connect` until the socket becomes
/// writable (handshake complete) or `deadline` elapses, then check
/// `SO_ERROR` to confirm success.
///
/// Use this after `ff_connect` returns -1 with `errno == EINPROGRESS`.
/// Returns `Ok(())` if the connect completed cleanly, or an error
/// describing the timeout / SO_ERROR result.
///
/// The polling cadence is 100 ms — short enough to react quickly on a
/// loopback peer, long enough to avoid burning CPU on the F-Stack
/// poll thread. Bench harnesses typically allow ~5 s for connect.
pub fn wait_connect_complete(fd: c_int, deadline: Instant) -> Result<(), String> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(format!(
                "ff_connect: timed out waiting for non-blocking connect to complete (fd={fd})"
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        // Cap each select wait at 100 ms so we re-check the deadline
        // frequently and surface timeouts without lag.
        let slice = remaining.min(Duration::from_millis(100));
        let mut tv = Timeval {
            tv_sec: slice.as_secs() as i64,
            tv_usec: slice.subsec_micros() as i64,
        };
        let mut wfds = FdSet::zero();
        wfds.set(fd);
        let rc = unsafe {
            ff_select(
                fd + 1,
                std::ptr::null_mut(),
                &mut wfds as *mut FdSet,
                std::ptr::null_mut(),
                &mut tv as *mut Timeval,
            )
        };
        if rc < 0 {
            let e = ff_errno();
            // EINTR transient: retry. Anything else is fatal.
            if e == FF_EINTR {
                continue;
            }
            return Err(format!("ff_select returned {rc} (errno={e})"));
        }
        if rc == 0 {
            // Timeout for this slice — loop and re-check the outer deadline.
            continue;
        }
        if !wfds.is_set(fd) {
            // Spurious — re-arm.
            continue;
        }
        // Socket is writable; check SO_ERROR to distinguish a clean
        // connect from a deferred error (e.g. ECONNREFUSED).
        let mut so_error: c_int = 0;
        let mut optlen: c_uint = std::mem::size_of::<c_int>() as c_uint;
        let rc = unsafe {
            ff_getsockopt(
                fd,
                SOL_SOCKET,
                SO_ERROR,
                &mut so_error as *mut c_int as *mut c_void,
                &mut optlen as *mut c_uint,
            )
        };
        if rc != 0 {
            let e = ff_errno();
            return Err(format!(
                "ff_getsockopt(SO_ERROR) returned {rc} (errno={e}) on fd={fd}"
            ));
        }
        if so_error != 0 {
            return Err(format!(
                "ff_connect failed: SO_ERROR={so_error} (fd={fd})"
            ));
        }
        return Ok(());
    }
}

/// Open a non-blocking F-Stack TCP socket connected to
/// `(peer_ip_host_order, peer_port)`, handling the `EINPROGRESS` flow
/// for non-blocking connect. Returns the connected fd in non-blocking
/// mode, ready for `ff_write` / `ff_read`.
///
/// The flow:
/// 1. `ff_socket(AF_INET, SOCK_STREAM)`.
/// 2. Set `FIONBIO` so `ff_write` behaves per the F-Stack docs.
/// 3. `ff_connect`. If it returns 0, we're done. If it returns -1
///    with `errno == EINPROGRESS`, that's the expected non-blocking
///    behaviour — `wait_connect_complete` polls for writable + checks
///    `SO_ERROR`. Any other errno is a real failure.
///
/// On any failure path the socket is closed before the error is
/// returned, so callers don't leak fds on partial setup.
pub fn connect_nonblocking(
    peer_ip_host_order: u32,
    peer_port: u16,
    connect_timeout: Duration,
) -> Result<c_int, String> {
    let fd = unsafe { ff_socket(AF_INET as c_int, SOCK_STREAM, 0) };
    if fd < 0 {
        let e = ff_errno();
        return Err(format!("ff_socket returned {fd} (errno={e})"));
    }
    let on: c_int = 1;
    let rc = unsafe { ff_ioctl(fd, FIONBIO, &on as *const c_int) };
    if rc != 0 {
        let e = ff_errno();
        unsafe { ff_close(fd) };
        return Err(format!("ff_ioctl(FIONBIO) returned {rc} (errno={e})"));
    }
    let sa = make_linux_sockaddr_in(peer_ip_host_order, peer_port);
    let rc = unsafe { ff_connect(fd, &sa, std::mem::size_of_val(&sa) as u32) };
    if rc == 0 {
        // Connect completed inline (loopback or kernel happy-path).
        return Ok(fd);
    }
    let e = ff_errno();
    if e != FF_EINPROGRESS {
        unsafe { ff_close(fd) };
        return Err(format!(
            "ff_connect returned {rc} (errno={e}); expected 0 or EINPROGRESS={FF_EINPROGRESS}"
        ));
    }
    // EINPROGRESS — non-blocking handshake in flight. Wait for
    // writable + check SO_ERROR.
    let deadline = Instant::now() + connect_timeout;
    if let Err(err) = wait_connect_complete(fd, deadline) {
        unsafe { ff_close(fd) };
        return Err(err);
    }
    Ok(fd)
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

    #[test]
    fn fd_set_size_is_128_bytes() {
        // 1024 bits / 8 = 128 bytes; matches Linux/FreeBSD fd_set
        // layout for FD_SETSIZE = 1024.
        assert_eq!(std::mem::size_of::<FdSet>(), 128);
    }

    #[test]
    fn fd_set_set_and_is_set_round_trip() {
        let mut s = FdSet::zero();
        assert!(!s.is_set(0));
        s.set(0);
        assert!(s.is_set(0));
        // Cross-word boundary (fd 64 = bit 0 of word 1).
        s.set(64);
        assert!(s.is_set(64));
        assert!(!s.is_set(63));
        assert!(!s.is_set(65));
    }

    #[test]
    fn fd_set_set_at_high_fd_does_not_overflow() {
        let mut s = FdSet::zero();
        // FD_SETSIZE - 1 = 1023, which is the last valid bit.
        s.set(1023);
        assert!(s.is_set(1023));
        // Out-of-range fd is a silent no-op (matches FD_SET behaviour
        // for fds >= FD_SETSIZE; F-Stack rejects via select's nfds
        // bound check).
        s.set(2048);
        assert!(!s.is_set(2048));
    }

    #[test]
    fn timeval_size_is_16_bytes() {
        // 8 (tv_sec) + 8 (tv_usec) on 64-bit; matches FreeBSD +
        // Linux struct timeval ABI.
        assert_eq!(std::mem::size_of::<Timeval>(), 16);
    }

    /// Errno constants are the Linux libc values (not FreeBSD).
    /// F-Stack's `ff_os_errno` translates internally before return.
    #[test]
    fn errno_constants_are_linux_values() {
        assert_eq!(FF_EAGAIN, 11);
        assert_eq!(FF_EWOULDBLOCK, 11);
        assert_eq!(FF_EINPROGRESS, 115);
        assert_eq!(FF_EINTR, 4);
    }
}
