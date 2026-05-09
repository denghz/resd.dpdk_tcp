//! bench-tx-burst — library façade for the K × G burst-grid binary.
//!
//! Phase 5 of the 2026-05-09 bench-suite overhaul split the legacy
//! bench-vs-mtcp tool into two focused crates:
//!
//! - `bench-tx-burst` (this crate) — one-shot K-byte burst grid (K × G,
//!   spec §11.1).
//! - `bench-tx-maxtp` (sibling crate) — sustained-rate W × C grid
//!   (spec §11.2).
//!
//! The mTCP arm was removed in Phase 2; the live comparator triplet
//! is `dpdk_net` (this stack) + `linux_kernel` (kernel TCP via
//! `std::net::TcpStream`) + `fstack` (F-Stack on DPDK, feature-gated).
//!
//! # Stacks
//!
//! - `dpdk_net` — driven via `dpdk_net_core::Engine` ([`dpdk`]).
//! - `linux_kernel` — kernel TCP via `std::net::TcpStream` ([`linux`]).
//!   New in Phase 5 — the legacy bench-vs-mtcp burst arm only ran
//!   dpdk_net + fstack.
//! - `fstack` — F-Stack on DPDK ([`fstack`], gated behind the
//!   `fstack` feature).

pub mod burst;
pub mod dpdk;
#[cfg(feature = "fstack")]
pub mod fstack;
pub mod linux;
pub mod peer_introspect;
pub mod preflight;

// Phase 5 Task 5.4 of the 2026-05-09 bench-suite overhaul lifted the
// `fstack_ffi` module into the shared `bench-fstack-ffi` crate. Re-
// export under the legacy path so the F-Stack pump's
// `crate::fstack_ffi::...` imports keep working without churn.
#[cfg(feature = "fstack")]
pub use bench_fstack_ffi as fstack_ffi;

/// Stack identifier for CSV `dimensions_json` + runner dispatch.
///
/// Variant names follow the snake_case form emitted into
/// `dimensions_json.stack` so downstream bench-report groups rows by
/// the verbatim string.
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
        assert_eq!(Stack::parse("f-stack").unwrap(), Stack::Fstack);
    }

    #[test]
    fn stack_parse_rejects_unknown() {
        assert!(Stack::parse("mtcp").is_err());
        assert!(Stack::parse("garbage").is_err());
    }

    #[test]
    fn stack_as_dimension_is_stable() {
        assert_eq!(Stack::DpdkNet.as_dimension(), "dpdk_net");
        assert_eq!(Stack::LinuxKernel.as_dimension(), "linux_kernel");
        assert_eq!(Stack::Fstack.as_dimension(), "fstack");
    }
}
