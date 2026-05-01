//! Spec §6: Markdown report + per-failed-scenario JSON failure bundle.
//!
//! The Markdown report is the operator-facing summary; the JSON bundle
//! is the forensic per-scenario detail (counter snapshots + last-N
//! events + the failing-assertion list).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Serialize;

use crate::counters_snapshot::Snapshot;
use crate::observation::{EventRecord, Verdict};
use crate::workload::ScenarioResult;

/// Header info for the report. `report.rs` doesn't reach into
/// `EngineConfig` directly — `main.rs` builds this struct from
/// `engine.config()` so the report code stays pure.
#[derive(Debug, Clone, Serialize)]
pub struct ReportHeader {
    pub run_id: String,
    pub commit_sha: String,
    pub branch: String,
    pub host: String,
    pub nic_model: String,
    pub dpdk_version: String,
    pub preset: &'static str,
    pub tcp_max_retrans_count: u32,
    pub hw_offload_rx_cksum: bool,
    pub fault_injector: bool,
    pub fi_spec: Option<String>,
}

/// Write the Markdown report to `path`. If `force` is false and the
/// path exists, returns an error (spec §7 `--force` semantics).
pub fn write_markdown_report(
    path: &Path,
    header: &ReportHeader,
    results: &[ScenarioResult],
    force: bool,
) -> io::Result<()> {
    if path.exists() && !force {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "report path {} already exists; pass --force to overwrite",
                path.display()
            ),
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = File::create(path)?;
    write_header(&mut f, header, results)?;
    write_scenarios_table(&mut f, results)?;
    write_failure_detail(&mut f, results)?;
    f.flush()
}

fn write_header<W: Write>(
    f: &mut W,
    h: &ReportHeader,
    results: &[ScenarioResult],
) -> io::Result<()> {
    let pass = results.iter().filter(|r| matches!(r.verdict, Verdict::Pass)).count();
    let total = results.len();
    let verdict = if pass == total { "PASS" } else { "FAIL" };
    let date = chrono::Utc::now().format("%Y-%m-%d");
    writeln!(f, "# Layer H Correctness Report — {date}\n")?;
    writeln!(f, "**Run ID:** {}", h.run_id)?;
    writeln!(f, "**Commit:** {}", h.commit_sha)?;
    writeln!(f, "**Branch:** {}", h.branch)?;
    writeln!(
        f,
        "**Host / NIC / DPDK:** {} / {} / {}",
        h.host, h.nic_model, h.dpdk_version
    )?;
    writeln!(f, "**Preset:** {}", h.preset)?;
    writeln!(f, "**Active config knobs:**")?;
    writeln!(f, "- tcp_max_retrans_count = {}", h.tcp_max_retrans_count)?;
    writeln!(
        f,
        "- hw-offload-rx-cksum = {}",
        if h.hw_offload_rx_cksum { "on" } else { "off" }
    )?;
    writeln!(
        f,
        "- fault-injector = {}",
        if h.fault_injector { "on" } else { "off" }
    )?;
    if let Some(fi) = &h.fi_spec {
        writeln!(f, "- DPDK_NET_FAULT_INJECTOR = {fi}")?;
    }
    writeln!(f, "\n**Verdict:** {verdict} ({pass}/{total} scenarios)\n")?;
    Ok(())
}

fn write_scenarios_table<W: Write>(f: &mut W, results: &[ScenarioResult]) -> io::Result<()> {
    writeln!(f, "## Per-scenario results\n")?;
    writeln!(f, "| # | Scenario | Duration | Verdict | Notes |")?;
    writeln!(f, "|---|----------|----------|---------|-------|")?;
    for (i, r) in results.iter().enumerate() {
        let dur_secs = r.duration_observed.as_secs_f64();
        let (verdict, notes) = match &r.verdict {
            Verdict::Pass => ("PASS".to_string(), "—".to_string()),
            Verdict::Fail { failures } => {
                ("FAIL".to_string(), failures_one_liner(failures))
            }
        };
        writeln!(
            f,
            "| {} | {} | {dur_secs:.1} s | {verdict} | {notes} |",
            i + 1,
            r.scenario_name,
        )?;
    }
    writeln!(f)?;
    Ok(())
}

fn write_failure_detail<W: Write>(f: &mut W, results: &[ScenarioResult]) -> io::Result<()> {
    let any_fail = results.iter().any(|r| matches!(r.verdict, Verdict::Fail { .. }));
    if !any_fail {
        return Ok(());
    }
    writeln!(f, "## Failure detail\n")?;
    for r in results {
        if let Verdict::Fail { failures } = &r.verdict {
            writeln!(f, "### Scenario: {} (FAIL)\n", r.scenario_name)?;
            for fr in failures {
                writeln!(f, "- {}", failure_md_line(fr))?;
            }
            writeln!(
                f,
                "- Bundle: `target/layer-h-bundles/<run-id>/{}.json`\n",
                r.scenario_name
            )?;
        }
    }
    Ok(())
}

fn failures_one_liner(failures: &[crate::observation::FailureReason]) -> String {
    let n = failures.len();
    if n == 0 {
        return "no detail".into();
    }
    let head = failure_md_line(&failures[0]);
    if n == 1 {
        head
    } else {
        format!("{head}; +{} more", n - 1)
    }
}

fn failure_md_line(fr: &crate::observation::FailureReason) -> String {
    use crate::observation::FailureReason as F;
    match fr {
        F::ConnectFailed { error } => format!("**ConnectFailed**: {error}"),
        F::FsmDeparted { observed } => {
            format!("**FsmDeparted**: state_of returned {observed:?}")
        }
        F::IllegalTransition { from, to, at_event_idx } => format!(
            "**IllegalTransition**: {from:?} → {to:?} at event idx {at_event_idx}"
        ),
        F::CounterRelation { counter, relation, observed_delta, .. } => format!(
            "**CounterRelation** — `{counter}` observed delta={observed_delta}, expected `{relation}`"
        ),
        F::DisjunctiveCounterRelation { counters, relation, observed_deltas, .. } => format!(
            "**DisjunctiveCounterRelation** — `{counters:?}` deltas={observed_deltas:?}, expected at least one `{relation}`"
        ),
        F::LiveCounterBelowMin { counter, observed, min } => format!(
            "**LiveCounterBelowMin** — `{counter}` observed={observed} below min={min}"
        ),
        F::EventsDropped { count } => format!("**EventsDropped**: count={count}"),
        F::WorkloadError { error } => format!("**WorkloadError**: {error}"),
    }
}

/// JSON failure-bundle structure (spec §6.2).
#[derive(Debug, Serialize)]
pub struct FailureBundle<'a> {
    pub scenario: &'a str,
    pub netem: Option<&'a str>,
    pub fault_injector: Option<&'a str>,
    pub duration_secs: f64,
    pub verdict: &'static str, // "fail"
    pub snapshot_pre: &'a Snapshot,
    pub snapshot_post: &'a Snapshot,
    pub failures: &'a [crate::observation::FailureReason],
    pub event_window: Vec<EventRecord>,
    pub event_window_truncated: bool,
}

/// Write the per-failed-scenario JSON bundle. Idempotent — overwrites
/// any existing file at `path`.
#[allow(clippy::too_many_arguments)]
pub fn write_failure_bundle(
    path: &Path,
    scenario_name: &str,
    netem: Option<&str>,
    fault_injector: Option<&str>,
    duration_secs: f64,
    snapshot_pre: &Snapshot,
    snapshot_post: &Snapshot,
    failures: &[crate::observation::FailureReason],
    event_window: Vec<EventRecord>,
    event_window_truncated: bool,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating bundle dir {}", parent.display()))?;
    }
    let bundle = FailureBundle {
        scenario: scenario_name,
        netem,
        fault_injector,
        duration_secs,
        verdict: "fail",
        snapshot_pre,
        snapshot_post,
        failures,
        event_window,
        event_window_truncated,
    };
    let json = serde_json::to_string_pretty(&bundle)
        .context("serialising failure bundle to JSON")?;
    fs::write(path, json)
        .with_context(|| format!("writing bundle to {}", path.display()))?;
    Ok(())
}

/// Build the per-scenario bundle path under `bundle_dir`.
pub fn bundle_path(bundle_dir: &Path, scenario_name: &str) -> PathBuf {
    bundle_dir.join(format!("{scenario_name}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counters_snapshot::Snapshot;
    use crate::observation::{EventKind, EventRecord, FailureReason, TcpStateName, Verdict};
    use crate::workload::ScenarioResult;
    use tempfile::tempdir;

    fn synth_header() -> ReportHeader {
        ReportHeader {
            run_id: "11111111-2222-3333-4444-555555555555".into(),
            commit_sha: "abcdef0".into(),
            branch: "phase-a10.5".into(),
            host: "test-host".into(),
            nic_model: "ena".into(),
            dpdk_version: "23.11".into(),
            preset: "trading-latency",
            tcp_max_retrans_count: 15,
            hw_offload_rx_cksum: true,
            fault_injector: true,
            fi_spec: None,
        }
    }

    fn synth_event() -> EventRecord {
        EventRecord {
            ord: 0,
            kind: EventKind::StateChange,
            conn_idx: 0,
            emitted_ts_ns: 1234,
            from: Some(TcpStateName::Established),
            to: Some(TcpStateName::Established),
            err: None,
            seq: None,
        }
    }

    fn pass_result(name: &'static str) -> ScenarioResult {
        ScenarioResult {
            scenario_name: name,
            duration_observed: std::time::Duration::from_secs(30),
            snapshot_pre: Snapshot::new(),
            snapshot_post: Snapshot::new(),
            verdict: Verdict::Pass,
            event_ring: crate::observation::EventRing::new(),
        }
    }

    fn fail_result(name: &'static str, reasons: Vec<FailureReason>) -> ScenarioResult {
        ScenarioResult {
            scenario_name: name,
            duration_observed: std::time::Duration::from_secs(30),
            snapshot_pre: Snapshot::new(),
            snapshot_post: Snapshot::new(),
            verdict: Verdict::Fail { failures: reasons },
            event_ring: crate::observation::EventRing::new(),
        }
    }

    #[test]
    fn markdown_report_writes_pass_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("report.md");
        let h = synth_header();
        let results = vec![pass_result("delay_20ms"), pass_result("loss_1pct")];
        write_markdown_report(&path, &h, &results, false).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("**Verdict:** PASS (2/2 scenarios)"));
        assert!(body.contains("delay_20ms"));
        assert!(body.contains("loss_1pct"));
        // No failure detail section when all pass.
        assert!(!body.contains("## Failure detail"));
    }

    #[test]
    fn markdown_report_includes_failure_detail_section() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("report.md");
        let h = synth_header();
        let results = vec![
            pass_result("delay_20ms"),
            fail_result(
                "loss_1pct",
                vec![FailureReason::counter_relation(
                    "tcp.tx_retrans",
                    crate::assertions::Relation::LessOrEqualThan(50_000),
                    51_234,
                )],
            ),
        ];
        write_markdown_report(&path, &h, &results, false).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("**Verdict:** FAIL (1/2 scenarios)"));
        assert!(body.contains("## Failure detail"));
        assert!(body.contains("Scenario: loss_1pct"));
        assert!(body.contains("CounterRelation"));
        assert!(body.contains("tcp.tx_retrans"));
        assert!(body.contains("51234"));
    }

    #[test]
    fn markdown_report_refuses_to_clobber_without_force() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "existing content").unwrap();
        let h = synth_header();
        let err =
            write_markdown_report(&path, &h, &[], false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // existing content untouched.
        assert_eq!(fs::read_to_string(&path).unwrap(), "existing content");
    }

    #[test]
    fn markdown_report_clobbers_with_force() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "existing content").unwrap();
        let h = synth_header();
        write_markdown_report(&path, &h, &[], true).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(!body.contains("existing content"));
        assert!(body.contains("# Layer H Correctness Report"));
    }

    #[test]
    fn failure_bundle_round_trips_through_serde() {
        let dir = tempdir().unwrap();
        let path = bundle_path(dir.path(), "loss_1pct");
        let mut pre = Snapshot::new();
        pre.insert("tcp.tx_retrans".into(), 0);
        let mut post = Snapshot::new();
        post.insert("tcp.tx_retrans".into(), 51_234);
        let failures = vec![FailureReason::counter_relation(
            "tcp.tx_retrans",
            crate::assertions::Relation::LessOrEqualThan(50_000),
            51_234,
        )];
        let event_window = vec![synth_event()];
        write_failure_bundle(
            &path,
            "loss_1pct",
            Some("loss 1%"),
            None,
            30.0,
            &pre,
            &post,
            &failures,
            event_window,
            false,
        )
        .unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["scenario"], "loss_1pct");
        assert_eq!(parsed["netem"], "loss 1%");
        assert_eq!(parsed["verdict"], "fail");
        assert_eq!(parsed["snapshot_post"]["tcp.tx_retrans"], 51_234);
        assert_eq!(parsed["failures"][0]["kind"], "CounterRelation");
        assert_eq!(parsed["event_window"][0]["kind"], "StateChange");
        assert_eq!(parsed["event_window_truncated"], false);
    }
}
