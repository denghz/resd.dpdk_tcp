//! Linux kernel TCP RX-burst arm — placeholder; the real
//! implementation lands in Task 8.3.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. This stub keeps
//! `pub mod linux;` compileable so the dpdk arm (Task 8.2) can land
//! without dragging the linux arm with it. Returns a clear error
//! when invoked.

use crate::segment::SegmentRecord;

/// Placeholder run-config — final shape lands in Task 8.3.
pub struct LinuxRxBurstCfg {
    pub bucket_id: u32,
    pub segment_size: usize,
    pub burst_count: usize,
    pub warmup_bursts: u64,
    pub measure_bursts: u64,
    pub peer_ip_host_order: u32,
    pub peer_control_port: u16,
}

/// Placeholder result — final shape mirrors `dpdk::DpdkRxBurstRun`.
pub struct LinuxRxBurstRun {
    pub samples: Vec<SegmentRecord>,
}

/// Stub. Task 8.3 replaces with a blocking `TcpStream::connect`
/// + per-burst `write_all`(BURST cmd) + read-into-buffer + chunk
/// parser.
pub fn run_bucket(_cfg: &LinuxRxBurstCfg) -> anyhow::Result<LinuxRxBurstRun> {
    anyhow::bail!(
        "bench-rx-burst linux_kernel arm not yet implemented \
         (Task 8.3 of Phase 8)"
    )
}
