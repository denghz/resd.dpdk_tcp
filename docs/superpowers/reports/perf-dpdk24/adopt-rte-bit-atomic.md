# Adopt `rte_bit_atomic_*` (DPDK 24.11)

**Worktree:** `a10-dpdk24-adopt` at `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24`
**Branch HEAD at survey:** `322cd60` (T4.5-4.6 baseline).
**DPDK header:** `/usr/local/dpdk-24.11/include/rte_bitops.h` — present.

## What `rte_bit_atomic_*` does

DPDK 24.11 introduced lighter atomic bit-manipulation operations (`rte_bit_atomic_test_and_set`, `rte_bit_atomic_clear`, `rte_bit_atomic_test_and_clear`, etc.) as part of the broader `rte_bitops.h` modernization. They expose explicit memory-ordering parameters so the caller can pick `__ATOMIC_RELAXED` for hot-path bit twiddles that don't actually need the full SeqCst barrier emitted by the platform's default fetch-or / fetch-and intrinsic. The win is observable on architectures where the default is a full memory barrier (e.g. arm64 with `dmb sy`); on x86 the default lock-prefix path is already mostly relaxed, so the win is closer to noise.

## Survey result

Searched `crates/dpdk-net-core/src/` for the upstream patterns this API supersedes:

| Pattern | Sites |
|---|---:|
| `AtomicU32` (any) | 0 |
| `AtomicU64` | many (counters; uses `fetch_add` / `load`, never bitwise) |
| `.fetch_or(` | 0 |
| `.fetch_and(` | 0 |
| `.fetch_xor(` | 0 |
| `.fetch_nand(` | 0 |

The entire dpdk-net-core uses atomics in exactly two patterns:

1. **Counter increment** — `fetch_add(1, Relaxed)` (via `counters::inc`) and `fetch_add(n, Relaxed)` (via `counters::add`). These are arithmetic, not bit operations.
2. **Counter snapshot** — `Atomic*::load(Relaxed)`. Read-only.

There is no `fetch_or` / `fetch_and` / `fetch_xor` / `fetch_nand` anywhere in the crate. The TCP state machine uses scalar enums (`TcpState`), connection flags use plain `bool` fields on `TcpConn` (single-lcore — no atomicity needed under our RTC ownership model), and offload state is tracked via `bool` latches on `Engine` populated once at bring-up.

- **Candidate sites:** 0
- **Bench-micro coverage:** N/A (no sites)
- **Difficulty:** N/A — there is nothing to port

## Decision

**not-applicable**

`rte_bit_atomic_*` solves a problem (atomic bit manipulation with relaxed ordering) that does not exist in our codebase. Our atomic surface is exclusively counter arithmetic + counter snapshot reads, both of which are already optimally lowered (relaxed `fetch_add` / `load`) on x86_64 and arm64.

If a future task introduces an atomic bitmask (e.g. a per-conn "events pending" flag-set, or a global "lcore busy" mask), this report should be revisited — `rte_bit_atomic_*` would be the correct primitive to reach for at that point. As of A6.7 (current HEAD baseline), no such bitmask exists.

## A/B results

n/a — no port performed.

## Commits

- Port: n/a — no candidate sites
- This report (deferral note): see `git log --oneline -- docs/superpowers/reports/perf-dpdk24/adopt-rte-bit-atomic.md`

## Caveats / future-work

- **Cross-lcore communication path.** Stage 1 is single-lcore-per-Engine (RTC). If Stage 2 introduces lcore-to-lcore notification (e.g. a worker pool consuming a producer-lcore's event ring), atomic bitmasks become a natural primitive for "wakeup needed" signaling and `rte_bit_atomic_*` becomes adoptable.
- **Hot-path counter feature gates.** A future feature-gated counter (currently `obs-poll-saturation`, `obs-byte-counters` — see `engine.rs:2111-2122`) might use a packed bitmask of "events fired this poll" to batch counter increments. If implemented, evaluate `rte_bit_atomic_*` for that bitmask.
- **Reference for any future re-evaluation:** https://doc.dpdk.org/api-24.11/rte__bitops_8h.html
