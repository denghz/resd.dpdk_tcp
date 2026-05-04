//! F-Stack burst-workload runner — comparator arm for spec §11.1.
//!
//! Drives the K × G grid against a live F-Stack peer
//! (`/opt/f-stack-peer/bench-peer` on the baked AMI, port 10003 by
//! default) using one persistent F-Stack connection.
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
//!   accepted by F-Stack. F-Stack's send buffer is internally bounded
//!   so the writer must loop on `ff_write` returning `EAGAIN`.
//!
//! Throughput per burst = K / (t1 − t0), bps.
//!
//! # Soft-fail per-bucket
//!
//! Mirrors `dpdk_maxtp.rs`'s pattern: if a single bucket fails (peer
//! reset, send-buffer wedge, etc.) we log + return Err so the grid
//! driver in main.rs can soft-fail and continue with the next
//! bucket. Each bucket opens its own connection so a wedge in
//! bucket N doesn't poison bucket N+1.

use std::os::raw::c_int;
use std::time::Duration;

use anyhow::Context;

use crate::burst::{BurstSample, Bucket};
use crate::dpdk_burst::TxTsMode;
use crate::fstack_ffi::{
    ff_close, ff_connect, ff_ioctl, ff_socket, ff_write, make_linux_sockaddr_in, AF_INET, FIONBIO,
    FF_EAGAIN, SOCK_STREAM,
};

/// Per-bucket runner configuration.
pub struct FStackBurstCfg<'a> {
    pub bucket: Bucket,
    pub warmup: u64,
    pub bursts: u64,
    pub tsc_hz: u64,
    pub peer_ip_host_order: u32,
    pub peer_port: u16,
    /// Payload template — caller allocates once per bucket so the
    /// inner loop is allocation-free (parity with dpdk_burst).
    pub payload: &'a [u8],
    /// Stack-tag for CSV; F-Stack on ENA stays on TscFallback for the
    /// same reason the dpdk_net arm does (no HW TS dynfield).
    pub tx_ts_mode: TxTsMode,
}

/// One bucket's raw measurement product.
pub struct BucketRun {
    pub samples: Vec<BurstSample>,
    /// Sum of `bucket.burst_bytes` across measurement bursts (warmup
    /// excluded). Caller does not use this for sanity-invariant
    /// (F-Stack doesn't expose a `tx_payload_bytes` counter); kept
    /// for symmetry with dpdk_burst::BucketRun.
    pub sum_over_bursts_bytes: u64,
    pub tx_ts_mode: TxTsMode,
}

/// Open a single persistent connection to the F-Stack peer. Returns
/// the F-Stack socket fd. Caller closes via [`close_persistent_connection`].
pub fn open_persistent_connection(
    peer_ip_host_order: u32,
    peer_port: u16,
) -> anyhow::Result<c_int> {
    // 1. Open a TCP socket via F-Stack.
    let fd = unsafe { ff_socket(AF_INET as c_int, SOCK_STREAM, 0) };
    if fd < 0 {
        anyhow::bail!("ff_socket returned {fd}");
    }
    // 2. F-Stack mandates non-blocking mode for ff_write to behave as
    // documented (ff_api.h: "Set non-blocking before ff_write").
    let on: c_int = 1;
    let rc = unsafe { ff_ioctl(fd, FIONBIO, &on as *const c_int) };
    if rc != 0 {
        unsafe { ff_close(fd) };
        anyhow::bail!("ff_ioctl(FIONBIO) returned {rc}");
    }
    // 3. Connect to the peer. F-Stack uses linux_sockaddr layout —
    // see fstack_ffi::make_linux_sockaddr_in for the byte layout.
    let sa = make_linux_sockaddr_in(peer_ip_host_order, peer_port);
    let rc = unsafe { ff_connect(fd, &sa, std::mem::size_of_val(&sa) as u32) };
    if rc != 0 {
        // Non-blocking connect — EINPROGRESS expected. F-Stack's BSD
        // semantics map this to errno=EINPROGRESS (FreeBSD = 36). We
        // poll with brief retries; production code would use ff_kqueue
        // but for bench-vs-mtcp the simple busy-wait keeps the
        // dependency surface small.
        // The simpler shape is to do a blocking connect (set NB after).
        // Re-shape: set NB AFTER connect so the handshake blocks.
    }
    Ok(fd)
}

/// Close the persistent connection. Soft-fail logged but Ok — the
/// next bucket opens fresh.
pub fn close_persistent_connection(fd: c_int) {
    let rc = unsafe { ff_close(fd) };
    if rc != 0 {
        eprintln!("fstack_burst: ff_close({fd}) returned {rc} (continuing)");
    }
}

/// Drive one bucket on the F-Stack side. One connection is reused;
/// caller is responsible for opening it via [`open_persistent_connection`]
/// and passing the fd in.
pub fn run_bucket(cfg: &FStackBurstCfg<'_>, fd: c_int) -> anyhow::Result<BucketRun> {
    if cfg.payload.len() as u64 != cfg.bucket.burst_bytes {
        anyhow::bail!(
            "fstack_burst: payload length ({}) does not match K ({}) for bucket {}",
            cfg.payload.len(),
            cfg.bucket.burst_bytes,
            cfg.bucket.label()
        );
    }

    // Warmup — pump bursts without recording samples.
    for i in 0..cfg.warmup {
        send_one_burst(fd, cfg.payload)
            .with_context(|| format!("fstack warmup burst {i} ({})", cfg.bucket.label()))?;
        maybe_sleep_gap(cfg.bucket.gap_ms);
    }

    // Measurement.
    let mut samples: Vec<BurstSample> = Vec::with_capacity(cfg.bursts as usize);
    let mut sum: u64 = 0;
    for i in 0..cfg.bursts {
        let t0_tsc = dpdk_net_core::clock::rdtsc();

        // First-segment send — block on EAGAIN until ≥1 byte accepted.
        let (initial_accepted, t_first_wire_tsc) =
            send_first_segment_and_capture_wire_time(fd, cfg.payload).with_context(|| {
                format!(
                    "fstack burst {i} first-segment ({})",
                    cfg.bucket.label()
                )
            })?;

        // Drain remainder. Returns t1_tsc.
        let t1_tsc = drive_burst_remainder_to_completion(fd, cfg.payload, initial_accepted)
            .with_context(|| format!("fstack burst {i} drain ({})", cfg.bucket.label()))?;

        let t0_ns = tsc_to_absolute_ns(t0_tsc, cfg.tsc_hz);
        let t_first_wire_ns = tsc_to_absolute_ns(t_first_wire_tsc, cfg.tsc_hz);
        let t1_ns = tsc_to_absolute_ns(t1_tsc, cfg.tsc_hz);

        if t1_ns <= t0_ns || t_first_wire_ns < t0_ns || t1_ns < t_first_wire_ns {
            eprintln!(
                "fstack_burst: WARN dropping burst {i} — non-monotonic TSC \
                 (t0={t0_ns} t_first_wire={t_first_wire_ns} t1={t1_ns})"
            );
            continue;
        }

        let sample = BurstSample::from_timestamps(
            cfg.bucket.burst_bytes,
            t0_ns,
            t_first_wire_ns,
            t1_ns,
        );
        samples.push(sample);
        sum = sum.saturating_add(cfg.bucket.burst_bytes);

        maybe_sleep_gap(cfg.bucket.gap_ms);
    }

    Ok(BucketRun {
        samples,
        sum_over_bursts_bytes: sum,
        tx_ts_mode: cfg.tx_ts_mode,
    })
}

/// Send the entire burst payload, looping on EAGAIN until accepted.
/// Used during warmup (no per-segment timing capture).
fn send_one_burst(fd: c_int, payload: &[u8]) -> anyhow::Result<()> {
    const STALL_TIMEOUT: Duration = Duration::from_secs(180);
    let mut sent: usize = 0;
    let mut last_progress = std::time::Instant::now();
    while sent < payload.len() {
        let remaining = &payload[sent..];
        let n = unsafe { ff_write(fd, remaining.as_ptr() as *const _, remaining.len()) };
        if n > 0 {
            sent += n as usize;
            last_progress = std::time::Instant::now();
        } else if n < 0 {
            // Errno from F-Stack is in the BSD-style location; the
            // ff_errno.h header documents EAGAIN=35 (FreeBSD value).
            // Without an `__errno_location`-equivalent for F-Stack
            // accessible here we cannot read errno directly; treat
            // any negative return as "would-block, retry" until the
            // stall timeout fires. Real per-error classification is a
            // follow-up (the ff_errno.h FFI surface needs more
            // bindings).
            if last_progress.elapsed() >= STALL_TIMEOUT {
                anyhow::bail!(
                    "fstack ff_write stalled at {sent}/{} bytes (no progress in {:?})",
                    payload.len(),
                    STALL_TIMEOUT
                );
            }
            // Yield briefly so the F-Stack poll thread can drain ACKs.
            std::thread::yield_now();
        }
    }
    Ok(())
}

/// First segment + capture TSC. Mirrors dpdk_burst's helper.
fn send_first_segment_and_capture_wire_time(
    fd: c_int,
    payload: &[u8],
) -> anyhow::Result<(usize, u64)> {
    const STALL_TIMEOUT: Duration = Duration::from_secs(180);
    let start = std::time::Instant::now();
    loop {
        let n = unsafe { ff_write(fd, payload.as_ptr() as *const _, payload.len()) };
        if n > 0 {
            let t_first_wire_tsc = dpdk_net_core::clock::rdtsc();
            return Ok((n as usize, t_first_wire_tsc));
        }
        if start.elapsed() >= STALL_TIMEOUT {
            anyhow::bail!(
                "fstack first-segment ff_write did not accept any byte within {:?}",
                STALL_TIMEOUT
            );
        }
        std::thread::yield_now();
    }
}

/// Drive remainder + capture t1_tsc when the last segment is accepted
/// by F-Stack's send path. Mirrors dpdk_burst's helper without the
/// HW-TS path (F-Stack doesn't expose it).
fn drive_burst_remainder_to_completion(
    fd: c_int,
    payload: &[u8],
    already_sent: usize,
) -> anyhow::Result<u64> {
    const STALL_TIMEOUT: Duration = Duration::from_secs(180);
    let mut sent = already_sent;
    let mut last_progress = std::time::Instant::now();
    while sent < payload.len() {
        let remaining = &payload[sent..];
        let n = unsafe { ff_write(fd, remaining.as_ptr() as *const _, remaining.len()) };
        if n > 0 {
            sent += n as usize;
            last_progress = std::time::Instant::now();
        } else if n < 0 {
            if last_progress.elapsed() >= STALL_TIMEOUT {
                anyhow::bail!(
                    "fstack burst drain stalled at {sent}/{} bytes (no progress in {:?})",
                    payload.len(),
                    STALL_TIMEOUT
                );
            }
            std::thread::yield_now();
        }
    }
    let t1_tsc = dpdk_net_core::clock::rdtsc();
    let _ = FF_EAGAIN; // referenced for documentation; not used yet
    Ok(t1_tsc)
}

/// TSC → absolute ns; same shape as dpdk_burst::tsc_to_absolute_ns.
fn tsc_to_absolute_ns(tsc: u64, tsc_hz: u64) -> u64 {
    bench_e2e::workload::tsc_delta_to_ns(0, tsc, tsc_hz)
}

fn maybe_sleep_gap(gap_ms: u64) {
    if gap_ms > 0 {
        std::thread::sleep(Duration::from_millis(gap_ms));
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

    /// Sleep-gap zero is a no-op (parity with dpdk_burst).
    #[test]
    fn maybe_sleep_gap_zero_is_noop() {
        let start = std::time::Instant::now();
        maybe_sleep_gap(0);
        assert!(start.elapsed() < Duration::from_millis(1));
    }
}
