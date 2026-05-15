//! Stack selector for `bench-rx-burst --stack`.
//!
//! Three values mirror the comparator triplet established in the
//! 2026-05-09 bench-suite overhaul: `dpdk_net` (this stack), `linux_kernel`
//! (kernel TCP socket path), and `fstack` (F-Stack — FreeBSD TCP/IP stack
//! ported to userspace on DPDK).
//!
//! The F-Stack arm is feature-gated behind the `fstack` cargo feature so
//! default builds compile without libfstack.a. Dispatch in `main.rs`
//! routes each variant to its module; unknown values are rejected by
//! clap before reaching the dispatch table.

use clap::ValueEnum;

/// Stack selector. Variant names use snake_case via `value(name="...")`
/// so the operator-facing form is `dpdk_net` / `linux_kernel` / `fstack`
/// (matching the historical bench-vs-linux `--stacks` token shape and
/// the bench-nightly.sh wiring), not clap's default kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Stack {
    /// dpdk_net (this crate's `dpdk_net_core::Engine` path).
    #[value(name = "dpdk_net")]
    DpdkNet,
    /// Linux kernel TCP via `std::net::TcpStream` — comparator baseline.
    #[value(name = "linux_kernel")]
    LinuxKernel,
    /// F-Stack on DPDK (feature-gated).
    #[value(name = "fstack")]
    Fstack,
}

impl Stack {
    /// Stable string form emitted into the CSV `dimensions_json.stack` cell.
    /// Downstream bench-report buckets rows by the verbatim string; do
    /// not change without coordinated downstream updates.
    pub const fn as_dimension(self) -> &'static str {
        match self {
            Stack::DpdkNet => "dpdk_net",
            Stack::LinuxKernel => "linux_kernel",
            Stack::Fstack => "fstack",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_dimension_is_stable() {
        assert_eq!(Stack::DpdkNet.as_dimension(), "dpdk_net");
        assert_eq!(Stack::LinuxKernel.as_dimension(), "linux_kernel");
        assert_eq!(Stack::Fstack.as_dimension(), "fstack");
    }
}
