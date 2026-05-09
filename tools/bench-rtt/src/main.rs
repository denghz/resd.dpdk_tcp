//! bench-rtt — cross-stack request/response RTT distribution.
//!
//! Phase 4 of the 2026-05-09 bench-suite overhaul (closes C-A5, C-B5,
//! C-C1, C-D3). Replaces bench-e2e (binary), bench-stress (matrix
//! runner), and bench-vs-linux mode A by parameterising the stack,
//! payload size, connection count, and (in nightly) netem-spec axes.
//!
//! Tasks 4.2-4.4 land the dpdk_net + linux_kernel + fstack inner
//! loops behind `--stack`. Task 4.5 adds the payload-axis sweep +
//! raw-sample CSV sidecar. Task 4.6 captures per-iter failure counts
//! into the `failed_iter_count` column instead of bailing.

use anyhow::Context;
use clap::Parser;

use bench_common::csv_row::{CsvRow, MetricAggregation};
use bench_common::percentile::{summarize, Summary};
use bench_common::preconditions::{PreconditionMode, PreconditionValue, Preconditions};
use bench_common::raw_samples::RawSamplesWriter;
use bench_common::run_metadata::RunMetadata;

use bench_rtt::attribution::AttributionMode;
use bench_rtt::fstack;
use bench_rtt::hw_task_18::{
    assert_all_events_rx_hw_ts_ns_zero, assert_hw_task_18_post_run, HwTask18Expectations,
};
use bench_rtt::linux_kernel;
use bench_rtt::stack::Stack;
use bench_rtt::sum_identity::assert_sum_identity;
use bench_rtt::workload::{open_connection, request_response_attributed, IterRecord};

use dpdk_net_core::engine::Engine;
use dpdk_net_core::flow_table::ConnHandle;

/// Command-line args. Mirrors bench-ab-runner's shape (see spec §6.1
/// for the full list); adds `sum-identity-tol-ns`, `assert-hw-task-18`,
/// `payload-bytes-sweep`, `connections`, and `raw-samples-csv`.
#[derive(Parser, Debug)]
#[command(version, about = "bench-rtt — request/response RTT + attribution")]
struct Args {
    /// Comparator stack to drive: `dpdk_net` (this stack),
    /// `linux_kernel` (kernel TCP), or `fstack` (F-Stack on DPDK,
    /// requires `--features fstack`).
    #[arg(long, value_enum)]
    stack: Stack,

    /// Peer IP (dotted-quad IPv4, e.g. 10.0.0.42).
    #[arg(long)]
    peer_ip: String,

    /// Peer TCP port.
    #[arg(long, default_value_t = 10_001)]
    peer_port: u16,

    /// Comma-separated list of payload sizes (bytes) to sweep over.
    /// Each value is used as both request and response size for the
    /// bucket. Default `128` matches the legacy bench-e2e workload.
    #[arg(long, value_delimiter = ',', default_value = "128")]
    payload_bytes_sweep: Vec<usize>,

    /// Number of concurrent connections per payload bucket. Default 1
    /// matches the legacy single-connection RTT workload; multi-conn
    /// runs round-robin per iteration.
    #[arg(long, default_value_t = 1)]
    connections: u32,

    /// Measurement iteration count per (payload, connection) bucket.
    #[arg(long, default_value_t = 100_000)]
    iterations: u64,

    /// Warmup iteration count per (payload, connection) bucket.
    #[arg(long, default_value_t = 1_000)]
    warmup: u64,

    /// Optional sidecar CSV for raw per-iter samples. One row per
    /// iteration with columns (bucket_id, iter_idx, rtt_ns).
    #[arg(long)]
    raw_samples_csv: Option<std::path::PathBuf>,

    /// Output CSV path. One row per (payload, aggregation) tuple — 7
    /// aggregations per payload bucket (p50, p99, p999, mean, stddev,
    /// ci95_lower, ci95_upper).
    #[arg(long)]
    output_csv: std::path::PathBuf,

    /// Precondition mode: `strict` aborts on any precondition failure;
    /// `lenient` warns and continues.
    #[arg(long, default_value = "strict")]
    precondition_mode: String,

    /// Local IP (dotted-quad IPv4). Required when `--stack dpdk_net`
    /// or `--stack fstack`.
    #[arg(long, default_value = "")]
    local_ip: String,

    /// Local gateway IP (dotted-quad IPv4). Required when `--stack dpdk_net`.
    #[arg(long, default_value = "")]
    gateway_ip: String,

    /// EAL args, whitespace-separated. Passed verbatim after an implicit
    /// argv[0]="bench-rtt" prefix. Required when `--stack dpdk_net`.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    eal_args: String,

    /// Sum-identity tolerance in ns. Default 50 ns per spec §6.
    /// Only meaningful for `--stack dpdk_net` (the linux_kernel /
    /// fstack arms have no attribution-bucket decomposition).
    #[arg(long, default_value_t = 50)]
    sum_identity_tol_ns: u64,

    /// Post-run, assert the ENA steady-state offload-counter profile
    /// plus per-event `rx_hw_ts_ns == 0`. Only valid with
    /// `--stack dpdk_net`; otherwise silently ignored.
    #[arg(long, default_value_t = false)]
    assert_hw_task_18: bool,

    /// Lcore to pin the engine to. Same shape as bench-ab-runner.
    #[arg(long, default_value_t = 2)]
    lcore: u32,

    /// Tool label emitted as the `tool` CSV column.
    #[arg(long, default_value = "bench-rtt")]
    tool: String,

    /// Feature-set label emitted as the `feature_set` CSV column.
    #[arg(long, default_value = "trading-latency")]
    feature_set: String,
}

/// One bucket's measurement product: aggregated samples + counters
/// for raw-sample emission and the failed-iter column.
struct BucketResult {
    /// `payload_<W>` (e.g. `payload_128`) — keys raw-sample rows back
    /// to the summary row's `dimensions_json` slot.
    bucket_id: String,
    /// Payload size for this bucket — both request and response.
    payload_bytes: usize,
    /// All collected RTT samples in ns (warmup excluded).
    samples: Vec<f64>,
    /// Failed iteration count — populated by Task 4.6.
    failed_iter_count: u64,
    /// `rx_hw_ts_ns` per measurement iter (dpdk_net only); empty for
    /// other stacks. Used by the optional A-HW Task 18 assertion at
    /// the call site that captures it (`run_dpdk_net`); the field is
    /// kept on the struct for downstream visibility but other stacks
    /// leave it empty.
    #[allow(dead_code)]
    rx_hw_ts_ns: Vec<u64>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.payload_bytes_sweep.is_empty() {
        anyhow::bail!("--payload-bytes-sweep resolved to an empty list");
    }
    if args.connections == 0 {
        anyhow::bail!("--connections must be at least 1");
    }
    let mode = parse_mode(&args.precondition_mode)?;

    // 1. Precondition check.
    let preconditions = run_preconditions_check(mode)?;
    if mode == PreconditionMode::Strict && !preconditions_all_pass(&preconditions) {
        eprintln!("bench-rtt: precondition failure in strict mode:");
        for (name, value) in preconditions_as_pairs(&preconditions) {
            if !(value.is_pass() || value.is_not_applicable()) {
                eprintln!("  {name} = {value}");
            }
        }
        std::process::exit(1);
    }

    let metadata = build_run_metadata(mode, preconditions)?;

    // 2. Optional raw-sample sidecar — open before the workload so
    // any header-write error surfaces fast.
    let mut raw_writer = match args.raw_samples_csv.as_ref() {
        Some(path) => Some(
            RawSamplesWriter::create(path, &["bucket_id", "iter_idx", "rtt_ns"])
                .with_context(|| format!("creating raw-samples CSV {path:?}"))?,
        ),
        None => None,
    };

    // 3. Dispatch to the per-stack runner. Each runner returns one
    // `BucketResult` per payload bucket; the outer loop emits the
    // summary CSV + raw-sample sidecar.
    let buckets = match args.stack {
        Stack::DpdkNet => run_dpdk_net(&args)?,
        Stack::LinuxKernel => run_linux_kernel(&args)?,
        Stack::Fstack => run_fstack(&args)?,
    };

    // 4. Emit raw samples (one row per iteration) before summary —
    // raw is the source of truth, summary derives from it.
    if let Some(writer) = raw_writer.as_mut() {
        for bucket in &buckets {
            for (i, rtt) in bucket.samples.iter().enumerate() {
                writer
                    .row(&[
                        &bucket.bucket_id,
                        &i.to_string(),
                        &(*rtt as u64).to_string(),
                    ])
                    .with_context(|| {
                        format!("writing raw-sample row bucket={} iter={i}", bucket.bucket_id)
                    })?;
            }
        }
        writer.flush().context("flushing raw-samples CSV")?;
    }

    // 5. Emit the summary CSV.
    emit_csv(&args, &metadata, &buckets)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// dpdk_net stack — preserved verbatim from bench-e2e (the gold standard).
// ---------------------------------------------------------------------------

fn run_dpdk_net(args: &Args) -> anyhow::Result<Vec<BucketResult>> {
    validate_dpdk_args(args)?;

    eal_init(args)?;
    let _eal_guard = EalGuard;

    let engine = build_engine(args)?;
    let tsc_hz = unsafe { dpdk_net_sys::rte_get_tsc_hz() };
    if tsc_hz == 0 {
        anyhow::bail!("rte_get_tsc_hz() returned 0 — EAL not initialised?");
    }

    let peer_ip = parse_ip_host_order(&args.peer_ip)?;

    let mut buckets: Vec<BucketResult> = Vec::with_capacity(args.payload_bytes_sweep.len());
    for &payload_bytes in &args.payload_bytes_sweep {
        // Open `connections` connections per bucket. Iterate them
        // round-robin so every connection contributes roughly the
        // same number of iterations. Each connection runs warmup +
        // its share of the iteration count.
        let conn_count = args.connections as usize;
        let mut conns: Vec<ConnHandle> = Vec::with_capacity(conn_count);
        for _ in 0..conn_count {
            conns.push(
                open_connection(&engine, peer_ip, args.peer_port)
                    .context("dpdk_net open_connection")?,
            );
        }

        // Run warmup + measurement on each connection. Per-iter
        // failures are counted into `failed_total` rather than
        // propagated; if more than 50% fail the bucket bails (see
        // C-D3 / Task 4.6).
        let mut samples_rtt: Vec<f64> = Vec::with_capacity(args.iterations as usize);
        let mut samples_rx_hw_ts: Vec<u64> = Vec::with_capacity(args.iterations as usize);
        let mut failed_total: u64 = 0;
        let per_conn_iters = args.iterations / conn_count as u64;
        for conn in &conns {
            let (rtt, rx_hw_ts, failed) = run_dpdk_workload_one(
                &engine,
                *conn,
                payload_bytes,
                args.warmup,
                per_conn_iters,
                tsc_hz,
                args.sum_identity_tol_ns,
            )?;
            samples_rtt.extend(rtt);
            samples_rx_hw_ts.extend(rx_hw_ts);
            failed_total += failed;
        }

        if args.assert_hw_task_18 {
            assert_hw_task_18_post_run(engine.counters(), &HwTask18Expectations::default())
                .map_err(anyhow::Error::msg)
                .context("A-HW Task 18 offload-counter post-run assertion failed")?;
            assert_all_events_rx_hw_ts_ns_zero(&samples_rx_hw_ts)
                .map_err(anyhow::Error::msg)
                .context("A-HW Task 18 rx_hw_ts_ns-per-event assertion failed")?;
        }

        buckets.push(BucketResult {
            bucket_id: format!("payload_{payload_bytes}"),
            payload_bytes,
            samples: samples_rtt,
            failed_iter_count: failed_total,
            rx_hw_ts_ns: samples_rx_hw_ts,
        });
    }
    Ok(buckets)
}

/// Drive warmup + measurement iterations on a single connection,
/// enforcing sum-identity per iter. Returns
/// `(rtt_samples, rx_hw_ts_per_sample, failed_iter_count)`. Per-iter
/// failures are counted into `failed_iter_count` rather than aborting
/// the bucket; the loop only bails if more than 50% of iterations
/// fail (closes C-D3 / Task 4.6).
fn run_dpdk_workload_one(
    engine: &Engine,
    conn: ConnHandle,
    payload_bytes: usize,
    warmup: u64,
    iterations: u64,
    tsc_hz: u64,
    sum_identity_tol_ns: u64,
) -> anyhow::Result<(Vec<f64>, Vec<u64>, u64)> {
    let request = vec![0u8; payload_bytes];
    let mut carry_forward: usize = 0;

    // Warmup still bails on any error — the bucket isn't primed yet
    // so a warmup failure means the measurement window isn't safe to
    // enter.
    for i in 0..warmup {
        request_response_attributed(
            engine,
            conn,
            &request,
            payload_bytes,
            tsc_hz,
            &mut carry_forward,
        )
        .with_context(|| format!("warmup iteration {i}"))?;
    }

    let mut rtt_ns: Vec<f64> = Vec::with_capacity(iterations as usize);
    let mut rx_hw_ts_ns: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut failed: u64 = 0;
    for i in 0..iterations {
        let rec_res: anyhow::Result<IterRecord> = request_response_attributed(
            engine,
            conn,
            &request,
            payload_bytes,
            tsc_hz,
            &mut carry_forward,
        )
        .with_context(|| format!("measurement iteration {i}"));

        let rec = match rec_res {
            Ok(rec) => rec,
            Err(e) => {
                eprintln!("bench-rtt: iter {i} failed: {e:#}");
                failed += 1;
                if failed > iterations / 2 {
                    anyhow::bail!(
                        "more than 50% of iterations failed ({failed}/{iterations}); \
                         aborting scenario (last error: {e:#})"
                    );
                }
                continue;
            }
        };

        let sum = match rec.mode {
            AttributionMode::Hw => rec.hw_buckets.unwrap_or_default().total_ns(),
            AttributionMode::Tsc => rec.tsc_buckets.unwrap_or_default().total_ns(),
        };
        // Sum-identity drift is treated as a hard error — it indicates
        // a TSC/clock-source problem, not a per-iter wedge, so the
        // bucket isn't recoverable by counting.
        assert_sum_identity(sum, rec.rtt_ns, sum_identity_tol_ns)
            .map_err(anyhow::Error::msg)
            .with_context(|| {
                format!(
                    "sum-identity check failed on iteration {i} (mode={:?})",
                    rec.mode
                )
            })?;

        rtt_ns.push(rec.rtt_ns as f64);
        rx_hw_ts_ns.push(rec.rx_hw_ts_ns);
    }
    Ok((rtt_ns, rx_hw_ts_ns, failed))
}

// ---------------------------------------------------------------------------
// linux_kernel stack — `std::net::TcpStream` over the host's kernel TCP.
// ---------------------------------------------------------------------------

fn run_linux_kernel(args: &Args) -> anyhow::Result<Vec<BucketResult>> {
    let peer_ip = parse_ip_host_order(&args.peer_ip)?;
    let conn_count = args.connections as usize;

    let mut buckets: Vec<BucketResult> = Vec::with_capacity(args.payload_bytes_sweep.len());
    for &payload_bytes in &args.payload_bytes_sweep {
        let mut samples: Vec<f64> = Vec::with_capacity(args.iterations as usize);
        let per_conn_iters = args.iterations / conn_count as u64;
        for _ in 0..conn_count {
            let mut stream = linux_kernel::connect(peer_ip, args.peer_port)
                .context("linux_kernel connect")?;
            let chunk = linux_kernel::run_rtt_workload(
                &mut stream,
                payload_bytes,
                payload_bytes,
                args.warmup,
                per_conn_iters,
            )
            .context("linux_kernel run_rtt_workload")?;
            samples.extend(chunk);
        }
        buckets.push(BucketResult {
            bucket_id: format!("payload_{payload_bytes}"),
            payload_bytes,
            samples,
            failed_iter_count: 0,
            rx_hw_ts_ns: Vec::new(),
        });
    }
    Ok(buckets)
}

// ---------------------------------------------------------------------------
// fstack stack — F-Stack on DPDK. Feature-gated; default builds bail
// at the imp::run_rtt_workload entry point.
// ---------------------------------------------------------------------------

fn run_fstack(args: &Args) -> anyhow::Result<Vec<BucketResult>> {
    let peer_ip = parse_ip_host_order(&args.peer_ip)?;
    let mut buckets: Vec<BucketResult> = Vec::with_capacity(args.payload_bytes_sweep.len());
    for &payload_bytes in &args.payload_bytes_sweep {
        // fstack's run_rtt_workload uses one ff_run invocation; we drive
        // a single connection per bucket. Multi-conn fstack RTT lands
        // alongside the bench-tx-maxtp work in Phase 5 (the F-Stack
        // ff_run model needs a unified state machine across buckets).
        if args.connections > 1 {
            anyhow::bail!(
                "--connections > 1 is not yet supported on the fstack arm \
                 (single ff_run invocation per process; multi-conn lands \
                 in Phase 5). Use --connections 1 or pick another stack."
            );
        }
        let samples = fstack::imp::run_rtt_workload(
            peer_ip,
            args.peer_port,
            payload_bytes,
            payload_bytes,
            args.warmup,
            args.iterations,
        )?;
        buckets.push(BucketResult {
            bucket_id: format!("payload_{payload_bytes}"),
            payload_bytes,
            samples,
            failed_iter_count: 0,
            rx_hw_ts_ns: Vec::new(),
        });
    }
    Ok(buckets)
}

// ---------------------------------------------------------------------------
// CLI parse + DPDK bring-up + preconditions plumbing.
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

fn validate_dpdk_args(args: &Args) -> anyhow::Result<()> {
    if args.local_ip.is_empty() {
        anyhow::bail!("--local-ip is required when --stack dpdk_net is selected");
    }
    if args.gateway_ip.is_empty() {
        anyhow::bail!("--gateway-ip is required when --stack dpdk_net is selected");
    }
    if args.eal_args.is_empty() {
        anyhow::bail!("--eal-args is required when --stack dpdk_net is selected");
    }
    Ok(())
}

fn eal_init(args: &Args) -> anyhow::Result<()> {
    let mut eal_argv: Vec<String> = vec!["bench-rtt".to_string()];
    eal_argv.extend(
        args.eal_args
            .split_whitespace()
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
                        "bench-rtt: check-bench-preconditions missing; \
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

/// Emit the summary CSV — one set of 7 aggregation rows per payload
/// bucket. `dimensions_json` carries `{stack, payload_bytes, connections}`
/// so bench-report can group by any axis.
fn emit_csv(args: &Args, meta: &RunMetadata, buckets: &[BucketResult]) -> anyhow::Result<()> {
    if buckets.is_empty() {
        anyhow::bail!("emit_csv: no buckets to summarise");
    }
    let file = std::fs::File::create(&args.output_csv)
        .with_context(|| format!("creating output CSV {:?}", args.output_csv))?;
    let mut wtr = csv::Writer::from_writer(file);

    let raw_samples_path: Option<String> = args
        .raw_samples_csv
        .as_ref()
        .map(|p| p.display().to_string());

    for bucket in buckets {
        if bucket.samples.is_empty() {
            anyhow::bail!(
                "bucket {} produced no samples (iterations=0?)",
                bucket.bucket_id
            );
        }
        let summary: Summary = summarize(&bucket.samples);

        let dims = serde_json::json!({
            "stack": args.stack.as_dimension(),
            "payload_bytes": bucket.payload_bytes,
            "connections": args.connections,
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
                cpu_family: None,
                cpu_model_name: None,
                dpdk_version_pkgconfig: None,
                worktree_branch: None,
                uprof_session_id: None,
                raw_samples_path: raw_samples_path.clone(),
                failed_iter_count: bucket.failed_iter_count,
            };
            wtr.serialize(&row)?;
        }
    }
    wtr.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
