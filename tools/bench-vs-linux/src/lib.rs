//! bench-vs-linux — library façade for the binary.
//!
//! After the 2026-05-09 bench-suite overhaul Phase 4 the bench-vs-linux
//! crate retains only **mode B (wire-diff, rfc_compliance preset)**.
//! Mode A (RTT comparison) was consolidated into the new `bench-rtt`
//! crate (`tools/bench-rtt/`) so the dpdk_net + linux_kernel + fstack
//! triplet share a single binary.
//!
//! The lib-façade exists so integration tests in `tests/` can validate
//! the pcap-canonicalise / byte-diff engine without going through the
//! binary entry. The binary consumes the same modules via
//! `use bench_vs_linux::*`.

pub mod mode_wire_diff;
// A10 Plan B Task 9: pcap divergence-normalisation for mode B.
pub mod normalize;

/// Mode selector. Pre-Phase-4 the binary supported `rtt` (mode A) and
/// `wire-diff` (mode B); mode A migrated to `bench-rtt` so this is now
/// effectively `wire-diff`-only. The `Mode` enum is retained for CLI
/// stability — operators can still invoke with `--mode wire-diff` and
/// the wire-diff path is the only valid value (mode A errors out via a
/// pointer to bench-rtt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Phase-4 stub: returns an error pointing at bench-rtt.
    Rtt,
    /// Wire-format divergence diff against pre-captured pcaps.
    WireDiff,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "rtt" => Ok(Mode::Rtt),
            "wire-diff" | "wire_diff" => Ok(Mode::WireDiff),
            other => Err(format!("unknown mode `{other}` (valid: rtt, wire-diff)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_accepts_both_forms() {
        assert_eq!(Mode::parse("rtt").unwrap(), Mode::Rtt);
        assert_eq!(Mode::parse("wire-diff").unwrap(), Mode::WireDiff);
        assert_eq!(Mode::parse("wire_diff").unwrap(), Mode::WireDiff);
    }

    #[test]
    fn mode_parse_rejects_garbage() {
        assert!(Mode::parse("loopback").is_err());
    }
}
