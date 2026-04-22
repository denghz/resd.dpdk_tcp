//! `maxtp` sub-workload — **placeholder** for Plan B Task 13.
//!
//! Task 13 will fill this out per spec §11.2:
//!
//! | Axis                         | Values                                       |
//! |------------------------------|----------------------------------------------|
//! | Application write size W (B) | {64, 256, 1024, 4096, 16_384, 65_536, 262_144} |
//! | Connection count C           | {1, 4, 16, 64}                               |
//!
//! Product = W × C = 28 buckets. Persistent connection(s); application
//! writes in a tight loop for T = 60 s per bucket post-warmup.
//!
//! T12 ships this module as a stub so the CLI dispatch path can
//! accept `--workload maxtp` on paper; the runner below fires
//! `todo!()` and fails fast.
//!
//! # What T13 will add
//!
//! - Grid enumeration helpers mirroring [`crate::burst`].
//! - Connection pool bring-up + round-robin writer loop.
//! - Goodput + packet-rate aggregation over a 60 s window with a 10 s
//!   warmup.
//! - Sanity invariant: ACKed bytes == `stack_tx_bytes_counter_delta`
//!   during window (minus in-flight bound).

/// Per spec §11.2: application write size W in bytes. T13 will wire
/// these; exposed here as a stable const so the grid dimensions
/// cannot drift between T12 and T13 landings.
pub const W_BYTES: &[u64] = &[64, 256, 1024, 4096, 16_384, 65_536, 262_144];

/// Per spec §11.2: connection count C.
pub const C_CONNS: &[u64] = &[1, 4, 16, 64];

/// Number of (W, C) buckets in the maxtp grid. Guard-asserted below to
/// catch drift if either array grows.
pub const BUCKET_COUNT: usize = 28;

/// T13 stub. Called by `main.rs` if the operator passes
/// `--workload maxtp` before Task 13 lands. Panics with a pointer to
/// the follow-up.
pub fn run_maxtp_workload() -> ! {
    todo!(
        "maxtp workload is a placeholder until Plan B Task 13 lands. \
         See tools/bench-vs-mtcp/src/maxtp.rs module docs for the plan."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_count_matches_grid_dimensions() {
        // Locked invariant — the product must equal `BUCKET_COUNT`.
        // If either axis grows, bump `BUCKET_COUNT` in lockstep.
        assert_eq!(W_BYTES.len() * C_CONNS.len(), BUCKET_COUNT);
    }

    #[test]
    fn grid_values_match_spec_11_2() {
        // Lock the exact spec §11.2 grid values. If the spec changes
        // these, update the constants AND this test together.
        assert_eq!(W_BYTES, &[64, 256, 1024, 4096, 16_384, 65_536, 262_144]);
        assert_eq!(C_CONNS, &[1, 4, 16, 64]);
    }
}
