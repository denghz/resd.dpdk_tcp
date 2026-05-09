//! bench-rx-burst — peer-driven RX-burst per-segment latency tool.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. Closes claims
//! C-A3, C-B3, C-C2 (see `lib.rs` for details).
//!
//! # Workload
//!
//! Per (W, N) bucket:
//!   1. Connect to the peer's `burst-echo-server` control port.
//!   2. For each burst (warmup + measure):
//!      a. Send `BURST <N> <W>\n`.
//!      b. Drive the engine until N×W bytes have arrived.
//!      c. Parse 16-byte headers from each W-byte segment and
//!         compute `latency_ns = clock::now_ns() - peer_send_ns`.
//!   3. Aggregate per-bucket percentiles + emit a summary CSV; raw
//!      samples (one row per (burst, segment)) optionally to a sidecar.
//!
//! # Stack triplet
//!
//! `dpdk_net` (this crate's engine) + `linux_kernel` (kernel TCP via
//! `std::net`) + `fstack` (F-Stack on DPDK, feature-gated). One arm
//! per invocation — dpdk_net + fstack cannot share a process.
//!
//! # Clock skew
//!
//! `peer_send_ns` is `CLOCK_REALTIME` on the peer; `dut_recv_ns` is
//! the DUT's local clock. NTP offset bound (~100 µs on AWS same-AZ)
//! dominates the absolute latency reading. Phase 9 c7i HW-TS will
//! tighten the cross-host bound; for now the relative ordering and
//! per-burst cadence are the value.

use anyhow::Context;
use clap::Parser;

use bench_common::csv_row::{CsvRow, MetricAggregation};
use bench_common::percentile::{summarize, Summary};
use bench_common::preconditions::{PreconditionMode, PreconditionValue, Preconditions};
use bench_common::raw_samples::RawSamplesWriter;
use bench_common::run_metadata::RunMetadata;

use bench_rx_burst::dpdk;
use bench_rx_burst::linux as linux_arm;
use bench_rx_burst::segment::SegmentRecord;
use bench_rx_burst::stack::Stack;

use dpdk_net_core::engine::Engine;

/// CLI args — mirrors bench-rtt + bench-tx-burst common flags + the
/// RX-burst-specific axes (segment-sizes, burst-counts).
#[derive(Parser, Debug)]
#[command(
    version,
    about = "bench-rx-burst — peer-driven RX-burst per-segment latency"
)]
struct Args {
    /// Comparator stack to drive: `dpdk_net`, `linux_kernel`, or
    /// `fstack` (requires `--features fstack`).
    #[arg(long, value_enum)]
    stack: Stack,

    /// Peer IP (dotted-quad IPv4).
    #[arg(long)]
    peer_ip: String,

    /// Peer control port (where the burst-echo-server listens).
    /// Default 10003 matches the bench-pair AMI's burst-echo-server
    /// listen port.
    #[arg(long, default_value_t = 10003)]
    peer_control_port: u16,

    /// Comma-separated list of segment-size (W) values in bytes.
    /// Each W is a separate bucket. Spec sweep is `64,128,256`.
    #[arg(long, value_delimiter = ',', default_value = "64,128,256")]
    segment_sizes: Vec<usize>,

    /// Comma-separated list of segments-per-burst (N) values.
    /// Each N is a separate bucket. Spec sweep is `16,64,256,1024`.
    #[arg(long, value_delimiter = ',', default_value = "16,64,256,1024")]
    burst_counts: Vec<usize>,

    /// Warmup bursts per bucket (samples discarded). Default 100.
    #[arg(long, default_value_t = 100)]
    warmup_bursts: u64,

    /// Measurement bursts per bucket. Default 10 000 — gives 10 000 ×
    /// burst_count segments per bucket, plenty for stable p999.
    #[arg(long, default_value_t = 10_000)]
    measure_bursts: u64,

    /// Output CSV path — one summary row per (bucket, aggregation).
    #[arg(long)]
    output_csv: std::path::PathBuf,

    /// Optional sidecar CSV for raw per-segment samples.
    #[arg(long)]
    raw_samples_csv: Option<std::path::PathBuf>,

    /// Precondition mode: `strict` aborts on any precondition failure;
    /// `lenient` warns and continues.
    #[arg(long, default_value = "strict")]
    precondition_mode: String,

    /// Local IP (dotted-quad IPv4). Required for `dpdk_net` / `fstack`.
    #[arg(long, default_value = "")]
    local_ip: String,

    /// Local gateway IP (dotted-quad IPv4). Required for `dpdk_net`.
    #[arg(long, default_value = "")]
    gateway_ip: String,

    /// EAL args, whitespace-separated. Required for `dpdk_net`.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    eal_args: String,

    /// Lcore to pin the engine to. Default 2.
    #[arg(long, default_value_t = 2)]
    lcore: u32,

    /// Tool label emitted as the `tool` CSV column.
    #[arg(long, default_value = "bench-rx-burst")]
    tool: String,

    /// Feature-set label emitted as the `feature_set` CSV column.
    #[arg(long, default_value = "trading-latency")]
    feature_set: String,

    /// F-Stack startup config file path. Required when
    /// `--stack fstack`.
    #[arg(long, default_value = "/etc/f-stack.conf")]
    fstack_conf: String,
}

/// One bucket's measurement product. Carries the per-segment records
/// and the bucket's (W, N) coordinates for CSV emit.
struct BucketResult {
    bucket_id: u32,
    segment_size: usize,
    burst_count: usize,
    samples: Vec<SegmentRecord>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.segment_sizes.is_empty() {
        anyhow::bail!("--segment-sizes resolved to an empty list");
    }
    if args.burst_counts.is_empty() {
        anyhow::bail!("--burst-counts resolved to an empty list");
    }
    for &w in &args.segment_sizes {
        if w < 16 {
            anyhow::bail!(
                "--segment-sizes value {} is below the 16-byte header floor",
                w
            );
        }
    }
    let mode = parse_mode(&args.precondition_mode)?;

    // 1. Precondition check.
    let preconditions = run_preconditions_check(mode)?;
    if mode == PreconditionMode::Strict && !preconditions_all_pass(&preconditions) {
        eprintln!("bench-rx-burst: precondition failure in strict mode:");
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
            RawSamplesWriter::create(
                path,
                &[
                    "bucket_id",
                    "burst_idx",
                    "seg_idx",
                    "peer_send_ns",
                    "dut_recv_ns",
                    "latency_ns",
                ],
            )
            .with_context(|| format!("creating raw-samples CSV {path:?}"))?,
        ),
        None => None,
    };

    // 3. Dispatch.
    let buckets = match args.stack {
        Stack::DpdkNet => run_dpdk_net(&args)?,
        Stack::LinuxKernel => run_linux_kernel(&args)?,
        Stack::Fstack => run_fstack(&args)?,
    };

    // 4. Emit raw samples first — raw is the source of truth.
    if let Some(writer) = raw_writer.as_mut() {
        for bucket in &buckets {
            for record in &bucket.samples {
                writer
                    .row(&[
                        &record.bucket_id.to_string(),
                        &record.burst_idx.to_string(),
                        &record.seg_idx.to_string(),
                        &record.peer_send_ns.to_string(),
                        &record.dut_recv_ns.to_string(),
                        &record.latency_ns.to_string(),
                    ])
                    .with_context(|| {
                        format!(
                            "writing raw-sample row bucket={} burst={} seg={}",
                            record.bucket_id, record.burst_idx, record.seg_idx
                        )
                    })?;
            }
        }
        writer.flush().context("flushing raw-samples CSV")?;
    }

    // 5. Summary CSV.
    emit_csv(&args, &metadata, &buckets)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// dpdk_net dispatch — the only stack wired in Task 8.2; linux_kernel +
// fstack land in Task 8.3.
// ---------------------------------------------------------------------------

fn run_dpdk_net(args: &Args) -> anyhow::Result<Vec<BucketResult>> {
    validate_dpdk_args(args)?;

    eal_init(args)?;
    let _eal_guard = EalGuard;

    let engine = build_engine(args)?;
    let peer_ip = parse_ip_host_order(&args.peer_ip)?;

    let conn = dpdk::open_control_connection(&engine, peer_ip, args.peer_control_port)?;

    let mut buckets: Vec<BucketResult> = Vec::new();
    let mut bucket_id: u32 = 0;
    for &segment_size in &args.segment_sizes {
        for &burst_count in &args.burst_counts {
            eprintln!(
                "bench-rx-burst: dpdk_net W={} N={} (bucket {})",
                segment_size, burst_count, bucket_id
            );
            let cfg = dpdk::DpdkRxBurstCfg {
                engine: &engine,
                conn,
                bucket_id,
                segment_size,
                burst_count,
                warmup_bursts: args.warmup_bursts,
                measure_bursts: args.measure_bursts,
            };
            let run = dpdk::run_bucket(&cfg).with_context(|| {
                format!("dpdk run_bucket W={segment_size} N={burst_count}")
            })?;
            buckets.push(BucketResult {
                bucket_id,
                segment_size,
                burst_count,
                samples: run.samples,
            });
            bucket_id += 1;
        }
    }
    Ok(buckets)
}

// ---------------------------------------------------------------------------
// linux_kernel — blocking TcpStream + per-read parsing.
// ---------------------------------------------------------------------------

fn run_linux_kernel(args: &Args) -> anyhow::Result<Vec<BucketResult>> {
    let peer_ip = parse_ip_host_order(&args.peer_ip)?;
    let mut stream = linux_arm::open_control_connection(peer_ip, args.peer_control_port)
        .context("linux_kernel open_control_connection")?;

    let mut buckets: Vec<BucketResult> = Vec::new();
    let mut bucket_id: u32 = 0;
    for &segment_size in &args.segment_sizes {
        for &burst_count in &args.burst_counts {
            eprintln!(
                "bench-rx-burst: linux_kernel W={} N={} (bucket {})",
                segment_size, burst_count, bucket_id
            );
            let mut cfg = linux_arm::LinuxRxBurstCfg {
                stream: &mut stream,
                bucket_id,
                segment_size,
                burst_count,
                warmup_bursts: args.warmup_bursts,
                measure_bursts: args.measure_bursts,
            };
            let run = linux_arm::run_bucket(&mut cfg).with_context(|| {
                format!("linux run_bucket W={segment_size} N={burst_count}")
            })?;
            buckets.push(BucketResult {
                bucket_id,
                segment_size,
                burst_count,
                samples: run.samples,
            });
            bucket_id += 1;
        }
    }
    Ok(buckets)
}

// ---------------------------------------------------------------------------
// fstack — F-Stack RX-burst arm.
// ---------------------------------------------------------------------------

#[cfg(feature = "fstack")]
fn run_fstack(args: &Args) -> anyhow::Result<Vec<BucketResult>> {
    use bench_rx_burst::fstack as fstack_arm;

    validate_fstack_args(args)?;
    init_fstack(args)?;

    let peer_ip = parse_ip_host_order(&args.peer_ip)?;

    // Build the bucket grid.
    let mut grid: Vec<fstack_arm::FstackBucketCfg> = Vec::new();
    let mut bucket_id: u32 = 0;
    let mut bucket_axis: Vec<(u32, usize, usize)> = Vec::new();
    for &segment_size in &args.segment_sizes {
        for &burst_count in &args.burst_counts {
            grid.push(fstack_arm::FstackBucketCfg {
                bucket_id,
                segment_size,
                burst_count,
            });
            bucket_axis.push((bucket_id, segment_size, burst_count));
            bucket_id += 1;
        }
    }

    let results = fstack_arm::run_grid(
        &grid,
        args.warmup_bursts,
        args.measure_bursts,
        peer_ip,
        args.peer_control_port,
    );

    let mut buckets: Vec<BucketResult> = Vec::with_capacity(results.len());
    for ((bid, segment_size, burst_count), res) in bucket_axis.iter().zip(results.into_iter()) {
        match res.result {
            Ok(run) => buckets.push(BucketResult {
                bucket_id: *bid,
                segment_size: *segment_size,
                burst_count: *burst_count,
                samples: run.samples,
            }),
            Err(e) => {
                eprintln!(
                    "bench-rx-burst: fstack bucket id={} (W={} N={}) failed: {}",
                    bid, segment_size, burst_count, e
                );
                buckets.push(BucketResult {
                    bucket_id: *bid,
                    segment_size: *segment_size,
                    burst_count: *burst_count,
                    samples: Vec::new(),
                });
            }
        }
    }
    Ok(buckets)
}

#[cfg(not(feature = "fstack"))]
fn run_fstack(_args: &Args) -> anyhow::Result<Vec<BucketResult>> {
    anyhow::bail!(
        "bench-rx-burst built without `fstack` feature; rebuild with \
         `--features fstack` on the AMI where libfstack.a is installed."
    )
}

#[cfg(feature = "fstack")]
fn validate_fstack_args(args: &Args) -> anyhow::Result<()> {
    if !std::path::Path::new(&args.fstack_conf).exists() {
        anyhow::bail!(
            "--fstack-conf path `{}` does not exist; create it with the \
             [dpdk] lcore_mask + allow=<PCI> + [port0] sections for the DUT",
            args.fstack_conf
        );
    }
    Ok(())
}

#[cfg(feature = "fstack")]
fn init_fstack(args: &Args) -> anyhow::Result<()> {
    let argv: Vec<String> = vec![
        "bench-rx-burst".to_string(),
        format!("--conf={}", args.fstack_conf),
        "--proc-id=0".to_string(),
    ];
    eprintln!("bench-rx-burst: ff_init argv={:?}", argv);
    bench_rx_burst::fstack_ffi::ff_init_from_args(&argv)
        .map_err(|e| anyhow::anyhow!("ff_init failed: {e}"))
}

// ---------------------------------------------------------------------------
// CLI helpers + EAL bring-up + preconditions plumbing.
// ---------------------------------------------------------------------------

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
    let mut eal_argv: Vec<String> = vec!["bench-rx-burst".to_string()];
    eal_argv.extend(args.eal_args.split_whitespace().map(|s| s.to_string()));
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
                        "bench-rx-burst: WARN lenient mode — check-bench-preconditions \
                         not found and BENCH_PRECONDITIONS_JSON unset; emitting \
                         preconditions as n/a (unverified)"
                    );
                    return Ok(all_unknown_preconditions());
                }
            },
        },
    };

    parse_preconditions_json(&json_bytes)
        .context("parsing check-bench-preconditions JSON output")
}

fn parse_preconditions_json(bytes: &[u8]) -> anyhow::Result<Preconditions> {
    let json: serde_json::Value = serde_json::from_slice(bytes)?;
    let checks = json
        .get("checks")
        .ok_or_else(|| anyhow::anyhow!("preconditions JSON missing top-level `checks` object"))?;
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

fn all_unknown_preconditions() -> Preconditions {
    Preconditions {
        isolcpus: PreconditionValue::NotApplicable,
        nohz_full: PreconditionValue::NotApplicable,
        rcu_nocbs: PreconditionValue::NotApplicable,
        governor: PreconditionValue::NotApplicable,
        cstate_max: PreconditionValue::NotApplicable,
        tsc_invariant: PreconditionValue::NotApplicable,
        coalesce_off: PreconditionValue::NotApplicable,
        tso_off: PreconditionValue::NotApplicable,
        lro_off: PreconditionValue::NotApplicable,
        rss_on: PreconditionValue::NotApplicable,
        thermal_throttle: PreconditionValue::NotApplicable,
        hugepages_reserved: PreconditionValue::NotApplicable,
        irqbalance_off: PreconditionValue::NotApplicable,
        wc_active: PreconditionValue::NotApplicable,
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

/// Emit the summary CSV — one set of 7 aggregation rows per
/// (W, N) bucket. `dimensions_json` carries `{stack, segment_size,
/// burst_count}` so bench-report can group by any axis.
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
            // Empty bucket — emit a stub row so downstream report sees
            // a marker. Use NaN-like 0 with `failed_iter_count` not
            // applicable here; we just skip the percentile rows.
            eprintln!(
                "bench-rx-burst: WARN bucket id={} (W={} N={}) produced no samples",
                bucket.bucket_id, bucket.segment_size, bucket.burst_count
            );
            continue;
        }
        let lat_ns: Vec<f64> = bucket.samples.iter().map(|r| r.latency_ns as f64).collect();
        let summary: Summary = summarize(&lat_ns);

        let dims = serde_json::json!({
            "stack": args.stack.as_dimension(),
            "segment_size_bytes": bucket.segment_size,
            "burst_count": bucket.burst_count,
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
                test_case: "rx_burst_segment_latency".to_string(),
                feature_set: args.feature_set.clone(),
                dimensions_json: dims.clone(),
                metric_name: "latency_ns".to_string(),
                metric_unit: "ns".to_string(),
                metric_value: value,
                metric_aggregation: agg,
                cpu_family: None,
                cpu_model_name: None,
                dpdk_version_pkgconfig: None,
                worktree_branch: None,
                uprof_session_id: None,
                raw_samples_path: raw_samples_path.clone(),
                failed_iter_count: 0,
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
