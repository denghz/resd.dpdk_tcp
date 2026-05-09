//! bench-tx-maxtp — library façade for the W × C maxtp grid binary.
//!
//! Phase 5 of the 2026-05-09 bench-suite overhaul split the legacy
//! bench-vs-mtcp tool into two focused crates:
//!
//! - `bench-tx-burst` — one-shot K-byte burst grid (K × G, spec §11.1).
//! - `bench-tx-maxtp` (this crate) — sustained-rate W × C grid
//!   (spec §11.2).
//!
//! The mTCP arm was removed in Phase 2; the live comparator triplet
//! is `dpdk_net` + `linux_kernel` + `fstack`.
//!
//! # Stacks
//!
//! - `dpdk_net` — driven via `dpdk_net_core::Engine` ([`dpdk`]).
//! - `linux_kernel` — kernel TCP via `std::net::TcpStream` ([`linux`]).
//!   Phase 5 Task 5.5 asserts the linux arm targets port 10002
//!   (linux-tcp-sink) so the recv path doesn't back-pressure the sender.
//! - `fstack` — F-Stack on DPDK ([`fstack`], gated behind the `fstack`
//!   feature).

pub mod dpdk;
#[cfg(feature = "fstack")]
pub mod fstack;
pub mod linux;
pub mod maxtp;

/// Phase 6 follow-up: shared helper that writes one `fstack_unsupported`
/// marker row into the unified 11-column send-ack CSV. Lives at the
/// crate root (always compiled) so both the live `#[cfg(feature =
/// "fstack")]` arm and the stub `#[cfg(not(feature = "fstack"))]` arm in
/// `main.rs` call the same emit, and so the unit test runs unconditionally.
///
/// FreeBSD `TCP_INFO` is reachable via `ff_getsockopt`, but the surface
/// is wide enough that per-segment / per-snapshot emission is deferred —
/// this single marker row per bucket keeps the CSV schema uniform across
/// the dpdk + linux + fstack arms.
pub fn emit_fstack_unsupported_marker(
    writer: &mut bench_common::raw_samples::RawSamplesWriter,
    bucket_id: &str,
) -> anyhow::Result<()> {
    writer.row(&[
        bucket_id,
        "0",
        "fstack_unsupported",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ])?;
    Ok(())
}

// Phase 5 Task 5.4 of the 2026-05-09 bench-suite overhaul lifted the
// `fstack_ffi` module into the shared `bench-fstack-ffi` crate. Re-
// export under the legacy path so the F-Stack pump's
// `crate::fstack_ffi::...` imports keep working without churn.
#[cfg(feature = "fstack")]
pub use bench_fstack_ffi as fstack_ffi;

/// Stack identifier for CSV `dimensions_json` + runner dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    DpdkNet,
    LinuxKernel,
    Fstack,
}

impl Stack {
    /// CSV `dimensions_json.stack` string form. Stable; bench-report
    /// groups rows by this exact value.
    pub const fn as_dimension(self) -> &'static str {
        match self {
            Stack::DpdkNet => "dpdk_net",
            Stack::LinuxKernel => "linux_kernel",
            Stack::Fstack => "fstack",
        }
    }

    /// Parse a single token from CLI input. Accepts both kebab-case
    /// and snake_case forms for the operator's convenience.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "dpdk" | "dpdk_net" => Ok(Stack::DpdkNet),
            "linux" | "linux_kernel" => Ok(Stack::LinuxKernel),
            "fstack" | "f-stack" | "f_stack" => Ok(Stack::Fstack),
            other => Err(format!(
                "unknown stack `{other}` (valid: dpdk_net, linux_kernel, fstack)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bench_common::raw_samples::RawSamplesWriter;

    /// Unified 11-column header, mirrored from `main.rs`. Kept as a
    /// const inside the test module so tests stay self-contained — the
    /// production code defines it inline at the writer-creation site.
    const SEND_ACK_HEADER: &[&str] = &[
        "bucket_id",
        "conn_id",
        "scope",
        "sample_idx",
        "t_ns",
        "begin_seq",
        "end_seq",
        "latency_ns",
        "tcpi_rtt_us",
        "tcpi_total_retrans",
        "tcpi_unacked",
    ];

    #[test]
    fn stack_parse_accepts_aliases() {
        assert_eq!(Stack::parse("dpdk").unwrap(), Stack::DpdkNet);
        assert_eq!(Stack::parse("dpdk_net").unwrap(), Stack::DpdkNet);
        assert_eq!(Stack::parse("linux").unwrap(), Stack::LinuxKernel);
        assert_eq!(Stack::parse("linux_kernel").unwrap(), Stack::LinuxKernel);
        assert_eq!(Stack::parse("fstack").unwrap(), Stack::Fstack);
    }

    #[test]
    fn stack_as_dimension_is_stable() {
        assert_eq!(Stack::DpdkNet.as_dimension(), "dpdk_net");
        assert_eq!(Stack::LinuxKernel.as_dimension(), "linux_kernel");
        assert_eq!(Stack::Fstack.as_dimension(), "fstack");
    }

    /// Phase 6 follow-up: `emit_fstack_unsupported_marker` writes
    /// exactly one row per call into the unified 11-column send-ack CSV,
    /// with `scope = "fstack_unsupported"` and the bucket's `bucket_id`
    /// column populated. All other columns are blank — the marker only
    /// signals "this stack does not produce per-segment latency rows."
    /// Bucket ids embed commas (e.g. `W=1024B,C=4`); the csv crate
    /// RFC-4180-quotes them, so we parse back through a `csv::Reader`
    /// rather than naively splitting on `,`.
    #[test]
    fn unsupported_marker_writes_one_row_per_bucket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("send-ack.csv");
        {
            let mut writer = RawSamplesWriter::create(&path, SEND_ACK_HEADER).expect("create");
            emit_fstack_unsupported_marker(&mut writer, "W=1024B,C=4")
                .expect("emit row 1");
            emit_fstack_unsupported_marker(&mut writer, "W=4096B,C=8")
                .expect("emit row 2");
            writer.flush().expect("flush");
        }
        // Round-trip through the csv crate so quoting on the bucket_id
        // (which contains a comma) is handled correctly.
        let mut rdr = csv::Reader::from_path(&path).expect("open csv");
        let header: Vec<String> = rdr
            .headers()
            .expect("headers")
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(header.len(), 11);
        let rows: Vec<csv::StringRecord> = rdr
            .records()
            .collect::<Result<_, _>>()
            .expect("collect");
        // Two rows — one per bucket.
        assert_eq!(rows.len(), 2, "expected 2 rows, got {}", rows.len());
        // Row 1: bucket id, conn_id 0, scope marker, all other cols blank.
        let row1 = &rows[0];
        assert_eq!(row1.len(), 11);
        assert_eq!(row1.get(0), Some("W=1024B,C=4"));
        assert_eq!(row1.get(1), Some("0"));
        assert_eq!(row1.get(2), Some("fstack_unsupported"));
        for i in 3..11 {
            assert_eq!(
                row1.get(i),
                Some(""),
                "row 1 col {i} must be blank, got {:?}",
                row1.get(i)
            );
        }
        // Row 2: second bucket id.
        let row2 = &rows[1];
        assert_eq!(row2.get(0), Some("W=4096B,C=8"));
        assert_eq!(row2.get(2), Some("fstack_unsupported"));
    }
}
