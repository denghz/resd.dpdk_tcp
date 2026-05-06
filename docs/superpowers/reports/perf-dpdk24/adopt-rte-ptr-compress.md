# Adopt `rte_ptr_compress` (DPDK 24.07)

**Worktree:** `a10-dpdk24-adopt` at `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24`
**Branch HEAD at survey:** `322cd60` (T4.5-4.6 baseline).
**DPDK header:** `/usr/local/dpdk-24.11/include/rte_ptr_compress.h` — present.

## What `rte_ptr_compress` does

DPDK 24.07 added `rte_ptr_compress` for pointer-array compression in mbuf-burst loops. The optimization observes that within a single mempool, all mbuf pointers share their upper bits (only the lower bits — relative to the mempool base — actually vary). By packing a `[*mut rte_mbuf; N]` burst array into a `[u32; N]` of mempool-relative offsets, the cache footprint of the burst array halves on 64-bit hosts. The benefit is memory-bandwidth — burst-loop iterators that previously pulled in 2 cachelines for a 32-pointer array now pull in 1.

## Survey result

Searched `crates/dpdk-net-core/src/` for the burst-array sites that would benefit:

| Site | Path | Array | Direction |
|---|---|---|---|
| RX top of poll | `engine.rs:2077-2086` | `[*mut sys::rte_mbuf; 32]` | RX |
| Single-frame TX (control) | `engine.rs:1823` | `pkts[1]` (1-element scratch) | TX |
| Single-frame TX (control) | `engine.rs:1891` | `pkts[1]` | TX |
| Single-frame TX (control) | `engine.rs:1935` | `pkts[1]` | TX |
| `drain_tx_pending_data` | `engine.rs:2222-2252` | `Vec<NonNull<rte_mbuf>>` (variable) | TX |

**Candidate sites:** 1 hot RX-burst array of 32 (the only site where compression provides a meaningful cache-footprint win). The four single-frame TX call sites use 1-element arrays — no compression possible in principle (the compress/decompress overhead exceeds any cache saving for N=1). The `tx_pending_data` ring is variable-sized; current cap is bounded by `tx_ring_size` and uses `NonNull<rte_mbuf>` (same width as `*mut rte_mbuf`).

**Bench-micro coverage of these sites:** **NONE.**

- The `EngineNoEalHarness::poll_once` (`engine.rs:125-149`) does NOT call `shim_rte_eth_rx_burst` — the harness has no DPDK port allocation, no RX queue, no real mbufs. It only walks the FlowTable + TimerWheel + EventQueue.
- The `bench_send_*` family is a pure-Rust stub per T2.7 deferral (see `docs/superpowers/reports/t2-7-deferral.md`); it does not touch `tx_pending_data`, `shim_rte_eth_tx_burst`, or `drain_tx_pending_data`.
- No bench-micro target exercises `Engine::poll_once`'s real RX-burst array.

**Difficulty (if it were to be ported):** medium. `rte_ptr_compress_pktmbuf_64_to_32` operates on a fixed-size array but requires the source mempool's base pointer. Our RX path uses one mempool (`_rx_mempool`), so the base is known — but the compressed form would only be useful if the consumer (the `for &m in &mbufs[..n]` decode loop at `engine.rs:2124`) is actually bottlenecked on prefetch latency for the `mbufs` array itself. On a 32-element array, the array is one or two cachelines, prefetched implicitly by the burst result write — wins, if any, are marginal even on a real ENA with a real bench.

## Decision

**deferred-to-e2e**

The bench-micro suite cannot measure this API. Adopting it without measurement would be speculative — the win is real-bench-only, and our send path is stubbed.

Per the plan's §6.2 go/no-go discipline ("adopt iff measurement shows improvement"), no port is performed at this stage. Re-evaluate when:

1. **T3.3 wires real send.** Real `shim_rte_eth_tx_burst` runs from `tx_pending_data` and the ENA driver's TX hot path becomes measurable.
2. **bench-e2e or a real bench-pair host runs.** A per-poll TBP capture against a saturated RX queue would show whether the `mbufs[..n]` iteration is bottlenecked on burst-array fetches at all. Today the dev host is KVM with TBP-resolution limits (per `docs/superpowers/reports/perf-host-capabilities.md`).

If both prerequisites land and a TBP shows measurable burst-array-iteration overhead, adopt this API on the RX-burst site only (the four 1-element TX sites and the variable-length tx_pending_data ring are not candidates regardless).

## A/B results

n/a — no port performed (deferred per scope-mismatch).

## Commits

- Port: n/a — deferred to e2e
- This report (deferral note): see `git log --oneline -- docs/superpowers/reports/perf-dpdk24/adopt-rte-ptr-compress.md`

## Caveats / future-work

- **Memory-bandwidth, not latency.** `rte_ptr_compress` targets cache-footprint reduction. Our trading-latency profile is dominated by per-segment fixed overhead (RTT prediction, ACK emission, RACK loss recovery), not burst-iterator cache lines. Even on a real bench, expect single-digit-ns or sub-ns wins on the RX-burst loop.
- **N=1 TX sites are not candidates.** The four `pkts[1]` arrays at `engine.rs:1823 / 1891 / 1935 / 2234` are too small to compress meaningfully; the compress/decompress overhead exceeds any cache saving at N=1.
- **The actual measurement criterion** for adoption is the per-poll TBP attribution to `mbufs[..n]` array iteration on a saturated RX queue, captured on a real bench-pair host with `bench-e2e` or a TBP-driven custom workload — neither of which exists at the bench-micro stage.
- **Reference for any future re-evaluation:** https://doc.dpdk.org/api-24.11/rte__ptr__compress_8h.html
