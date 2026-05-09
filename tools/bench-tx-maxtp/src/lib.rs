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
#[cfg(feature = "fstack")]
pub mod fstack_ffi;
pub mod linux;
pub mod maxtp;
pub mod peer_introspect;
pub mod preflight;

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
}
