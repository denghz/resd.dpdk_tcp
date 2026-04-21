//! bench-micro::summarize — reads `target/criterion/**/new/estimates.json`
//! emitted by cargo-criterion and produces a summarized CSV matching
//! `bench-common`'s `CsvRow` schema, ready for `bench-report` ingest.
//!
//! Criterion's native JSON remains the authoritative sidecar (flame-graph
//! and regression-diff consumers read it directly, per spec §13); the
//! CSV here is the unified-schema projection for the cross-tool report.
//!
//! # Usage
//!
//! ```text
//! summarize [input_root] [output_csv]
//! ```
//!
//! Defaults:
//! - input_root = `target/criterion`
//! - output_csv = `target/bench-results/bench-micro/<YYYYmmddTHHMMSSZ>.csv`
//!
//! Empty / missing input root produces a header-only CSV and "wrote 0
//! rows" on stderr — matches the spec §5.5 smoke test.

use bench_common::csv_row::{CsvRow, MetricAggregation};
use bench_common::preconditions::{PreconditionMode, PreconditionValue, Preconditions};
use bench_common::run_metadata::RunMetadata;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let input_root = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "target/criterion".into());
    let output_path = args.get(2).cloned().unwrap_or_else(|| {
        format!(
            "target/bench-results/bench-micro/{}.csv",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        )
    });

    if let Some(parent) = std::path::Path::new(&output_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let metadata = build_run_metadata()?;

    let mut wtr = csv::Writer::from_path(&output_path)?;
    let mut count = 0usize;

    // Criterion lays out its output as:
    //   target/criterion/<benchmark_id>/new/estimates.json
    //   target/criterion/<benchmark_id>/new/sample.json
    //   ... (plus base/, change/, report/, etc.)
    //
    // We walk the whole tree looking for `*/new/estimates.json`. The
    // `<benchmark_id>` is the criterion target name (e.g. `bench_poll_empty`),
    // which becomes the CSV's `test_case` column. Walking may surface the
    // root's own "report/" directory if present; ignored via the path-suffix
    // check.
    if std::path::Path::new(&input_root).exists() {
        for entry in walkdir::WalkDir::new(&input_root)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            // We want exactly `<input_root>/<target_name>/new/estimates.json`.
            if !path.is_file() || path.file_name() != Some(std::ffi::OsStr::new("estimates.json")) {
                continue;
            }
            // Parent chain must be `.../<target_name>/new/`.
            let Some(parent) = path.parent() else { continue };
            if parent.file_name() != Some(std::ffi::OsStr::new("new")) {
                continue;
            }
            let Some(target_dir) = parent.parent() else {
                continue;
            };
            let target_name = match target_dir.file_name().and_then(|s| s.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };
            // Criterion also emits top-level roll-up directories like "report"
            // without a `new/` sibling. The filter above already narrowed us
            // down to real per-target estimates — we don't need another guard.

            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            let median = json
                .get("median")
                .and_then(|v| v.get("point_estimate"))
                .and_then(|v| v.as_f64());
            let mean = json
                .get("mean")
                .and_then(|v| v.get("point_estimate"))
                .and_then(|v| v.as_f64());
            let stddev = json
                .get("std_dev")
                .and_then(|v| v.get("point_estimate"))
                .and_then(|v| v.as_f64());
            let ci_low = json
                .get("mean")
                .and_then(|v| v.get("confidence_interval"))
                .and_then(|v| v.get("lower_bound"))
                .and_then(|v| v.as_f64());
            let ci_high = json
                .get("mean")
                .and_then(|v| v.get("confidence_interval"))
                .and_then(|v| v.get("upper_bound"))
                .and_then(|v| v.as_f64());

            for (agg, value) in [
                (MetricAggregation::P50, median),
                (MetricAggregation::Mean, mean),
                (MetricAggregation::Stddev, stddev),
                (MetricAggregation::Ci95Lower, ci_low),
                (MetricAggregation::Ci95Upper, ci_high),
            ] {
                if let Some(v) = value {
                    let row = CsvRow {
                        run_metadata: metadata.clone(),
                        tool: "bench-micro".into(),
                        test_case: target_name.clone(),
                        feature_set: "default".into(),
                        dimensions_json: "{}".into(),
                        metric_name: "rtt_ns".into(),
                        metric_unit: "ns".into(),
                        metric_value: v,
                        metric_aggregation: agg,
                    };
                    wtr.serialize(&row)?;
                    count += 1;
                }
            }
        }
    }

    wtr.flush()?;

    // When the input tree held no `estimates.json` files, `wtr` never
    // serialised a record and csv never emitted the header. The spec §5.5
    // smoke test expects a header-only CSV in that case — mirror the
    // `CsvRow` Serialize impl's column order so downstream readers that
    // inspect the file still see the schema even on a zero-row run.
    if count == 0 {
        let mut wtr = csv::Writer::from_path(&output_path)?;
        wtr.write_record(CSV_HEADER)?;
        wtr.flush()?;
    }

    eprintln!("wrote {} rows to {}", count, output_path);
    Ok(())
}

/// Mirror of the private `COLUMNS` constant in `bench_common::csv_row`.
/// Kept in sync with the `Serialize` impl there; see the
/// `serialised_header_matches_columns` regression test in that crate
/// which locks drift down when paired with a bench-report round-trip.
const CSV_HEADER: &[&str] = &[
    "run_id",
    "run_started_at",
    "commit_sha",
    "branch",
    "host",
    "instance_type",
    "cpu_model",
    "dpdk_version",
    "kernel",
    "nic_model",
    "nic_fw",
    "ami_id",
    "precondition_mode",
    "precondition_isolcpus",
    "precondition_nohz_full",
    "precondition_rcu_nocbs",
    "precondition_governor",
    "precondition_cstate_max",
    "precondition_tsc_invariant",
    "precondition_coalesce_off",
    "precondition_tso_off",
    "precondition_lro_off",
    "precondition_rss_on",
    "precondition_thermal_throttle",
    "precondition_hugepages_reserved",
    "precondition_irqbalance_off",
    "precondition_wc_active",
    "tool",
    "test_case",
    "feature_set",
    "dimensions_json",
    "metric_name",
    "metric_unit",
    "metric_value",
    "metric_aggregation",
];

fn build_run_metadata() -> anyhow::Result<RunMetadata> {
    let commit_sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let kernel = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    // bench-micro's `precondition_wc_active` is `n/a` (spec §4.1 line 222):
    // wc_active reads a DPDK-side memtype list which requires EAL init.
    // All other preconditions default to `Pass(None)` for in-process benches
    // — lenient mode documents that we aren't enforcing any of them.
    let preconditions = Preconditions {
        wc_active: PreconditionValue::not_applicable(),
        ..Default::default()
    };

    Ok(RunMetadata {
        run_id: uuid::Uuid::new_v4(),
        run_started_at: chrono::Utc::now().to_rfc3339(),
        commit_sha,
        branch,
        host: hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default(),
        instance_type: std::env::var("INSTANCE_TYPE").unwrap_or_default(),
        cpu_model: std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
            })
            .unwrap_or_default(),
        dpdk_version: String::new(), // bench-micro doesn't init DPDK
        kernel,
        nic_model: String::new(),
        nic_fw: String::new(),
        ami_id: std::env::var("AMI_ID").unwrap_or_default(),
        precondition_mode: PreconditionMode::Lenient, // in-process benches; lenient
        preconditions,
    })
}
