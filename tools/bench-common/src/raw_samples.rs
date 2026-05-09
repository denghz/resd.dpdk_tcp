//! Streaming raw-sample CSV writer.
//!
//! Every bench tool that produces percentile distributions also emits a
//! sidecar CSV containing one row per measurement so post-hoc analysis
//! (additional percentiles, histograms, bimodality detection) does not
//! require re-running the bench. Writers are streaming — they flush
//! per-row to bound peak memory at iteration counts up to 10^7.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct RawSamplesWriter {
    inner: BufWriter<File>,
    expected_cols: usize,
}

impl RawSamplesWriter {
    pub fn create(path: &Path, header: &[&str]) -> Result<Self> {
        let f = File::create(path)
            .with_context(|| format!("create {}", path.display()))?;
        let mut inner = BufWriter::new(f);
        inner.write_all(header.join(",").as_bytes())?;
        inner.write_all(b"\n")?;
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
        self.inner.write_all(cols.join(",").as_bytes())?;
        self.inner.write_all(b"\n")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }
}
