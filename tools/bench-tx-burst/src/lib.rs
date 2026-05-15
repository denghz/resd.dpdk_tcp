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

    /// CSV `metric_name` for the per-burst throughput sample on this
    /// stack. Per-arm completion semantics diverge in a way that
    /// matters for downstream readers (T57 follow-up #2, codex I2
    /// 2026-05-13):
    ///
    /// - `dpdk_net` captures `t1` immediately after `rte_eth_tx_burst`
    ///   returns. This is the rate at which the application handed
    ///   bytes to the PMD send ring — NOT wire rate. The PMD ring
    ///   buffers (and `rte_eth_tx_burst` may also enqueue mbufs into
    ///   driver-internal queues for batched DMA), so on low-payload,
    ///   low-iteration counts the figure can exceed line rate (e.g.
    ///   18 Gbps on a 5 Gbps NIC). We emit `pmd_handoff_rate_bps` on
    ///   dpdk_net rows to make the measurement gap explicit and
    ///   prevent readers from treating the value as a wire-rate proxy.
    ///   Wire-rate calibration requires either HW TX timestamps
    ///   (ENA does not advertise the `rte_mbuf::tx_timestamp` dynfield)
    ///   or peer-side packet capture.
    /// - `linux_kernel` captures `t1` after `write_all` returns —
    ///   `write()` accepts bytes into the kernel send buffer and
    ///   returns long before the bytes leave the NIC. K / (t1 − t0)
    ///   is the rate the kernel ingests payload, NOT wire rate. We
    ///   emit `write_acceptance_rate_bps` to make this explicit.
    /// - `fstack` is the same shape as linux_kernel — `ff_write`
    ///   accepts into F-Stack's BSD-shaped send buffer, returns
    ///   before the segment hits the wire. Same `write_acceptance_rate_bps`
    ///   label.
    ///
    /// All three labels are app-handoff-rate metrics on different
    /// boundaries: PMD-handoff for dpdk_net, kernel-send-buffer-accept
    /// for linux_kernel, F-Stack-send-buffer-accept for fstack. None
    /// of them is wire-rate. Once HW TX timestamps land (mlx5/ice or
    /// a future-gen ENA advertising the dynfield, or `Engine::last_tx_hw_ts`
    /// for fstack-on-DPDK), the wire-rate proxy is a separate
    /// follow-up — see codex I2 (2026-05-13).
    pub const fn throughput_metric_name(self) -> &'static str {
        match self {
            Stack::DpdkNet => "pmd_handoff_rate_bps",
            Stack::LinuxKernel | Stack::Fstack => "write_acceptance_rate_bps",
        }
    }

    /// True iff this stack's `throughput_metric_name()` value
    /// approximates wire rate (t1 captured at the NIC-egress boundary
    /// via HW TX timestamps). Currently always `false`: every arm
    /// captures `t1` at an application/PMD-handoff boundary, not on
    /// the wire. Reserved for the HW-TS follow-up — see codex I2
    /// (2026-05-13) which flagged that dpdk_net's
    /// `rte_eth_tx_burst`-return is PMD-handoff, not wire-rate.
    pub const fn throughput_is_wire_rate_calibrated(self) -> bool {
        // Codex I2 (2026-05-13): no arm currently captures t1 at the
        // wire — dpdk_net's `rte_eth_tx_burst`-return is PMD-handoff,
        // not NIC completion. Flips to `true` for `Stack::DpdkNet`
        // once a HW-TS-based completion timestamp is wired in.
        false
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

    // T57 follow-up #2 + codex I2 (2026-05-13): per-arm metric-name +
    // calibration accessors so downstream readers can tell PMD-handoff
    // (dpdk_net) apart from buffer-fill rate (linux_kernel, fstack).
    // No arm captures wire-rate today — the rename from
    // `throughput_per_burst_bps` → `pmd_handoff_rate_bps` makes that
    // explicit on the dpdk_net side.
    #[test]
    fn stack_throughput_metric_name_distinguishes_handoff_arms() {
        // dpdk_net captures t1 at rte_eth_tx_burst-return → bytes
        // handed to the PMD send ring, NOT on the wire.
        assert_eq!(
            Stack::DpdkNet.throughput_metric_name(),
            "pmd_handoff_rate_bps"
        );
        // linux_kernel and fstack capture t1 after write()/ff_write()
        // returns → measures buffer-acceptance rate, NOT wire rate →
        // emits write_acceptance_rate_bps.
        assert_eq!(
            Stack::LinuxKernel.throughput_metric_name(),
            "write_acceptance_rate_bps"
        );
        assert_eq!(
            Stack::Fstack.throughput_metric_name(),
            "write_acceptance_rate_bps"
        );
    }

    #[test]
    fn stack_throughput_is_wire_rate_calibrated_is_false_on_every_arm() {
        // Codex I2 (2026-05-13): every arm captures t1 at an
        // application/PMD-handoff boundary, not on the wire. The
        // accessor is reserved for the HW-TS follow-up; until then
        // it returns `false` everywhere.
        assert!(!Stack::DpdkNet.throughput_is_wire_rate_calibrated());
        assert!(!Stack::LinuxKernel.throughput_is_wire_rate_calibrated());
        assert!(!Stack::Fstack.throughput_is_wire_rate_calibrated());
    }
}
