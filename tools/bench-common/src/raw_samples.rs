//! Streaming raw-sample CSV writer.
//!
//! Every bench tool that produces percentile distributions also emits a
//! sidecar CSV containing one row per measurement so post-hoc analysis
//! (additional percentiles, histograms, bimodality detection) does not
//! require re-running the bench. Writers are streaming — they flush
//! per-row to bound peak memory at iteration counts up to 10^7.
//!
//! Cell values may contain commas, newlines, or double quotes — the
//! `csv` crate handles RFC 4180 quoting so callers do not have to
//! sanitise their inputs.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::path::Path;

pub struct RawSamplesWriter {
    inner: csv::Writer<File>,
    expected_cols: usize,
}

impl RawSamplesWriter {
    pub fn create(path: &Path, header: &[&str]) -> Result<Self> {
        let f = File::create(path)
            .with_context(|| format!("create {}", path.display()))?;
        let mut inner = csv::WriterBuilder::new()
            .buffer_capacity(64 * 1024)
            .from_writer(f);
        inner.write_record(header).context("write header")?;
        Ok(Self { inner, expected_cols: header.len() })
    }

    pub fn row(&mut self, cols: &[&str]) -> Result<()> {
        if cols.len() != self.expected_cols {
            return Err(anyhow!(
                "raw_samples row column count {} != header {}",
                cols.len(),
                self.expected_cols,
            ));
        }
        self.inner.write_record(cols).context("write row")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }
}
