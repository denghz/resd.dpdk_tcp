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
