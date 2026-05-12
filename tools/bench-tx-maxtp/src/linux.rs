//! Linux kernel TCP max-sustained-throughput runner — comparator arm
//! for spec §11.2.
//!
//! # Phase 6 Task 6.2: send→ACK latency asymmetry vs dpdk arm
//!
//! The dpdk arm's `--send-ack-csv` emits ONE ROW PER TCP SEGMENT for
//! every cumulative ACK, with a precise `latency_ns` per (begin_seq,
//! end_seq) range. The linux arm cannot replicate that granularity from
//! user space — there's no kernel API that exposes per-segment send
//! timestamps + ACK timestamps. The closest available knob is
//! `getsockopt(IPPROTO_TCP, TCP_INFO)` which returns aggregate state
//! (`tcpi_rtt`, `tcpi_total_retrans`, `tcpi_unacked`).
//!
//! When `--send-ack-csv` is passed, the linux arm emits one row per
//! conn per `TCP_INFO_SAMPLE_INTERVAL` (1 s) during the measurement
//! window, with `scope = "linux_tcp_info"` so downstream pivots can
//! split linux rows from dpdk rows. The row carries `tcpi_rtt_us`
//! (smoothed-RTT estimate, microseconds), `tcpi_total_retrans`
//! (cumulative segment retransmits), and `tcpi_unacked` (in-flight
//! segment count). All three are kernel-side counters / estimators —
//! they describe the SAME phenomenon as the dpdk arm's per-segment
//! samples, but at coarser cadence and without per-segment attribution.
//!
//! Bench analysis tools should treat the two stacks' rows asymmetrically:
//! per-segment latency CDF for dpdk_segment scope; time-series of RTT /
//! retrans for linux_tcp_info scope. A full per-segment kernel view
//! would require eBPF instrumentation of `tcp_event_data_sent` /
//! `tcp_event_data_recv` — out of scope for Phase 6.
//!
//! Drives the W × C grid against a live kernel-TCP sink peer using
//! `std::net::TcpStream` (kernel sockets, no DPDK). For each bucket,
//! opens `C` persistent connections, pumps W-byte writes round-robin
//! for T = 60 s after a 10 s warmup, and reports sustained goodput.
//!
//! # Why this exists
//!
//! The dpdk_net maxtp arm in `dpdk_maxtp.rs` measures our user-space
//! TCP stack at sustained-throughput. The mTCP arm in `mtcp.rs` is
//! stubbed (Plan A AMI rebuild blocked on libmtcp / gcc 13). To still
//! produce a meaningful comparison row, we land a kernel-TCP arm
//! here; it shares the peer (`linux-tcp-sink` on port 10002 in the
//! bench-pair script's [6/12] step) so the only delta vs dpdk_net is
//! the client-side stack.
//!
//! # Measurement contract
//!
//! Goodput is measured as bytes-actually-sent in the window (the sum
//! of `write` return values during `[t_warmup_end, t_warmup_end + T]`)
//! divided by T, in bits/sec. We don't have the dpdk_net stack's
//! `snd_una` introspection, so "ACKed bytes" maps to "bytes the
//! kernel sent + that didn't error" — which under TCP semantics
//! converges to the same number across a 60 s window because the
//! socket's send buffer + in-flight cap is small relative to the
//! window.
//!
//! Packet rate (`pps`) is read from `getsockopt(TCP_INFO).tcpi_segs_out`
//! snapshots at the start + end of the measurement window — closes T56
//! open follow-up #1 (linux arm's `tx_pps` previously hardcoded to 0.0
//! made the SUMMARY.md `tx_pps` table show 0.0 for every (W,C) cell,
//! which was easy to misread as "the linux arm is broken"). The pps
//! reported is segments (NOT bytes) per second summed across all
//! conns — matches the dpdk arm's `eth.tx_pkts / T` shape per spec
//! §11.2. We still emit `tx_ts_mode = "n/a"` because the kernel doesn't
//! expose per-segment HW TX timestamps.
//!
//! # Multi-connection pump loop
//!
//! Mirrors `dpdk_maxtp::pump_round_robin`'s shape: for `C > 1` we open
//! `C` connections up-front and round-robin writes across them in the
//! inner hot loop. Each connection is a separate kernel socket so the
//! kernel handles per-conn TX-side fan-out itself.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::Context;
use bench_common::raw_samples::RawSamplesWriter;

use crate::maxtp::{Bucket, MaxtpSample};

/// Phase 6 Task 6.2 sample interval for the linux arm's coarse
/// `getsockopt(TCP_INFO)` snapshots. Mirrors the dpdk arm's
/// `SAMPLE_INTERVAL` so the two stacks have comparable cadence — but the
/// linux samples are aggregate (`tcpi_rtt`, `tcpi_total_retrans`,
/// `tcpi_unacked`) rather than per-segment latency. Documented in the
/// module header.
const TCP_INFO_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Phase 6 Task 6.2 — minimal `tcp_info` mirror (linux/tcp.h). Only the
/// fields we read are decoded; the rest of the kernel struct is treated
/// as opaque trailing bytes and silently ignored. This is FAR less than
/// the kernel's full `tcp_info` shape, but `getsockopt(TCP_INFO)` writes
/// up to whatever buffer length we pass — a short buffer just gets
/// truncated to fit, with `optlen` returning the actual write count.
///
/// Layout matches `linux/tcp.h::tcp_info` (linux 5.10+) for the prefix
/// up through `tcpi_segs_out` (T56 follow-up #1: needed for the
/// segments-per-second metric). Since we read only the listed fields and
/// rely on the offsets, the prefix layout is what matters — any future
/// kernel additions appending past `tcpi_segs_in` are absorbed into the
/// remaining bytes of our larger buffer.
#[repr(C)]
#[derive(Default, Copy, Clone, Debug)]
struct LinuxTcpInfoMinimal {
    tcpi_state: u8,
    tcpi_ca_state: u8,
    tcpi_retransmits: u8,
    tcpi_probes: u8,
    tcpi_backoff: u8,
    tcpi_options: u8,
    /// Two 4-bit fields packed into a byte (snd_wscale lo, rcv_wscale hi).
    tcpi_wscales: u8,
    /// Three 1-bit flags packed into a byte (delivery_rate_app_limited,
    /// fastopen_client_fail, _).
    tcpi_flags: u8,

    tcpi_rto: u32,
    tcpi_ato: u32,
    tcpi_snd_mss: u32,
    tcpi_rcv_mss: u32,

    tcpi_unacked: u32,
    tcpi_sacked: u32,
    tcpi_lost: u32,
    tcpi_retrans: u32,
    tcpi_fackets: u32,

    tcpi_last_data_sent: u32,
    tcpi_last_ack_sent: u32,
    tcpi_last_data_recv: u32,
    tcpi_last_ack_recv: u32,

    tcpi_pmtu: u32,
    tcpi_rcv_ssthresh: u32,
    tcpi_rtt: u32,
    tcpi_rttvar: u32,
    tcpi_snd_ssthresh: u32,
    tcpi_snd_cwnd: u32,
    tcpi_advmss: u32,
    tcpi_reordering: u32,
    tcpi_rcv_rtt: u32,
    tcpi_rcv_space: u32,

    tcpi_total_retrans: u32,

    // T56 follow-up #1: extend the layout past `tcpi_total_retrans` to
    // reach `tcpi_segs_out`. The intervening fields are kernel-aligned
    // u64s (linux/tcp.h "Metrics" continued); we DON'T read them, but
    // their offsets must match so `tcpi_segs_out` lands in the right
    // place. Rust's #[repr(C)] guarantees natural u64 alignment, which
    // matches the kernel struct's layout on x86_64 + aarch64.
    /// `tcpi_pacing_rate` (u64, RFC4898 reserved).
    tcpi_pacing_rate: u64,
    /// `tcpi_max_pacing_rate` (u64, app-set cap).
    tcpi_max_pacing_rate: u64,
    /// `tcpi_bytes_acked` (u64, RFC4898 tcpEStatsAppHCThruOctetsAcked).
    tcpi_bytes_acked: u64,
    /// `tcpi_bytes_received` (u64, RFC4898 tcpEStatsAppHCThruOctetsReceived).
    tcpi_bytes_received: u64,
    /// `tcpi_segs_out` (u32, RFC4898 tcpEStatsPerfSegsOut) — closes T56
    /// follow-up #1 by giving the linux arm a real pps numerator instead
    /// of the previous hardcoded 0.
    tcpi_segs_out: u32,
    /// `tcpi_segs_in` (u32, RFC4898 tcpEStatsPerfSegsIn) — included so
    /// the struct's trailing alignment matches the kernel's; not read.
    tcpi_segs_in: u32,
}

/// Connect deadline for the initial kernel-TCP handshake. Same shape
/// as bench-vs-linux's `linux_kernel.rs::CONNECT_TIMEOUT`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-bucket configuration.
pub struct LinuxMaxtpCfg {
    pub bucket: Bucket,
    /// Warmup window (mirrors dpdk_maxtp; spec §11.2: 10 s default).
    pub warmup: Duration,
    /// Measurement window (spec §11.2: 60 s default).
    pub duration: Duration,
    /// Peer IPv4 (host byte order, parity with dpdk path).
    pub peer_ip_host_order: u32,
    /// Peer TCP port. Bench-nightly script puts `linux-tcp-sink` on
    /// port 10002; CLI lets the operator override.
    pub peer_port: u16,
    /// Payload template. Caller allocates once at bucket entry so the
    /// inner loop doesn't allocate.
    pub payload: Vec<u8>,
}

/// One bucket's raw measurement product.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketRun {
    /// Sustained-rate sample over the measurement window (bps + pps).
    /// `pps` is the sum of `tcpi_segs_out` deltas across every conn in
    /// the bucket, divided by the window duration in seconds. T56
    /// follow-up #1: previously pps was hardcoded to 0.0, which made the
    /// linux arm's `tx_pps` row in the maxtp CSV easy to misread as a
    /// broken bench. The dpdk arm uses `eth.tx_pkts`; the linux arm uses
    /// the kernel's per-conn segment counter — semantically the same
    /// segment-rate metric, just sourced from the kernel.
    pub sample: MaxtpSample,
    /// Bytes that the kernel accepted across the measurement window
    /// (sum of successful write return values). The "goodput" of this
    /// run before T-normalisation; surfaced separately for assertion
    /// in tests.
    pub bytes_sent_in_window: u64,
    /// Total `tcpi_segs_out` delta across the measurement window summed
    /// across every conn. Surfaced separately from `sample.pps` for unit
    /// tests that assert the metric computation path. Zero only when
    /// every conn's `getsockopt(TCP_INFO)` snapshot failed (closed
    /// socket etc.) — in which case the runner still emits the bps row
    /// but pps falls back to 0.0.
    pub segs_out_in_window: u64,
}

/// Open `C` persistent connections to the peer. Each carries
/// `TCP_NODELAY = on` to mirror the dpdk_net path's per-segment send
/// behaviour (no Nagle coalescing).
pub fn open_persistent_connections(
    peer_ip_host_order: u32,
    peer_port: u16,
    conn_count: u64,
) -> anyhow::Result<Vec<TcpStream>> {
    if conn_count == 0 {
        anyhow::bail!("linux_maxtp: conn_count must be > 0");
    }
    let octets = peer_ip_host_order.to_be_bytes();
    let addr = SocketAddrV4::new(Ipv4Addr::from(octets), peer_port);
    let mut out = Vec::with_capacity(conn_count as usize);
    for i in 0..conn_count {
        let stream = TcpStream::connect_timeout(&addr.into(), CONNECT_TIMEOUT)
            .with_context(|| format!("linux_maxtp: open connection {i} to {addr}"))?;
        // TCP_NODELAY: parity with dpdk_net's per-segment send. The
        // peer (linux-tcp-sink) also sets NODELAY, so neither side
        // coalesces.
        stream
            .set_nodelay(true)
            .with_context(|| format!("linux_maxtp: set_nodelay on conn {i}"))?;
        // Non-blocking writes so a transient peer-window-full on one
        // conn doesn't stall the round-robin pump. We treat
        // `WouldBlock` exactly the way `dpdk_maxtp` treats `Ok(0)`:
        // skip to the next conn in the round.
        stream
            .set_nonblocking(true)
            .with_context(|| format!("linux_maxtp: set_nonblocking on conn {i}"))?;
        out.push(stream);
    }
    Ok(out)
}

/// Drive one bucket on the Linux side.
///
/// Phase 5 Task 5.5 contract: bench-tx-maxtp's linux_kernel arm
/// requires the peer to be `linux-tcp-sink` (which DISCARDS bytes).
/// The historical `echo-server` peer at port 10001 echoes back what
/// it receives — under kernel TCP that fills the recv buffer +
/// back-pressures the sender to ~0 Gbps for any non-trivial W
/// (T50 report observation).
///
/// `linux-tcp-sink` is started on port 10002 in
/// `scripts/bench-nightly.sh` step [6/12]. This function bails if
/// the operator passed any other port; matches the spec §11.2
/// kernel-TCP measurement assumption that bytes are dropped on the
/// peer side. Closes C-F2.
pub fn assert_peer_is_sink(peer_port: u16) -> anyhow::Result<()> {
    if peer_port != 10002 {
        anyhow::bail!(
            "bench-tx-maxtp linux_kernel arm requires peer_port=10002 (linux-tcp-sink); \
             got {peer_port}. The historical echo-server peer at port 10001 echoes \
             bytes back to the sender, which back-pressures the kernel TCP recv \
             buffer to ~0 Gbps for any non-trivial write size — see T50 report. \
             Use --peer-port 10002 (linux-tcp-sink) instead."
        );
    }
    Ok(())
}

/// Sequence (parity with `dpdk_maxtp::run_bucket`):
/// 1. Pump writes for `warmup`, no sampling.
/// 2. Snapshot per-conn `tcpi_segs_out` (T56 follow-up #1 — the start
///    of the pps numerator), then capture `t_measure_start`.
/// 3. Pump writes for `duration`, accumulating bytes-sent.
/// 4. Capture `t_measure_end`, then snapshot per-conn `tcpi_segs_out`
///    a second time; sum the deltas to get total segments across all
///    conns.
/// 5. Return `MaxtpSample::from_window(bytes_sent, total_segs, elapsed_ns)`.
pub fn run_bucket(
    cfg: &LinuxMaxtpCfg,
    conns: &mut [TcpStream],
    mut send_ack_writer: Option<&mut RawSamplesWriter>,
    bucket_id: &str,
) -> anyhow::Result<BucketRun> {
    if conns.len() as u64 != cfg.bucket.conn_count {
        anyhow::bail!(
            "linux_maxtp: conns.len() = {} does not match bucket.conn_count = {}",
            conns.len(),
            cfg.bucket.conn_count
        );
    }
    if cfg.payload.len() as u64 != cfg.bucket.write_bytes {
        anyhow::bail!(
            "linux_maxtp: payload.len() = {} does not match bucket.write_bytes = {}",
            cfg.payload.len(),
            cfg.bucket.write_bytes
        );
    }
    if cfg.duration.as_nanos() == 0 {
        anyhow::bail!("linux_maxtp: measurement duration must be > 0");
    }

    // Warmup — pump until the warmup deadline, ignore the byte total.
    // No TCP_INFO sink during warmup — those rows belong purely to the
    // measurement window.
    let warmup_deadline = Instant::now() + cfg.warmup;
    let _ = pump_round_robin(conns, &cfg.payload, warmup_deadline, None)
        .context("linux_maxtp warmup phase")?;

    // Measurement window — capture (t_start, t_end) tightly around the
    // pump call so the elapsed-ns denominator matches the byte
    // numerator's window exactly. T56 follow-up #1: the per-conn
    // `tcpi_segs_out` snapshots bracket the pump (start snapshot ⇒
    // pump ⇒ end snapshot), and `t_measure_start` / `t_measure_end`
    // are read at the corresponding boundaries so elapsed-ns and the
    // segment delta describe the same wallclock window. Syscall budget
    // for snapshotting C conns is on the order of microseconds — well
    // inside the noise floor for a 60 s window.
    let segs_out_start: Vec<Option<u32>> = conns
        .iter()
        .map(|s| getsockopt_tcp_info(s.as_raw_fd()).map(|info| info.tcpi_segs_out))
        .collect();
    let t_measure_start = Instant::now();
    let measure_deadline = t_measure_start + cfg.duration;
    // Phase 6 Task 6.2: build the linux send-ack sink only if the caller
    // wired a writer. The sink anchors `t_ns` to `t_measure_start` so
    // emitted timestamps are window-relative.
    let mut sink = send_ack_writer.as_deref_mut().map(|w| LinuxSendAckSink {
        writer: w,
        bucket_id,
        measure_start: t_measure_start,
        next_sample_at: t_measure_start + TCP_INFO_SAMPLE_INTERVAL,
        sample_idx: 0,
    });
    let bytes_sent_in_window =
        pump_round_robin(conns, &cfg.payload, measure_deadline, sink.as_mut())
            .context("linux_maxtp measurement phase")?;
    let t_measure_end = Instant::now();
    // T56 follow-up #1: end-of-window `tcpi_segs_out` snapshot,
    // symmetric with the pre-pump snapshot. Counters that wrapped
    // (u32 monotonic) are still safe — `u32::wrapping_sub` handles
    // the wrap, and a 60 s window cannot emit more than ~2^32
    // segments at ENA line rate.
    let segs_out_end: Vec<Option<u32>> = conns
        .iter()
        .map(|s| getsockopt_tcp_info(s.as_raw_fd()).map(|info| info.tcpi_segs_out))
        .collect();
    let segs_out_in_window = sum_segs_out_deltas(&segs_out_start, &segs_out_end);

    let elapsed_ns = t_measure_end
        .saturating_duration_since(t_measure_start)
        .as_nanos() as u64;
    // 60 s window in ns < 2^36, comfortably inside u64.

    let sample = MaxtpSample::from_window(bytes_sent_in_window, segs_out_in_window, elapsed_ns);

    Ok(BucketRun {
        sample,
        bytes_sent_in_window,
        segs_out_in_window,
    })
}

/// T56 follow-up #1: sum `tcpi_segs_out` deltas across every conn in the
/// bucket. Conns where either snapshot failed (`None`) contribute 0 — we
/// don't have a defensible delta for them, so we'd rather under-count
/// than guess. `u32::wrapping_sub` handles the kernel's u32-wrapping
/// counter; the per-bucket window is bounded to 60 s and the kernel
/// would have to emit 2^32 ≈ 4 G segments in that time, i.e. ~70 G
/// packets per second — far above any plausible line rate, so a single
/// wrap is the worst the math has to absorb.
fn sum_segs_out_deltas(start: &[Option<u32>], end: &[Option<u32>]) -> u64 {
    assert_eq!(
        start.len(),
        end.len(),
        "linux_maxtp: sum_segs_out_deltas length mismatch — start={}, end={}",
        start.len(),
        end.len()
    );
    let mut total: u64 = 0;
    for (s, e) in start.iter().zip(end.iter()) {
        match (s, e) {
            (Some(s_val), Some(e_val)) => {
                // u32 wrapping diff — kernel counter is u32-monotonic.
                let delta = e_val.wrapping_sub(*s_val);
                total = total.saturating_add(delta as u64);
            }
            _ => {
                // One side missing — skip this conn's contribution.
            }
        }
    }
    total
}

/// Phase 6 Task 6.2 sink: per-conn TCP_INFO snapshots emitted once per
/// `TCP_INFO_SAMPLE_INTERVAL` during the measurement window.
pub struct LinuxSendAckSink<'a> {
    pub writer: &'a mut RawSamplesWriter,
    pub bucket_id: &'a str,
    /// Wall-clock anchor for the `t_ns` column — `t_measure_start`.
    pub measure_start: Instant,
    /// Next sample wall-clock (`t_measure_start + k * SAMPLE_INTERVAL`).
    pub next_sample_at: Instant,
    /// 1-based sample index counter; bumped each time we emit a row.
    pub sample_idx: u32,
}

/// Take one `getsockopt(IPPROTO_TCP, TCP_INFO)` snapshot from `fd`.
/// Returns `None` if the syscall fails (closed socket, bad fd, etc.) —
/// caller treats that as "no row this interval".
fn getsockopt_tcp_info(fd: std::os::fd::RawFd) -> Option<LinuxTcpInfoMinimal> {
    let mut info = LinuxTcpInfoMinimal::default();
    let mut len = std::mem::size_of::<LinuxTcpInfoMinimal>() as libc::socklen_t;
    // SAFETY: `info` is a stack-resident #[repr(C)] struct of `len` bytes;
    // libc::getsockopt writes up to `len` bytes into it and updates `len`
    // with the actual write count. Truncated writes (kernel struct larger
    // than ours) are fine — we only read the prefix fields we care about.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            (&mut info as *mut LinuxTcpInfoMinimal).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(info)
}

/// Emit one row per conn into the send-ack CSV with the three TCP_INFO
/// fields we care about. Uses the conn index (`conns` iteration order)
/// as the `conn_id` so per-conn time series stay grouped in the CSV.
fn emit_tcp_info_rows(
    conns: &[TcpStream],
    sink: &mut LinuxSendAckSink<'_>,
    now: Instant,
) -> anyhow::Result<()> {
    sink.sample_idx = sink.sample_idx.saturating_add(1);
    let t_ns = now
        .saturating_duration_since(sink.measure_start)
        .as_nanos() as u64;
    for (conn_idx, stream) in conns.iter().enumerate() {
        let info = match getsockopt_tcp_info(stream.as_raw_fd()) {
            Some(info) => info,
            // Skip rows where the syscall failed; the conn may have been
            // closed mid-window. Other conns still produce rows.
            None => continue,
        };
        write_linux_tcp_info_row(
            sink.writer,
            sink.bucket_id,
            conn_idx as u32,
            sink.sample_idx,
            t_ns,
            &info,
        )?;
    }
    Ok(())
}

/// Phase 6 follow-up: testable inner helper that writes one
/// `linux_tcp_info` scope row into the unified 11-column send-ack CSV.
/// Extracted so unit tests can drive a synthetic `LinuxTcpInfoMinimal`
/// without needing a live socket / `getsockopt` syscall.
fn write_linux_tcp_info_row(
    w: &mut RawSamplesWriter,
    bucket_id: &str,
    conn_id: u32,
    sample_idx: u32,
    t_ns: u64,
    info: &LinuxTcpInfoMinimal,
) -> anyhow::Result<()> {
    w.row(&[
        bucket_id,
        &conn_id.to_string(),
        "linux_tcp_info",
        &sample_idx.to_string(),
        &t_ns.to_string(),
        "",
        "",
        "",
        &info.tcpi_rtt.to_string(),
        &info.tcpi_total_retrans.to_string(),
        &info.tcpi_unacked.to_string(),
    ])?;
    Ok(())
}

/// Pump writes round-robin across `conns` until `deadline` fires.
/// Returns the total bytes the kernel accepted on successful `write`
/// calls. Errors only on a non-`WouldBlock` write failure — TCP
/// reset, broken pipe, etc.
///
/// When `tcp_info_sink` is `Some`, every `TCP_INFO_SAMPLE_INTERVAL` the
/// sink emits one row per conn carrying the kernel-side aggregate
/// telemetry (`tcpi_rtt_us`, `tcpi_total_retrans`, `tcpi_unacked`).
/// See module header for the asymmetry vs the dpdk arm.
fn pump_round_robin(
    conns: &mut [TcpStream],
    payload: &[u8],
    deadline: Instant,
    mut tcp_info_sink: Option<&mut LinuxSendAckSink<'_>>,
) -> anyhow::Result<u64> {
    if conns.is_empty() {
        anyhow::bail!("linux_maxtp: pump_round_robin: conns is empty");
    }
    let mut total: u64 = 0;
    // Per-conn discard buffer for draining echo bytes. The peer
    // (echo-server / linux-tcp-sink — currently both echo) writes
    // bytes back; without draining, the kernel TCP recv buffer fills
    // and the peer's `read()` blocks because its send buffer fills
    // too, which transitively backpressures our `write()` to ~0
    // throughput. Draining here on every round keeps the recv
    // buffer empty so the peer can keep accepting our writes.
    let mut discard = vec![0u8; 65536];
    // Mirror dpdk_maxtp's M1 micro-optimisation: only do the
    // between-conn deadline check on the C=1 path (otherwise the
    // outer-loop check fires often enough).
    let check_between_conns = conns.len() == 1;
    loop {
        let now_outer = Instant::now();
        if now_outer >= deadline {
            return Ok(total);
        }
        for stream in conns.iter_mut() {
            // Drain inbound (non-blocking) before writing — each
            // pass drains whatever the peer has echoed back since
            // the previous round. WouldBlock = nothing pending,
            // continue to write. EOF / hard error from read =
            // log + treat as no-op (the write below will catch
            // a genuine connection problem with a clearer error).
            let mut drained = 0;
            while drained < discard.len() * 4 {
                match stream.read(&mut discard) {
                    Ok(0) => break, // EOF — peer closed
                    Ok(n) => {
                        drained += n;
                        // Partial read is normal on TCP — keep draining
                        // until WouldBlock or the per-round cap fires.
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break, // surface via subsequent write
                }
            }
            // Write payload.
            match stream.write(payload) {
                Ok(n) => {
                    total = total.saturating_add(n as u64);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // Peer-window full / send buffer full for this
                    // conn — skip to next conn (parity with
                    // dpdk_maxtp's Ok(0) handling).
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {
                    // EINTR — retry on the next round, no byte
                    // counted yet.
                }
                Err(e) => {
                    anyhow::bail!("linux_maxtp: write failed: {e}");
                }
            }
            if check_between_conns && Instant::now() >= deadline {
                return Ok(total);
            }
        }
        // Phase 6 Task 6.2: emit per-conn TCP_INFO snapshot rows once
        // per SAMPLE_INTERVAL. Skipped on warmup pumps (sink is None).
        if let Some(sink) = tcp_info_sink.as_deref_mut() {
            let now = Instant::now();
            if now >= sink.next_sample_at {
                emit_tcp_info_rows(conns, sink, now)?;
                sink.next_sample_at = now + TCP_INFO_SAMPLE_INTERVAL;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, TcpListener};

    /// Phase 5 Task 5.5 contract: bench-tx-maxtp's linux_kernel arm
    /// requires the peer to be `linux-tcp-sink` (port 10002, DISCARDS
    /// bytes). Pointing at any other port fails the assertion.
    #[test]
    fn assert_peer_is_sink_accepts_10002() {
        assert_peer_is_sink(10_002).expect("port 10002 is the sink contract");
    }

    #[test]
    fn assert_peer_is_sink_rejects_other_ports() {
        // 10001 is the dpdk echo-server port; pointing there back-
        // pressures kernel TCP to ~0 Gbps (T50 report).
        let err = assert_peer_is_sink(10_001).unwrap_err();
        assert!(
            err.to_string().contains("requires peer_port=10002"),
            "expected sink-port error, got: {err}"
        );
        // Arbitrary other port also fails.
        let err = assert_peer_is_sink(54321).unwrap_err();
        assert!(err.to_string().contains("got 54321"));
    }

    /// `open_persistent_connections` rejects `conn_count == 0` without
    /// hitting the network.
    #[test]
    fn open_rejects_zero_conn_count() {
        // 127.0.0.1:1 — never reached because conn_count == 0 short-
        // circuits.
        let err = open_persistent_connections(0x7f00_0001, 1, 0).unwrap_err();
        assert!(err.to_string().contains("conn_count must be > 0"));
    }

    /// `run_bucket` rejects mismatched `conns.len()` / `bucket.conn_count`.
    #[test]
    fn run_bucket_rejects_conn_count_mismatch() {
        // We build a real (unused) TcpStream so `conns` isn't empty.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stream = TcpStream::connect((IpAddr::V4(Ipv4Addr::LOCALHOST), port)).unwrap();
        let mut conns = vec![stream];

        let cfg = LinuxMaxtpCfg {
            bucket: Bucket::new(64, 4), // expects C=4
            warmup: Duration::from_millis(10),
            duration: Duration::from_millis(10),
            peer_ip_host_order: 0x7f00_0001,
            peer_port: port,
            payload: vec![0u8; 64],
        };
        let err = run_bucket(&cfg, &mut conns, None, "test").unwrap_err();
        assert!(
            err.to_string().contains("does not match bucket.conn_count"),
            "expected conn_count mismatch error, got: {err}"
        );
    }

    /// `run_bucket` rejects a payload whose length doesn't match
    /// `bucket.write_bytes`.
    #[test]
    fn run_bucket_rejects_payload_length_mismatch() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stream = TcpStream::connect((IpAddr::V4(Ipv4Addr::LOCALHOST), port)).unwrap();
        let mut conns = vec![stream];

        let cfg = LinuxMaxtpCfg {
            bucket: Bucket::new(64, 1),
            warmup: Duration::from_millis(10),
            duration: Duration::from_millis(10),
            peer_ip_host_order: 0x7f00_0001,
            peer_port: port,
            payload: vec![0u8; 32], // wrong size
        };
        let err = run_bucket(&cfg, &mut conns, None, "test").unwrap_err();
        assert!(
            err.to_string().contains("does not match bucket.write_bytes"),
            "expected write_bytes mismatch error, got: {err}"
        );
    }

    /// `run_bucket` rejects `duration == 0` (would divide by zero in
    /// `MaxtpSample::from_window`).
    #[test]
    fn run_bucket_rejects_zero_duration() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stream = TcpStream::connect((IpAddr::V4(Ipv4Addr::LOCALHOST), port)).unwrap();
        let mut conns = vec![stream];

        let cfg = LinuxMaxtpCfg {
            bucket: Bucket::new(64, 1),
            warmup: Duration::from_millis(10),
            duration: Duration::ZERO, // bad
            peer_ip_host_order: 0x7f00_0001,
            peer_port: port,
            payload: vec![0u8; 64],
        };
        let err = run_bucket(&cfg, &mut conns, None, "test").unwrap_err();
        assert!(
            err.to_string().contains("measurement duration must be > 0"),
            "expected zero-duration error, got: {err}"
        );
    }

    /// Phase 6 follow-up: the linux arm's `write_linux_tcp_info_row` helper
    /// must produce the unified 11-column row shape with the three
    /// TCP_INFO fields populated and `begin_seq`/`end_seq`/`latency_ns`
    /// blank. Drives a synthetic `LinuxTcpInfoMinimal` so the test is
    /// self-contained — no live socket, no `getsockopt` syscall.
    #[test]
    fn linux_tcp_info_emits_correct_row_shape() {
        let header = [
            "bucket_id",
            "conn_id",
            "scope",
            "sample_idx",
            "t_ns",
            "begin_seq",
            "end_seq",
            "latency_ns",
            "tcpi_rtt_us",
            "tcpi_total_retrans",
            "tcpi_unacked",
        ];
        let info = LinuxTcpInfoMinimal {
            tcpi_rtt: 1234,
            tcpi_total_retrans: 7,
            tcpi_unacked: 42,
            ..LinuxTcpInfoMinimal::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("send-ack.csv");
        {
            let mut w = RawSamplesWriter::create(&path, &header).expect("create");
            write_linux_tcp_info_row(&mut w, "bucket1", 5, 3, 1_500_000_000, &info)
                .expect("write row");
            w.flush().expect("flush");
        }
        let csv = std::fs::read_to_string(&path).expect("read");
        let row = csv.lines().nth(1).expect("data row");
        let cols: Vec<&str> = row.split(',').collect();
        // Verify the unified 11-column layout: bucket_id, conn_id, scope,
        // sample_idx, t_ns, begin_seq, end_seq, latency_ns, tcpi_rtt_us,
        // tcpi_total_retrans, tcpi_unacked.
        assert_eq!(cols.len(), 11, "row has 11 cols, got {}: {row}", cols.len());
        assert_eq!(cols[0], "bucket1");
        assert_eq!(cols[1], "5");
        assert_eq!(cols[2], "linux_tcp_info");
        assert_eq!(cols[3], "3");
        assert_eq!(cols[4], "1500000000");
        assert_eq!(cols[5], ""); // begin_seq blank
        assert_eq!(cols[6], ""); // end_seq blank
        assert_eq!(cols[7], ""); // latency_ns blank
        assert_eq!(cols[8], "1234");
        assert_eq!(cols[9], "7");
        assert_eq!(cols[10], "42");
    }

    /// End-to-end sanity check on a loopback peer: open two connections
    /// to a TcpListener that drains in a background thread, pump for
    /// ~50 ms, confirm the run produced a non-zero byte total and
    /// non-zero throughput, and cleaned up cleanly.
    ///
    /// This is a unit test (not behind `#[ignore]`) because it only
    /// uses loopback + a self-contained drain thread — no external
    /// peer or DPDK dependency.
    #[test]
    fn run_bucket_sanity_against_loopback_drain() {
        use std::io::Read;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(false).unwrap();

        // Background thread: accept up to 2 connections and drain them
        // until they close. Stops when `done` is set or an accept
        // errors.
        let done = Arc::new(AtomicBool::new(false));
        let done_t = done.clone();
        let bg = thread::spawn(move || {
            // Accept up to 2 conns (matches C=2 below).
            let mut sinks: Vec<TcpStream> = Vec::new();
            listener.set_nonblocking(true).unwrap();
            let accept_deadline = Instant::now() + Duration::from_secs(2);
            while sinks.len() < 2 && Instant::now() < accept_deadline {
                match listener.accept() {
                    Ok((s, _)) => {
                        s.set_nonblocking(true).ok();
                        sinks.push(s);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
            // Drain until `done` is set.
            let mut buf = vec![0u8; 8192];
            while !done_t.load(Ordering::Relaxed) {
                let mut any = false;
                for s in sinks.iter_mut() {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => {}
                        Ok(_) => any = true,
                    }
                }
                if !any {
                    thread::sleep(Duration::from_micros(100));
                }
            }
        });

        // Open 2 conns to the loopback peer.
        let mut conns = open_persistent_connections(0x7f00_0001, port, 2).unwrap();
        assert_eq!(conns.len(), 2);

        // Run a tiny bucket: W=64, C=2, warmup 10ms, measurement 50ms.
        let cfg = LinuxMaxtpCfg {
            bucket: Bucket::new(64, 2),
            warmup: Duration::from_millis(10),
            duration: Duration::from_millis(50),
            peer_ip_host_order: 0x7f00_0001,
            peer_port: port,
            payload: vec![0u8; 64],
        };
        let run = run_bucket(&cfg, &mut conns, None, "test").unwrap();
        assert!(
            run.bytes_sent_in_window > 0,
            "expected non-zero bytes sent in window"
        );
        assert!(
            run.sample.goodput_bps > 0.0,
            "expected non-zero goodput, got {}",
            run.sample.goodput_bps
        );
        // T56 follow-up #1: linux arm now populates pps from
        // `tcpi_segs_out` deltas. On loopback with a 50 ms window the
        // kernel emits >> 1 segment, so the metric must be non-zero.
        // (The bucket pumps W=64 bytes at line rate; even a 1 ms window
        // would land hundreds of segments.)
        assert!(
            run.segs_out_in_window > 0,
            "expected non-zero tcpi_segs_out delta, got {}",
            run.segs_out_in_window
        );
        assert!(
            run.sample.pps > 0.0,
            "expected non-zero pps from TCP_INFO snapshots, got {}",
            run.sample.pps
        );

        // Tear down — drop conns + signal bg drain thread to exit.
        drop(conns);
        done.store(true, Ordering::Relaxed);
        // Best-effort join (give it 1 s).
        let _ = bg.join();
    }

    /// T56 follow-up #1: pure synthetic unit test for the segment-rate
    /// metric path. Drives `sum_segs_out_deltas` with hand-constructed
    /// per-conn snapshots so the test is deterministic and doesn't need
    /// a live socket / `getsockopt` syscall — closes the testing gap
    /// the task spec flagged ("Add a unit test that exercises the metric
    /// computation path with synthetic data, asserting non-zero output").
    #[test]
    fn sum_segs_out_deltas_sums_across_conns() {
        // 3 conns; each emitted 100 segments during the window.
        let start = vec![Some(1_000u32), Some(2_000u32), Some(3_000u32)];
        let end = vec![Some(1_100u32), Some(2_100u32), Some(3_100u32)];
        let total = sum_segs_out_deltas(&start, &end);
        assert_eq!(total, 300, "expected 100 segs/conn × 3 conns");
    }

    /// `sum_segs_out_deltas` handles the kernel's u32-wrapping counter.
    /// A start of `u32::MAX - 10` and end of `5` is a +16-segment delta.
    #[test]
    fn sum_segs_out_deltas_wraps_around_u32() {
        let start = vec![Some(u32::MAX - 10)];
        let end = vec![Some(5u32)];
        let total = sum_segs_out_deltas(&start, &end);
        // Wrapping diff: 5 - (u32::MAX - 10) wraps to 16 (5 + 11).
        assert_eq!(total, 16, "wrap-around delta = 11 + 5 = 16, got {total}");
    }

    /// A conn whose `getsockopt(TCP_INFO)` failed at either end of the
    /// window contributes 0 to the bucket total — closed sockets are
    /// silently ignored rather than poisoning the whole bucket.
    #[test]
    fn sum_segs_out_deltas_skips_failed_snapshots() {
        let start = vec![Some(0u32), None, Some(500u32)];
        let end = vec![Some(100u32), Some(200u32), None];
        // Only the first conn has both snapshots → 100 - 0 = 100. The
        // other two contribute 0.
        let total = sum_segs_out_deltas(&start, &end);
        assert_eq!(total, 100);
    }

    /// Both inputs empty (zero conns) → zero delta. Edge case kept
    /// explicit so a future refactor that changes the iteration shape
    /// can't silently regress.
    #[test]
    fn sum_segs_out_deltas_empty_inputs_is_zero() {
        let total = sum_segs_out_deltas(&[], &[]);
        assert_eq!(total, 0);
    }

    /// Mismatched start / end lengths is a caller bug — assert-panic
    /// rather than silently produce a wrong number.
    #[test]
    #[should_panic(expected = "length mismatch")]
    fn sum_segs_out_deltas_panics_on_length_mismatch() {
        let start = vec![Some(0u32), Some(100u32)];
        let end = vec![Some(50u32)];
        let _ = sum_segs_out_deltas(&start, &end);
    }

    /// T56 follow-up #1: the `tcpi_segs_out` field of `LinuxTcpInfoMinimal`
    /// must land at the right offset for `getsockopt(TCP_INFO)` to populate
    /// it correctly. The kernel struct's layout on x86_64 + aarch64
    /// (linux 5.10+; verified against `/usr/include/linux/tcp.h`):
    ///
    /// | field                 | offset |
    /// |-----------------------|-------:|
    /// | `tcpi_total_retrans`  |    100 |
    /// | `tcpi_pacing_rate`    |    104 |
    /// | `tcpi_max_pacing_rate`|    112 |
    /// | `tcpi_bytes_acked`    |    120 |
    /// | `tcpi_bytes_received` |    128 |
    /// | `tcpi_segs_out`       |    136 |
    /// | `tcpi_segs_in`        |    140 |
    ///
    /// `tcpi_total_retrans` (u32, 4 bytes) ends at offset 104, and
    /// `tcpi_pacing_rate` (u64) is already 8-aligned at offset 104 — so
    /// no padding is inserted by the C compiler. Rust's `#[repr(C)]`
    /// produces the same layout, which we assert here to guard against
    /// future kernels reordering fields (rare) or Rust changing its
    /// `#[repr(C)]` layout rules (extremely rare).
    #[test]
    fn tcpi_segs_out_offset_matches_kernel_layout() {
        // Use ptr arithmetic instead of `offset_of!` to keep the test
        // ergonomic on older toolchains where the macro is gated.
        let info = LinuxTcpInfoMinimal::default();
        let base = &info as *const LinuxTcpInfoMinimal as usize;
        let segs_out_addr = &info.tcpi_segs_out as *const u32 as usize;
        let segs_out_offset = segs_out_addr - base;
        let total_retrans_addr = &info.tcpi_total_retrans as *const u32 as usize;
        let total_retrans_offset = total_retrans_addr - base;
        // tcpi_segs_out follows tcpi_total_retrans (offset 100) by:
        //   4 bytes (tcpi_total_retrans u32) +
        //   4 × 8 = 32 bytes (4 × u64 metric fields)
        // = 36-byte gap → segs_out at offset 136.
        assert_eq!(
            total_retrans_offset, 100,
            "tcpi_total_retrans expected at offset 100 (kernel layout), got {total_retrans_offset}"
        );
        assert_eq!(
            segs_out_offset, 136,
            "tcpi_segs_out expected at offset 136 (kernel layout), got {segs_out_offset}"
        );
    }
}
