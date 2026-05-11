//! Per-conn raw-sample emission test (Phase 5 Task 5.3 + Phase 11 Task 11.2).
//!
//! Phase 5 Task 5.3 of the 2026-05-09 bench-suite overhaul changes
//! the maxtp dpdk arm from a single (W, C) Mean sample to a per-
//! sample-interval per-connection time series — every SAMPLE_INTERVAL
//! tick during the measurement window, the pump emits a
//! `MaxtpRawPoint { conn_id, sample_idx, t_ns, goodput_bps_window,
//! snd_nxt_minus_una, snd_wnd, room_in_peer_wnd }` row to a sidecar
//! `RawSamplesWriter` while still rolling up an aggregate goodput
//! percentile summary at end of window.
//!
//! Phase 11 Task 11.2 (C-E1) appends `snd_wnd` and `room_in_peer_wnd`
//! to the row shape (and CSV header), retaining the leading six column
//! positions so existing downstream consumers indexing by position keep
//! working.
//!
//! The full run_bucket integration test would require a live engine +
//! peer pair, which this unit-test layer can't spin up. Instead we
//! exercise the pure-Rust raw-sample emit path directly: feed the
//! emitter synthetic samples that simulate a 5-second window at C=4
//! conns with SAMPLE_INTERVAL=1s ticks (= 4 conns × 5 intervals =
//! 20 raw rows), verify the resulting CSV has the expected shape +
//! row count.

use bench_common::raw_samples::RawSamplesWriter;
use bench_tx_maxtp::dpdk::{emit_per_conn_raw_sample, MaxtpRawPoint};
use std::path::PathBuf;

/// Helper that wraps the row column shape. Phase 11 Task 11.2 (C-E1):
/// trailing `snd_wnd` + `room_in_peer_wnd` columns appended.
fn raw_samples_header() -> [&'static str; 8] {
    [
        "bucket_id",
        "conn_id",
        "sample_idx",
        "t_ns",
        "goodput_bps_window",
        "snd_nxt_minus_una",
        "snd_wnd",
        "room_in_peer_wnd",
    ]
}

#[test]
fn maxtp_emits_one_raw_row_per_sample_interval_per_conn() {
    // Simulate a 5 s window at C=4, SAMPLE_INTERVAL=1 s. Expect 4 conns
    // × 5 intervals = 20 raw rows in the sidecar CSV.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path: PathBuf = tmp.path().to_path_buf();
    let header = raw_samples_header();
    let mut writer =
        RawSamplesWriter::create(&path, &header).expect("create raw-samples writer");

    let bucket_id = "W=4096B,C=4";
    let conns = 4;
    let intervals = 5;
    for sample_idx in 0..intervals {
        for conn_id in 0..conns {
            // Synthetic sample — 1 GiB/s per conn at 1 s intervals so
            // goodput_bps_window = 8 Gbps; snd_nxt_minus_una grows as
            // a rough function of conn_id × sample_idx so each row is
            // distinguishable in the resulting CSV. Phase 11: snd_wnd
            // is fixed at 65 535 (the default no-WSCALE 16-bit max);
            // room_in_peer_wnd shrinks as inflight grows so the
            // partition `snd_nxt_minus_una + room_in_peer_wnd ==
            // snd_wnd` holds across rows (peer-bottleneck regime).
            let inflight = (conn_id as u32 + 1) * sample_idx as u32 * 1024;
            let snd_wnd = 65_535u32;
            let room = snd_wnd.saturating_sub(inflight);
            let point = MaxtpRawPoint {
                conn_id: conn_id as u32,
                sample_idx,
                t_ns: (sample_idx as u64) * 1_000_000_000,
                goodput_bps_window: 8.0e9,
                snd_nxt_minus_una: inflight,
                snd_wnd,
                room_in_peer_wnd: room,
            };
            emit_per_conn_raw_sample(&mut writer, bucket_id, &point)
                .expect("emit raw sample");
        }
    }
    writer.flush().expect("flush raw-samples CSV");

    // Re-open the CSV and verify the schema + row count.
    let mut reader = csv::Reader::from_path(&path).expect("open raw-samples CSV");
    let headers = reader.headers().expect("read CSV headers").clone();
    let cols: Vec<&str> = headers.iter().collect();
    assert_eq!(
        cols,
        vec![
            "bucket_id",
            "conn_id",
            "sample_idx",
            "t_ns",
            "goodput_bps_window",
            "snd_nxt_minus_una",
            "snd_wnd",
            "room_in_peer_wnd"
        ]
    );

    let row_count = reader.records().count();
    assert_eq!(
        row_count,
        (conns * intervals) as usize,
        "expected {}*{} = {} raw rows, got {}",
        conns,
        intervals,
        conns * intervals,
        row_count
    );
}

#[test]
fn maxtp_raw_sample_columns_are_string_serialised_in_order() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path: PathBuf = tmp.path().to_path_buf();
    let header = raw_samples_header();
    let mut writer = RawSamplesWriter::create(&path, &header).unwrap();

    let point = MaxtpRawPoint {
        conn_id: 7,
        sample_idx: 3,
        t_ns: 12_345_678,
        goodput_bps_window: 9_500_000_000.5,
        snd_nxt_minus_una: 65_536,
        snd_wnd: 1_048_576,
        room_in_peer_wnd: 983_040,
    };
    emit_per_conn_raw_sample(&mut writer, "W=64KiB,C=16", &point).unwrap();
    writer.flush().unwrap();

    let mut reader = csv::Reader::from_path(&path).unwrap();
    let row = reader
        .records()
        .next()
        .expect("at least one row")
        .expect("decode row");
    let cells: Vec<&str> = row.iter().collect();
    assert_eq!(cells.len(), 8, "expected 8 columns, got {}", cells.len());
    assert_eq!(cells[0], "W=64KiB,C=16");
    assert_eq!(cells[1], "7");
    assert_eq!(cells[2], "3");
    assert_eq!(cells[3], "12345678");
    // Floats are formatted with full precision.
    assert!(cells[4].starts_with("9500000000"), "got {}", cells[4]);
    assert_eq!(cells[5], "65536");
    // Phase 11 Task 11.2 (C-E1): trailing queue-depth columns.
    assert_eq!(cells[6], "1048576");
    assert_eq!(cells[7], "983040");
}

/// Phase 11 Task 11.2 (C-E1) acceptance: every emitted row carries the
/// queue-depth column trio (`snd_nxt_minus_una`, `snd_wnd`,
/// `room_in_peer_wnd`) and the engine's invariant `room <= snd_wnd`
/// passes through to the CSV unchanged.
#[test]
fn queue_depth_columns_present_and_room_bounded_by_snd_wnd() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path: PathBuf = tmp.path().to_path_buf();
    let header = raw_samples_header();
    let mut writer = RawSamplesWriter::create(&path, &header).unwrap();

    // Three rows spanning the regimes the bench cares about:
    //   row 0: peer wide-open (snd_wnd >> inflight) — no peer pressure.
    //   row 1: peer half-throttled (room == snd_wnd / 2) — partial pressure.
    //   row 2: zero-window (snd_wnd == 0) — peer is slow.
    let scenarios = [
        ("wide_open", 65_535u32, 4_096u32),
        ("half", 32_768u32, 16_384u32),
        ("zero", 0u32, 0u32),
    ];
    for (idx, (label, snd_wnd, inflight)) in scenarios.iter().enumerate() {
        let _ = label; // labels keep the row purpose readable; not in CSV.
        let room = snd_wnd.saturating_sub(*inflight);
        let point = MaxtpRawPoint {
            conn_id: idx as u32,
            sample_idx: 1,
            t_ns: idx as u64 * 1_000_000_000,
            goodput_bps_window: 1.0e9,
            snd_nxt_minus_una: *inflight,
            snd_wnd: *snd_wnd,
            room_in_peer_wnd: room,
        };
        emit_per_conn_raw_sample(&mut writer, "P11=test", &point).unwrap();
    }
    writer.flush().unwrap();

    let mut reader = csv::Reader::from_path(&path).unwrap();
    let mut rows: Vec<csv::StringRecord> = Vec::new();
    for r in reader.records() {
        rows.push(r.unwrap());
    }
    assert_eq!(rows.len(), scenarios.len());
    for (i, row) in rows.iter().enumerate() {
        // 8 columns: bucket_id + 7 data fields.
        assert_eq!(row.len(), 8, "row {i} should have 8 columns, got {}", row.len());
        let snd_wnd: u32 = row[6].parse().unwrap();
        let room: u32 = row[7].parse().unwrap();
        assert!(
            room <= snd_wnd,
            "row {i}: room ({room}) must be <= snd_wnd ({snd_wnd})"
        );
    }
}
