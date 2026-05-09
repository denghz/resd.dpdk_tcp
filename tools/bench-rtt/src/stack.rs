//! Stack selector for `bench-rtt --stack`.
//!
//! Three values mirror the comparator-triplet established in the
//! 2026-05-09 bench-suite overhaul: `dpdk_net` (this stack), `linux_kernel`
//! (kernel TCP socket path), and `fstack` (F-Stack — FreeBSD TCP/IP stack
//! ported to userspace on DPDK).
//!
//! The F-Stack arm is feature-gated behind the `fstack` cargo feature so
//! default builds compile without libfstack.a. Dispatch in `main.rs`
//! routes each variant to its module; unknown values are rejected by
//! clap before reaching the dispatch table.

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Stack {
    /// dpdk_net (this crate's `dpdk_net_core::Engine` path).
    DpdkNet,
    /// Linux kernel TCP via `std::net::TcpStream` — comparator baseline.
    LinuxKernel,
    /// F-Stack on DPDK (feature-gated).
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
