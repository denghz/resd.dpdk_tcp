//! bench-vs-linux — pcap wire-diff comparator (mode B).
//!
//! After the 2026-05-09 bench-suite overhaul Phase 4 the bench-vs-linux
//! crate retains only **mode B**: rfc_compliance-preset pcap
//! canonicalisation + byte-diff against a peer reference capture.
//! Mode A (RTT comparison across stacks) consolidated into the new
//! `bench-rtt` crate (`tools/bench-rtt/`).
//!
//! # Process shape
//!
//! No EAL init or DPDK bring-up — mode B operates on pre-captured
//! pcaps from the DUT and the peer. Live tcpdump+SSH capture
//! orchestration is a Task 15 follow-up; for now the operator passes
//! `--local-pcap` and `--peer-pcap` directly.
//!
//! # CSV output
//!
//! One CSV row per divergence verdict (empty-diff / divergence
//! detected). `dimensions_json` carries `{preset:"rfc_compliance",
//! mode:"wire-diff"}` so bench-report can group cleanly.

use anyhow::Context;
use clap::Parser;

use bench_common::preconditions::{PreconditionMode, PreconditionValue, Preconditions};
use bench_common::run_metadata::RunMetadata;

use bench_vs_linux::mode_wire_diff;
use bench_vs_linux::Mode;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "bench-vs-linux — pcap wire-diff comparator (mode B only)"
)]
struct Args {
    /// Mode selector. Only `wire-diff` is implemented after the
    /// 2026-05-09 bench-suite overhaul; `rtt` errors with a pointer
    /// to `bench-rtt`.
    #[arg(long, default_value = "wire-diff")]
    mode: String,

    /// Output CSV path.
    #[arg(long)]
    output_csv: std::path::PathBuf,

    /// Precondition mode: `strict` aborts on precondition failure;
    /// `lenient` warns and continues.
    #[arg(long, default_value = "strict")]
    precondition_mode: String,

    /// Tool label emitted as the `tool` CSV column.
    #[arg(long, default_value = "bench-vs-linux")]
    tool: String,

    /// Feature-set label emitted as the `feature_set` CSV column.
    /// Default `rfc-compliance` matches spec §8 mode B.
    #[arg(long, default_value = "rfc-compliance")]
    feature_set: String,

    /// Mode B — path to the local (DUT) pcap. Required.
    #[arg(long, default_value = "")]
    local_pcap: String,

    /// Mode B — path to the peer pcap. Required.
    #[arg(long, default_value = "")]
    peer_pcap: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mode = parse_precondition_mode(&args.precondition_mode)?;
    let run_mode = Mode::parse(&args.mode).map_err(|e| anyhow::anyhow!(e))?;

    if matches!(run_mode, Mode::Rtt) {
        anyhow::bail!(
            "bench-vs-linux mode A (RTT comparison) was consolidated into \
             `bench-rtt` in the 2026-05-09 bench-suite overhaul. Use \
             `bench-rtt --stack {{dpdk_net|linux_kernel|fstack}}` instead."
        );
    }

    run_wire_diff_mode(&args, mode)
}

// ---------------------------------------------------------------------------
// Mode B — wire-diff dispatch. MVP: requires --local-pcap + --peer-pcap
// paths. Runs preconditions in the same strict/lenient shape as mode A
// so downstream reports can filter wire-diff rows identically.
// ---------------------------------------------------------------------------

fn run_wire_diff_mode(args: &Args, mode: PreconditionMode) -> anyhow::Result<()> {
    if args.local_pcap.is_empty() || args.peer_pcap.is_empty() {
        anyhow::bail!(
            "--mode wire-diff requires both --local-pcap and --peer-pcap. \
             T9 MVP consumes pre-captured pcaps; live tcpdump+SSH orchestration \
             is a Task 15 follow-up (see src/mode_wire_diff.rs module docs)."
        );
    }
    let preconditions = run_preconditions_check(mode)?;
    if mode == PreconditionMode::Strict && !preconditions_all_pass(&preconditions) {
        eprintln!("bench-vs-linux: precondition failure in strict mode (wire-diff):");
        for (name, value) in preconditions_as_pairs(&preconditions) {
            if !(value.is_pass() || value.is_not_applicable()) {
                eprintln!("  {name} = {value}");
            }
        }
        std::process::exit(1);
    }
    let metadata = build_run_metadata(mode, preconditions)?;
    let code = mode_wire_diff::run_mode_wire_diff_from_paths(
        std::path::PathBuf::from(&args.local_pcap),
        std::path::PathBuf::from(&args.peer_pcap),
        args.output_csv.clone(),
        &args.tool,
        &args.feature_set,
        &metadata,
    )?;
    // Exit codes per mode_wire_diff docs: 0 = empty-diff; 1 = divergence.
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CLI parse + preconditions plumbing — same shape as bench-rtt.
// ---------------------------------------------------------------------------

fn parse_precondition_mode(s: &str) -> anyhow::Result<PreconditionMode> {
    s.parse().map_err(|e: String| anyhow::anyhow!(e))
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
                        "bench-vs-linux: WARN lenient mode — check-bench-preconditions \
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

// ---------------------------------------------------------------------------
// Run metadata.
// ---------------------------------------------------------------------------

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
    fn parse_precondition_mode_accepts_strict_and_lenient() {
        assert_eq!(
            parse_precondition_mode("strict").unwrap(),
            PreconditionMode::Strict
        );
        assert_eq!(
            parse_precondition_mode("lenient").unwrap(),
            PreconditionMode::Lenient
        );
    }

    #[test]
    fn parse_precondition_mode_rejects_garbage() {
        assert!(parse_precondition_mode("loose").is_err());
    }
}
