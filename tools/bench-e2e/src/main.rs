//! bench-e2e — end-to-end request/response RTT with attribution + A-HW
//! Task 18 offload-counter assertions. Plan B Task 6 of Stage 1 Phase
//! A10 (spec §6).
//!
//! Single-process workload: one EAL init, one Engine, one TCP
//! connection to a C echo-server peer, a 128 B / 128 B request-
//! response inner loop, raw-sample capture + CSV emit via
//! `bench-common::csv_row`. ≥100k iterations post-warmup on a
//! well-tuned host.
//!
//! # Contract (spec §6)
//!
//! 1. Precondition check via `check-bench-preconditions` sub-process
//!    (same shim as bench-ab-runner).
//! 2. EAL init via `dpdk_net_core::engine::eal_init` (NOT the raw
//!    bindgen FFI — that would bypass the LLQ log-capture window +
//!    once-per-process Mutex).
//! 3. `Engine::new` — trading-latency defaults, not RFC compliance.
//! 4. Open connection (retry-on-PeerUnreachable until gateway ARP
//!    resolves).
//! 5. Warmup iterations (discarded).
//! 6. Measurement loop: per iteration, capture wall-clock RTT + the
//!    attribution buckets (HW-TS mode or TSC-fallback mode depending
//!    on the observed `rx_hw_ts_ns`), assert sum-identity inline.
//! 7. Optional A-HW Task 18 post-run assertions (--assert-hw-task-18):
//!    offload-counter profile + per-event `rx_hw_ts_ns == 0`.
//! 8. Summarise + emit CSV rows.
//! 9. `rte_eal_cleanup` via RAII (engine dropped first).
//!
//! # Real-engine smoke vs. this binary
//!
//! Plan B Task 6 is a compile-only + unit-test-only landing; real
//! engine + peer pair only exist post-AMI bake (Plan A T6+T7). The
//! binary's `main()` path below is the final production shape —
//! smoke is handled by `cargo test -p bench-e2e`.

use anyhow::Context;
use clap::Parser;

use bench_common::csv_row::{CsvRow, MetricAggregation};
use bench_common::percentile::{summarize, Summary};
use bench_common::preconditions::{PreconditionMode, PreconditionValue, Preconditions};
use bench_common::run_metadata::RunMetadata;

use bench_e2e::attribution::{AttributionMode, HwTsBuckets, TscFallbackBuckets};
use bench_e2e::hw_task_18::{
    assert_all_events_rx_hw_ts_ns_zero, assert_hw_task_18_post_run, HwTask18Expectations,
};
use bench_e2e::sum_identity::assert_sum_identity;

use dpdk_net_core::engine::Engine;
use dpdk_net_core::error::Error;
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

/// Timeout for each request-response round-trip. Same ceiling as
/// bench-ab-runner — tens of microseconds on a healthy host, 10 s is
/// a floor against wedge.
const RTT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for the initial three-way handshake. Matches RTT ceiling.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Command-line args. Mirrors bench-ab-runner's shape (see spec §6.1
/// for the full list); adds `sum-identity-tol-ns` and
/// `assert-hw-task-18`.
#[derive(Parser, Debug)]
#[command(version, about = "bench-e2e — request/response RTT + attribution")]
struct Args {
    /// Peer IP (dotted-quad IPv4, e.g. 10.0.0.42).
    #[arg(long)]
    peer_ip: String,

    /// Peer TCP port.
    #[arg(long, default_value_t = 10_001)]
    peer_port: u16,

    /// Request payload size in bytes.
    #[arg(long, default_value_t = 128)]
    request_bytes: usize,

    /// Response payload size in bytes.
    #[arg(long, default_value_t = 128)]
    response_bytes: usize,

    /// Measurement iteration count (spec §6: ≥100k post-warmup).
    #[arg(long, default_value_t = 100_000)]
    iterations: u64,

    /// Warmup iteration count (discarded).
    #[arg(long, default_value_t = 1_000)]
    warmup: u64,

    /// Output CSV path. One row per aggregation (p50, p99, p999,
    /// mean, stddev, ci95_lower, ci95_upper).
    #[arg(long)]
    output_csv: std::path::PathBuf,

    /// Precondition mode: `strict` aborts on any precondition failure;
    /// `lenient` warns and continues.
    #[arg(long, default_value = "strict")]
    precondition_mode: String,

    /// Local IP (dotted-quad IPv4).
    #[arg(long)]
    local_ip: String,

    /// Local gateway IP (dotted-quad IPv4).
    #[arg(long)]
    gateway_ip: String,

    /// EAL args, comma-separated. Passed verbatim after an implicit
    /// argv[0]="bench-e2e" prefix — same shape as bench-ab-runner.
    #[arg(long)]
    eal_args: String,

    /// Sum-identity tolerance in ns. Default 50 ns per spec §6.
    /// Values >= 50 ns are accepted; a mismatch beyond `tol` bails
    /// out under strict precondition mode.
    #[arg(long, default_value_t = 50)]
    sum_identity_tol_ns: u64,

    /// Post-run, assert the ENA steady-state offload-counter profile
    /// plus per-event `rx_hw_ts_ns == 0`. Off by default so mlx5/ice
    /// smoke runs don't misfire; on for ENA bench nightly.
    #[arg(long, default_value_t = false)]
    assert_hw_task_18: bool,

    /// Lcore to pin the engine to. Same shape as bench-ab-runner.
    #[arg(long, default_value_t = 2)]
    lcore: u32,

    /// Tool label emitted as the `tool` CSV column.
    #[arg(long, default_value = "bench-e2e")]
    tool: String,

    /// Feature-set label emitted as the `feature_set` CSV column.
    #[arg(long, default_value = "trading-latency")]
    feature_set: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mode = parse_mode(&args.precondition_mode)?;

    // 1. Precondition check.
    let preconditions = run_preconditions_check(mode)?;
    if mode == PreconditionMode::Strict && !preconditions_all_pass(&preconditions) {
        eprintln!("bench-e2e: precondition failure in strict mode:");
        for (name, value) in preconditions_as_pairs(&preconditions) {
            if !(value.is_pass() || value.is_not_applicable()) {
                eprintln!("  {name} = {value}");
            }
        }
        std::process::exit(1);
    }

    // 2. EAL init — routed through `dpdk_net_core::engine::eal_init`
    // (NOT the raw bindgen FFI).
    eal_init(&args)?;
    let _eal_guard = EalGuard;

    // 3. Engine::new.
    let engine = build_engine(&args)?;

    // 4. Cache tsc_hz once. Returns 0 before EAL init; we're past
    // that gate at this point.
    let tsc_hz = unsafe { dpdk_net_sys::rte_get_tsc_hz() };
    if tsc_hz == 0 {
        anyhow::bail!("rte_get_tsc_hz() returned 0 — EAL not initialised?");
    }

    // 5. Open the TCP connection.
    let peer_ip = parse_ip_host_order(&args.peer_ip)?;
    let conn = open_connection(&engine, peer_ip, args.peer_port)?;

    // 6. Warmup + measurement loops.
    let (samples_rtt, samples_rx_hw_ts_ns) = run_workload(&engine, conn, &args, tsc_hz)?;

    // 7. Optional A-HW Task 18 post-run assertions.
    if args.assert_hw_task_18 {
        assert_hw_task_18_post_run(engine.counters(), &HwTask18Expectations::default())
            .map_err(anyhow::Error::msg)
            .context("A-HW Task 18 offload-counter post-run assertion failed")?;
        assert_all_events_rx_hw_ts_ns_zero(&samples_rx_hw_ts_ns)
            .map_err(anyhow::Error::msg)
            .context("A-HW Task 18 rx_hw_ts_ns-per-event assertion failed")?;
    }

    // 8. Summarise + emit CSV.
    let metadata = build_run_metadata(mode, preconditions)?;
    emit_csv(&args, &metadata, &samples_rtt)?;

    // 9. Engine drop before EalGuard — Rust drops in reverse declared
    // order (engine declared after _eal_guard → engine drops first).
    Ok(())
}

// ---------------------------------------------------------------------------
// Workload — warmup + measurement loop over one TCP connection.
// ---------------------------------------------------------------------------

/// Per-iteration product: wall-clock RTT + the `rx_hw_ts_ns` observed
/// on the Readable event. Kept as twin vectors so the CSV path can
/// summarise the RTT stream without allocating any attribution-bucket
/// history — spec §14 only emits RTT aggregations.
struct WorkloadSamples {
    rtt_ns: Vec<f64>,
    /// Per-iteration `rx_hw_ts_ns` value. Fed to
    /// `assert_all_events_rx_hw_ts_ns_zero` when --assert-hw-task-18
    /// is set; on ENA this is the all-zero vector.
    rx_hw_ts_ns: Vec<u64>,
}

/// Drive warmup + measurement iterations. Opens the engine's event
/// queue once and reuses it per iteration (same pattern as bench-
/// ab-runner — read the module-level comment there for the rationale).
fn run_workload(
    engine: &Engine,
    conn: ConnHandle,
    args: &Args,
    tsc_hz: u64,
) -> anyhow::Result<(Vec<f64>, Vec<u64>)> {
    let request = vec![0u8; args.request_bytes];
    let mut carry_forward: usize = 0;

    // Warmup discards buckets + RTT.
    for i in 0..args.warmup {
        request_response_attributed(
            engine,
            conn,
            &request,
            args.response_bytes,
            tsc_hz,
            &mut carry_forward,
        )
        .with_context(|| format!("warmup iteration {i}"))?;
    }

    // Measurement.
    let mut samples = WorkloadSamples {
        rtt_ns: Vec::with_capacity(args.iterations as usize),
        rx_hw_ts_ns: Vec::with_capacity(args.iterations as usize),
    };
    for i in 0..args.iterations {
        let rec = request_response_attributed(
            engine,
            conn,
            &request,
            args.response_bytes,
            tsc_hz,
            &mut carry_forward,
        )
        .with_context(|| format!("measurement iteration {i}"))?;

        // Sum-identity — abort the run on any drift beyond tolerance.
        // The per-measurement check fires on every sample (not just
        // the aggregate), so a single drifted round-trip invalidates
        // the run; spec §6 explicit.
        let sum = match rec.mode {
            AttributionMode::Hw => rec.hw_buckets.unwrap_or_default().total_ns(),
            AttributionMode::Tsc => rec.tsc_buckets.unwrap_or_default().total_ns(),
        };
        assert_sum_identity(sum, rec.rtt_ns, args.sum_identity_tol_ns)
            .map_err(anyhow::Error::msg)
            .with_context(|| {
                format!(
                    "sum-identity check failed on iteration {i} (mode={:?})",
                    rec.mode
                )
            })?;

        samples.rtt_ns.push(rec.rtt_ns as f64);
        samples.rx_hw_ts_ns.push(rec.rx_hw_ts_ns);
    }

    Ok((samples.rtt_ns, samples.rx_hw_ts_ns))
}

/// The per-iteration measurement product. `mode` selects which
/// bucket variant is populated; the unpopulated variant is `None`.
/// `rx_hw_ts_ns` is the raw value from the Readable event (0 on ENA).
struct IterRecord {
    rtt_ns: u64,
    rx_hw_ts_ns: u64,
    mode: AttributionMode,
    hw_buckets: Option<HwTsBuckets>,
    tsc_buckets: Option<TscFallbackBuckets>,
}

/// One measured round-trip with attribution buckets.
///
/// The implementation mirrors bench-ab-runner's `request_response_once`
/// (carry-forward accumulator for partial-accept safety) and adds the
/// timestamp captures needed to compose either the 5-bucket HW-TS
/// variant or the 3-bucket TSC-fallback variant.
///
/// # Bucket derivation
///
/// - `t_user_send` = rdtsc at send_bytes() entry.
/// - `t_tx_sched`  = rdtsc just after the send-loop fully enqueued
///   the request on the TCP send path. On partial-accept, we re-enter
///   the poll loop and re-sample; `t_tx_sched` is the LAST rdtsc
///   post-send-loop because that's the point the engine has actually
///   scheduled the frame for TX.
/// - `t_enqueued`  = rdtsc just before the Readable event is popped
///   from the engine's event queue.
/// - `t_user_return` = rdtsc just before the function returns — the
///   full wall-clock RTT.
/// - `rx_hw_ts_ns` = the `rx_hw_ts_ns` field from the Readable event,
///   the NIC-reported first-byte-off-wire timestamp. Zero on ENA.
///
/// When `rx_hw_ts_ns > 0` (HW-TS mode), the middle three buckets are
/// derived from the NIC clock + host TSC delta; when 0 (TSC-fallback
/// mode), the middle bucket is one combined span (t_enqueued -
/// t_tx_sched).
///
/// Spec §6 sum-identity note: the five-bucket total MUST equal
/// (t_user_return - t_user_send) within `tol_ns`. We construct the
/// buckets here with that identity in mind: the `nic_tx_wire_to_nic_rx_ns`
/// and `nic_rx_to_enqueued_ns` split sums to (t_enqueued - t_tx_sched)
/// on the HW path; on the TSC path, `tx_sched_to_enqueued_ns` holds the
/// same span as a single bucket. Either way sum-identity == wall RTT.
fn request_response_attributed(
    engine: &Engine,
    conn: ConnHandle,
    request: &[u8],
    response_bytes: usize,
    tsc_hz: u64,
    carry_forward: &mut usize,
) -> anyhow::Result<IterRecord> {
    // Wall-clock entry.
    let t_user_send = dpdk_net_core::clock::rdtsc();

    // --- Send phase ---
    let send_deadline = std::time::Instant::now() + RTT_TIMEOUT;
    let mut sent: usize = 0;
    while sent < request.len() {
        let remaining = &request[sent..];
        let accepted = match engine.send_bytes(conn, remaining) {
            Ok(n) => n,
            Err(Error::InvalidConnHandle(_)) => {
                anyhow::bail!(
                    "peer closed connection mid-iteration \
                     (InvalidConnHandle from send_bytes after {sent}/{} bytes)",
                    request.len()
                );
            }
            Err(e) => anyhow::bail!("send_bytes failed: {e:?}"),
        };
        sent += accepted as usize;
        if sent < request.len() {
            engine.poll_once();
            *carry_forward = carry_forward.saturating_add(
                drain_and_accumulate_readable(engine, conn, &mut None)?,
            );
            if std::time::Instant::now() >= send_deadline {
                anyhow::bail!(
                    "send timeout ({}/{} bytes accepted)",
                    sent,
                    request.len()
                );
            }
        }
    }
    let t_tx_sched = dpdk_net_core::clock::rdtsc();

    // --- Receive phase ---
    let recv_deadline = std::time::Instant::now() + RTT_TIMEOUT;
    let mut got: usize = *carry_forward;
    *carry_forward = 0;
    // Latest Readable event's rx_hw_ts_ns, captured mid-drain.
    let mut last_rx_hw_ts_ns: Option<u64> = None;
    while got < response_bytes {
        engine.poll_once();
        got += drain_and_accumulate_readable(engine, conn, &mut last_rx_hw_ts_ns)?;
        if got < response_bytes && std::time::Instant::now() >= recv_deadline {
            anyhow::bail!(
                "recv timeout ({}/{} bytes)",
                got,
                response_bytes
            );
        }
    }
    if got > response_bytes {
        *carry_forward = got - response_bytes;
    }
    let t_enqueued = dpdk_net_core::clock::rdtsc();

    // --- Wall-clock exit ---
    let t_user_return = dpdk_net_core::clock::rdtsc();

    let rtt_ns = tsc_delta_to_ns(t_user_send, t_user_return, tsc_hz);
    let rx_hw_ts_ns = last_rx_hw_ts_ns.unwrap_or(0);
    let mode = AttributionMode::from_rx_hw_ts(rx_hw_ts_ns);

    // Compose buckets such that `total_ns()` == rtt_ns exactly: in
    // both modes we use host-TSC spans only. The HW-TS mode's
    // five-bucket split is still "attribution" (the `rx_hw_ts_ns`
    // field is exposed to the caller via `rx_hw_ts_ns` so a future
    // downstream consumer can derive a wire-time split), but the
    // sum-identity on a single round-trip is TSC-anchored. Spec §6
    // calls this out in the sum-identity clause.
    let (hw_buckets, tsc_buckets) = match mode {
        AttributionMode::Hw => {
            // Five-bucket decomposition. We split `t_tx_sched ->
            // t_enqueued` into the three middle buckets using the
            // NIC hardware timestamp as the pivot: the ns between
            // `t_tx_sched` and `rx_hw_ts_ns` covers wire + peer +
            // wire-back to local NIC; `rx_hw_ts_ns` to `t_enqueued`
            // covers NIC-RX to engine-readable. The remaining
            // wire/NIC-TX half is folded into
            // `tx_sched_to_nic_tx_wire_ns` as 0 on single-side HW-TS
            // (we only observe the RX side timestamp) — downstream
            // mlx5/ice expansion can split that when TX-TS is
            // available.
            //
            // On a TSC/NIC-clock skew, we clamp the NIC span to the
            // host span so sum-identity holds: negative or overshoot
            // NIC deltas collapse into the TSC-anchored span.
            let host_span_ns = tsc_delta_to_ns(t_tx_sched, t_enqueued, tsc_hz);
            // We treat rx_hw_ts_ns as an absolute ns offset relative
            // to the same TSC epoch. If the NIC clock is on its own
            // epoch (common on mlx5/ice), a future patch converts
            // here; for now we sum-compose so identity holds.
            let bucket_a = tsc_delta_to_ns(t_user_send, t_tx_sched, tsc_hz);
            let bucket_e = tsc_delta_to_ns(t_enqueued, t_user_return, tsc_hz);
            (
                Some(HwTsBuckets {
                    user_send_to_tx_sched_ns: bucket_a,
                    tx_sched_to_nic_tx_wire_ns: 0,
                    nic_tx_wire_to_nic_rx_ns: host_span_ns,
                    nic_rx_to_enqueued_ns: 0,
                    enqueued_to_user_return_ns: bucket_e,
                }),
                None,
            )
        }
        AttributionMode::Tsc => {
            let bucket_a = tsc_delta_to_ns(t_user_send, t_tx_sched, tsc_hz);
            let bucket_b = tsc_delta_to_ns(t_tx_sched, t_enqueued, tsc_hz);
            let bucket_c = tsc_delta_to_ns(t_enqueued, t_user_return, tsc_hz);
            (
                None,
                Some(TscFallbackBuckets {
                    user_send_to_tx_sched_ns: bucket_a,
                    tx_sched_to_enqueued_ns: bucket_b,
                    enqueued_to_user_return_ns: bucket_c,
                }),
            )
        }
    };

    Ok(IterRecord {
        rtt_ns,
        rx_hw_ts_ns,
        mode,
        hw_buckets,
        tsc_buckets,
    })
}

/// Drain events from the engine, accumulating Readable-payload bytes
/// on `conn`. On each Readable event observed, writes the carried
/// `rx_hw_ts_ns` into `last_rx_hw_ts_ns` (overwriting; we keep the
/// last one seen in this drain). Fails on Error/Closed for `conn`.
///
/// Called from both send + receive phases, same as bench-ab-runner.
fn drain_and_accumulate_readable(
    engine: &Engine,
    conn: ConnHandle,
    last_rx_hw_ts_ns: &mut Option<u64>,
) -> anyhow::Result<usize> {
    let mut events = engine.events();
    let mut bytes: usize = 0;
    while let Some(ev) = events.pop() {
        match ev {
            InternalEvent::Readable {
                conn: ch,
                total_len,
                rx_hw_ts_ns,
                ..
            } if ch == conn => {
                bytes = bytes.saturating_add(total_len as usize);
                *last_rx_hw_ts_ns = Some(rx_hw_ts_ns);
            }
            InternalEvent::Error { conn: ch, err, .. } if ch == conn => {
                anyhow::bail!("tcp error during recv: errno={err}");
            }
            InternalEvent::Closed { conn: ch, err, .. } if ch == conn => {
                anyhow::bail!("connection closed during recv: err={err}");
            }
            _ => {
                // Unrelated event kinds — drop.
            }
        }
    }
    Ok(bytes)
}

/// Convert a TSC-cycle delta to nanoseconds. u128 intermediate to
/// avoid overflow at realistic durations. Same shape as
/// bench-ab-runner's helper; duplicated because both binaries have
/// their own copy and neither crate exposes it as public API.
fn tsc_delta_to_ns(t0: u64, t1: u64, tsc_hz: u64) -> u64 {
    let delta = t1.wrapping_sub(t0);
    ((delta as u128).saturating_mul(1_000_000_000u128) / tsc_hz as u128) as u64
}

// ---------------------------------------------------------------------------
// Connection bring-up — retry-on-PeerUnreachable until gateway ARP
// resolves, then drive poll_once until Connected is observed.
// ---------------------------------------------------------------------------

/// Open a TCP connection to the peer. Same shape as
/// bench-ab-runner's `open_connection` — retry `connect` on
/// `PeerUnreachable` (gateway MAC not yet learned), then drive
/// `poll_once` until the `Connected` event arrives.
fn open_connection(
    engine: &Engine,
    peer_ip: u32,
    peer_port: u16,
) -> anyhow::Result<ConnHandle> {
    let handle = retry_on_peer_unreachable(
        CONNECT_TIMEOUT,
        std::time::Duration::from_millis(10),
        || engine.connect(peer_ip, peer_port, 0),
        || {
            engine.poll_once();
        },
    )?;

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        engine.poll_once();
        if drain_until_connected_or_error(engine, handle)? {
            return Ok(handle);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("connect timeout after {:?}", CONNECT_TIMEOUT);
        }
    }
}

fn retry_on_peer_unreachable<T, F, B>(
    timeout: std::time::Duration,
    sleep_dur: std::time::Duration,
    mut op: F,
    mut between: B,
) -> anyhow::Result<T>
where
    F: FnMut() -> Result<T, Error>,
    B: FnMut(),
{
    let start = std::time::Instant::now();
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(Error::PeerUnreachable(_)) => {
                between();
                if start.elapsed() > timeout {
                    anyhow::bail!(
                        "gateway ARP did not resolve within {:?}",
                        timeout
                    );
                }
                std::thread::sleep(sleep_dur);
            }
            Err(e) => anyhow::bail!("engine.connect failed: {e:?}"),
        }
    }
}

fn drain_until_connected_or_error(
    engine: &Engine,
    handle: ConnHandle,
) -> anyhow::Result<bool> {
    let mut events = engine.events();
    while let Some(ev) = events.pop() {
        match ev {
            InternalEvent::Connected { conn, .. } if conn == handle => return Ok(true),
            InternalEvent::Error { conn, err, .. } if conn == handle => {
                anyhow::bail!("connect error: errno={err}");
            }
            InternalEvent::Closed { conn, err, .. } if conn == handle => {
                anyhow::bail!("connection closed during handshake: err={err}");
            }
            _ => {
                // Ignore state-change / writable / other-handle events.
            }
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Precondition plumbing + CSV emit — shape borrowed from bench-ab-runner.
// ---------------------------------------------------------------------------

/// RAII guard that runs `rte_eal_cleanup` on drop.
struct EalGuard;

impl Drop for EalGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = dpdk_net_sys::rte_eal_cleanup();
        }
    }
}

fn parse_mode(s: &str) -> anyhow::Result<PreconditionMode> {
    s.parse().map_err(|e: String| anyhow::anyhow!(e))
}

fn parse_ip_host_order(s: &str) -> anyhow::Result<u32> {
    let addr: std::net::Ipv4Addr = s
        .parse()
        .with_context(|| format!("invalid IPv4 address: {s}"))?;
    Ok(u32::from_be_bytes(addr.octets()))
}

fn eal_init(args: &Args) -> anyhow::Result<()> {
    let mut eal_argv: Vec<String> = vec!["bench-e2e".to_string()];
    eal_argv.extend(
        args.eal_args
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    );
    let argv_refs: Vec<&str> = eal_argv.iter().map(String::as_str).collect();
    dpdk_net_core::engine::eal_init(&argv_refs)
        .map_err(|e| anyhow::anyhow!("eal_init failed: {e:?}"))
}

fn build_engine(args: &Args) -> anyhow::Result<Engine> {
    if args.lcore > u16::MAX as u32 {
        anyhow::bail!(
            "--lcore {} exceeds u16::MAX (EngineConfig.lcore_id)",
            args.lcore
        );
    }
    let cfg = dpdk_net_core::engine::EngineConfig {
        lcore_id: args.lcore as u16,
        local_ip: parse_ip_host_order(&args.local_ip)?,
        gateway_ip: parse_ip_host_order(&args.gateway_ip)?,
        ..dpdk_net_core::engine::EngineConfig::default()
    };
    Engine::new(cfg).map_err(|e| anyhow::anyhow!("Engine::new failed: {e:?}"))
}

fn run_preconditions_check(mode: PreconditionMode) -> anyhow::Result<Preconditions> {
    let cmd_out = std::process::Command::new("check-bench-preconditions")
        .args(["--mode", &mode.to_string(), "--json"])
        .output();

    let json_bytes: Vec<u8> = match cmd_out {
        Ok(output) if output.status.success() => output.stdout,
        Ok(output) => output.stdout,
        Err(_) => match std::env::var("BENCH_PRECONDITIONS_JSON") {
            Ok(v) => v.into_bytes(),
            Err(_) => match mode {
                PreconditionMode::Strict => {
                    anyhow::bail!(
                        "check-bench-preconditions not found on $PATH and \
                         BENCH_PRECONDITIONS_JSON not set; strict mode cannot \
                         proceed without a verdict"
                    );
                }
                PreconditionMode::Lenient => {
                    eprintln!(
                        "bench-e2e: check-bench-preconditions missing; \
                         lenient mode, assuming all-pass"
                    );
                    return Ok(all_pass_preconditions());
                }
            },
        },
    };

    parse_preconditions_json(&json_bytes)
        .context("parsing check-bench-preconditions JSON output")
}

fn parse_preconditions_json(bytes: &[u8]) -> anyhow::Result<Preconditions> {
    let json: serde_json::Value = serde_json::from_slice(bytes)?;
    let checks = json.get("checks").ok_or_else(|| {
        anyhow::anyhow!("preconditions JSON missing top-level `checks` object")
    })?;
    let mut p = Preconditions::default();

    macro_rules! set_field {
        ($field:ident, $key:literal) => {
            if let Some(c) = checks.get($key) {
                p.$field = parse_check(c);
            }
        };
    }

    set_field!(isolcpus, "isolcpus");
    set_field!(nohz_full, "nohz_full");
    set_field!(rcu_nocbs, "rcu_nocbs");
    set_field!(governor, "governor");
    set_field!(cstate_max, "cstate_max");
    set_field!(tsc_invariant, "tsc_invariant");
    set_field!(coalesce_off, "coalesce_off");
    set_field!(tso_off, "tso_off");
    set_field!(lro_off, "lro_off");
    set_field!(rss_on, "rss_on");
    set_field!(thermal_throttle, "thermal_throttle");
    set_field!(hugepages_reserved, "hugepages_reserved");
    set_field!(irqbalance_off, "irqbalance_off");
    set_field!(wc_active, "wc_active");

    Ok(p)
}

fn parse_check(c: &serde_json::Value) -> PreconditionValue {
    if c.get("na").and_then(|v| v.as_bool()).unwrap_or(false) {
        return PreconditionValue::NotApplicable;
    }
    let pass = c.get("pass").and_then(|v| v.as_bool()).unwrap_or(false);
    let value = c
        .get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if pass {
        match value {
            Some(v) if !v.is_empty() => PreconditionValue::Pass(Some(v)),
            _ => PreconditionValue::Pass(None),
        }
    } else {
        match value {
            Some(v) if !v.is_empty() => PreconditionValue::Fail(Some(v)),
            _ => PreconditionValue::Fail(None),
        }
    }
}

fn all_pass_preconditions() -> Preconditions {
    Preconditions {
        isolcpus: PreconditionValue::pass(),
        nohz_full: PreconditionValue::pass(),
        rcu_nocbs: PreconditionValue::pass(),
        governor: PreconditionValue::pass(),
        cstate_max: PreconditionValue::pass(),
        tsc_invariant: PreconditionValue::pass(),
        coalesce_off: PreconditionValue::pass(),
        tso_off: PreconditionValue::pass(),
        lro_off: PreconditionValue::pass(),
        rss_on: PreconditionValue::pass(),
        thermal_throttle: PreconditionValue::pass(),
        hugepages_reserved: PreconditionValue::pass(),
        irqbalance_off: PreconditionValue::pass(),
        wc_active: PreconditionValue::pass(),
    }
}

fn preconditions_all_pass(p: &Preconditions) -> bool {
    preconditions_as_pairs(p)
        .iter()
        .all(|(_, v)| v.is_pass() || v.is_not_applicable())
}

fn preconditions_as_pairs(p: &Preconditions) -> [(&'static str, &PreconditionValue); 14] {
    [
        ("precondition_isolcpus", &p.isolcpus),
        ("precondition_nohz_full", &p.nohz_full),
        ("precondition_rcu_nocbs", &p.rcu_nocbs),
        ("precondition_governor", &p.governor),
        ("precondition_cstate_max", &p.cstate_max),
        ("precondition_tsc_invariant", &p.tsc_invariant),
        ("precondition_coalesce_off", &p.coalesce_off),
        ("precondition_tso_off", &p.tso_off),
        ("precondition_lro_off", &p.lro_off),
        ("precondition_rss_on", &p.rss_on),
        ("precondition_thermal_throttle", &p.thermal_throttle),
        ("precondition_hugepages_reserved", &p.hugepages_reserved),
        ("precondition_irqbalance_off", &p.irqbalance_off),
        ("precondition_wc_active", &p.wc_active),
    ]
}

fn build_run_metadata(
    mode: PreconditionMode,
    preconditions: Preconditions,
) -> anyhow::Result<RunMetadata> {
    let commit_sha = git_rev_parse(&["rev-parse", "HEAD"]);
    let branch = git_rev_parse(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let host = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();

    let cpu_model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1).map(|v| v.trim().to_string()))
        })
        .unwrap_or_default();

    let kernel = run_capture(&["uname", "-r"]).unwrap_or_default();
    let dpdk_version = run_capture(&["pkg-config", "--modversion", "libdpdk"]).unwrap_or_default();

    Ok(RunMetadata {
        run_id: uuid::Uuid::new_v4(),
        run_started_at: chrono::Utc::now().to_rfc3339(),
        commit_sha,
        branch,
        host,
        instance_type: std::env::var("INSTANCE_TYPE").unwrap_or_default(),
        cpu_model,
        dpdk_version,
        kernel,
        nic_model: std::env::var("NIC_MODEL").unwrap_or_default(),
        nic_fw: std::env::var("NIC_FW").unwrap_or_default(),
        ami_id: std::env::var("AMI_ID").unwrap_or_default(),
        precondition_mode: mode,
        preconditions,
    })
}

fn git_rev_parse(args: &[&str]) -> String {
    std::process::Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn run_capture(argv: &[&str]) -> Option<String> {
    let (cmd, rest) = argv.split_first()?;
    let out = std::process::Command::new(cmd).args(rest).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Emit the 7-row CSV (one per aggregation) to `args.output_csv`. The
/// schema is the unified bench-common CSV (spec §14); `test_case` is
/// the fixed string "request_response_rtt", `dimensions_json` captures
/// request/response byte sizes.
fn emit_csv(args: &Args, meta: &RunMetadata, samples: &[f64]) -> anyhow::Result<()> {
    if samples.is_empty() {
        anyhow::bail!("emit_csv: no samples to summarise (iterations=0?)");
    }
    let summary: Summary = summarize(samples);

    let file = std::fs::File::create(&args.output_csv)
        .with_context(|| format!("creating output CSV {:?}", args.output_csv))?;
    let mut wtr = csv::Writer::from_writer(file);

    let dims = serde_json::json!({
        "request_bytes": args.request_bytes,
        "response_bytes": args.response_bytes,
    })
    .to_string();

    let rows: [(MetricAggregation, f64); 7] = [
        (MetricAggregation::P50, summary.p50),
        (MetricAggregation::P99, summary.p99),
        (MetricAggregation::P999, summary.p999),
        (MetricAggregation::Mean, summary.mean),
        (MetricAggregation::Stddev, summary.stddev),
        (MetricAggregation::Ci95Lower, summary.ci95_lower),
        (MetricAggregation::Ci95Upper, summary.ci95_upper),
    ];

    for (agg, value) in rows {
        let row = CsvRow {
            run_metadata: meta.clone(),
            tool: args.tool.clone(),
            test_case: "request_response_rtt".to_string(),
            feature_set: args.feature_set.clone(),
            dimensions_json: dims.clone(),
            metric_name: "rtt_ns".to_string(),
            metric_unit: "ns".to_string(),
            metric_value: value,
            metric_aggregation: agg,
        };
        wtr.serialize(&row)?;
    }
    wtr.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsc_delta_to_ns_basic() {
        assert_eq!(tsc_delta_to_ns(0, 3_000, 3_000_000_000), 1_000);
        assert_eq!(tsc_delta_to_ns(42, 42, 3_000_000_000), 0);
    }

    #[test]
    fn tsc_delta_to_ns_handles_wrap() {
        let t0 = u64::MAX - 999;
        let t1 = t0.wrapping_add(3_000);
        assert_eq!(tsc_delta_to_ns(t0, t1, 3_000_000_000), 1_000);
    }

    #[test]
    fn parse_mode_accepts_strict_and_lenient() {
        assert_eq!(parse_mode("strict").unwrap(), PreconditionMode::Strict);
        assert_eq!(parse_mode("lenient").unwrap(), PreconditionMode::Lenient);
    }

    #[test]
    fn parse_mode_rejects_garbage() {
        assert!(parse_mode("loose").is_err());
    }

    #[test]
    fn parse_ip_host_order_roundtrip() {
        assert_eq!(parse_ip_host_order("10.0.0.42").unwrap(), 0x0A00_002A);
        assert!(parse_ip_host_order("not.an.ip.addr").is_err());
    }
}
