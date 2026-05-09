//! bench-tx-maxtp — W × C sustained max-throughput grid (spec §11.2).
//!
//! Phase 5 of the 2026-05-09 bench-suite overhaul split the legacy
//! bench-vs-mtcp binary into bench-tx-burst and bench-tx-maxtp
//! (this binary). The mTCP arm was removed in Phase 2; the live
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
use bench_common::raw_samples::RawSamplesWriter;
use bench_common::run_metadata::RunMetadata;

use bench_tx_maxtp::dpdk::{self, DpdkMaxtpCfg};
use bench_tx_maxtp::maxtp;
use bench_tx_maxtp::preflight::{check_nic_saturation_bps, check_peer_window, BucketVerdict};
use bench_tx_maxtp::Stack;

use dpdk_net_core::engine::Engine;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "bench-tx-maxtp — W × C sustained-rate grid"
)]
struct Args {
    /// Peer IP (dotted-quad IPv4).
    #[arg(long)]
    peer_ip: String,

    /// Peer TCP port. Defaults vary by stack:
    /// - dpdk_net: 10001 (echo-server)
    /// - linux_kernel: 10002 (linux-tcp-sink — DISCARDS bytes; pointing
    ///   at echo-server back-pressures the kernel TCP recv buffer to
    ///   ~0 Gbps). Task 5.5 asserts peer_port=10002 for the linux arm.
    /// - fstack: 10001 (echo-server, same as dpdk_net)
    #[arg(long, default_value_t = 10_001)]
    peer_port: u16,

    /// Stack arm to drive: `dpdk_net`, `linux_kernel`, or `fstack`.
    /// One arm per invocation — dpdk_net and fstack cannot share a
    /// process (both call `rte_eal_init`).
    #[arg(long)]
    stack: String,

    /// Output CSV path.
    #[arg(long)]
    output_csv: std::path::PathBuf,

    /// Optional sidecar CSV for per-conn raw maxtp samples (Phase 5
    /// Task 5.3). When set, the dpdk_net arm emits one row per conn
    /// per SAMPLE_INTERVAL during the measurement window with columns
    /// `bucket_id, conn_id, sample_idx, t_ns, goodput_bps_window,
    /// snd_nxt_minus_una`. Only the dpdk arm currently emits raw
    /// samples — linux + fstack arms ignore this flag (they have no
    /// equivalent per-conn snd_una hook).
    #[arg(long)]
    raw_samples_csv: Option<std::path::PathBuf>,

    /// Optional sidecar CSV for per-segment send→ACK latency samples
    /// (Phase 6 Task 6.2). When set:
    /// - dpdk_net arm: emits one row per TCP segment for every cumulative
    ///   ACK across the measurement window with columns
    ///   `bucket_id, conn_id, begin_seq, end_seq, latency_ns`. Closes C-B4.
    /// - linux_kernel arm: emits coarse `getsockopt(TCP_INFO)` snapshots
    ///   per SAMPLE_INTERVAL with columns
    ///   `bucket_id, conn_id, sample_idx, t_ns, tcpi_rtt_us,
    ///   tcpi_total_retrans, tcpi_unacked, scope`.
    /// - fstack arm: emits a single `unsupported` marker row per bucket
    ///   so the schema stays uniform across stacks.
    #[arg(long)]
    send_ack_csv: Option<std::path::PathBuf>,

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

    /// MSS in bytes. Spec §11.2 locks at 1460.
    #[arg(long, default_value_t = 1460)]
    mss: u16,

    /// Tool label emitted as the `tool` CSV column.
    #[arg(long, default_value = "bench-tx-maxtp")]
    tool: String,

    /// Feature-set label emitted as the `feature_set` CSV column.
    #[arg(long, default_value = "trading-latency")]
    feature_set: String,

    /// Maxtp grid subset — comma-separated W values in bytes.
    /// Empty = run all 7 W values.
    #[arg(long, default_value = "")]
    write_sizes: String,

    /// Maxtp grid subset — comma-separated C values (connection counts).
    /// Empty = run all 4 C values.
    #[arg(long, default_value = "")]
    conn_counts: String,

    /// Warmup duration in seconds. Spec §11.2 locks at 10.
    #[arg(long, default_value_t = maxtp::WARMUP_SECS)]
    warmup_secs: u64,

    /// Measurement duration in seconds. Spec §11.2 locks at 60.
    #[arg(long, default_value_t = maxtp::DURATION_SECS)]
    duration_secs: u64,

    /// NIC line-rate cap (bits-per-second) for the post-run
    /// NIC-saturation check. Defaults to the `NIC_MAX_BPS` env var if
    /// set; otherwise the check is skipped with a warning.
    #[arg(long)]
    nic_max_bps: Option<u64>,

    /// Peer SSH target for pre-run peer rcv_space introspection
    /// (`ss -ti | rcv_space`). When unset, the pre-run guard degrades
    /// to a placebo (peer_rwnd := bucket W).
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

    let preconditions = run_preconditions_check(mode)?;
    if mode == PreconditionMode::Strict && !preconditions_all_pass(&preconditions) {
        eprintln!("bench-tx-maxtp: precondition failure in strict mode:");
        for (name, value) in preconditions_as_pairs(&preconditions) {
            if !(value.is_pass() || value.is_not_applicable()) {
                eprintln!("  {name} = {value}");
            }
        }
        std::process::exit(1);
    }

    let needs_dpdk = stack == Stack::DpdkNet;
    let needs_fstack = stack == Stack::Fstack;
    let mut _eal_guard: Option<EalGuard> = None;
    let mut engine: Option<Engine> = None;
    if needs_dpdk {
        validate_dpdk_args(&args)?;
        eal_init(&args)?;
        _eal_guard = Some(EalGuard);
        engine = Some(build_engine(&args)?);
        let tsc_hz = unsafe { dpdk_net_sys::rte_get_tsc_hz() };
        if tsc_hz == 0 {
            anyhow::bail!("rte_get_tsc_hz() returned 0 — EAL not initialised?");
        }
    }

    #[cfg(feature = "fstack")]
    if needs_fstack {
        validate_fstack_args(&args)?;
        init_fstack(&args)?;
        let tsc_hz = unsafe { dpdk_net_sys::rte_get_tsc_hz() };
        if tsc_hz == 0 {
            anyhow::bail!("rte_get_tsc_hz() returned 0 after ff_init");
        }
    }
    #[cfg(not(feature = "fstack"))]
    let _ = needs_fstack;

    let metadata = build_run_metadata(mode, preconditions)?;
    let mut writer = csv::Writer::from_path(&args.output_csv)
        .with_context(|| format!("creating output CSV {:?}", args.output_csv))?;

    // Phase 5 Task 5.3: optional sidecar raw-sample CSV. Header order is
    // co-located with `dpdk::emit_per_conn_raw_sample`'s row layout.
    let mut raw_samples_writer: Option<RawSamplesWriter> = match args.raw_samples_csv.as_ref() {
        Some(path) => {
            let header = [
                "bucket_id",
                "conn_id",
                "sample_idx",
                "t_ns",
                "goodput_bps_window",
                "snd_nxt_minus_una",
            ];
            Some(
                RawSamplesWriter::create(path, &header)
                    .with_context(|| format!("creating raw-samples CSV {path:?}"))?,
            )
        }
        None => None,
    };

    // Phase 6 Task 6.2: optional sidecar send→ACK latency CSV.
    //
    // Header is a union over the per-stack row shapes:
    //   * `scope` distinguishes per-segment (`dpdk_segment`), TCP_INFO
    //     snapshot (`linux_tcp_info`), and stub (`fstack_unsupported`).
    //   * `t_ns`/`sample_idx` populated by linux + fstack scopes;
    //     dpdk_segment leaves them empty.
    //   * `begin_seq`/`end_seq`/`latency_ns` populated by dpdk_segment;
    //     others leave them empty.
    //   * `tcpi_rtt_us`/`tcpi_total_retrans`/`tcpi_unacked` populated
    //     by linux_tcp_info; others leave empty.
    // Empty values are emitted as the empty string so downstream pivots
    // can split rows by `scope`.
    let mut send_ack_writer: Option<RawSamplesWriter> = match args.send_ack_csv.as_ref() {
        Some(path) => {
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
            Some(
                RawSamplesWriter::create(path, &header)
                    .with_context(|| format!("creating send-ack CSV {path:?}"))?,
            )
        }
        None => None,
    };

    let peer_ip = parse_ip_host_order(&args.peer_ip)?;
    let nic_max_bps = resolve_nic_max_bps(args.nic_max_bps);
    if nic_max_bps.is_none() {
        eprintln!(
            "bench-tx-maxtp: WARN --nic-max-bps unset and NIC_MAX_BPS env var \
             unset; skipping post-run NIC-saturation check."
        );
    }

    let w_filter = parse_u64_list(&args.write_sizes)?;
    let c_filter = parse_u64_list(&args.conn_counts)?;
    let grid = maxtp::enumerate_filtered_grid(w_filter.as_deref(), c_filter.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;

    match stack {
        Stack::DpdkNet => {
            let engine = engine.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "DpdkNet stack selected but engine not provided — main.rs invariant violated"
                )
            })?;
            run_maxtp_grid_dpdk(
                engine,
                peer_ip,
                args.peer_port,
                &grid,
                &args,
                &metadata,
                nic_max_bps,
                &mut writer,
                raw_samples_writer.as_mut(),
                send_ack_writer.as_mut(),
            )?;
        }
        Stack::LinuxKernel => {
            run_maxtp_grid_linux(
                peer_ip,
                args.peer_port,
                &grid,
                &args,
                &metadata,
                nic_max_bps,
                &mut writer,
                send_ack_writer.as_mut(),
            )?;
        }
        Stack::Fstack => {
            run_maxtp_grid_fstack(
                peer_ip,
                args.peer_port,
                &grid,
                &args,
                &metadata,
                nic_max_bps,
                &mut writer,
                send_ack_writer.as_mut(),
            )?;
        }
    }

    writer.flush()?;
    if let Some(rsw) = raw_samples_writer.as_mut() {
        rsw.flush().context("flushing raw-samples CSV")?;
    }
    if let Some(saw) = send_ack_writer.as_mut() {
        saw.flush().context("flushing send-ack CSV")?;
    }
    Ok(())
}

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
            "bench-tx-maxtp: WARN --peer-ssh unset; peer_rwnd pre-run check \
             degraded to placebo ({placebo_rwnd} B)"
        );
        return placebo_rwnd;
    };
    match bench_tx_maxtp::peer_introspect::fetch_peer_rwnd_bytes(ssh, dut_ip, peer_port) {
        Ok(v) => v as u64,
        Err(e) => {
            eprintln!(
                "bench-tx-maxtp: WARN peer_rwnd introspection via `ss -ti` \
                 on {ssh}:{peer_port} failed: {e}; falling back to placebo \
                 ({placebo_rwnd} B)"
            );
            placebo_rwnd
        }
    }
}

// ---------------------------------------------------------------------------
// dpdk_net maxtp grid driver (spec §11.2).
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_maxtp_grid_dpdk<W: std::io::Write>(
    engine: &Engine,
    peer_ip: u32,
    peer_port: u16,
    grid: &[maxtp::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
    mut raw_samples_writer: Option<&mut RawSamplesWriter>,
    mut send_ack_writer: Option<&mut RawSamplesWriter>,
) -> anyhow::Result<()> {
    // Phase 6 Task 6.2: opt the engine into per-segment send→ACK
    // latency tracking. Cap of 4096 — comfortably above the in-flight
    // burst depth between cumulative-ACK arrivals at line-rate
    // (a 1 Gbit/s flow with MSS=1460 absorbs ~85 in-flight segments
    // per 1 ms RTT). When `--send-ack-csv` is unset, the toggle is left
    // off — no per-conn allocation, hot-path branch stays predictable.
    if send_ack_writer.is_some() {
        engine.enable_send_ack_logging(4096);
    }
    let mut payload_cache: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();

    let dut_ip: std::net::Ipv4Addr = args
        .local_ip
        .parse()
        .with_context(|| format!("parsing --local-ip `{}` for peer rwnd probe", args.local_ip))?;

    let raw_samples_path_str: Option<String> = args
        .raw_samples_csv
        .as_ref()
        .map(|p| p.display().to_string());

    for bucket in grid {
        eprintln!("bench-tx-maxtp: running dpdk_net bucket {}", bucket.label());

        let payload = payload_cache
            .entry(bucket.write_bytes)
            .or_insert_with(|| vec![0u8; bucket.write_bytes as usize]);

        let peer_rwnd = resolve_peer_rwnd_bytes(
            args.peer_ssh.as_deref(),
            dut_ip,
            peer_port,
            bucket.write_bytes,
        );
        let rwnd_verdict = check_peer_window(peer_rwnd, bucket.write_bytes);
        let tx_ts_mode = dpdk::TxTsMode::TscFallback;

        if !rwnd_verdict.is_ok() {
            eprintln!(
                "bench-tx-maxtp: dpdk_net bucket {} invalidated pre-run: {}",
                bucket.label(),
                rwnd_verdict.reason().unwrap_or("<unknown>")
            );
            let agg = maxtp::BucketAggregate::from_sample(
                *bucket,
                Stack::DpdkNet,
                None,
                rwnd_verdict,
                Some(tx_ts_mode),
            );
            maxtp::emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
                .context("emit invalidated dpdk_net maxtp bucket row")?;
            continue;
        }

        eprintln!(
            "POOL pre-open bucket(C={},W={}): tx_data_avail={}",
            bucket.conn_count, bucket.write_bytes,
            engine.tx_data_mempool_avail(),
        );
        let conns = match dpdk::open_persistent_connections(
            engine,
            peer_ip,
            peer_port,
            bucket.conn_count,
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "bench-tx-maxtp: dpdk maxtp bucket {} open_persistent_connections failed: {e:#}",
                    bucket.label()
                );
                continue;
            }
        };

        let bucket_id = format!(
            "W={}B,C={}",
            bucket.write_bytes, bucket.conn_count
        );
        let mut cfg = DpdkMaxtpCfg {
            engine,
            conns: &conns,
            bucket: *bucket,
            warmup: std::time::Duration::from_secs(args.warmup_secs),
            duration: std::time::Duration::from_secs(args.duration_secs),
            payload,
            tx_ts_mode,
            bucket_id: &bucket_id,
            raw_samples: raw_samples_writer.as_deref_mut(),
            send_ack_samples: send_ack_writer.as_deref_mut(),
        };
        let bucket_outcome = (|| -> anyhow::Result<()> {
            let run = match dpdk::run_bucket(&mut cfg).with_context(|| {
                format!("dpdk::run_bucket for {}", bucket.label())
            }) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!(
                        "bench-tx-maxtp: dpdk maxtp bucket {} failed: {e:#}",
                        bucket.label()
                    );
                    return Ok(());
                }
            };

            // Sanity invariant: ACKed bytes during window == tx_payload_bytes
            // delta, minus in-flight bound. Skipped if obs-byte-counters is off.
            if run.tx_payload_bytes_delta > 0 {
                if let Err(e) = maxtp::check_sanity_invariant(
                    run.acked_bytes_in_window,
                    run.tx_payload_bytes_delta,
                    run.inflight_bytes_at_end,
                ) {
                    eprintln!(
                        "bench-tx-maxtp: maxtp sanity invariant violated for bucket {}: {e}",
                        bucket.label()
                    );
                    anyhow::bail!(e);
                }
            } else {
                eprintln!(
                    "bench-tx-maxtp: maxtp sanity invariant check skipped for bucket {} \
                     (tx_payload_bytes counter is 0 — build with \
                     `--features obs-byte-counters` to enable)",
                    bucket.label()
                );
            }

            let mut agg = maxtp::BucketAggregate::from_sample(
                *bucket,
                Stack::DpdkNet,
                Some(run.sample),
                BucketVerdict::Ok,
                Some(tx_ts_mode),
            );
            if let Some(max_bps) = nic_max_bps {
                let sat_verdict =
                    check_nic_saturation_bps(run.sample.goodput_bps as u64, max_bps);
                if !sat_verdict.is_ok() {
                    eprintln!(
                        "bench-tx-maxtp: maxtp bucket {} NIC-bound post-run: {}",
                        bucket.label(),
                        sat_verdict.reason().unwrap_or("<unknown>")
                    );
                    agg.override_verdict(sat_verdict);
                }
            }
            // Phase 5 Task 5.3: fold the per-conn-per-interval goodput
            // samples across every conn (and every interval) for the
            // bucket-level percentile aggregate. Each MaxtpRawPoint
            // carries the goodput over its just-closed
            // SAMPLE_INTERVAL window — the aggregate distribution is
            // the natural per-bucket "percentile of per-second
            // goodput" view.
            let raw_window_samples: Vec<f64> = run
                .raw_points
                .iter()
                .map(|p| p.goodput_bps_window)
                .collect();
            maxtp::emit_bucket_rows_with_percentiles(
                writer,
                metadata,
                &args.tool,
                &args.feature_set,
                &agg,
                &raw_window_samples,
                raw_samples_path_str.as_deref(),
            )
            .context("emit maxtp bucket rows")
        })();

        if let Err(e) = dpdk::close_persistent_connections(engine, &conns) {
            eprintln!(
                "bench-tx-maxtp: dpdk maxtp bucket {} close_persistent_connections failed: {e:#}; \
                 continuing to next bucket",
                bucket.label()
            );
        }

        bucket_outcome?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// linux_kernel maxtp grid driver.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_maxtp_grid_linux<W: std::io::Write>(
    peer_ip: u32,
    peer_port: u16,
    grid: &[maxtp::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
    mut send_ack_writer: Option<&mut RawSamplesWriter>,
) -> anyhow::Result<()> {
    use bench_tx_maxtp::linux::{self, LinuxMaxtpCfg};

    // Phase 5 Task 5.5: pin the peer-port contract before any bucket
    // opens connections. T50 reported the linux maxtp pass was
    // accidentally pointed at echo-server (port 10001) instead of
    // linux-tcp-sink (port 10002), back-pressuring goodput to ~0 Gbps.
    // The assertion makes the operator-visible contract explicit so
    // misconfiguration fails fast at start-of-bench rather than
    // surfacing as bogus low-throughput rows in the CSV.
    linux::assert_peer_is_sink(peer_port)?;

    for bucket in grid {
        eprintln!(
            "bench-tx-maxtp: running linux_kernel maxtp bucket {}",
            bucket.label()
        );

        let dut_ip: std::net::Ipv4Addr = args.local_ip.parse().with_context(|| {
            format!("parsing --local-ip `{}` for peer rwnd probe", args.local_ip)
        })?;
        let peer_rwnd = resolve_peer_rwnd_bytes(
            args.peer_ssh.as_deref(),
            dut_ip,
            peer_port,
            bucket.write_bytes,
        );
        let rwnd_verdict = check_peer_window(peer_rwnd, bucket.write_bytes);
        let tx_ts_mode_str = "n/a";

        if !rwnd_verdict.is_ok() {
            eprintln!(
                "bench-tx-maxtp: linux_kernel maxtp bucket {} invalidated pre-run: {}",
                bucket.label(),
                rwnd_verdict.reason().unwrap_or("<unknown>")
            );
            let agg = maxtp::BucketAggregate::from_sample(
                *bucket,
                Stack::LinuxKernel,
                None,
                rwnd_verdict,
                None,
            );
            emit_linux_bucket_rows(
                writer,
                metadata,
                &args.tool,
                &args.feature_set,
                &agg,
                tx_ts_mode_str,
            )
            .context("emit invalidated linux_kernel maxtp bucket row")?;
            continue;
        }

        let mut conns = match linux::open_persistent_connections(
            peer_ip,
            peer_port,
            bucket.conn_count,
        )
        .with_context(|| {
            format!(
                "linux_kernel open_persistent_connections (C={})",
                bucket.conn_count
            )
        }) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "bench-tx-maxtp: linux_kernel maxtp bucket {} open_persistent_connections failed: {e:#}",
                    bucket.label()
                );
                continue;
            }
        };

        let cfg = LinuxMaxtpCfg {
            bucket: *bucket,
            warmup: std::time::Duration::from_secs(args.warmup_secs),
            duration: std::time::Duration::from_secs(args.duration_secs),
            peer_ip_host_order: peer_ip,
            peer_port,
            payload: vec![0u8; bucket.write_bytes as usize],
        };
        let bucket_id = format!(
            "W={}B,C={}",
            bucket.write_bytes, bucket.conn_count
        );
        let run = match linux::run_bucket(
            &cfg,
            &mut conns,
            send_ack_writer.as_deref_mut(),
            &bucket_id,
        )
        .with_context(|| format!("linux::run_bucket for {}", bucket.label())) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "bench-tx-maxtp: linux_kernel maxtp bucket {} failed: {e:#}",
                    bucket.label()
                );
                continue;
            }
        };

        let mut agg = maxtp::BucketAggregate::from_sample(
            *bucket,
            Stack::LinuxKernel,
            Some(run.sample),
            BucketVerdict::Ok,
            None,
        );
        if let Some(max_bps) = nic_max_bps {
            let sat_verdict =
                check_nic_saturation_bps(run.sample.goodput_bps as u64, max_bps);
            if !sat_verdict.is_ok() {
                eprintln!(
                    "bench-tx-maxtp: linux_kernel maxtp bucket {} NIC-bound post-run: {}",
                    bucket.label(),
                    sat_verdict.reason().unwrap_or("<unknown>")
                );
                agg.override_verdict(sat_verdict);
            }
        }
        emit_linux_bucket_rows(
            writer,
            metadata,
            &args.tool,
            &args.feature_set,
            &agg,
            tx_ts_mode_str,
        )
        .context("emit linux_kernel maxtp bucket rows")?;

        drop(conns);
    }
    Ok(())
}

/// Emit linux maxtp bucket rows with `tx_ts_mode = "n/a"` overlaid on
/// the standard maxtp dimensions JSON. The pure dpdk path uses
/// `maxtp::emit_bucket_rows` which only writes `tx_ts_mode` when the
/// aggregate carries one (None on linux); we want the field present
/// with the explicit "n/a" string so CSV consumers can filter linux
/// rows symmetrically with dpdk_net rows.
fn emit_linux_bucket_rows<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    metadata: &RunMetadata,
    tool: &str,
    feature_set: &str,
    aggregate: &maxtp::BucketAggregate,
    tx_ts_mode: &str,
) -> anyhow::Result<()> {
    use bench_common::csv_row::{CsvRow, MetricAggregation};

    let mut dims_value = serde_json::json!({
        "workload": "maxtp",
        "W_bytes": aggregate.bucket.write_bytes as i64,
        "C": aggregate.bucket.conn_count as i64,
        "stack": aggregate.stack.as_dimension(),
        "tx_ts_mode": tx_ts_mode,
    });
    if let Some(reason) = aggregate.verdict.reason() {
        if let Some(m) = dims_value.as_object_mut() {
            m.insert(
                "bucket_invalid".to_string(),
                serde_json::Value::String(reason.to_string()),
            );
        }
    }
    let dims = dims_value.to_string();

    match &aggregate.sample {
        Some(sample) => {
            let row = CsvRow {
                run_metadata: metadata.clone(),
                tool: tool.to_string(),
                test_case: "maxtp".to_string(),
                feature_set: feature_set.to_string(),
                dimensions_json: dims.clone(),
                metric_name: "sustained_goodput_bps".to_string(),
                metric_unit: "bits_per_sec".to_string(),
                metric_value: sample.goodput_bps,
                metric_aggregation: MetricAggregation::Mean,
                cpu_family: None,
                cpu_model_name: None,
                dpdk_version_pkgconfig: None,
                worktree_branch: None,
                uprof_session_id: None,
                raw_samples_path: None,
                failed_iter_count: 0,
            };
            writer.serialize(&row)?;

            let row = CsvRow {
                run_metadata: metadata.clone(),
                tool: tool.to_string(),
                test_case: "maxtp".to_string(),
                feature_set: feature_set.to_string(),
                dimensions_json: dims,
                metric_name: "tx_pps".to_string(),
                metric_unit: "pps".to_string(),
                metric_value: sample.pps,
                metric_aggregation: MetricAggregation::Mean,
                cpu_family: None,
                cpu_model_name: None,
                dpdk_version_pkgconfig: None,
                worktree_branch: None,
                uprof_session_id: None,
                raw_samples_path: None,
                failed_iter_count: 0,
            };
            writer.serialize(&row)?;
        }
        None => {
            let row = CsvRow {
                run_metadata: metadata.clone(),
                tool: tool.to_string(),
                test_case: "maxtp".to_string(),
                feature_set: feature_set.to_string(),
                dimensions_json: dims,
                metric_name: "sustained_goodput_bps".to_string(),
                metric_unit: "bits_per_sec".to_string(),
                metric_value: 0.0,
                metric_aggregation: MetricAggregation::Mean,
                cpu_family: None,
                cpu_model_name: None,
                dpdk_version_pkgconfig: None,
                worktree_branch: None,
                uprof_session_id: None,
                raw_samples_path: None,
                failed_iter_count: 0,
            };
            writer.serialize(&row)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// fstack maxtp grid driver (feature-gated).
// ---------------------------------------------------------------------------

#[cfg(feature = "fstack")]
#[allow(clippy::too_many_arguments)]
fn run_maxtp_grid_fstack<W: std::io::Write>(
    peer_ip: u32,
    peer_port: u16,
    grid: &[maxtp::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
    mut send_ack_writer: Option<&mut RawSamplesWriter>,
) -> anyhow::Result<()> {
    use bench_tx_maxtp::fstack;

    let tx_ts_mode = dpdk::TxTsMode::TscFallback;
    let warmup = std::time::Duration::from_secs(args.warmup_secs);
    let duration = std::time::Duration::from_secs(args.duration_secs);

    let grid_results = fstack::run_maxtp_grid(
        grid, warmup, duration, peer_ip, peer_port, tx_ts_mode,
    );

    for gr in grid_results {
        let bucket = gr.bucket;
        // Phase 6 Task 6.2: emit a single `fstack_unsupported` marker row
        // per bucket so the CSV schema stays uniform with dpdk + linux
        // arms. FreeBSD TCP_INFO is reachable via ff_getsockopt but the
        // surface is wide enough that we defer per-segment / per-snapshot
        // emission to a future phase.
        if let Some(saw) = send_ack_writer.as_deref_mut() {
            let bucket_id = format!(
                "W={}B,C={}",
                bucket.write_bytes, bucket.conn_count
            );
            bench_tx_maxtp::emit_fstack_unsupported_marker(saw, &bucket_id)
                .context("emit fstack_unsupported marker row")?;
        }
        match gr.result {
            Err(e) => {
                eprintln!(
                    "bench-tx-maxtp: fstack maxtp bucket {} failed: {e}",
                    bucket.label()
                );
                let agg = maxtp::BucketAggregate::from_sample(
                    bucket,
                    Stack::Fstack,
                    None,
                    BucketVerdict::Invalid(e),
                    Some(tx_ts_mode),
                );
                maxtp::emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
                    .context("emit fstack maxtp bucket rows")?;
            }
            Ok(run) => {
                let mut agg = maxtp::BucketAggregate::from_sample(
                    bucket,
                    Stack::Fstack,
                    Some(run.sample),
                    BucketVerdict::Ok,
                    Some(tx_ts_mode),
                );
                if let Some(max_bps) = nic_max_bps {
                    let sat = check_nic_saturation_bps(run.sample.goodput_bps as u64, max_bps);
                    if !sat.is_ok() {
                        eprintln!(
                            "bench-tx-maxtp: fstack maxtp bucket {} NIC-bound post-run: {}",
                            bucket.label(),
                            sat.reason().unwrap_or("<unknown>")
                        );
                        agg.override_verdict(sat);
                    }
                }
                maxtp::emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
                    .context("emit fstack maxtp bucket rows")?;
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "fstack"))]
#[allow(clippy::too_many_arguments)]
fn run_maxtp_grid_fstack<W: std::io::Write>(
    _peer_ip: u32,
    _peer_port: u16,
    grid: &[maxtp::Bucket],
    args: &Args,
    metadata: &RunMetadata,
    _nic_max_bps: Option<u64>,
    writer: &mut csv::Writer<W>,
    mut send_ack_writer: Option<&mut RawSamplesWriter>,
) -> anyhow::Result<()> {
    eprintln!(
        "bench-tx-maxtp: WARN --stack fstack selected but binary built without `fstack` \
         feature; emitting marker rows. Rebuild with `--features fstack` on the AMI \
         where libfstack.a is installed."
    );
    for bucket in grid {
        // Phase 6 Task 6.2: even on the stub path emit the
        // `fstack_unsupported` marker row when --send-ack-csv was set,
        // for schema uniformity.
        if let Some(saw) = send_ack_writer.as_deref_mut() {
            let bucket_id = format!(
                "W={}B,C={}",
                bucket.write_bytes, bucket.conn_count
            );
            bench_tx_maxtp::emit_fstack_unsupported_marker(saw, &bucket_id)
                .context("emit fstack_unsupported marker row (stub)")?;
        }
        let agg = maxtp::BucketAggregate::from_sample(
            *bucket,
            Stack::Fstack,
            None,
            BucketVerdict::Invalid(
                "fstack feature not compiled in (libfstack.a not available)".to_string(),
            ),
            None,
        );
        maxtp::emit_bucket_rows(writer, metadata, &args.tool, &args.feature_set, &agg)
            .context("emit fstack-stub maxtp marker row")?;
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
        "bench-tx-maxtp".to_string(),
        format!("--conf={}", args.fstack_conf),
        "--proc-id=0".to_string(),
    ];
    eprintln!("bench-tx-maxtp: ff_init argv={:?}", argv);
    bench_tx_maxtp::fstack_ffi::ff_init_from_args(&argv)
        .map_err(|e| anyhow::anyhow!("ff_init failed: {e}"))
}

// ---------------------------------------------------------------------------
// CLI parse + DPDK bring-up + preconditions (mirrors bench-tx-burst).
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
    let mut eal_argv: Vec<String> = vec!["bench-tx-maxtp".to_string()];
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
        cc_mode: 0,
        max_connections: 512,
        tx_data_mempool_size: 32_768,
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
                        "bench-tx-maxtp: WARN lenient mode — check-bench-preconditions \
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
    }

    #[test]
    fn parse_u64_list_accepts_multiple_values() {
        assert_eq!(
            parse_u64_list("64,256,1024").unwrap(),
            Some(vec![64, 256, 1024])
        );
    }
}
