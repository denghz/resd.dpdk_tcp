//! bench-tx-burst — K × G one-shot burst-write throughput grid (spec §11.1).
//!
//! Phase 5 of the 2026-05-09 bench-suite overhaul split the legacy
//! bench-vs-mtcp binary into bench-tx-burst (this binary) and
//! bench-tx-maxtp. The mTCP arm was removed in Phase 2; the live
//! comparator triplet is `dpdk_net` + `linux_kernel` + `fstack`.
//!
//! # Comparator triplet wiring
//!
//! The `--stack` arg picks exactly ONE arm per invocation. dpdk_net and
//! fstack cannot share a process (both call `rte_eal_init`); separate
//! invocations are required. The linux_kernel arm uses the host kernel's
//! TCP stack via `std::net::TcpStream` and has no DPDK constraints.

use anyhow::Context;
use clap::Parser;

use bench_common::preconditions::{PreconditionMode, PreconditionValue, Preconditions};
use bench_common::run_metadata::RunMetadata;

use bench_common::preflight::{
    check_mss_and_burst_agreement, check_nic_saturation_bps, check_peer_window,
    check_sanity_invariant, BucketVerdict,
};
use bench_tx_burst::burst::{
    emit_bucket_rows, enumerate_filtered_grid, BucketAggregate,
};
use bench_tx_burst::dpdk::{self, DpdkBurstCfg, TxTsMode};
use bench_tx_burst::Stack;

use dpdk_net_core::engine::Engine;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "bench-tx-burst — K × G one-shot burst-write throughput grid"
)]
struct Args {
    /// Peer IP (dotted-quad IPv4).
    #[arg(long)]
    peer_ip: String,

    /// Peer TCP port. Default 10001 matches the bench-pair AMI's
    /// echo-server listen port (used by dpdk_net + linux_kernel +
    /// fstack — all three send standard TCP so the dpdk echo-server
    /// drains all three).
    #[arg(long, default_value_t = 10_001)]
    peer_port: u16,

    /// Stack arm to drive: `dpdk_net`, `linux_kernel`, or `fstack`.
    /// One arm per invocation — dpdk_net and fstack cannot share a
    /// process (both call `rte_eal_init`).
    #[arg(long)]
    stack: String,

    /// Bursts per bucket post-warmup. Spec §11.1 requires ≥10 k per
    /// bucket for the aggregation to be statistically stable.
    #[arg(long, default_value_t = 10_000)]
    bursts_per_bucket: u64,

    /// Warmup bursts per bucket (discarded). Spec §11.1 locks this
    /// at 100.
    #[arg(long, default_value_t = 100)]
    warmup: u64,

    /// MSS in bytes. Spec §11.1 locks this at 1460.
    #[arg(long, default_value_t = 1460)]
    mss: u16,

    /// Output CSV path.
    #[arg(long)]
    output_csv: std::path::PathBuf,

    /// Precondition mode: `strict` aborts on precondition failure;
    /// `lenient` warns and continues.
    #[arg(long, default_value = "strict")]
    precondition_mode: String,

    /// Local IP (dotted-quad IPv4). Required iff `--stack dpdk_net`.
    #[arg(long, default_value = "")]
    local_ip: String,

    /// Local gateway IP (dotted-quad IPv4). Required iff `--stack dpdk_net`.
    #[arg(long, default_value = "")]
    gateway_ip: String,

    /// EAL args, whitespace-separated. Required iff `--stack dpdk_net`.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    eal_args: String,

    /// Lcore to pin the dpdk_net engine to.
    #[arg(long, default_value_t = 2)]
    lcore: u32,

    /// Tool label emitted as the `tool` CSV column.
    #[arg(long, default_value = "bench-tx-burst")]
    tool: String,

    /// Feature-set label emitted as the `feature_set` CSV column.
    #[arg(long, default_value = "trading-latency")]
    feature_set: String,

    /// Grid subset filter — comma-separated K values in bytes to run.
    /// Empty = run all 5 K values. Must match a subset of spec §11.1's
    /// {65536, 262144, 1048576, 4194304, 16777216}.
    #[arg(long, default_value = "")]
    burst_sizes: String,

    /// Grid subset filter — comma-separated G values in ms to run.
    /// Empty = run all 4 G values.
    #[arg(long, default_value = "")]
    gap_mss: String,

    /// NIC line-rate cap (bits-per-second) for the post-run
    /// NIC-saturation check. Defaults to the `NIC_MAX_BPS` env var if
    /// set; otherwise the check is skipped with a warning.
    #[arg(long)]
    nic_max_bps: Option<u64>,

    /// Peer SSH target for pre-run peer rcv_space introspection
    /// (`ss -ti | rcv_space`). When unset, the pre-run `peer_rwnd`
    /// guard degrades to the placebo (peer_rwnd := bucket K).
    #[arg(long)]
    peer_ssh: Option<String>,

    /// F-Stack startup config file path (`--conf` forwarded to
    /// `ff_init`). Required when `--stack fstack` is selected.
    #[arg(long, default_value = "/etc/f-stack.conf")]
    fstack_conf: String,

    /// Reserved for future F-Stack variants. Currently unused;
    /// F-Stack reads EAL flags from the `[dpdk]` section of `--fstack-conf`.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    fstack_eal_args: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mode = parse_precondition_mode(&args.precondition_mode)?;
    let stack = Stack::parse(&args.stack).map_err(|e| anyhow::anyhow!(e))?;

    // Host preconditions.
    let preconditions = run_preconditions_check(mode)?;
    if mode == PreconditionMode::Strict && !preconditions_all_pass(&preconditions) {
        eprintln!("bench-tx-burst: precondition failure in strict mode:");
        for (name, value) in preconditions_as_pairs(&preconditions) {
            if !(value.is_pass() || value.is_not_applicable()) {
                eprintln!("  {name} = {value}");
            }
        }
        std::process::exit(1);
    }

    // Bring up the engine only when needed. Drop-order invariant: engine
    // must drop BEFORE _eal_guard so Engine::Drop can call DPDK APIs
    // before rte_eal_cleanup fires in EalGuard::drop.
    let needs_dpdk = stack == Stack::DpdkNet;
    let needs_fstack = stack == Stack::Fstack;
    let mut _eal_guard: Option<EalGuard> = None;
    let mut engine: Option<Engine> = None;
    let mut tsc_hz: u64 = 0;
    if needs_dpdk {
        validate_dpdk_args(&args)?;
        eal_init(&args)?;
        _eal_guard = Some(EalGuard);
        engine = Some(build_engine(&args)?);
        tsc_hz = unsafe { dpdk_net_sys::rte_get_tsc_hz() };
        if tsc_hz == 0 {
            anyhow::bail!("rte_get_tsc_hz() returned 0 — EAL not initialised?");
        }
    }

    #[cfg(feature = "fstack")]
    if needs_fstack {
        validate_fstack_args(&args)?;
        init_fstack(&args)?;
        tsc_hz = unsafe { dpdk_net_sys::rte_get_tsc_hz() };
        if tsc_hz == 0 {
            anyhow::bail!("rte_get_tsc_hz() returned 0 after ff_init");
        }
    }
    #[cfg(not(feature = "fstack"))]
    let _ = needs_fstack;

    let metadata = build_run_metadata(mode, preconditions)?;
    let mut writer = csv::Writer::from_path(&args.output_csv)
        .with_context(|| format!("creating output CSV {:?}", args.output_csv))?;

    let peer_ip = parse_ip_host_order(&args.peer_ip)?;
    let nic_max_bps = resolve_nic_max_bps(args.nic_max_bps);
    if nic_max_bps.is_none() {
        eprintln!(
            "bench-tx-burst: WARN --nic-max-bps unset and NIC_MAX_BPS env var \
             unset; skipping post-run NIC-saturation check (spec §11.1 check 3)."
        );
    }

    let k_filter = parse_u64_list(&args.burst_sizes)?;
    let g_filter = parse_u64_list(&args.gap_mss)?;
    let grid = enumerate_filtered_grid(k_filter.as_deref(), g_filter.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;

    match stack {
        Stack::DpdkNet => {
            let engine = engine.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "DpdkNet stack selected but engine not provided — main.rs invariant violated"
                )
            })?;
            run_burst_grid_dpdk(
                engine,
                peer_ip,
                args.peer_port,
                &grid,
                &args,
                &metadata,
                tsc_hz,
                nic_max_bps,
                &mut writer,
            )?;
        }
        Stack::LinuxKernel => {
            run_burst_grid_linux(
                peer_ip,
                args.peer_port,
                &grid,
                &args,
                &metadata,
                nic_max_bps,
                &mut writer,
            )?;
        }
        Stack::Fstack => {
            run_burst_grid_fstack(
                peer_ip,
                args.peer_port,
                &grid,
                &args,
                &metadata,
                tsc_hz,
                nic_max_bps,
                &mut writer,
            )?;
        }
    }

    writer.flush()?;
    Ok(())
}

/// Resolve the NIC line-rate cap from CLI flag → `NIC_MAX_BPS` env var.
fn resolve_nic_max_bps(flag: Option<u64>) -> Option<u64> {
    if let Some(v) = flag {
        return Some(v);
    }
    match std::env::var("NIC_MAX_BPS") {
        Ok(s) => s.trim().parse::<u64>().ok(),
        Err(_) => None,
    }
}

fn resolve_peer_rwnd_bytes(
    peer_ssh: Option<&str>,
    dut_ip: std::net::Ipv4Addr,
    peer_port: u16,
    placebo_rwnd: u64,
) -> u64 {
    let Some(ssh) = peer_ssh else {
        eprintln!(
            "bench-tx-burst: WARN --peer-ssh unset; peer_rwnd pre-run check \
             degraded to placebo ({placebo_rwnd} B)"
        );
        return placebo_rwnd;
    };
    match bench_common::peer_introspect::fetch_peer_rwnd_bytes(ssh, dut_ip, peer_port) {
        Ok(v) => v as u64,
        Err(e) => {
            eprintln!(
                "bench-tx-burst: WARN peer_rwnd introspection via `ss -ti` \
                 on {ssh}:{peer_port} failed: {e}; falling back to placebo \
                 ({placebo_rwnd} B)"
            );
            placebo_rwnd
        }
    }
}

// ---------------------------------------------------------------------------
// dpdk_net grid driver.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_burst_grid_dpdk<W: std::io::Write>(
    engine: &Engine,
    peer_ip: u32,
    peer_port: u16,
    grid: &[bench_tx_burst::burst::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    tsc_hz: u64,
    nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
) -> anyhow::Result<()> {
    eprintln!(
        "bench-tx-burst: opening persistent dpdk_net connection to {}:{}",
        args.peer_ip, peer_port
    );
    let conn = dpdk::open_persistent_connection(engine, peer_ip, peer_port)?;

    let dut_ip: std::net::Ipv4Addr = args
        .local_ip
        .parse()
        .with_context(|| format!("parsing --local-ip `{}` for peer rwnd probe", args.local_ip))?;

    let mut payload_cache: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();

    for bucket in grid {
        eprintln!("bench-tx-burst: running dpdk_net bucket {}", bucket.label());

        let payload = payload_cache
            .entry(bucket.burst_bytes)
            .or_insert_with(|| vec![0u8; bucket.burst_bytes as usize]);

        let mss_verdict = check_mss_and_burst_agreement(args.mss, args.mss, 32, 32);

        let peer_rwnd = resolve_peer_rwnd_bytes(
            args.peer_ssh.as_deref(),
            dut_ip,
            peer_port,
            bucket.burst_bytes,
        );
        let rwnd_verdict = check_peer_window(peer_rwnd, bucket.burst_bytes);

        let verdict = if !mss_verdict.is_ok() {
            mss_verdict
        } else if !rwnd_verdict.is_ok() {
            rwnd_verdict
        } else {
            BucketVerdict::Ok
        };

        let tx_ts_mode = TxTsMode::TscFallback;

        if !verdict.is_ok() {
            eprintln!(
                "bench-tx-burst: bucket {} invalidated pre-run: {}",
                bucket.label(),
                verdict.reason().unwrap_or("<unknown>")
            );
            let agg = BucketAggregate::from_samples(
                *bucket,
                Stack::DpdkNet,
                &[],
                verdict,
                Some(tx_ts_mode),
            );
            emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
                .context("emit invalidated bucket row")?;
            continue;
        }

        let tx_payload_pre = engine
            .counters()
            .tcp
            .tx_payload_bytes
            .load(std::sync::atomic::Ordering::Relaxed);

        let cfg = DpdkBurstCfg {
            engine,
            conn,
            bucket: *bucket,
            warmup: args.warmup,
            bursts: args.bursts_per_bucket,
            tsc_hz,
            payload,
            tx_ts_mode,
        };
        let run = dpdk::run_bucket(&cfg).with_context(|| {
            format!("dpdk::run_bucket for {}", bucket.label())
        })?;

        let tx_payload_post = engine
            .counters()
            .tcp
            .tx_payload_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let counter_delta = tx_payload_post.saturating_sub(tx_payload_pre);
        if counter_delta > 0 {
            if let Err(e) = check_sanity_invariant(run.sum_over_bursts_bytes, counter_delta) {
                eprintln!(
                    "bench-tx-burst: sanity invariant violated for bucket {}: {e}",
                    bucket.label()
                );
                anyhow::bail!(e);
            }
        } else {
            eprintln!(
                "bench-tx-burst: sanity invariant check skipped for bucket {} \
                 (tx_payload_bytes counter is 0 — build with \
                 `--features obs-byte-counters` to enable)",
                bucket.label()
            );
        }

        let mut agg = BucketAggregate::from_samples(
            *bucket,
            Stack::DpdkNet,
            &run.samples,
            BucketVerdict::Ok,
            Some(tx_ts_mode),
        );
        if let Some(max_bps) = nic_max_bps {
            let achieved_bps = mean_throughput_bps_dpdk(&run) as u64;
            let sat_verdict = check_nic_saturation_bps(achieved_bps, max_bps);
            if !sat_verdict.is_ok() {
                eprintln!(
                    "bench-tx-burst: bucket {} NIC-bound post-run: {}",
                    bucket.label(),
                    sat_verdict.reason().unwrap_or("<unknown>")
                );
                agg.override_verdict(sat_verdict);
            }
        }
        emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
            .context("emit bucket rows")?;
    }
    Ok(())
}

fn mean_throughput_bps_dpdk(run: &dpdk::BucketRun) -> f64 {
    if run.samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = run.samples.iter().map(|s| s.throughput_bps).sum();
    sum / (run.samples.len() as f64)
}

// ---------------------------------------------------------------------------
// linux_kernel grid driver — Phase 5 new arm.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_burst_grid_linux<W: std::io::Write>(
    peer_ip: u32,
    peer_port: u16,
    grid: &[bench_tx_burst::burst::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
) -> anyhow::Result<()> {
    use bench_tx_burst::linux as linux_arm;

    eprintln!(
        "bench-tx-burst: opening persistent linux_kernel connection to {}:{}",
        args.peer_ip, peer_port
    );
    let mut stream = linux_arm::open_persistent_connection(peer_ip, peer_port)
        .context("linux_kernel open_persistent_connection")?;

    let mut payload_cache: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
    for bucket in grid {
        eprintln!("bench-tx-burst: running linux_kernel bucket {}", bucket.label());

        let payload = payload_cache
            .entry(bucket.burst_bytes)
            .or_insert_with(|| vec![0u8; bucket.burst_bytes as usize]);

        let mut cfg = linux_arm::LinuxBurstCfg {
            stream: &mut stream,
            bucket: *bucket,
            warmup: args.warmup,
            bursts: args.bursts_per_bucket,
            payload,
        };
        let run = match linux_arm::run_bucket(&mut cfg).with_context(|| {
            format!("linux_kernel::run_bucket for {}", bucket.label())
        }) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "bench-tx-burst: linux_kernel bucket {} failed: {e:#}",
                    bucket.label()
                );
                let agg = BucketAggregate::from_samples(
                    *bucket,
                    Stack::LinuxKernel,
                    &[],
                    BucketVerdict::Invalid(format!("linux_kernel run failed: {e}")),
                    None,
                );
                emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
                    .context("emit failed linux_kernel bucket row")?;
                continue;
            }
        };

        let mut agg = BucketAggregate::from_samples(
            *bucket,
            Stack::LinuxKernel,
            &run.samples,
            BucketVerdict::Ok,
            None,
        );
        if let Some(max_bps) = nic_max_bps {
            let achieved_bps = mean_throughput_bps_linux(&run) as u64;
            let sat_verdict = check_nic_saturation_bps(achieved_bps, max_bps);
            if !sat_verdict.is_ok() {
                eprintln!(
                    "bench-tx-burst: linux_kernel bucket {} NIC-bound post-run: {}",
                    bucket.label(),
                    sat_verdict.reason().unwrap_or("<unknown>")
                );
                agg.override_verdict(sat_verdict);
            }
        }
        emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
            .context("emit linux_kernel bucket rows")?;
    }
    Ok(())
}

fn mean_throughput_bps_linux(run: &bench_tx_burst::linux::BucketRun) -> f64 {
    if run.samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = run.samples.iter().map(|s| s.throughput_bps).sum();
    sum / (run.samples.len() as f64)
}

// ---------------------------------------------------------------------------
// fstack grid driver (feature-gated). When the feature is off we emit
// stub-marker rows so downstream bench-report still sees a row.
// ---------------------------------------------------------------------------

#[cfg(feature = "fstack")]
#[allow(clippy::too_many_arguments)]
fn run_burst_grid_fstack<W: std::io::Write>(
    peer_ip: u32,
    peer_port: u16,
    grid: &[bench_tx_burst::burst::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    tsc_hz: u64,
    nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
) -> anyhow::Result<()> {
    use bench_tx_burst::fstack;

    let tx_ts_mode = TxTsMode::TscFallback;

    let grid_results = fstack::run_burst_grid(
        grid,
        args.warmup,
        args.bursts_per_bucket,
        tsc_hz,
        peer_ip,
        peer_port,
        tx_ts_mode,
    );

    for gr in grid_results {
        let bucket = gr.bucket;
        match gr.result {
            Err(e) => {
                eprintln!(
                    "bench-tx-burst: fstack bucket {} failed: {e}",
                    bucket.label()
                );
                let agg = BucketAggregate::from_samples(
                    bucket,
                    Stack::Fstack,
                    &[],
                    BucketVerdict::Invalid(e),
                    Some(tx_ts_mode),
                );
                emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
                    .context("emit fstack bucket rows")?;
            }
            Ok(run) => {
                let mut agg = BucketAggregate::from_samples(
                    bucket,
                    Stack::Fstack,
                    &run.samples,
                    BucketVerdict::Ok,
                    Some(tx_ts_mode),
                );
                if let Some(max_bps) = nic_max_bps {
                    let achieved =
                        mean_throughput_from_burst_samples(&run.samples) as u64;
                    let sat = check_nic_saturation_bps(achieved, max_bps);
                    if !sat.is_ok() {
                        eprintln!(
                            "bench-tx-burst: fstack bucket {} NIC-bound post-run: {}",
                            bucket.label(),
                            sat.reason().unwrap_or("<unknown>")
                        );
                        agg.override_verdict(sat);
                    }
                }
                emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
                    .context("emit fstack bucket rows")?;
            }
        }
    }
    Ok(())
}

#[cfg(feature = "fstack")]
fn mean_throughput_from_burst_samples(
    samples: &[bench_tx_burst::burst::BurstSample],
) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| s.throughput_bps).sum();
    sum / (samples.len() as f64)
}

#[cfg(not(feature = "fstack"))]
#[allow(clippy::too_many_arguments)]
fn run_burst_grid_fstack<W: std::io::Write>(
    _peer_ip: u32,
    _peer_port: u16,
    grid: &[bench_tx_burst::burst::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    _tsc_hz: u64,
    _nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
) -> anyhow::Result<()> {
    eprintln!(
        "bench-tx-burst: WARN --stack fstack selected but binary built without `fstack` \
         feature; emitting marker rows. Rebuild with `--features fstack` on the AMI \
         where libfstack.a is installed."
    );
    for bucket in grid {
        let agg = BucketAggregate::from_samples(
            *bucket,
            Stack::Fstack,
            &[],
            BucketVerdict::Invalid(
                "fstack feature not compiled in (libfstack.a not available)".to_string(),
            ),
            None,
        );
        emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
            .context("emit fstack-stub burst marker row")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// F-Stack init helpers (feature-gated).
// ---------------------------------------------------------------------------

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
        "bench-tx-burst".to_string(),
        format!("--conf={}", args.fstack_conf),
        "--proc-id=0".to_string(),
    ];
    eprintln!("bench-tx-burst: ff_init argv={:?}", argv);
    bench_tx_burst::fstack_ffi::ff_init_from_args(&argv)
        .map_err(|e| anyhow::anyhow!("ff_init failed: {e}"))
}

// ---------------------------------------------------------------------------
// CLI parse helpers + DPDK bring-up.
// ---------------------------------------------------------------------------

fn parse_u64_list(csv: &str) -> anyhow::Result<Option<Vec<u64>>> {
    let tokens: Vec<&str> = csv.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if tokens.is_empty() {
        return Ok(None);
    }
    let mut out: Vec<u64> = Vec::with_capacity(tokens.len());
    for t in tokens {
        let v: u64 = t.parse().with_context(|| format!("parsing `{t}` as u64"))?;
        out.push(v);
    }
    Ok(Some(out))
}

struct EalGuard;

impl Drop for EalGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = dpdk_net_sys::rte_eal_cleanup();
        }
    }
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

fn parse_precondition_mode(s: &str) -> anyhow::Result<PreconditionMode> {
    s.parse().map_err(|e: String| anyhow::anyhow!(e))
}

fn parse_ip_host_order(s: &str) -> anyhow::Result<u32> {
    let addr: std::net::Ipv4Addr = s
        .parse()
        .with_context(|| format!("invalid IPv4 address: {s}"))?;
    Ok(u32::from_be_bytes(addr.octets()))
}

fn eal_init(args: &Args) -> anyhow::Result<()> {
    let mut eal_argv: Vec<String> = vec!["bench-tx-burst".to_string()];
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
        tcp_mss: args.mss as u32,
        cc_mode: 0, // trading-latency
        max_connections: 512,
        tx_data_mempool_size: 32_768,
        ..dpdk_net_core::engine::EngineConfig::default()
    };
    Engine::new(cfg).map_err(|e| anyhow::anyhow!("Engine::new failed: {e:?}"))
}

// ---------------------------------------------------------------------------
// Preconditions plumbing.
// ---------------------------------------------------------------------------

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
                        "bench-tx-burst: WARN lenient mode — check-bench-preconditions \
                         not found and BENCH_PRECONDITIONS_JSON unset; emitting \
                         preconditions as n/a (unverified)"
                    );
                    return Ok(all_unknown_preconditions());
                }
            },
        },
    };

    parse_preconditions_json(&json_bytes).context("parsing check-bench-preconditions JSON output")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_u64_list_empty_is_none() {
        assert_eq!(parse_u64_list("").unwrap(), None);
        assert_eq!(parse_u64_list("  ").unwrap(), None);
    }

    #[test]
    fn parse_u64_list_accepts_multiple_values() {
        assert_eq!(
            parse_u64_list("65536,1048576,16777216").unwrap(),
            Some(vec![65536, 1_048_576, 16_777_216])
        );
    }

    #[test]
    fn resolve_nic_max_bps_prefers_cli_flag() {
        std::env::set_var("NIC_MAX_BPS", "42");
        let got = resolve_nic_max_bps(Some(100_000_000_000));
        std::env::remove_var("NIC_MAX_BPS");
        assert_eq!(got, Some(100_000_000_000));
    }

    #[test]
    fn parse_ip_host_order_roundtrip() {
        assert_eq!(parse_ip_host_order("10.0.0.42").unwrap(), 0x0A00_002A);
        assert!(parse_ip_host_order("not.an.ip.addr").is_err());
    }
}
