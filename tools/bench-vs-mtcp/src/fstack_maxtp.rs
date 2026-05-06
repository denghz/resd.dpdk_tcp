//! F-Stack max-sustained-throughput runner — comparator arm for spec §11.2.
//!
//! Drives the W × C grid against a live F-Stack peer
//! (`/opt/f-stack-peer/bench-peer` on the baked AMI, port 10003) using
//! `C` persistent F-Stack connections. Mirrors `dpdk_maxtp.rs`'s shape
//! and `linux_maxtp.rs`'s round-robin pump, both of which are the
//! existing comparator arms on this grid.
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
//! Goodput = `bytes_sent_in_window / window_duration_ns`, in bps.
//! `bytes_sent_in_window` is the running sum of successful
//! `ff_write` return values during the 60s measurement window after
//! a 10s warmup. F-Stack does not expose a per-conn `snd_una` or
//! ACKed-bytes counter through its BSD-shaped API, so we use bytes-
//! sent as the goodput proxy — under TCP semantics the two converge
//! across a 60s window because the in-flight cap is small relative
//! to the window.
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

use std::os::raw::c_int;
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::dpdk_maxtp::TxTsMode;
use crate::fstack_burst::CONNECT_TIMEOUT;
use crate::fstack_ffi::{
    connect_nonblocking, ff_close, ff_errno, ff_read, ff_write, FF_EAGAIN, FF_EWOULDBLOCK,
};
use crate::maxtp::{Bucket, MaxtpSample};

/// Per-bucket runner configuration.
pub struct FStackMaxtpCfg {
    pub bucket: Bucket,
    pub warmup: Duration,
    pub duration: Duration,
    pub peer_ip_host_order: u32,
    pub peer_port: u16,
    /// Payload — caller allocates once per bucket entry.
    pub payload: Vec<u8>,
    pub tx_ts_mode: TxTsMode,
}

/// One bucket's raw measurement product. Mirrors `linux_maxtp::BucketRun`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketRun {
    pub sample: MaxtpSample,
    /// Bytes the F-Stack send path accepted across the measurement
    /// window. Surfaced for the post-run sanity-invariant check (the
    /// caller asserts this is roughly `goodput_bps × duration / 8`).
    pub bytes_sent_in_window: u64,
    pub tx_ts_mode: TxTsMode,
}

/// Open `C` persistent F-Stack connections to the peer. Each carries
/// non-blocking mode (`FIONBIO`) so partial-accept doesn't stall the
/// round-robin pump.
///
/// Each socket is opened via [`connect_nonblocking`], which sets
/// `FIONBIO` *before* `ff_connect` and threads the resulting
/// `EINPROGRESS` through `ff_select` + `ff_getsockopt(SO_ERROR)`. The
/// previous shape (blocking-connect-then-flip-NB) relied on F-Stack
/// sockets defaulting to blocking — true today, but a fragile
/// assumption that left no signal for connect failures other than
/// "ff_connect returned -1" with no errno classification. This shape
/// surfaces real failures (ECONNREFUSED, ETIMEDOUT) cleanly.
pub fn open_persistent_connections(
    peer_ip_host_order: u32,
    peer_port: u16,
    conn_count: u64,
) -> anyhow::Result<Vec<c_int>> {
    if conn_count == 0 {
        anyhow::bail!("fstack_maxtp: conn_count must be > 0");
    }
    let mut out: Vec<c_int> = Vec::with_capacity(conn_count as usize);
    for i in 0..conn_count {
        match connect_nonblocking(peer_ip_host_order, peer_port, CONNECT_TIMEOUT) {
            Ok(fd) => out.push(fd),
            Err(e) => {
                close_all(&out);
                anyhow::bail!(
                    "fstack_maxtp: connect on conn {i} to {ip}:{peer_port} failed: {e}",
                    ip = format_ip_host_order(peer_ip_host_order)
                );
            }
        }
    }
    Ok(out)
}

/// Close every F-Stack socket from a finished bucket. Soft-fail on
/// per-fd errors — the bucket's CSV row is already emitted, this is
/// hygiene for the next bucket's open.
pub fn close_persistent_connections(conns: &[c_int]) {
    for &fd in conns {
        let rc = unsafe { ff_close(fd) };
        if rc != 0 {
            eprintln!("fstack_maxtp: ff_close({fd}) returned {rc}; continuing");
        }
    }
}

/// Drive one bucket on the F-Stack side. Mirrors
/// `dpdk_maxtp::run_bucket` / `linux_maxtp::run_bucket` shape.
pub fn run_bucket(cfg: &FStackMaxtpCfg, conns: &[c_int]) -> anyhow::Result<BucketRun> {
    if conns.len() as u64 != cfg.bucket.conn_count {
        anyhow::bail!(
            "fstack_maxtp: conns.len() = {} does not match bucket.conn_count = {}",
            conns.len(),
            cfg.bucket.conn_count
        );
    }
    if cfg.payload.len() as u64 != cfg.bucket.write_bytes {
        anyhow::bail!(
            "fstack_maxtp: payload.len() = {} does not match bucket.write_bytes = {}",
            cfg.payload.len(),
            cfg.bucket.write_bytes
        );
    }
    if cfg.duration.as_nanos() == 0 {
        anyhow::bail!("fstack_maxtp: measurement duration must be > 0");
    }

    // Warmup.
    let warmup_deadline = Instant::now() + cfg.warmup;
    let _ = pump_round_robin(conns, &cfg.payload, warmup_deadline)
        .context("fstack_maxtp warmup phase")?;

    // Measurement window.
    let t_measure_start = Instant::now();
    let measure_deadline = t_measure_start + cfg.duration;
    let bytes_sent_in_window = pump_round_robin(conns, &cfg.payload, measure_deadline)
        .context("fstack_maxtp measurement phase")?;
    let t_measure_end = Instant::now();

    let elapsed_ns = t_measure_end
        .saturating_duration_since(t_measure_start)
        .as_nanos() as u64;
    let sample = MaxtpSample::from_window(bytes_sent_in_window, 0, elapsed_ns);

    Ok(BucketRun {
        sample,
        bytes_sent_in_window,
        tx_ts_mode: cfg.tx_ts_mode,
    })
}

/// Pump writes round-robin across F-Stack sockets until `deadline`.
/// Returns the total bytes accepted (sum of successful `ff_write`
/// return values).
///
/// `ff_write` < 0 with `errno == EAGAIN` is treated as transient —
/// skip to the next conn. Any other errno (ECONNRESET, EPIPE, EBADF)
/// is recorded but not fatal: the bucket keeps pumping the remaining
/// healthy conns, and the operator sees the anomaly via the bucket's
/// goodput floor. We log the first non-EAGAIN error per fd so a
/// wedged conn surfaces in the harness output without spamming.
fn pump_round_robin(
    conns: &[c_int],
    payload: &[u8],
    deadline: Instant,
) -> anyhow::Result<u64> {
    if conns.is_empty() {
        anyhow::bail!("fstack_maxtp: pump_round_robin: conns is empty");
    }
    let mut total: u64 = 0;
    let check_between_conns = conns.len() == 1;
    // Track which fds have already logged a non-EAGAIN error so we
    // don't spam the harness output for a wedged peer. Bounded by
    // the conns slice length so this stays cheap.
    let mut logged_error: Vec<bool> = vec![false; conns.len()];
    // Per-conn discard buffer — drain inbound bytes the peer echoes
    // (F-Stack peer is an echo-server, same as bench-e2e's). Without
    // draining, the recv buffer fills + backpressures the writer
    // through the BSD layer.
    let mut discard = vec![0u8; 65536];
    loop {
        if Instant::now() >= deadline {
            return Ok(total);
        }
        for (idx, &fd) in conns.iter().enumerate() {
            // Drain inbound (non-blocking). EAGAIN -> nothing pending.
            let mut drained = 0usize;
            while drained < discard.len() * 4 {
                let n = unsafe { ff_read(fd, discard.as_mut_ptr() as *mut _, discard.len()) };
                if n > 0 {
                    drained += n as usize;
                    if (n as usize) < discard.len() {
                        break;
                    }
                } else {
                    // 0 = EOF, <0 = error/EAGAIN — both stop the inner
                    // drain loop. A genuine connection error will surface
                    // on the subsequent ff_write below.
                    break;
                }
            }

            // Write payload.
            let n = unsafe { ff_write(fd, payload.as_ptr() as *const _, payload.len()) };
            if n > 0 {
                total = total.saturating_add(n as u64);
            } else if n < 0 {
                let e = ff_errno();
                if e != FF_EAGAIN && e != FF_EWOULDBLOCK && !logged_error[idx] {
                    eprintln!(
                        "fstack_maxtp: ff_write returned {n} on fd={fd}; errno={e} \
                         (not EAGAIN; bucket continues with remaining conns)"
                    );
                    logged_error[idx] = true;
                }
                // Even on a real error we keep pumping the remaining
                // conns — the bucket's CSV row will reflect the loss
                // via reduced goodput. A retry on a dead fd is cheap
                // (returns instantly with the same errno).
            }

            if check_between_conns && Instant::now() >= deadline {
                return Ok(total);
            }
        }
    }
}

/// Close-all helper for per-conn open failure. Drops every fd opened
/// up to that point; soft-fails on per-fd errors.
fn close_all(conns: &[c_int]) {
    for &fd in conns {
        let _ = unsafe { ff_close(fd) };
    }
}

/// Format an IP host-order u32 as dotted-quad for log messages.
fn format_ip_host_order(ip: u32) -> String {
    let b = ip.to_be_bytes();
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_rejects_zero_conn_count() {
        let err = open_persistent_connections(0x7f00_0001, 1, 0).unwrap_err();
        assert!(err.to_string().contains("conn_count must be > 0"));
    }

    /// `format_ip_host_order` produces dotted-quad — verify the byte
    /// order matches `Ipv4Addr::to_string()`.
    #[test]
    fn format_ip_host_order_dotted_quad() {
        assert_eq!(format_ip_host_order(0x0A_00_00_2A), "10.0.0.42");
        assert_eq!(format_ip_host_order(0xC0_A8_01_0A), "192.168.1.10");
    }
}
