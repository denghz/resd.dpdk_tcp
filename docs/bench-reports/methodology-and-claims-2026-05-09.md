# bench-overhaul — methodology and claims (2026-05-09 baseline)

Reference doc for the per-tool methodology used by the 2026-05-09 bench-suite
overhaul (Phase 5 split into `bench-rtt`, `bench-tx-burst`, `bench-tx-maxtp`,
`bench-rx-burst`). Captures the operational definition behind each CSV
`metric_name` and the limitations on the claim it supports.

This is the canonical "what does this column actually mean" reference;
per-run reports (`t51-*`, ..., `t57-*`, ...) cite back here for the
operational definitions and link to specific changes (e.g. codex I2
2026-05-13 renamed dpdk_net's `throughput_per_burst_bps` →
`pmd_handoff_rate_bps`).

## Metrics — bench-tx-burst per-arm labels

`bench-tx-burst` emits one primary throughput metric per arm, with the
metric_name chosen to advertise **which handoff boundary** the sample was
captured at. Every arm captures `t1` at an application- or PMD-level
boundary; **none** is wire-rate today. Wire-rate claims require either HW
TX timestamps (mlx5/ice today, an ENA dynfield is not advertised on
current AWS gen) or peer-side packet capture; both are out-of-scope for
the 2026-05-09 baseline.

| arm | metric_name (post-codex-I2) | what `K / (t1 − t0)` measures | NOT a measurement of |
|---|---|---|---|
| `dpdk_net` | `pmd_handoff_rate_bps` | bytes the application handed to the PMD send ring per second — `t1` captured at `rte_eth_tx_burst`-return | wire rate. The PMD ring + driver-internal queues buffer mbufs; on low-payload / low-iteration counts the value can exceed line rate (e.g. 18 Gbps reported on a 5 Gbps NIC). |
| `linux_kernel` | `write_acceptance_rate_bps` | bytes the application handed to the kernel send buffer per second — `t1` captured after `write_all` returns | wire rate. `write()` returns when the kernel has accepted bytes into the per-socket send buffer; the segment has NOT yet been DMA'd out by the NIC. |
| `fstack` | `write_acceptance_rate_bps` | bytes the application handed to F-Stack's BSD-shaped send buffer per second — `t1` captured after `ff_write` returns | wire rate. `ff_write` accepts into the F-Stack send buffer, returns before the segment reaches `rte_eth_tx_burst`. |

Secondary metrics (`burst_initiation_ns`, `burst_steady_bps`) share the
same handoff-boundary `t_first_wire` / `t1` capture semantics — see
`tools/bench-tx-burst/src/{dpdk,linux,fstack}.rs` module headers.

### History

- **Phase 1 (legacy `bench-vs-mtcp`):** `throughput_per_burst_bps` emitted
  on every arm. Mismatched: linux/fstack figures (8–78 Gbps) far exceeded
  ENA line rate because the label claimed wire-rate but the actual
  measurement was the application-to-kernel buffer accept rate.
- **Phase 2 (T57 follow-up #2, 2026-05-12):** linux + fstack rows renamed
  to `write_acceptance_rate_bps`; dpdk_net kept `throughput_per_burst_bps`
  under the (mistaken) framing that `rte_eth_tx_burst`-return was a
  wire-rate proxy.
- **Phase 3 (codex I2, 2026-05-13):** dpdk_net renamed
  `throughput_per_burst_bps` → `pmd_handoff_rate_bps`. Codex's adversarial
  review pointed out that `rte_eth_tx_burst` returning only means the
  mbuf has been enqueued onto the PMD send ring; the PMD ring and the
  ENA driver-internal queues both buffer, and on low-payload /
  low-iteration counts the figure can exceed line rate. The dpdk_net row
  is therefore an application-to-PMD-handoff rate, not wire rate.

### Source pointers

- Per-arm metric name + calibration accessor: `tools/bench-tx-burst/src/lib.rs`
  (`Stack::throughput_metric_name`, `Stack::throughput_is_wire_rate_calibrated`).
- Per-arm timestamp capture rationale:
  - `tools/bench-tx-burst/src/dpdk.rs` module header (PMD-handoff capture).
  - `tools/bench-tx-burst/src/linux.rs` module header (write-acceptance capture).
  - `tools/bench-tx-burst/src/fstack.rs` module header (ff_write-acceptance capture).
- Per-arm assertion coverage: `tools/bench-tx-burst/tests/burst_grid.rs`
  (`dpdk_arm_emits_pmd_handoff_rate_bps`,
  `linux_arm_emits_write_acceptance_rate_not_pmd_handoff`,
  `fstack_arm_emits_write_acceptance_rate_not_pmd_handoff`).

### Open follow-ups for wire-rate calibration

Wire-rate `K / (t1 − t0)` requires `t1` at the NIC-egress boundary, not
at the application/PMD handoff. Three viable paths:

1. **HW TX timestamps via `rte_mbuf::tx_timestamp` dynfield.** Available
   on mlx5 / ice today; not advertised on AWS ENA on the current bench
   instance (`c6in.metal` ENA gen). A future-gen ENA exposing this
   dynfield enables a `wire_rate_bps` metric on dpdk_net (and, with an
   `Engine::last_tx_hw_ts(conn)` hook, on fstack).
2. **Peer-side packet capture.** Capture egress timestamps at the peer's
   NIC via `tcpdump`/`xdp` and post-process the delta to t0. Requires
   peer-side instrumentation but bypasses NIC dynfield availability.
3. **Kernel `SO_TIMESTAMPING` (`SCM_TSTAMP_SND` or hardware TX).** Linux
   `linux_kernel` arm can opt into the cmsg-style TX timestamps to
   surface a `wire_rate_bps` proxy. Out-of-scope for the 2026-05-09
   baseline; flagged for the post-codex-I2 calibration follow-up.

When any of the above lands, `throughput_is_wire_rate_calibrated()`
flips to `true` for the corresponding arm and a `wire_rate_bps` metric
appears alongside the existing `pmd_handoff_rate_bps` /
`write_acceptance_rate_bps` rows.

## Metrics — bench-rtt (per-RPC RTT)

| arm | metric_name | what it measures |
|---|---|---|
| all | `rtt_ns` | per-RPC application-observed round-trip from `Instant::now()` (or rdtsc) immediately before the send to immediately after the matching receive |

This is a DUT-side wall-clock RTT — no cross-host clock skew. Captures
both DUT TX-path overhead and the peer's echo-server processing
latency. Comparable across arms iff the peer side is held constant
(same peer, same NIC config).

## Metrics — bench-tx-maxtp (sustained-rate W × C grid)

| arm | metric_name | what it measures |
|---|---|---|
| all | `sustained_goodput_bps` | bytes ACKed by the peer per second over the measurement window, derived from the stack's `tx_payload_bytes` counter divided by wall-clock window seconds |

Different shape from `bench-tx-burst` — the maxtp arm uses ACK-gated
goodput (peer-side acknowledgement of payload bytes received) over a
long sustained window, NOT instantaneous send-loop rate. The
ACK-gating makes it a wire-rate proxy in the sense that bytes have
been delivered to the peer's kernel; it does NOT measure NIC-egress
timestamp directly.

## Metrics — bench-rx-burst (per-segment RX latency)

| arm | metric_name | what it measures |
|---|---|---|
| all | `rx_latency_ns` | per-segment latency from the inline TSC capture right after `rte_eth_rx_burst` returns to the application-level handoff to the receive callback |

DUT-side measurement — does NOT include the network path latency.
Captures the engine's RX-path internal latency only.
