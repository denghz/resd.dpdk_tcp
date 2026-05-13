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

## What this suite IS — controlled three-stack harness comparison

The fast-iter suite produces a **controlled three-stack comparison
(dpdk_net + linux_kernel + fstack) with disclosed methodology**, not a
calibrated wire-rate or pure-stack-cost measurement. Every metric is
captured at a user-space handoff boundary defined per-tool in the tables
below; absolute numbers are **end-to-end harness behavior** (DUT stack
+ peer-side scheduling + NIC + driver + AWS shared-tenancy network +
the test driver itself).

Comparisons across stacks within a single suite invocation are the
publication-grade claim. Absolute numbers are useful only as
within-run sanity bounds and **must not** be quoted as portable
stack-performance characteristics.

## What this suite is NOT

The disclaimers below are load-bearing for any reader using the
fast-iter SUMMARY.md or T57-style report. Codex's 2026-05-13 adversarial
review (I5) flagged earlier wordings ("pure stack overhead",
"user-space TCP stack performance", "same peer, same wire") as
overclaims; the methodology described here is narrower.

- **NOT a wire-rate measurement.** `bench-tx-burst` captures `t1` at
  the application-to-PMD or application-to-kernel handoff (see the
  per-arm table below). No arm reads a NIC TX completion timestamp
  today. Wire-rate calibration is gated on HW-TS dynfield availability
  (mlx5/ice today; AWS ENA: not advertised on the current bench
  instance) or peer-side packet capture.
- **NOT a pure software-stack overhead measurement.** Codex review I5
  noted that linux uses blocking `TcpStream::write_all/read_exact`,
  dpdk_net uses `poll_once()` + Readable drain, fstack uses
  nonblocking `ff_write/ff_read` loops — i.e. the harness's
  per-arm API model is part of every number. "Pure stack" deltas
  would require API-model normalization that this suite does not do.
- **NOT a same-NIC comparison.** dpdk_net + fstack drive PCI
  `0000:28:00.0` via vfio-pci; linux_kernel drives `0000:27:00.0`
  via the in-tree ENA driver under `sudo nsenter -t 1 -n`. Both ENIs
  are AWS ENA (vendor `0x1d0f`, device `0xec20`) on the same subnet
  to the same peer, but they have independent IRQ steering, queue
  counts, interrupt-coalescing defaults, and `ethtool -k` offload
  state. Codex B2. Disclosed in T57's "Methodology — two-ENI
  comparison" section; `scripts/log-nic-state.sh` snapshots both
  NICs into every run's `nic-state.txt`.
- **NOT isolated from AWS-shared-network conditions.** Both hosts
  share AWS-tenancy data-plane bandwidth + queue-credit accounting.
  The same code (`61c5e00`) produced ~78-100 µs RTT in T57
  (2026-05-12 09:38 UTC) and ~200-300 µs RTT four hours later in
  T58 — purely environmental (see `regression-diagnosis-2026-05-13.md`).
  Absolute µs values can swing ~3× across an afternoon on the same
  hardware. Ordering across stacks within a run is stable; absolute
  µs across runs is not.
- **NOT isolated from peer-side scheduling.** The peer runs
  pthread-per-conn echo servers with `TCP_NODELAY`. Per-iter RTT
  numbers include the peer's user-space wakeup + epoll/recv +
  send-reply path; that latency is held roughly constant across
  stacks but is non-zero and is part of every `rtt_ns` sample.
- **NOT a deterministic-loss validation environment.** `verify-rack-tlp`
  drives netem-injected loss on the peer side. Loss + reorder +
  delay are stochastic; counter-floor assertions are conservative
  ANY-of (RTO + RACK + TLP > floor) for the loss-only scenarios.
  RACK is validated separately under `rack_reorder_4k` (peer-ingress
  reorder + SACK-on; see T57 B3 repair).
- **NOT a hardware-timestamped measurement.** No arm uses
  `SO_TIMESTAMPING` (kernel) or `tx_timestamp` dynfield (DPDK).
  Cross-host clock skew is bounded by NTP (~100 µs same-AZ); any
  cross-host timestamp delta below ~100 µs is dominated by skew.
  RX-segment latency is a **cross-host** delta `dut_recv_ns −
  peer_send_ns` with both endpoints anchored on `CLOCK_REALTIME`
  (not pure DUT-side internal latency). The ~100 µs NTP skew is
  therefore an absolute-value floor on every `rx_latency_ns`
  sample. See "Metrics — bench-rx-burst" below for the exact
  capture surface and the codex 2026-05-13 re-review correction
  that prompted this disclosure.
- **NOT statistically saturated.** Codex review I3 flagged the
  publication-grade T57 as effectively one run per cell with p50
  only. T58 (variance probe) ran 3 repeats. Confidence intervals,
  p99/p999 in the headline, and randomized stack order (codex I4,
  now in `scripts/fast-iter-suite.sh --seed N`) are post-codex
  follow-ups; the published T57 numbers should be read as a
  single-shot snapshot, not a statistically saturated mean.

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

- **Phase 1 (legacy `bench-vs-mtcp` — historical; the mTCP comparator
  arm was removed in Phase 2 of the bench-overhaul, see plan Task 2.1):**
  `throughput_per_burst_bps` emitted on every arm. Mismatched:
  linux/fstack figures (8–78 Gbps) far exceeded ENA line rate because
  the label claimed wire-rate but the actual measurement was the
  application-to-kernel buffer accept rate.
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

> **Codex 2026-05-13 re-review correction (BLOCKER):** an earlier
> version of this section claimed `rx_latency_ns` was a DUT-side
> internal TSC measurement. That was wrong. The code in
> `tools/bench-rx-burst/src/{segment,dpdk,linux,fstack}.rs`
> computes a cross-host `CLOCK_REALTIME` delta. This section is
> the corrected description.

| arm | metric_name | what it actually measures |
|---|---|---|
| all | `rx_latency_ns` | per-segment **cross-host** latency `dut_recv_ns − peer_send_ns` (saturating-subtract; clamps to 0 when peer_send_ns > dut_recv_ns due to clock skew). `peer_send_ns` is the peer's `CLOCK_REALTIME` ns captured just before `send()` in `tools/bench-e2e/peer/burst-echo-server.c`; `dut_recv_ns` is the DUT's `CLOCK_REALTIME` ns captured immediately after the engine event / `read()` / `ff_read()` returns (`tools/bench-rx-burst/src/segment.rs:15-19`, `dpdk.rs:53-64`, `linux.rs:36-46`, `fstack.rs:48-54`). |

**What this measurement includes** (every `rx_latency_ns` sample
contains all of these — none of them is isolable today):

1. Peer's `clock_gettime(CLOCK_REALTIME)` → `send()` latency on the
   peer host.
2. Peer-side kernel TX path (TCP segmentation, NIC enqueue, driver
   doorbell), NIC TX queue + DMA.
3. AWS shared-tenancy data-plane network latency to the DUT NIC.
4. DUT NIC RX → driver → engine (dpdk_net / fstack) or kernel
   `read()` (linux_kernel).
5. Engine event-dispatch latency to the bench's read callback
   plus the bench-side `clock_gettime(CLOCK_REALTIME)` capture
   call.
6. **NTP offset between the two hosts.** Same-AZ EC2 NTP keeps
   absolute clock offset bounded at ~100 µs; that is the
   absolute-value floor on every sample. The
   `saturating_sub` in `SegmentRecord::new` clamps negative
   deltas (where peer's clock was ahead of DUT's) to zero — so
   the visible sub-100µs tail is partially an artifact of
   clock-skew clamping, not a real latency mode.

**What this measurement does NOT isolate:**

- Pure DUT RX-path internal latency. To get that we would need
  HW RX timestamps (`tx_timestamp`/`rx_timestamp` dynfield on
  the DUT NIC, or `SO_TIMESTAMPING`) plus a coupled peer-side
  HW TX timestamp — neither is available on the current AWS ENA
  bench instance (Phase 9 c7i HW-TS, future work).
- Per-stack absolute RX latency. Because every sample carries
  the same peer + network components, **cross-stack ordering
  on this metric is the publication-grade claim**; absolute µs
  values are *cross-host* end-to-end values bounded below by
  the NTP-skew floor.

**How to read `rx_latency_ns` correctly:**

- Cross-stack **ordering** within a single suite invocation
  (e.g. "fstack p50 < dpdk_net p50 < linux_kernel p50") is the
  intended signal — peer + network components cancel since the
  same peer is hit by all three arms in the same run.
- **Absolute µs values are cross-host end-to-end** and include
  peer-send timestamp, peer-host NIC TX, AWS data-plane
  transit, DUT NIC RX, and an NTP-bounded clock-offset term.
  Do **not** quote `rx_latency_ns` p50/p99 as a DUT-stack RX
  cost.
- p999 tails on linux_kernel (~1.4–2.5 ms) come from kernel
  TCP retransmit + scheduler-quantum tails that this metric
  *does* expose, but they include the same peer-side and NTP
  components as the bulk distribution.

## Stack-order randomization (codex IMPORTANT I4, 2026-05-13)

The 2026-05-09 fast-iter-suite originally ran `dpdk_net → linux_kernel →
fstack` in a fixed order for every arm of every tool. Codex's 2026-05-13
adversarial review (IMPORTANT I4) flagged that AWS ENA's bandwidth-
allowance / burst-credit accounting drains over the ~35-min suite
wallclock, so the third-run stack was systematically disadvantaged — and
T58 variance runs also showed ~2-3× environmental drift over a single
suite run on the same code, confirming that fixed-order comparisons
carried a built-in order bias.

Fix: `scripts/fast-iter-suite.sh` now randomizes the per-tool stack
execution order. The order is derived from a master `$SEED` (CLI flag
`--seed N`, default current epoch); per-tool seeds are `SEED + tool_index`
so a single seed reproduces the full 4×3 order matrix deterministically.
The resolved order is logged into `$RESULTS_DIR/metadata.json` (machine-
readable) and rendered as a "Stack-order matrix" table in
`$RESULTS_DIR/SUMMARY.md` (reviewer-readable).

Smoke verification: `./scripts/fast-iter-suite.sh --seed N --dry-run`
prints the planned matrix without invoking any bench, so reviewers can
sanity-check the seed → order mapping in a few seconds.

Implication for comparisons: cross-stack deltas in a single run are now
order-symmetric in expectation (no stack is systematically last); for
absolute-numbers-grade comparisons, average over multiple runs with
different seeds to marginalize out the per-run order effect.
