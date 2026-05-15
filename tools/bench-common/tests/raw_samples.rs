use bench_common::raw_samples::RawSamplesWriter;
use std::io::Read;

#[test]
fn writes_header_and_one_sample_per_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("raw.csv");
    let mut w = RawSamplesWriter::create(&path, &["bucket_id", "iter", "rtt_ns"])
        .expect("create");
    w.row(&["b1", "0", "1234"]).expect("row 0");
    w.row(&["b1", "1", "5678"]).expect("row 1");
    w.flush().expect("flush");
    drop(w);

    let mut got = String::new();
    std::fs::File::open(&path).unwrap().read_to_string(&mut got).unwrap();
    assert_eq!(got, "bucket_id,iter,rtt_ns\nb1,0,1234\nb1,1,5678\n");
}

#[test]
fn rejects_row_with_wrong_column_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.csv");
    let mut w = RawSamplesWriter::create(&path, &["a", "b"]).unwrap();
    let err = w.row(&["only_one"]).unwrap_err();
    assert!(err.to_string().contains("column count"));
}

#[test]
fn quotes_values_containing_commas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.csv");
    let mut w = RawSamplesWriter::create(&path, &["bucket_id", "rtt_ns"]).unwrap();
    w.row(&["K=262144B,G=10ms", "1234"]).unwrap();
    w.flush().unwrap();
    drop(w);

    // Re-read with csv::Reader to confirm the comma-bearing bucket_id
    // round-trips as a single field, not two.
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(&path)
        .unwrap();
    let headers = rdr.headers().unwrap().clone();
    assert_eq!(&headers[0], "bucket_id");
    assert_eq!(&headers[1], "rtt_ns");

    let row = rdr.records().next().unwrap().unwrap();
    assert_eq!(&row[0], "K=262144B,G=10ms");
    assert_eq!(&row[1], "1234");
}

#[test]
fn quotes_values_containing_newlines_and_doublequotes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.csv");
    let mut w = RawSamplesWriter::create(&path, &["a", "b"]).unwrap();
    w.row(&["line1\nline2", r#"has "quotes""#]).unwrap();
    w.flush().unwrap();
    drop(w);

    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(&path).unwrap();
    let row = rdr.records().next().unwrap().unwrap();
    assert_eq!(&row[0], "line1\nline2");
    assert_eq!(&row[1], r#"has "quotes""#);
}
