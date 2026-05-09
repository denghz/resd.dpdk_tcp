//! F-Stack RX-burst arm — placeholder; the real implementation lands
//! in Task 8.3.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. This stub keeps
//! `pub mod fstack;` compileable under the `fstack` feature so the
//! dpdk arm (Task 8.2) can land without dragging the F-Stack arm with
//! it. Returns a clear error when invoked.

#![cfg(feature = "fstack")]

use crate::segment::SegmentRecord;

/// Placeholder run-config — final shape lands in Task 8.3.
pub struct FstackRxBurstCfg {
    pub bucket_id: u32,
    pub segment_size: usize,
    pub burst_count: usize,
    pub warmup_bursts: u64,
    pub measure_bursts: u64,
    pub peer_ip_host_order: u32,
    pub peer_control_port: u16,
}

/// Placeholder result — final shape mirrors `dpdk::DpdkRxBurstRun`.
pub struct FstackRxBurstRun {
    pub samples: Vec<SegmentRecord>,
}

/// Stub. Task 8.3 replaces with an `ff_run`-driven state machine
/// (mirroring `bench-tx-burst::fstack`) that drives a single
/// connection through the BURST cmd / drain cycle inside one
/// `ff_run` invocation.
pub fn run_grid(_cfgs: &[FstackRxBurstCfg]) -> anyhow::Result<Vec<FstackRxBurstRun>> {
    anyhow::bail!(
        "bench-rx-burst fstack arm not yet implemented \
         (Task 8.3 of Phase 8)"
    )
}
