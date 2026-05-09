//! bench-rtt — cross-stack request/response RTT distribution.
//!
//! Replaces bench-e2e (binary), bench-stress (matrix runner), and
//! bench-vs-linux mode A by parameterising the stack, payload size,
//! connection count, and netem-spec axes. Phase 4 of the 2026-05-09
//! bench-suite overhaul (closes C-A5, C-B5, C-C1, C-D3).

fn main() -> anyhow::Result<()> {
    anyhow::bail!("bench-rtt scaffold — wiring lands in Task 4.2+")
}
