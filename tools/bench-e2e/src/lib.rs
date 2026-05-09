//! bench-e2e — peer-server-only crate (post-Phase-4 overhaul).
//!
//! Originally this crate hosted the request/response RTT inner loop
//! (Plan B Task 6). The 2026-05-09 bench-suite overhaul Phase 4
//! consolidated those modules into the new `bench-rtt` crate
//! (`tools/bench-rtt/`). The `tools/bench-e2e/peer/` subdirectory
//! remains as the C echo-server build artefact; this Rust lib crate
//! is now a stub kept around so the workspace still builds while the
//! peer/Makefile is reachable from the same path.
//!
//! Phase 4 Task 4.7 will remove the Rust-side files entirely. Until
//! then the lib stays empty and the binary entry is dropped from
//! Cargo.toml so cargo does not try to compile the now-orphan
//! `src/main.rs`.
