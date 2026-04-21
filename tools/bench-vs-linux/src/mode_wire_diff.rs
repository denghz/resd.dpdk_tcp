//! Mode B — wire-level byte-diff, `preset=rfc_compliance`. Spec §8.
//!
//! T9 territory: Plan B Task 9 lands the implementation. T8 ships
//! only the stub so `Mode::WireDiff` is a recognised enum variant
//! and the CLI dispatch doesn't panic on a typo — it returns a
//! clear "not yet implemented" error that points at T9.
//!
//! The real implementation will:
//!   1. Build the engine with `apply_preset(1)` (rfc_compliance).
//!   2. Run the same workload against the peer via both dpdk_net +
//!      linux kernel path.
//!   3. `pcap`-capture both sides' traffic.
//!   4. Pipe through the divergence-normalisation layer
//!      (normalise ISS, TS base, MSS) in `src/normalize.rs`
//!      (also T9).
//!   5. Byte-diff the normalised captures.
//!
//! None of that plumbing lives here yet. Keeping the stub makes the
//! enum exhaustive and the mode selector total — a misspelled
//! `--mode wiri-diff` on the CLI gets a parse error, not a runtime
//! panic from an unhandled match arm in `main.rs`.

/// Placeholder entry point for mode B. Always errors with a message
/// pointing at T9. The signature matches what T9 will land so the
/// binary's `main.rs` dispatch can call this directly once the stub
/// is replaced.
pub fn run_mode_wire_diff() -> anyhow::Result<()> {
    // `todo!()` gives a clearer backtrace than a bail!, but `bail!`
    // is the caller-facing contract (anyhow::Result). Use bail! with
    // an explicit T9 pointer.
    anyhow::bail!(
        "--mode wire-diff is a T9 deliverable; T8 ships --mode rtt only. \
         See docs/superpowers/plans/2026-04-21-stage1-phase-a10-benchmark-harness.md \
         Task 9."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_mode_wire_diff_errors_with_t9_pointer() {
        let err = run_mode_wire_diff().unwrap_err().to_string();
        assert!(
            err.contains("T9") || err.contains("Task 9"),
            "error must reference T9 so operators know where to look: {err}"
        );
        assert!(err.contains("wire-diff"));
    }
}
