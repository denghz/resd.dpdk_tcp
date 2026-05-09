//! bench-vs-mtcp — Phase 5 stub.
//!
//! Phase 5 of the 2026-05-09 bench-suite overhaul split the burst and
//! maxtp workloads into dedicated `bench-tx-burst` and `bench-tx-maxtp`
//! binaries. Task 5.4 deletes this crate entirely; until then this
//! stub bails with a pointer so any leftover script invocation surfaces
//! the rename instead of running an empty grid.
//!
//! - `bench-tx-burst --stack <dpdk_net|linux_kernel|fstack>` for the
//!   K × G one-shot burst grid (spec §11.1).
//! - `bench-tx-maxtp --stack <dpdk_net|linux_kernel|fstack>` for the
//!   W × C sustained-rate grid (spec §11.2).
//!
//! The mTCP comparator was removed in Phase 2.

fn main() -> anyhow::Result<()> {
    anyhow::bail!(
        "bench-vs-mtcp is gone (Phase 5 of the 2026-05-09 bench-suite overhaul). \
         Use `bench-tx-burst --stack <dpdk_net|linux_kernel|fstack>` for the burst \
         workload (spec §11.1), or `bench-tx-maxtp --stack ...` for the maxtp \
         workload (spec §11.2)."
    );
}
