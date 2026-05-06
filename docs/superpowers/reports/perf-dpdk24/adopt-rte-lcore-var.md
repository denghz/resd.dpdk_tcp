# Adopt `rte_lcore_var` (DPDK 24.11)

**Worktree:** `a10-dpdk24-adopt` at `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24`
**Branch HEAD at survey:** `322cd60` (T4.5-4.6 baseline).
**DPDK header:** `/usr/local/dpdk-24.11/include/rte_lcore_var.h` — present.

## What `rte_lcore_var` does

DPDK 24.11 introduced `rte_lcore_var` (RFC: per-lcore static storage). It replaces the `RTE_PER_LCORE` thread-local pattern + the `__rte_cache_aligned` + `RTE_CACHE_GUARD` padding ritual that drivers and EAL libraries have used to give each lcore its own cacheline-isolated copy of state. The intended replacement target is a per-lcore-indexed array — typically `static __rte_cache_aligned T data[RTE_MAX_LCORE];` accessed by `rte_lcore_id()` index — which `rte_lcore_var` reorganizes so each lcore's slot is a stride into a single allocation, eliminating the per-element padding waste.

## Survey result

Searched `crates/dpdk-net-core/src/` (the only Rust crate that owns runtime data structures) for the upstream patterns this API supersedes:

| Pattern | Sites |
|---|---:|
| `__rte_cache_aligned` (Rust C-bridged macro) | 0 |
| `RTE_CACHE_GUARD` | 0 |
| `#[repr(align(64))]` standalone | 0 |
| `#[repr(C, align(64))]` | 5 (`counters.rs` × 4, `rtt_histogram.rs` × 1) |
| `RTE_PER_LCORE` / `rte_lcore_var` already in use | 0 |
| Per-lcore arrays indexed by `rte_lcore_id()` | 0 |

The 5 `repr(C, align(64))` hits are NOT the upstream pattern this API replaces. They're single-instance cache-aligned structs (`EthCounters`, `IpCounters`, `TcpCounters`, `PollCounters`, `RttHistogram`) owned by a single `Engine` and embedded in a `Box<Counters>` — there is no per-lcore array, no `RTE_MAX_LCORE`-element slab, and no per-lcore index access. Cache-line alignment is for false-sharing-isolation between counter groups, which `rte_lcore_var` does not address.

The Stage 1 Engine architecture is **single-Engine-per-lcore** (per `crates/dpdk-net-core/src/engine.rs:533`): each `Engine` instance holds its own `Counters`, `flow_table`, `timer_wheel`, etc. via `RefCell` / `Cell` — not via a `[T; RTE_MAX_LCORE]` array. Multi-lcore deployments allocate N independent engines, one per lcore, in the application layer (e.g. one per ENI on dual-NIC EC2 — see EngineConfig commentary at `engine.rs:401-405`). There is no shared `[T; RTE_MAX_LCORE]` slab anywhere in our crate.

- **Candidate sites:** 0
- **Bench-micro coverage:** N/A (no sites)
- **Difficulty:** N/A — there is nothing to port

## Decision

**not-applicable**

`rte_lcore_var` solves a problem (per-lcore-array padding waste / stride packing) that does not exist in our codebase. The `repr(C, align(64))` decorations on counter structs serve a different purpose (false-sharing isolation between hot counter groups on a single owner) and have no overlap with the lcore-var API surface.

If a future Stage 2 architectural change introduces a per-lcore array (e.g. shared mempool stats keyed by `rte_lcore_id()`, or a global flow-hash table indexed by lcore for dispatch), this report should be revisited — `rte_lcore_var` would be the correct primitive to adopt at that point.

## A/B results

n/a — no port performed.

## Commits

- Port: n/a — no candidate sites
- This report (deferral note): see `git log --oneline -- docs/superpowers/reports/perf-dpdk24/adopt-rte-lcore-var.md`

## Caveats / future-work

- **Stage 2 multi-lcore.** If we add a worker-pool model where multiple lcores cooperate on a single Engine's flow table or a shared mempool stats slab, the per-lcore-array pattern becomes natural and `rte_lcore_var` becomes adoptable. Until then, this is an architectural mismatch, not a code-style oversight.
- **C-side shim layer.** If we ever back any of the `Counters` groups with C storage (e.g. for direct DPDK telemetry integration), revisit. Today the shim layer is exclusively pass-through wrappers for inline DPDK functions (`shim_rte_pktmbuf_*`); it owns no data structures.
- **Reference for any future re-evaluation:** https://doc.dpdk.org/api-24.11/rte__lcore__var_8h.html
