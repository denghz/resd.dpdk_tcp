//! bench-rx-burst — library façade for the binary.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. Closes claims:
//!
//! - **C-A3** — final replacement for the bench-rx-zero-copy placeholder
//!   purpose: a real RX-burst measurement tool.
//! - **C-B3** — per-RX-segment app-delivery latency. No other bench
//!   tool measures this dimension; bench-rtt is single-segment
//!   request/response, bench-tx-burst is TX-side throughput.
//! - **C-C2** — RX burst workload. The quote/trade-sized burst shape
//!   (N × W small segments) is what trading workloads actually see on
//!   the wire; this tool captures it.
//!
//! # Library surface
//!
//! Modules public so integration tests + future bench tooling can pull
//! in the per-stack arms and the segment-record shape:
//!
//! - [`stack`] — the `--stack` enum (dpdk_net / linux_kernel / fstack).
//! - [`segment`] — `SegmentRecord` and the per-Readable header parser.
//! - [`dpdk`] — dpdk_net arm.
//! - [`linux`] — linux_kernel arm.
//! - [`fstack`] — F-Stack arm (feature-gated).

pub mod dpdk;
#[cfg(feature = "fstack")]
pub mod fstack;
pub mod linux;
pub mod segment;
pub mod stack;

// Phase 5 Task 5.4 lifted the F-Stack FFI bindings into the shared
// `bench-fstack-ffi` crate; re-export under the legacy path so the
// fstack arm's `crate::fstack_ffi::...` imports keep working without
// churn through the F-Stack pump state machine.
#[cfg(feature = "fstack")]
pub use bench_fstack_ffi as fstack_ffi;
