//! Round-trip: write CsvRow → read back → assert equal.

use bench_common::csv_row::{CsvRow, MetricAggregation, PreconditionValue};
use bench_common::run_metadata::RunMetadata;

fn sample_row() -> CsvRow {
    CsvRow {
        run_metadata: RunMetadata {
            run_id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            run_started_at: "2026-04-22T03:14:07Z".to_string(),
            commit_sha: "7f70ea50000000000000000000000000000000ab".to_string(),
            branch: "phase-a10".to_string(),
            host: "ip-10-0-0-42".to_string(),
            instance_type: "c6a.2xlarge".to_string(),
            cpu_model: "AMD EPYC 7R13".to_string(),
            dpdk_version: "23.11.2".to_string(),
            kernel: "6.17.0-1009-generic".to_string(),
            nic_model: "Elastic Network Adapter (ENA)".to_string(),
            nic_fw: String::new(),
            ami_id: "ami-0123456789abcdef0".to_string(),
            precondition_mode: bench_common::preconditions::PreconditionMode::Strict,
            preconditions: Default::default(),
        },
        tool: "bench-vs-mtcp".into(),
        test_case: "burst".into(),
        feature_set: "default".into(),
        dimensions_json: r#"{"K_bytes":262144,"G_ms":10,"stack":"dpdk_net"}"#.into(),
        metric_name: "throughput_per_burst_bps".into(),
        metric_unit: "bytes_per_sec".into(),
        metric_value: 8.7e9,
        metric_aggregation: MetricAggregation::P99,
    }
}

#[test]
fn csv_row_round_trip_one_row() {
    let row = sample_row();
    let mut buf = Vec::new();
    {
        let mut wtr = csv::Writer::from_writer(&mut buf);
        row.write_with_header(&mut wtr).unwrap();
    }
    let mut rdr = csv::Reader::from_reader(&buf[..]);
    let parsed: CsvRow = rdr.deserialize().next().unwrap().unwrap();
    assert_eq!(parsed, row);
}

#[test]
fn metric_aggregation_serde() {
    let values = [
        "p50",
        "p99",
        "p999",
        "mean",
        "stddev",
        "ci95_lower",
        "ci95_upper",
    ];
    for v in values {
        let enumv: MetricAggregation = serde_json::from_str(&format!("\"{}\"", v)).unwrap();
        let back: String = serde_json::to_string(&enumv).unwrap();
        assert_eq!(back, format!("\"{}\"", v));
    }
}

#[test]
fn precondition_value_parses_pass_and_fail() {
    let a: PreconditionValue = "pass=2-7".parse().unwrap();
    assert_eq!(a.passed, true);
    assert_eq!(a.value, "2-7");
    let b: PreconditionValue = "fail=C6".parse().unwrap();
    assert_eq!(b.passed, false);
    assert_eq!(b.value, "C6");
    let c: PreconditionValue = "pass".parse().unwrap();
    assert_eq!(c.passed, true);
    assert_eq!(c.value, "");
}
