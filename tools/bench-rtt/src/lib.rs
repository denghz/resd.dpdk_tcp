//! bench-rtt — library façade for the binary.
//!
//! Cross-stack request/response RTT distribution. Phase 4 of the
//! 2026-05-09 bench-suite overhaul: replaces bench-e2e (binary),
//! bench-stress (matrix runner), and bench-vs-linux mode A by
//! parameterising the stack, payload size, connection count, and
//! netem-spec axes (closes C-A5, C-B5, C-C1, C-D3).
//!
//! The lib-façade exists so integration tests in `tests/` can pull
//! `attribution`, `sum_identity`, and `hw_task_18` in without going
//! through the binary entry. The binary consumes the same modules via
//! `use bench_rtt::*`.

pub mod attribution;
pub mod hw_task_18;
pub mod linux_kernel;
pub mod stack;
pub mod sum_identity;
pub mod workload;
