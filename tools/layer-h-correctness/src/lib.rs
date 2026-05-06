//! Stage 1 Phase A10.5 — Layer H correctness gate.
//!
//! 2026-05-04: gated behind the `test-server` feature (non-default).
//! Workspace builds without `--features test-server` skip the entire
//! crate so dpdk-net-core/test-server doesn't get unified into the
//! production benches' linkage (which would reroute tx_frame via the
//! test_tx_intercept buffer + break gateway ARP). Build standalone:
//!   cargo build -p layer-h-correctness --features test-server
//!
//! See `docs/superpowers/specs/2026-05-01-stage1-phase-a10-5-layer-h-
//! correctness-design.md`.
//!
//! The lib façade exposes the matrix, assertion engine, and observation
//! primitives so the integration tests in `tests/*` can import them
//! without going through the binary.

#![cfg(feature = "test-server")]

pub mod assertions;
pub mod counters_snapshot;
pub mod observation;
pub mod report;
pub mod scenarios;
pub mod workload;
