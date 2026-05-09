//! bench-vs-mtcp — library façade for the binary.
//!
//! A10 Plan B Task 12 (spec §11.1, parent spec §11.5.1). Task 13
//! added the `maxtp` sub-workload (spec §11.2) on the same binary.
//! The 2026-05-03 follow-up landed a `Linux` stack arm so the maxtp
//! grid can compare dpdk_net against the kernel TCP stack directly.
//! The 2026-05-04 follow-up landed F-Stack as a third comparator.
//!
//! The mTCP comparator arm was removed in the 2026-05-09 bench-suite
//! overhaul — the upstream mTCP project is dormant, the AMI driver
//! never had a working workload pump (always returned ENOSYS), and
//! maintaining the DPDK 20.11 sidecar alongside our DPDK 23.11 build
//! was disproportionately expensive for a permanently-stub arm.
//!
//! # Stacks
//!
//! - `dpdk_net` — our stack, driven via `dpdk_net_core::Engine`. Full
//!   implementation.
//! - `linux` — Linux kernel TCP, driven via `std::net::TcpStream`.
//!   Currently only wired into the `maxtp` workload (the `burst`
//!   workload stays dpdk-only for now). Re-uses
//!   `tools/bench-vs-linux/peer/linux-tcp-sink` on the peer side.
//! - `fstack` — FreeBSD TCP/IP stack ported to userspace on DPDK,
//!   feature-gated behind `--features fstack`.
//!
//! # Sub-workloads
//!
//! - `burst` (T12) — K × G = 20 buckets (spec §11.1). Linux arm not
//!   wired (out of scope for the 2026-05-03 follow-up).
//! - `maxtp` (T13) — W × C = 28 buckets (spec §11.2). Linux arm wired.

// Phase 5 of the 2026-05-09 bench-suite overhaul moved the burst and
// maxtp grid modules out of this crate (Tasks 5.1 + 5.2) into the new
// sibling crates `bench-tx-burst` and `bench-tx-maxtp`. Task 5.4 will
// delete this crate entirely; until then it stays as a stub binary that
// bails with a pointer to the new crates.
pub mod peer_introspect;
pub mod preflight;

/// Stack identifier for CSV `dimensions_json` + runner dispatch.
///
/// Spec §11.3 reserved `mtcp` as a comparator slot; the 2026-05-09
/// bench-suite overhaul dropped it because the upstream project is
/// dormant and the driver never landed. Live values are: `dpdk_net`
/// (our stack), `linux` (kernel TCP, wired for the maxtp workload as
/// of the 2026-05-03 follow-up), and `fstack` (F-Stack — FreeBSD
/// TCP/IP stack ported to userspace on DPDK, feature-gated behind
/// `--features fstack` so default builds don't require libfstack.a;
/// the AMI build provides libfstack.a at
/// `/opt/f-stack/lib/libfstack.a` — see image-builder component
/// `04b-install-f-stack.yaml`). The enum serialises to the exact
/// snake_case string emitted into `dimensions_json.stack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    DpdkNet,
    Linux,
    FStack,
}

impl Stack {
    /// CSV `dimensions_json.stack` string form. Stable; bench-report
    /// groups rows by this exact value.
    pub const fn as_dimension(self) -> &'static str {
        match self {
            Stack::DpdkNet => "dpdk_net",
            Stack::Linux => "linux",
            Stack::FStack => "fstack",
        }
    }

    /// Parse a single token from the `--stacks` CSV arg. Unknown
    /// tokens error; the outer parser in `main.rs` aggregates errors.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "dpdk" | "dpdk_net" => Ok(Stack::DpdkNet),
            "linux" | "linux_kernel" => Ok(Stack::Linux),
            "fstack" | "f-stack" | "f_stack" => Ok(Stack::FStack),
            other => Err(format!(
                "unknown stack `{other}` (valid: dpdk, linux, fstack)"
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
        assert_eq!(Stack::parse("linux").unwrap(), Stack::Linux);
        assert_eq!(Stack::parse("linux_kernel").unwrap(), Stack::Linux);
        assert_eq!(Stack::parse("fstack").unwrap(), Stack::FStack);
        assert_eq!(Stack::parse("f-stack").unwrap(), Stack::FStack);
        assert_eq!(Stack::parse("f_stack").unwrap(), Stack::FStack);
    }

    #[test]
    fn stack_parse_rejects_unknown() {
        let err = Stack::parse("mtcp").unwrap_err();
        assert!(err.contains("unknown stack"));
        let err = Stack::parse("afpacket").unwrap_err();
        assert!(err.contains("unknown stack"));
    }

    #[test]
    fn stack_as_dimension_is_stable() {
        assert_eq!(Stack::DpdkNet.as_dimension(), "dpdk_net");
        assert_eq!(Stack::Linux.as_dimension(), "linux");
        assert_eq!(Stack::FStack.as_dimension(), "fstack");
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
