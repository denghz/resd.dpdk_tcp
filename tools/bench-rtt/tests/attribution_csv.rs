//! TDD: T51 deferred-work item 4 — bench-rtt's CSV pipeline must
//! surface Phase 9's `unsupported_buckets` bitfield so c7i validation
//! can tell "0 ns measured" from "no data" on Hw-mode rows.
//!
//! These tests don't touch DPDK — they validate the pure-Rust CSV-row
//! construction helper exposed by the binary's library facade. The
//! integration boundary they exercise is:
//!
//!   IterRecord  →  attribution_row_cols(...)  →  RawSamplesWriter
//!
//! Live c7i validation of the end-to-end pipeline (real DPDK rx HW-TS,
//! real engine NIC-RX probe) stays deferred to Phase 12+ per the plan.

use bench_rtt::attribution::{
    AttributionMode, HwTsBuckets, IterRecord, TscFallbackBuckets,
};
use bench_rtt::attribution_csv::{
    attribution_csv_header, attribution_row_cols, ATTRIBUTION_CSV_HEADER,
};

/// Hw-mode row composed by `compose_iter_record` with both unsupported
/// flags lit must serialise:
///   - the 5 Hw-bucket columns populated;
///   - the 3 Tsc-bucket columns empty;
///   - `unsupported_mask = "3"` (binary 0b11).
#[test]
fn hw_mode_row_carries_full_unsupported_mask() {
    let rec = IterRecord {
        rtt_ns: 10_430,
        rx_hw_ts_ns: 12_345,
        mode: AttributionMode::Hw,
        hw_buckets: Some(HwTsBuckets {
            user_send_to_tx_sched_ns: 150,
            tx_sched_to_nic_tx_wire_ns: 0,
            nic_tx_wire_to_nic_rx_ns: 10_250,
            nic_rx_to_enqueued_ns: 0,
            enqueued_to_user_return_ns: 30,
            unsupported_buckets: HwTsBuckets::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE
                | HwTsBuckets::UNSUPPORTED_NIC_RX_TO_ENQUEUED,
        }),
        tsc_buckets: None,
    };

    let cols = attribution_row_cols("payload_128", 42, &rec);
    assert_eq!(cols.len(), ATTRIBUTION_CSV_HEADER.len());

    // Header field order must match the row-cols order. Index by the
    // header name so the test stays robust to column reordering.
    let by_name = |name: &str| -> &str {
        let idx = ATTRIBUTION_CSV_HEADER
            .iter()
            .position(|h| *h == name)
            .unwrap_or_else(|| panic!("header missing column {name}"));
        cols[idx].as_str()
    };

    assert_eq!(by_name("bucket_id"), "payload_128");
    assert_eq!(by_name("iter"), "42");
    assert_eq!(by_name("mode"), "Hw");
    assert_eq!(by_name("rtt_ns"), "10430");
    assert_eq!(by_name("rx_hw_ts_ns"), "12345");

    // 5 Hw-bucket columns populated.
    assert_eq!(by_name("user_send_to_tx_sched_ns"), "150");
    assert_eq!(by_name("tx_sched_to_nic_tx_wire_ns"), "0");
    assert_eq!(by_name("nic_tx_wire_to_nic_rx_ns"), "10250");
    assert_eq!(by_name("nic_rx_to_enqueued_ns"), "0");
    assert_eq!(by_name("enqueued_to_user_return_ns"), "30");

    // 3 Tsc-only columns must be empty on a Hw row.
    assert_eq!(by_name("tsc_user_send_to_tx_sched_ns"), "");
    assert_eq!(by_name("tsc_tx_sched_to_enqueued_ns"), "");
    assert_eq!(by_name("tsc_enqueued_to_user_return_ns"), "");

    // unsupported_mask: 0b11 = both flags lit.
    assert_eq!(by_name("unsupported_mask"), "3");
}

/// Tsc-mode row must carry an empty mask (no unsupported semantics
/// apply) and leave the 5 Hw-bucket columns blank.
#[test]
fn tsc_mode_row_has_zero_mask_and_blank_hw_cols() {
    let rec = IterRecord {
        rtt_ns: 10_430,
        rx_hw_ts_ns: 0,
        mode: AttributionMode::Tsc,
        hw_buckets: None,
        tsc_buckets: Some(TscFallbackBuckets {
            user_send_to_tx_sched_ns: 100,
            tx_sched_to_enqueued_ns: 10_250,
            enqueued_to_user_return_ns: 80,
        }),
    };

    let cols = attribution_row_cols("payload_64", 7, &rec);
    let by_name = |name: &str| -> &str {
        let idx = ATTRIBUTION_CSV_HEADER
            .iter()
            .position(|h| *h == name)
            .unwrap_or_else(|| panic!("header missing column {name}"));
        cols[idx].as_str()
    };

    assert_eq!(by_name("bucket_id"), "payload_64");
    assert_eq!(by_name("iter"), "7");
    assert_eq!(by_name("mode"), "Tsc");
    assert_eq!(by_name("rtt_ns"), "10430");
    assert_eq!(by_name("rx_hw_ts_ns"), "0");

    // 5 Hw-bucket columns must be empty on a Tsc row.
    assert_eq!(by_name("user_send_to_tx_sched_ns"), "");
    assert_eq!(by_name("tx_sched_to_nic_tx_wire_ns"), "");
    assert_eq!(by_name("nic_tx_wire_to_nic_rx_ns"), "");
    assert_eq!(by_name("nic_rx_to_enqueued_ns"), "");
    assert_eq!(by_name("enqueued_to_user_return_ns"), "");

    // 3 Tsc columns populated.
    assert_eq!(by_name("tsc_user_send_to_tx_sched_ns"), "100");
    assert_eq!(by_name("tsc_tx_sched_to_enqueued_ns"), "10250");
    assert_eq!(by_name("tsc_enqueued_to_user_return_ns"), "80");

    // Tsc mode has no unsupported-bucket concept by construction.
    assert_eq!(by_name("unsupported_mask"), "0");
}

/// Header must lock in the column set at the documented order.
/// Reordering breaks downstream consumers that index by position;
/// the assertion here is the contract.
#[test]
fn attribution_csv_header_is_locked() {
    let header = attribution_csv_header();
    assert_eq!(
        header,
        &[
            "bucket_id",
            "iter",
            "mode",
            "rtt_ns",
            "rx_hw_ts_ns",
            "user_send_to_tx_sched_ns",
            "tx_sched_to_nic_tx_wire_ns",
            "nic_tx_wire_to_nic_rx_ns",
            "nic_rx_to_enqueued_ns",
            "enqueued_to_user_return_ns",
            "tsc_user_send_to_tx_sched_ns",
            "tsc_tx_sched_to_enqueued_ns",
            "tsc_enqueued_to_user_return_ns",
            "unsupported_mask",
        ]
    );
}

/// End-to-end roundtrip via `RawSamplesWriter`: write a Hw row and a
/// Tsc row to a tempfile, parse the CSV back, and assert the row
/// contents survive the writer + parser layer.
#[test]
fn attribution_csv_roundtrips_through_raw_samples_writer() {
    use bench_common::raw_samples::RawSamplesWriter;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("attribution.csv");

    let header = attribution_csv_header();
    let mut writer = RawSamplesWriter::create(&path, header).expect("create writer");

    let hw = IterRecord {
        rtt_ns: 10_430,
        rx_hw_ts_ns: 12_345,
        mode: AttributionMode::Hw,
        hw_buckets: Some(HwTsBuckets {
            user_send_to_tx_sched_ns: 150,
            tx_sched_to_nic_tx_wire_ns: 0,
            nic_tx_wire_to_nic_rx_ns: 10_250,
            nic_rx_to_enqueued_ns: 0,
            enqueued_to_user_return_ns: 30,
            unsupported_buckets: HwTsBuckets::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE
                | HwTsBuckets::UNSUPPORTED_NIC_RX_TO_ENQUEUED,
        }),
        tsc_buckets: None,
    };
    let tsc = IterRecord {
        rtt_ns: 10_430,
        rx_hw_ts_ns: 0,
        mode: AttributionMode::Tsc,
        hw_buckets: None,
        tsc_buckets: Some(TscFallbackBuckets {
            user_send_to_tx_sched_ns: 100,
            tx_sched_to_enqueued_ns: 10_250,
            enqueued_to_user_return_ns: 80,
        }),
    };

    let hw_cols = attribution_row_cols("payload_128", 0, &hw);
    let hw_refs: Vec<&str> = hw_cols.iter().map(String::as_str).collect();
    writer.row(&hw_refs).expect("write hw row");

    let tsc_cols = attribution_row_cols("payload_128", 1, &tsc);
    let tsc_refs: Vec<&str> = tsc_cols.iter().map(String::as_str).collect();
    writer.row(&tsc_refs).expect("write tsc row");

    writer.flush().expect("flush");
    drop(writer);

    let body = std::fs::read_to_string(&path).expect("read csv");
    let mut lines = body.lines();
    let header_line = lines.next().expect("header line");
    assert!(header_line.contains("unsupported_mask"));
    assert!(header_line.contains("user_send_to_tx_sched_ns"));
    assert!(header_line.contains("tsc_tx_sched_to_enqueued_ns"));

    let hw_line = lines.next().expect("hw row line");
    assert!(hw_line.starts_with("payload_128,0,Hw,10430,12345,"));
    // unsupported_mask=3 is the trailing column.
    assert!(hw_line.ends_with(",3"));

    let tsc_line = lines.next().expect("tsc row line");
    assert!(tsc_line.starts_with("payload_128,1,Tsc,10430,0,"));
    // Tsc rows close with mask=0.
    assert!(tsc_line.ends_with(",0"));

    assert!(lines.next().is_none(), "expected exactly two data rows");
}

/// `--attribution-csv` arg appears in `bench-rtt --help`. Smoke-check
/// via the bin's --help so we don't need a live engine.
#[test]
fn cli_advertises_attribution_csv_flag() {
    use std::process::Command;
    let bin = env!("CARGO_BIN_EXE_bench-rtt");
    let out = Command::new(bin).args(["--help"]).output().expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--attribution-csv"),
        "--attribution-csv not in --help output:\n{stdout}"
    );
}
