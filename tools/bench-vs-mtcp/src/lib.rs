//! bench-vs-mtcp — library façade for the binary.
//!
//! A10 Plan B Task 12 (spec §11.1, parent spec §11.5.1). Task 13
//! added the `maxtp` sub-workload (spec §11.2) on the same binary.
//! The 2026-05-03 follow-up landed a `Linux` stack arm so the maxtp
//! grid can compare dpdk_net against the kernel TCP stack directly
//! (mTCP comparator stays deferred while the AMI rebuild is blocked).
//!
//! # Stacks
//!
//! - `dpdk_net` — our stack, driven via `dpdk_net_core::Engine`. Full
//!   implementation.
//! - `mtcp` — MIT mTCP stack. Stubbed as [`mtcp::Error::Unimplemented`]
//!   in this landing because the mTCP install lives in the AMI that
//!   Plan A's sister plan bakes, and that AMI does not exist yet. The
//!   stub mirrors T8's AF_PACKET shape: `MtcpConfig` + `validate_config`
//!   so the CLI fails fast on bad args. The CSV `dimensions_json.stack
//!   = "mtcp"` is reserved so downstream (bench-report) can handle rows
//!   emitted by the real implementation without schema drift.
//! - `linux` — Linux kernel TCP, driven via `std::net::TcpStream`.
//!   Currently only wired into the `maxtp` workload (the `burst`
//!   workload stays dpdk-only for now). Re-uses
//!   `tools/bench-vs-linux/peer/linux-tcp-sink` on the peer side.
//!
//! # Sub-workloads
//!
//! - `burst` (T12) — K × G = 20 buckets (spec §11.1). Linux arm not
//!   wired (out of scope for the 2026-05-03 follow-up).
//! - `maxtp` (T13) — W × C = 28 buckets (spec §11.2). Linux arm wired.

pub mod burst;
pub mod dpdk_burst;
pub mod dpdk_maxtp;
pub mod linux_maxtp;
pub mod maxtp;
pub mod mtcp;
pub mod peer_introspect;
pub mod preflight;

/// Stack identifier for CSV `dimensions_json` + runner dispatch.
///
/// Spec §11.3 reserves three values: `dpdk_net` (our stack), `mtcp`
/// (MIT mTCP stack, stub while the AMI rebuild is blocked), and
/// `linux` (kernel TCP, wired for the maxtp workload as of the
/// 2026-05-03 follow-up). The enum serialises to the exact
/// snake_case string emitted into `dimensions_json.stack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    DpdkNet,
    Mtcp,
    Linux,
}

impl Stack {
    /// CSV `dimensions_json.stack` string form. Stable; bench-report
    /// groups rows by this exact value.
    pub const fn as_dimension(self) -> &'static str {
        match self {
            Stack::DpdkNet => "dpdk_net",
            Stack::Mtcp => "mtcp",
            Stack::Linux => "linux",
        }
    }

    /// Parse a single token from the `--stacks` CSV arg. Unknown
    /// tokens error; the outer parser in `main.rs` aggregates errors.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "dpdk" | "dpdk_net" => Ok(Stack::DpdkNet),
            "mtcp" => Ok(Stack::Mtcp),
            "linux" | "linux_kernel" => Ok(Stack::Linux),
            other => Err(format!(
                "unknown stack `{other}` (valid: dpdk, mtcp, linux)"
            )),
        }
    }
}

/// Workload selector — `burst` (T12) or `maxtp` (T13). The T12 binary
/// only accepts `burst`; `maxtp` parses but `run_workload` bails with
/// a pointer to the T13 follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    Burst,
    Maxtp,
}

impl Workload {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "burst" => Ok(Workload::Burst),
            "maxtp" => Ok(Workload::Maxtp),
            other => Err(format!("unknown workload `{other}` (valid: burst, maxtp)")),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Workload::Burst => "burst",
            Workload::Maxtp => "maxtp",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_parse_accepts_both_aliases() {
        assert_eq!(Stack::parse("dpdk").unwrap(), Stack::DpdkNet);
        assert_eq!(Stack::parse("dpdk_net").unwrap(), Stack::DpdkNet);
        assert_eq!(Stack::parse("mtcp").unwrap(), Stack::Mtcp);
        assert_eq!(Stack::parse("linux").unwrap(), Stack::Linux);
        assert_eq!(Stack::parse("linux_kernel").unwrap(), Stack::Linux);
    }

    #[test]
    fn stack_parse_rejects_unknown() {
        let err = Stack::parse("afpacket").unwrap_err();
        assert!(err.contains("unknown stack"));
    }

    #[test]
    fn stack_as_dimension_is_stable() {
        assert_eq!(Stack::DpdkNet.as_dimension(), "dpdk_net");
        assert_eq!(Stack::Mtcp.as_dimension(), "mtcp");
        assert_eq!(Stack::Linux.as_dimension(), "linux");
    }

    #[test]
    fn workload_parse_round_trip() {
        assert_eq!(Workload::parse("burst").unwrap(), Workload::Burst);
        assert_eq!(Workload::parse("maxtp").unwrap(), Workload::Maxtp);
        assert_eq!(Workload::Burst.as_str(), "burst");
        assert_eq!(Workload::Maxtp.as_str(), "maxtp");
    }

    #[test]
    fn workload_parse_rejects_unknown() {
        assert!(Workload::parse("ab").is_err());
    }
}
