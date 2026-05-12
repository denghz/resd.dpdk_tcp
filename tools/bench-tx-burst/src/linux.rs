//! Linux kernel TCP burst-workload runner — comparator arm for
//! spec §11.1.
//!
//! Phase 5 of the 2026-05-09 bench-suite overhaul extends the
//! comparator triplet (dpdk_net + linux_kernel + fstack) to cover the
//! burst grid as well as RTT (bench-rtt). The legacy bench-vs-mtcp
//! burst arm only ran dpdk_net + fstack and printed a WARN line for
//! linux_kernel.
//!
//! # What this measures
//!
//! Per spec §11.1, one persistent kernel-TCP `TcpStream`, K bytes
//! pumped via `write_all` per burst, gap G between bursts. The DUT's
//! TX path is the unit-under-test; the peer is the existing
//! `tools/bench-e2e/peer/echo-server` on port 10001. We do NOT
//! measure the peer's recv path — the recv buffer is drained off the
//! kernel socket between bursts so the kernel's send-side back-pressure
//! doesn't stall the next burst's `write_all`.
//!
//! # Why blocking I/O
//!
//! `TcpStream::write_all` blocks until every byte is queued in the
//! kernel send buffer; that's the closest equivalent of bench-tx-burst's
//! "K bytes accepted by the stack" semantics for the Linux comparator.
//! Non-blocking + a syscall-level write loop would add poll overhead
//! to the very thing we're measuring (the kernel's TX path).
//!
//! # TX-TS mode
//!
//! Linux kernel TCP doesn't expose NIC HW TX timestamps to user-space
//! through the `TcpStream` API — we mark `tx_ts_mode = "n/a"` on the
//! resulting CSV rows. The primary metric on this arm is
//! `write_acceptance_rate_bps = K / (t1 - t0)` measured with
//! `Instant::now()` — t1 is captured right after the final
//! `write_all` returns, when the kernel has accepted all K bytes into
//! its send buffer but has NOT necessarily put them on the wire. We
//! deliberately use a DIFFERENT metric name from the dpdk_net arm's
//! `throughput_per_burst_bps` (which captures t1 at
//! `rte_eth_tx_burst`-return ≈ wire-rate) so downstream readers don't
//! conflate buffer-fill rate with wire rate; see
//! `Stack::throughput_metric_name` (T57 follow-up #2). Secondary
//! `burst_initiation_ns` and `burst_steady_bps` mirror the dpdk arm
//! but use the same `Instant::now()` clock for `t_first_wire` (right
//! after the first `write` call returns) — the asymmetry vs the dpdk
//! arm's TSC-at-NIC capture is documented in the bucket's
//! `dimensions_json`.
//!
//! # Per-conn contract
//!
//! One persistent `TcpStream` per bucket-grid run; reused for every
//! bucket. Spec §11.1 calls for "one connection per lcore, established
//! once, reused for the whole run" — we apply the same shape on the
//! kernel side. The connection is opened with `TCP_NODELAY` so small
//! tail segments don't sit waiting for an ACK.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::burst::{Bucket, BurstSample};

/// Connect deadline for the initial kernel-TCP handshake. Same shape
/// as bench-rtt's `linux_kernel::CONNECT_TIMEOUT`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-burst write deadline. Matches the dpdk arm's stall watchdog
/// scope so a wedged peer surfaces uniformly across stacks.
const BURST_TIMEOUT: Duration = Duration::from_secs(180);

/// Per-bucket runner configuration.
pub struct LinuxBurstCfg<'a> {
    pub stream: &'a mut TcpStream,
    pub bucket: Bucket,
    pub warmup: u64,
    pub bursts: u64,
    /// Payload template — one allocation per bucket reused across
    /// every burst inside the bucket. Caller sizes this to
    /// `bucket.burst_bytes`.
    pub payload: &'a [u8],
}

/// One bucket's raw measurement product. Mirrors `dpdk::BucketRun`
/// minus the `tx_ts_mode` field — the linux arm always runs without
/// HW timestamps.
pub struct BucketRun {
    pub samples: Vec<BurstSample>,
    /// Sum of `bucket.burst_bytes` across measurement bursts (warmup
    /// excluded). Mirrors `dpdk::BucketRun::sum_over_bursts_bytes` for
    /// CSV-shape symmetry; the linux arm has no `tx_payload_bytes`
    /// counter to cross-check against, so this is reporting-only.
    pub sum_over_bursts_bytes: u64,
}

/// Open a persistent kernel-TCP connection to the peer. Sets
/// `TCP_NODELAY` so the trailing short writes inside a burst flush
/// without waiting for delayed ACK on the peer side.
pub fn open_persistent_connection(
    peer_ip_host_order: u32,
    peer_port: u16,
) -> anyhow::Result<TcpStream> {
    let octets = peer_ip_host_order.to_be_bytes();
    let addr = SocketAddrV4::new(Ipv4Addr::from(octets), peer_port);
    let stream = TcpStream::connect_timeout(&addr.into(), CONNECT_TIMEOUT)
        .with_context(|| format!("kernel TCP connect to {}", addr))?;
    stream
        .set_nodelay(true)
        .context("set_nodelay on kernel TCP burst stream")?;
    // No global read/write timeouts — per-burst we set a write timeout
    // before the burst, and we drain reads non-blockingly between
    // bursts.
    Ok(stream)
}

/// Drive one bucket on the linux_kernel side. One persistent stream is
/// reused; caller is responsible for opening it and passing it in.
pub fn run_bucket(cfg: &mut LinuxBurstCfg<'_>) -> anyhow::Result<BucketRun> {
    if cfg.payload.len() as u64 != cfg.bucket.burst_bytes {
        anyhow::bail!(
            "linux burst: payload length ({}) does not match K ({}) for bucket {}",
            cfg.payload.len(),
            cfg.bucket.burst_bytes,
            cfg.bucket.label()
        );
    }

    cfg.stream
        .set_write_timeout(Some(BURST_TIMEOUT))
        .context("set_write_timeout pre-warmup")?;

    // Warmup — pump N bursts without recording samples.
    for i in 0..cfg.warmup {
        write_one_burst(cfg.stream, cfg.payload)
            .with_context(|| format!("warmup burst {i} ({})", cfg.bucket.label()))?;
        drain_pending_reads(cfg.stream)
            .with_context(|| format!("warmup drain {i} ({})", cfg.bucket.label()))?;
        sleep_gap(cfg.bucket.gap_ms);
    }

    // Measurement — record one sample per burst.
    let mut samples: Vec<BurstSample> = Vec::with_capacity(cfg.bursts as usize);
    let mut sum: u64 = 0;
    for i in 0..cfg.bursts {
        // t0 = pre-first-write Instant.
        let t0 = Instant::now();

        // First chunk — write a small slice so we can capture
        // t_first_wire after the kernel queued segment 1. We use the
        // same MSS-ish slice (1460 B or the full payload if smaller)
        // as the "first segment" boundary marker.
        let first_chunk_len = std::cmp::min(1460, cfg.payload.len());
        let mut sent: usize = 0;
        cfg.stream
            .write_all(&cfg.payload[..first_chunk_len])
            .with_context(|| {
                format!("burst {i} first-chunk write ({})", cfg.bucket.label())
            })?;
        sent += first_chunk_len;
        let t_first_wire = Instant::now();

        // Remainder — write_all blocks until queued.
        if sent < cfg.payload.len() {
            cfg.stream
                .write_all(&cfg.payload[sent..])
                .with_context(|| format!("burst {i} remainder write ({})", cfg.bucket.label()))?;
        }
        let t1 = Instant::now();

        // Instant doesn't expose absolute ns; compute deltas relative
        // to t0 and feed them into BurstSample::from_timestamps which
        // uses the same anchor convention (the asserts inside care
        // only about relative ordering).
        let t_first_wire_ns = t_first_wire.duration_since(t0).as_nanos() as u64;
        let t1_ns = t1.duration_since(t0).as_nanos() as u64;

        // Guard against monotonicity hiccups (shouldn't happen with
        // Instant; defensive parity with the dpdk arm).
        if t1_ns == 0 || t_first_wire_ns > t1_ns {
            eprintln!(
                "bench-tx-burst: WARN linux dropping burst {i} — non-monotonic Instant \
                 (t_first_wire={t_first_wire_ns} t1={t1_ns})"
            );
            sleep_gap(cfg.bucket.gap_ms);
            continue;
        }
        // BurstSample::from_timestamps requires t1 > t0 strictly. Use
        // `1u64` as the t0 anchor when needed to satisfy the assert; the
        // delta math stays correct because we feed t_first_wire and t1
        // as deltas measured from the same Instant.
        let safe_t0 = 0u64;
        let safe_t_first = std::cmp::max(t_first_wire_ns, 1);
        let safe_t1 = std::cmp::max(t1_ns, safe_t_first.saturating_add(1));
        let sample = BurstSample::from_timestamps(
            cfg.bucket.burst_bytes,
            safe_t0,
            safe_t_first,
            safe_t1,
        );
        samples.push(sample);
        sum = sum.saturating_add(cfg.bucket.burst_bytes);

        // Drain echo bytes between bursts so the recv buffer doesn't
        // back up. echo-server returns the bytes we just sent; we
        // discard them.
        drain_pending_reads(cfg.stream)
            .with_context(|| format!("burst {i} drain ({})", cfg.bucket.label()))?;
        sleep_gap(cfg.bucket.gap_ms);
    }

    Ok(BucketRun {
        samples,
        sum_over_bursts_bytes: sum,
    })
}

/// Write `payload.len()` bytes via `write_all`. Used in warmup and as
/// a building block for the timed measurement path.
fn write_one_burst(stream: &mut TcpStream, payload: &[u8]) -> anyhow::Result<()> {
    stream
        .write_all(payload)
        .context("write_all on kernel TCP burst stream")
}

/// Drain whatever bytes the peer has echoed back into the kernel
/// recv buffer non-blockingly. The bytes are discarded — the linux
/// arm doesn't measure the recv path. Returns when there's nothing
/// left to read at this moment.
fn drain_pending_reads(stream: &mut TcpStream) -> anyhow::Result<()> {
    // Switch to non-blocking just for the drain so the loop doesn't
    // wait on a future segment.
    stream
        .set_nonblocking(true)
        .context("set_nonblocking for drain")?;
    let mut scratch = [0u8; 64 * 1024];
    let mut total: usize = 0;
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => break, // EOF (peer closed) — caller will surface.
            Ok(n) => total = total.saturating_add(n),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                stream.set_nonblocking(false).ok();
                return Err(e).context("read during drain");
            }
        }
    }
    let _ = total; // discarded — recv path is not measured.
    stream
        .set_nonblocking(false)
        .context("set_nonblocking(false) restoring blocking")?;
    Ok(())
}

/// Sleep `gap_ms` milliseconds. `gap_ms == 0` is a no-op — we don't
/// call `thread::sleep(Duration::ZERO)` since some platforms still
/// yield on a zero-duration sleep.
fn sleep_gap(gap_ms: u64) {
    if gap_ms > 0 {
        std::thread::sleep(Duration::from_millis(gap_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Spin up an in-process listener that just drains whatever it
    /// reads. Returns `(host_order_ip, port)` for the connect side.
    fn spawn_drain_peer() -> (u32, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if let Ok(mut s) = stream {
                    // Echo back so the bench's drain side has bytes to read.
                    let mut buf = [0u8; 64 * 1024];
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        if s.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        // 127.0.0.1 host-order = 0x7F000001
        (0x7F00_0001u32, port)
    }

    #[test]
    fn open_and_one_warmup_burst() {
        let (ip, port) = spawn_drain_peer();
        let mut stream = open_persistent_connection(ip, port).unwrap();
        // Tiny burst — 64 B, no gap. Just proves the connect + write
        // path lights up without panics.
        let payload = vec![0u8; 64];
        let bucket = Bucket::new(64, 0);
        let mut cfg = LinuxBurstCfg {
            stream: &mut stream,
            bucket,
            warmup: 1,
            bursts: 1,
            payload: &payload,
        };
        let run = run_bucket(&mut cfg).unwrap();
        assert_eq!(run.samples.len(), 1);
        assert_eq!(run.sum_over_bursts_bytes, 64);
        let s = run.samples[0];
        assert!(s.throughput_bps > 0.0);
    }
}
