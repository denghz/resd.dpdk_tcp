//! Linux kernel TCP RX-burst arm.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. Drives the peer's
//! `burst-echo-server` over a single blocking `TcpStream`: per bucket
//! (W, N) sends `BURST N W\n`, reads N×W bytes back-to-back, captures
//! `wall_ns()` (CLOCK_REALTIME) at each `read()` return, parses
//! headers, records per-event app-delivery latency.
//!
//! # Why blocking I/O
//!
//! Mirrors `bench-tx-burst::linux` and `bench-rtt::linux_kernel` —
//! `read` blocks until the kernel has at least one byte buffered and
//! returns whatever is available (possibly multiple coalesced
//! segments). For RX-burst measurement we want exactly that: one
//! `read()` syscall per kernel-side delivery, with `wall_ns()`
//! captured immediately on return. Non-blocking + epoll would add
//! poll overhead to the very thing we're measuring.
//!
//! # No SO_RCVTIMEO on the read path
//!
//! Crucially, the stream is left with `read_timeout() == None`. A
//! prior version set `set_read_timeout(Some(BURST_TIMEOUT))` to bound
//! a wedged peer; that turned `read()` into a timed-block which
//! surfaces `ErrorKind::WouldBlock` (EAGAIN, errno 11) when the
//! kernel buffer is empty at the timeout. Live verification on
//! 2026-05-12 hit this on warmup burst 0: with `BURST_TIMEOUT=60s`
//! the failure was a peer-initialization race, but ANY transient
//! peer slowness would surface as a spurious EAGAIN. The trading-
//! latency contract is "blocking sockets either succeed or block";
//! a wedged peer is the operator's signal to ctrl-C, NOT a bench
//! arm error. The dpdk arm uses a wall-clock `STALL_TIMEOUT`
//! watchdog instead; the linux arm leans on the operator since
//! kernel `read()` does not have an out-of-band cancel path that
//! doesn't risk EAGAIN.
//!
//! # Clock model
//!
//! All three arms anchor on CLOCK_REALTIME on both peer and DUT:
//!   - Peer: `clock_gettime(CLOCK_REALTIME, ...)` inside
//!     `burst-echo-server.c`.
//!   - DUT: `wall_ns()` (`SystemTime::now() - UNIX_EPOCH`) at chunk
//!     arrival.
//! `latency_ns = dut_recv_ns - peer_send_ns` is therefore wall-clock-
//! vs-wall-clock and skewed only by NTP offset (~100 µs typical on
//! AWS same-AZ). Phase 9 c7i HW-TS will tighten the cross-host bound
//! below the NTP floor.
//!
//! # Per-event coalescing
//!
//! The kernel may deliver several 16-byte-header segments in a single
//! `read()` chunk. The recv-side parser in
//! `crate::segment::parse_burst_chunk` decomposes a chunk of length
//! M*W into M `(seq_idx, peer_send_ns)` entries. All M entries share
//! the same `dut_recv_ns` (the time of the read return), so the
//! latency CDF for a coalesced burst will show per-event plateaus —
//! the metric measured is per-event delivery, not per-segment NIC
//! arrival.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::Duration;

use anyhow::Context;

use crate::segment::{parse_burst_chunk, SegmentRecord};

/// Connect deadline for the initial kernel-TCP handshake. Same shape
/// as `bench-rtt::linux_kernel::CONNECT_TIMEOUT`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-burst write-side deadline. Writes a small BURST command with
/// TCP_NODELAY enabled — should never block long, but a wedged peer
/// kernel could swallow the write into a full socket buffer; the
/// deadline surfaces that as a visible error rather than letting the
/// bench hang in `write()`. The read path is INTENTIONALLY left with
/// no SO_RCVTIMEO — see the module-level "No SO_RCVTIMEO" note.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-bucket configuration. Caller owns `stream`.
pub struct LinuxRxBurstCfg<'a> {
    pub stream: &'a mut TcpStream,
    pub bucket_id: u32,
    pub segment_size: usize,
    pub burst_count: usize,
    pub warmup_bursts: u64,
    pub measure_bursts: u64,
}

/// One bucket's measurement product.
pub struct LinuxRxBurstRun {
    pub samples: Vec<SegmentRecord>,
}

/// Open a persistent kernel-TCP connection to the peer's
/// burst-echo-server control port. Sets `TCP_NODELAY` so small
/// `BURST` command writes don't sit in Nagle.
pub fn open_control_connection(
    peer_ip_host_order: u32,
    peer_control_port: u16,
) -> anyhow::Result<TcpStream> {
    let octets = peer_ip_host_order.to_be_bytes();
    let addr = SocketAddrV4::new(Ipv4Addr::from(octets), peer_control_port);
    let stream = TcpStream::connect_timeout(&addr.into(), CONNECT_TIMEOUT)
        .with_context(|| format!("kernel TCP connect to {}", addr))?;
    stream
        .set_nodelay(true)
        .context("set_nodelay on linux burst-echo control stream")?;
    // DO NOT call set_read_timeout here. SO_RCVTIMEO turns `read()`
    // into a timed-block that surfaces ErrorKind::WouldBlock (EAGAIN,
    // errno 11) when the kernel buffer is empty at the deadline. The
    // RX-burst arm must be deterministic-blocking: succeed or block,
    // never EAGAIN. See the module-level "No SO_RCVTIMEO" note and the
    // `open_control_connection_leaves_read_timeout_unset` regression
    // test for the 2026-05-12 bug history.
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .context("set_write_timeout pre-bucket")?;
    Ok(stream)
}

/// Drive one (W, N) bucket on the linux_kernel side.
pub fn run_bucket(cfg: &mut LinuxRxBurstCfg<'_>) -> anyhow::Result<LinuxRxBurstRun> {
    if cfg.segment_size < 16 {
        anyhow::bail!(
            "linux_rx_burst: segment_size ({}) must be ≥ 16 (header size)",
            cfg.segment_size
        );
    }
    if cfg.burst_count == 0 {
        anyhow::bail!("linux_rx_burst: burst_count must be ≥ 1");
    }

    let mut samples: Vec<SegmentRecord> =
        Vec::with_capacity((cfg.measure_bursts as usize) * cfg.burst_count);

    // Warmup — discard records.
    for i in 0..cfg.warmup_bursts {
        let _ = run_one_burst(cfg, i, false)
            .with_context(|| format!("warmup burst {i}"))?;
    }

    // Measurement.
    for i in 0..cfg.measure_bursts {
        let chunk = run_one_burst(cfg, i, true)
            .with_context(|| format!("measurement burst {i}"))?;
        samples.extend(chunk);
    }

    Ok(LinuxRxBurstRun { samples })
}

/// Send one `BURST N W\n` and drain N×W bytes back. Captures
/// per-`read()` `wall_ns()` (CLOCK_REALTIME) so the latency cadence
/// is preserved across coalesced kernel deliveries.
fn run_one_burst(
    cfg: &mut LinuxRxBurstCfg<'_>,
    burst_idx: u64,
    record: bool,
) -> anyhow::Result<Vec<SegmentRecord>> {
    let cmd = format!("BURST {} {}\n", cfg.burst_count, cfg.segment_size);
    cfg.stream
        .write_all(cmd.as_bytes())
        .with_context(|| format!("burst {burst_idx} BURST command write"))?;
    cfg.stream
        .flush()
        .with_context(|| format!("burst {burst_idx} flush"))?;

    let total = cfg.burst_count * cfg.segment_size;
    let mut recv_buf: Vec<u8> = Vec::with_capacity(total);
    let mut records: Vec<SegmentRecord> = if record {
        Vec::with_capacity(cfg.burst_count)
    } else {
        Vec::new()
    };
    let mut next_seg_idx: u64 = 0;
    let mut scratch = vec![0u8; 64 * 1024];

    while recv_buf.len() < total {
        let want = (total - recv_buf.len()).min(scratch.len());
        let n = cfg
            .stream
            .read(&mut scratch[..want])
            .with_context(|| format!("burst {burst_idx} read"))?;
        if n == 0 {
            anyhow::bail!(
                "linux_rx_burst: peer closed connection mid-burst {} \
                 ({}/{} bytes read)",
                burst_idx,
                recv_buf.len(),
                total
            );
        }
        // We read `peer_send_ns` as CLOCK_REALTIME ns from the segment
        // header, so anchor `dut_recv_ns` on the same wall clock to
        // keep the namespace consistent. NTP offset (~100 µs on AWS
        // same-AZ) bounds the absolute correctness of the diff; the
        // distribution shape (p50/p99 spread) is what we report.
        let dut_recv_ns = wall_ns();

        recv_buf.extend_from_slice(&scratch[..n]);
        if !record {
            continue;
        }

        let parsed = parse_burst_chunk(&recv_buf, cfg.segment_size);
        while next_seg_idx < parsed.len() as u64 {
            let (seq_idx, peer_send_ns) = parsed[next_seg_idx as usize];
            records.push(SegmentRecord::new(
                cfg.bucket_id,
                burst_idx,
                seq_idx,
                peer_send_ns,
                dut_recv_ns,
            ));
            next_seg_idx += 1;
        }
    }

    Ok(records)
}

/// CLOCK_REALTIME ns reading. We use `SystemTime::now()` so the
/// latency anchor matches `peer_send_ns`'s namespace — both are
/// wall-clock ns since the Unix epoch. CLOCK_REALTIME on AWS is
/// NTP-disciplined; same-AZ skew bound is ~100 µs.
fn wall_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Spin up an in-process listener that mimics the burst-echo-server
    /// protocol — read `BURST N W\n` lines and ship N segments of W
    /// bytes back. Returns `(host_order_ip, port)`.
    fn spawn_burst_peer() -> (u32, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                s.set_nodelay(true).ok();
                let mut line = Vec::with_capacity(64);
                let mut byte = [0u8; 1];
                'cmd_loop: loop {
                    line.clear();
                    loop {
                        match s.read(&mut byte) {
                            Ok(0) => break 'cmd_loop, // EOF
                            Ok(_) => {
                                line.push(byte[0]);
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            Err(_) => break 'cmd_loop,
                        }
                    }
                    let cmd = String::from_utf8_lossy(&line).into_owned();
                    if cmd.starts_with("BURST ") {
                        let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
                        if parts.len() != 3 {
                            continue;
                        }
                        let n: u64 = parts[1].parse().unwrap_or(0);
                        let w: usize = parts[2].parse().unwrap_or(0);
                        if w < 16 {
                            continue;
                        }
                        let mut buf = vec![0u8; w];
                        for i in 0..n {
                            // Header: be64 seq_idx | be64 peer_send_ns
                            buf[..8].copy_from_slice(&i.to_be_bytes());
                            // Use a fake increasing peer_send_ns so the
                            // parser sees deterministic data.
                            let peer_ns = 1_000_000u64 + i * 1_000;
                            buf[8..16].copy_from_slice(&peer_ns.to_be_bytes());
                            for k in 16..w {
                                buf[k] = 0;
                            }
                            if s.write_all(&buf).is_err() {
                                break 'cmd_loop;
                            }
                        }
                    } else if cmd.starts_with("QUIT") {
                        break;
                    }
                }
            }
        });
        (0x7F00_0001u32, port)
    }

    /// Spawn a peer that delays the burst response by `delay`, so the
    /// DUT-side read() blocks for `delay` before seeing data. Used to
    /// prove the socket is truly blocking and EAGAIN is not surfaced.
    fn spawn_slow_burst_peer(delay: Duration) -> (u32, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                s.set_nodelay(true).ok();
                let mut line = Vec::with_capacity(64);
                let mut byte = [0u8; 1];
                'cmd_loop: loop {
                    line.clear();
                    loop {
                        match s.read(&mut byte) {
                            Ok(0) => break 'cmd_loop,
                            Ok(_) => {
                                line.push(byte[0]);
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            Err(_) => break 'cmd_loop,
                        }
                    }
                    let cmd = String::from_utf8_lossy(&line).into_owned();
                    if cmd.starts_with("BURST ") {
                        let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
                        if parts.len() != 3 {
                            continue;
                        }
                        let n: u64 = parts[1].parse().unwrap_or(0);
                        let w: usize = parts[2].parse().unwrap_or(0);
                        if w < 16 {
                            continue;
                        }
                        // Inject the delay BEFORE any byte hits the
                        // wire — this is what stalls the DUT's read().
                        thread::sleep(delay);
                        let mut buf = vec![0u8; w];
                        for i in 0..n {
                            buf[..8].copy_from_slice(&i.to_be_bytes());
                            let peer_ns = 1_000_000u64 + i * 1_000;
                            buf[8..16].copy_from_slice(&peer_ns.to_be_bytes());
                            for k in 16..w {
                                buf[k] = 0;
                            }
                            if s.write_all(&buf).is_err() {
                                break 'cmd_loop;
                            }
                        }
                    } else if cmd.starts_with("QUIT") {
                        break;
                    }
                }
            }
        });
        (0x7F00_0001u32, port)
    }

    #[test]
    fn run_bucket_against_in_process_peer_emits_expected_record_count() {
        let (ip, port) = spawn_burst_peer();
        let mut stream = open_control_connection(ip, port).unwrap();
        let mut cfg = LinuxRxBurstCfg {
            stream: &mut stream,
            bucket_id: 0,
            segment_size: 64,
            burst_count: 8,
            warmup_bursts: 0,
            measure_bursts: 2,
        };
        let run = run_bucket(&mut cfg).unwrap();
        // 2 measurement bursts × 8 segments each = 16 records.
        assert_eq!(run.samples.len(), 16);
        // Each segment's seq_idx should match its position within
        // the burst — peer ships 0..N-1.
        for (i, rec) in run.samples.iter().enumerate() {
            assert_eq!(rec.seg_idx, (i as u64) % 8);
            assert!(rec.peer_send_ns > 0);
        }
    }

    /// Regression test for the 2026-05-12 EAGAIN bug: the linux arm
    /// must use deterministically blocking I/O. A `read_timeout` set
    /// on the kernel TcpStream causes `read()` to surface
    /// `ErrorKind::WouldBlock` (errno EAGAIN, os error 11) when the
    /// timeout fires before any data arrives — which the burst-echo-
    /// server protocol can hit if the peer is briefly slow on the
    /// first response (cold-cache effects, NIC IRQ coalescing, peer
    /// initialization race, etc.). Asserting `read_timeout() == None`
    /// post-`open_control_connection` pins the contract: the linux
    /// arm is deterministic-blocking, never EAGAIN-on-timeout.
    ///
    /// This test fails on the buggy code (which set
    /// `set_read_timeout(Some(BURST_TIMEOUT))`) and passes after the
    /// timeout is removed.
    #[test]
    fn open_control_connection_leaves_read_timeout_unset() {
        let (ip, port) = spawn_burst_peer();
        let stream = open_control_connection(ip, port).unwrap();
        assert_eq!(
            stream.read_timeout().expect("read_timeout() syscall"),
            None,
            "linux arm must use deterministically blocking reads — \
             set_read_timeout(Some(_)) on the kernel TcpStream causes \
             read() to surface EAGAIN on timeout, which the bench then \
             reports as 'burst N read: Resource temporarily unavailable'"
        );
    }

    /// End-to-end check that the linux arm tolerates a peer that is
    /// briefly slow to respond (200ms). With the buggy
    /// `set_read_timeout(Some(BURST_TIMEOUT))` the 60-second deadline
    /// is huge so this test wouldn't surface the EAGAIN per se — the
    /// failure mode in the field is the SAME read_timeout firing on
    /// a hung peer. The companion contract test above
    /// (`open_control_connection_leaves_read_timeout_unset`) is the
    /// one that fails on the buggy code; this test asserts the run
    /// still completes correctly with a real delay on the wire, so
    /// the fix doesn't accidentally regress the happy path.
    #[test]
    fn run_bucket_tolerates_slow_peer_response_without_eagain() {
        let (ip, port) = spawn_slow_burst_peer(Duration::from_millis(200));
        let mut stream = open_control_connection(ip, port).unwrap();
        let mut cfg = LinuxRxBurstCfg {
            stream: &mut stream,
            bucket_id: 0,
            segment_size: 64,
            burst_count: 16,
            warmup_bursts: 1,
            measure_bursts: 1,
        };
        let run = run_bucket(&mut cfg).expect(
            "run_bucket against slow peer must not surface EAGAIN — \
             blocking sockets either succeed or block",
        );
        assert_eq!(run.samples.len(), 16);
    }
}
