//! bench-vs-mtcp — library façade for the binary.
//!
//! A10 Plan B Task 12 (spec §11.1, parent spec §11.5.1). Task 13
//! added the `maxtp` sub-workload (spec §11.2) on the same binary.
//! The 2026-05-03 follow-up landed a `Linux` stack arm so the maxtp
//! grid can compare dpdk_net against the kernel TCP stack directly.
//!
//! The 2026-05-04 follow-up landed the **mTCP-on-DPDK-20.11 sidecar**
//! path (image-builder component `04-install-mtcp.yaml`). DPDK 20.11
//! LTS installs to `/usr/local/dpdk-20.11/`, mTCP builds against it
//! (libmtcp.a + 3 small patches: Makefile pkg-config switch,
//! `-fcommon` link flag, `lcore_config[]` shim), and a dedicated
//! client-side `mtcp-driver` C binary at `/opt/mtcp-peer/mtcp-driver`
//! is invoked via subprocess from this Rust crate. The driver's
//! workload pump is currently a stub returning ENOSYS — see
//! `src/mtcp.rs` module docs + `peer/mtcp-driver.c` for the frozen
//! CLI + JSON contracts. The Rust subprocess wrapper, error
//! taxonomy, validation layer, and AMI layout are landed and stable.
//!
//! # Stacks
//!
//! - `dpdk_net` — our stack, driven via `dpdk_net_core::Engine`. Full
//!   implementation.
//! - `mtcp` — MIT mTCP stack. Subprocess-wrapped via
//!   `/opt/mtcp-peer/mtcp-driver`. Wrapper validates configs, builds
//!   the C-side argv, parses JSON results, and surfaces the driver's
//!   ENOSYS as `mtcp::Error::DriverUnimplemented` until the C-side
//!   workload pump implementation lands.
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
#[cfg(feature = "fstack")]
pub mod fstack_burst;
#[cfg(feature = "fstack")]
pub mod fstack_ffi;
#[cfg(feature = "fstack")]
pub mod fstack_maxtp;
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
/// 2026-05-03 follow-up). The 2026-05-04 follow-up added `fstack`
/// (F-Stack — FreeBSD TCP/IP stack ported to userspace on DPDK,
/// actively maintained, builds against DPDK 23.11) as the third
/// real comparator alongside dpdk_net + linux. The F-Stack arms are
/// feature-gated behind `--features fstack` so default builds don't
/// require libfstack.a; the AMI build provides libfstack.a at
/// `/opt/f-stack/lib/libfstack.a` (see image-builder component
/// `04b-install-f-stack.yaml`). The enum serialises to the exact
/// snake_case string emitted into `dimensions_json.stack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    DpdkNet,
    Mtcp,
    Linux,
    FStack,
}

impl Stack {
    /// CSV `dimensions_json.stack` string form. Stable; bench-report
    /// groups rows by this exact value.
    pub const fn as_dimension(self) -> &'static str {
        match self {
            Stack::DpdkNet => "dpdk_net",
            Stack::Mtcp => "mtcp",
            Stack::Linux => "linux",
            Stack::FStack => "fstack",
        }
    }

    /// Parse a single token from the `--stacks` CSV arg. Unknown
    /// tokens error; the outer parser in `main.rs` aggregates errors.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "dpdk" | "dpdk_net" => Ok(Stack::DpdkNet),
            "mtcp" => Ok(Stack::Mtcp),
            "linux" | "linux_kernel" => Ok(Stack::Linux),
            "fstack" | "f-stack" | "f_stack" => Ok(Stack::FStack),
            other => Err(format!(
                "unknown stack `{other}` (valid: dpdk, mtcp, linux, fstack)"
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
        assert_eq!(Stack::parse("fstack").unwrap(), Stack::FStack);
        assert_eq!(Stack::parse("f-stack").unwrap(), Stack::FStack);
        assert_eq!(Stack::parse("f_stack").unwrap(), Stack::FStack);
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
