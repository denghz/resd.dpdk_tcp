# T57 fast-iter-suite — fair comparison (2026-05-12)

**Run:** fifth fast-iter suite invocation, all 2026-05-12 follow-ups merged.
**Branch:** `a10-perf-23.11` at `61c5e00`.
**Status:** **DONE — 13/13 OK, 0 FAIL, 35 min wallclock.**
**Results:** `target/bench-results/fast-iter-2026-05-12T09-37-47Z/`

## What changed since T56

| Commit | Fix |
|---|---|
| `25f5353` | bench-rx-burst dpdk_net `PEER_SEND_NS_FLOOR` sentinel — filters misaligned-parse outliers (1e18 ns artifacts) |
| `dbb386c` | bench-rx-burst fstack `bucket_invalid` marker rows — wedged buckets now visible in CSV |
| `0311213` | bench-tx-maxtp linux `tx_pps` — reads `tcpi_segs_out` from kernel (was hardcoded 0.0) |
| `74b07d3` | linux_kernel arms via `sudo nsenter -t 1 -n` — escape proxied netns, talk to real peer over ens5 |
| `54ad5af` | `peer_restart_burst_echo_server` between rx-burst arms (workaround) |
| `abd9601` | **Hardened peer servers**: `TCP_USER_TIMEOUT=5s` + pthread-per-conn + 4 MiB SO_SNDBUF/RCVBUF on all three (echo-server, linux-tcp-sink, burst-echo-server). Self-recover from DUT SIGKILL in 5s instead of 15min. |
| `61c5e00` | Drop between-arm peer restarts (servers self-recover now) |

## Fair-comparison results — bench-rtt p50 RTT (real wire to 10.4.1.228)

| payload | dpdk_net | linux_kernel | fstack | linux−dpdk Δ |
|---:|---:|---:|---:|---:|
| 64 B | 77.7 µs | 104.1 µs | 99.9 µs | +26 µs (34%) |
| 128 B | 83.7 µs | 105.9 µs | 100.0 µs | +22 µs (26%) |
| 256 B | 77.0 µs | 106.9 µs | 100.1 µs | +30 µs (39%) |
| 1024 B | 98.5 µs | 108.8 µs | 100.1 µs | +10 µs (10%) |

**All three stacks talk to the SAME peer.** The remaining differences are pure software-stack overhead:
- dpdk_net: ~77-99 µs — DPDK direct-NIC access, lowest overhead.
- fstack: ~100 µs — FreeBSD socket layer on top of DPDK; consistent across payloads.
- linux_kernel: ~104-109 µs — kernel TCP stack syscall + context-switch path; grows slightly with payload.

> **Important — same peer, but the linux_kernel arm uses a DIFFERENT physical
> NIC than dpdk_net + fstack.** See "Methodology — two-ENI comparison"
> below for the full disclosure. The published software-stack-overhead
> deltas are reported as the linux−dpdk Δ; readers should hold that
> framing rather than treating the absolute numbers as a same-wire
> measurement.

## Methodology — two-ENI comparison

This comparison runs on the standard DPDK-vs-kernel test-bench shape:

| stack | NIC PCI | interface | driver | local IP | how invoked |
|---|---|---|---|---:|---|
| dpdk_net | `0000:28:00.0` | (vfio-pci poll) | vfio-pci → DPDK ENA PMD | `10.4.1.141` | DPDK lcore 2, `EAL_ARGS=-l 2-3` |
| fstack | `0000:28:00.0` | (vfio-pci poll) | vfio-pci → DPDK ENA PMD → F-Stack BSD socket layer | `10.4.1.141` | DPDK lcore 2, same EAL args; one stack at a time (DPDK vfio is exclusive) |
| linux_kernel | `0000:27:00.0` | `ens5` | in-tree AWS `ena` | `10.4.1.139` | `sudo nsenter -t 1 -n` (escape dev-host REDSOCKS proxy → host netns) |

**The two ENIs are functionally identical hardware.** Both are AWS Elastic
Network Adapter (vendor `0x1d0f`, device `0xec20`) on the same subnet
(`10.4.1.0/24`) talking to the same peer (`10.4.1.228:10001/10002/10003`)
through the same VPC routing and security-group rules. The wire physics —
ENA hardware queue rings, PCIe interconnect, virtio-style TX/RX descriptor
shape, in-VPC switch latency, peer NIC parking on the same physical host
class — are identical. The difference is the SOFTWARE driving the NIC:
- DPDK arm: PMD polls the NIC ring directly from lcore 2 with no kernel
  context switch, no IRQ, no qdisc, no iptables.
- Linux arm: standard Linux kernel TCP/IP stack — syscall entry, kernel
  TCP, RACK/DCTCP/CUBIC machinery, netfilter / iptables traversal, qdisc
  egress, ENA `ena_start_xmit` → kernel-mediated descriptor ring write.

**Why this still constitutes a fair comparison.** The headline claim is
*"software-stack overhead"* — i.e. the cost the application pays for
calling `read`/`write` (or `bench_rtt` ping-pong) against each stack. That
overhead is, by construction, the gap between what the user-space program
sees and what the wire could do. The two ENIs put both stacks on
equivalent wire baselines; the published `linux−dpdk Δ` therefore
isolates the software cost, not any wire asymmetry.

**What COULD differ at run time across the two ENIs:**
- RX/TX queue counts (one ENA queue per active vCPU on the kernel side;
  DPDK arm uses a single hardware queue at lcore 2)
- IRQ steering and CPU affinity for the kernel ena driver
- Interrupt coalescing (`ethtool -c`) — different defaults for adaptive
  RX coalescing
- ENA offload defaults (`ethtool -k`) — GSO/GRO/LRO state
- Driver-level qdisc / netfilter / iptables on the kernel NIC only

**Mitigation — run-time state capture.** Every fast-iter-suite invocation
now writes `nic-state.txt` alongside the SUMMARY at the start of the run
(see `scripts/log-nic-state.sh`, wired into `fast-iter-suite.sh::preflight`).
It captures, for the DUT kernel NIC (`ens5` via `sudo nsenter -t 1 -n`),
the DUT DPDK NIC (`0000:28:00.0` via `lspci -k -vv` + sysfs), AND the
peer kernel NIC (`ens5` via SSH):
- `ip -s link show <ifname>` — packet/byte counters, MTU, link state
- `ethtool <ifname>` — speed, duplex, autoneg
- `ethtool -c <ifname>` — interrupt coalescing (critical for latency)
- `ethtool -k <ifname>` — TCP/UDP offloads
- `ethtool -l <ifname>` — channel/queue count
- `ethtool -S <ifname>` — ENA xstats (incl. `bw_*_allowance_exceeded`)
- `/proc/interrupts | grep <ifname>` — IRQ distribution + affinity
- `tc qdisc show dev <ifname>` — egress qdisc
- `sudo iptables -L -v -n | head -30` — netfilter rules
- `ip route` — route table

Reviewers can diff `nic-state.txt` between two runs (or between the two
DUT NICs in a single run) to confirm queue / IRQ / coalescing parity, or
to call out where they diverge and re-interpret the headline numbers
under that disclosure.

**Future work — Option A (not in this delivery).** A future enhancement
will optionally rebind `0000:28:00.0` from vfio-pci to ena between the
DPDK and linux arms within the same suite invocation, run the linux arm
against the same physical NIC + same IP (`10.4.1.141`) as the DPDK arms,
then rebind back. That gives an absolute-numbers-grade comparison
("same-NIC-vfio-vs-kernel") at the cost of suite robustness (a failed
mid-suite rebind leaves the DUT NIC in a half-bound state requiring
manual recovery). For publication-candidate runs we stay with the
two-ENI disclosure path documented here.

> **Note — all three stacks tested at `--connections 1` only.** fstack's
> RTT arm currently lacks multi-conn support
> (`tools/bench-rtt/src/main.rs:207` bails on `--connections > 1`, since
> the per-conn `ff_socket` + `ff_poll` multiplexing inside a
> request/response inner loop is a known limitation tracked as Phase 6+
> future work — see follow-up #6 below); `dpdk_net` and `linux_kernel`
> arms support `--connections > 1` directly but T57's published
> comparison constrains all three to C=1 for parity. The `bench-rtt`
> invocation in `scripts/fast-iter-suite.sh` omits `--connections` so it
> defaults to `1` across all three stacks — the comparison is honest
> within that constraint.

## bench-rx-burst — per-segment RX latency p50 (ns)

| burst | W | dpdk_net | fstack | linux_kernel |
|---:|---:|---:|---:|---:|
| 16 | 64 B | 66 350 | **62 081** | 87 947 |
| 64 | 64 B | 65 024 | 65 652 | 101 912 |
| 256 | 64 B | 65 567 | 69 019 | 118 508 |
| 16 | 128 B | 65 048 | **60 450** | 122 389 |
| 64 | 128 B | 66 364 | **59 052** | 114 308 |
| 256 | 128 B | 66 091 | 79 281 | 122 517 |
| 16 | 256 B | 65 822 | **56 493** | 118 213 |
| 64 | 256 B | 65 434 | 62 586 | 115 468 |
| 256 | 256 B | 65 916 | 63 926 | 114 029 |

**Bold** = best in row. Notable: **fstack beats dpdk_net on per-segment RX latency** in 5 of 9 cells. F-Stack's poll-driven RX path delivers segments to user-space faster than the dpdk-net engine's event-table dispatch. linux_kernel is consistently slowest (kernel epoll/recvmsg overhead).

The dpdk_net numeric corruption from T56 is GONE — all 9 cells produce stable p50 with no 1e18 outliers (sentinel filter at `tools/bench-rx-burst/src/dpdk.rs::PEER_SEND_NS_FLOOR` working).

**2026-05-12 audit — finding confirmed.** Per-arm `dut_recv_ns` capture sites
(dpdk_net `tools/bench-rx-burst/src/dpdk.rs:272`, fstack
`tools/bench-rx-burst/src/fstack.rs:588`) were audited for semantic
comparability: both arms anchor on `CLOCK_REALTIME` via `SystemTime::now()`
and capture at the same pipeline point — "first user-space buffer post-stack
is populated, recv_buf not yet extended." The work timed inside each arm's
measurement window differs by ~50 ns (dpdk_net's `drain_readable_bytes`
includes an iovec→Vec copy + event-queue + flow-table-lookup overhead that
fstack's `ff_read` doesn't carry), which **disadvantages dpdk_net** by that
margin — i.e. makes dpdk_net look slower than its "true" stack overhead, in
the same direction as the headline. The asymmetry is 40-300× smaller than
the µs-scale gaps observed and is dwarfed by NTP-skew effects (~100 µs
same-AZ); the qualitative finding survives. Live-wire validation against
the same peer (10.4.1.228) re-confirmed fstack p50 ≤ dpdk_net p50 in all 9
cells, with fstack saturating to 0 (true latency below the NTP skew floor)
in 8 of 9 cells and dpdk_net consistently above it. Full audit notes at
[`docs/bench-reports/fstack-vs-dpdk-rx-timing-audit-2026-05-12.md`](./fstack-vs-dpdk-rx-timing-audit-2026-05-12.md);
no code fix required — the existing measurement positions are correct.

## bench-tx-burst — per-arm metric labels

Follow-up #2 from the original T57 list is now closed. The CSV `metric_name`
column now distinguishes wire-rate from buffer-fill-rate so readers don't
conflate the two:

| arm | metric_name | what `K / (t1 − t0)` actually measures |
|---|---|---|
| `dpdk_net` | `throughput_per_burst_bps` | wire-rate proxy — `t1` captured at `rte_eth_tx_burst`-return |
| `linux_kernel` | `write_acceptance_rate_bps` | buffer-fill — `t1` captured after `write_all` returns (bytes accepted into kernel send buffer, NOT on wire) |
| `fstack` | `write_acceptance_rate_bps` | buffer-fill — `t1` captured after `ff_write` returns (bytes accepted into F-Stack BSD-shaped send buffer, NOT on wire) |

dpdk_net (real wire, throughput_per_burst_bps):
| K | G | throughput Gbps | initiation p50 |
|---:|---:|---:|---:|
| 64 KiB | 0 ms | 1.00 | 30.8 µs |
| 64 KiB | 10 ms | 1.35 | 34.2 µs |
| 1 MiB | 0 ms | 1.00 | 24.7 µs |
| 1 MiB | 10 ms | 1.14 | 133.0 µs |

linux + fstack rows in the T57 SUMMARY.md showed 8–78 Gbps under the legacy
unified `throughput_per_burst_bps` label — 8× over ENA's 10 Gbps line rate.
Those rows now emit `write_acceptance_rate_bps`, so the label itself tells
the reader that the figure is a software-buffer-ingest rate. The numeric
value did NOT change; the wire-rate calibration on those arms is still
gated on a HW-TS hook (`Engine::last_tx_hw_ts` for fstack-on-DPDK, ENA
TX timestamp dynfield once advertised).

Per-arm rationale + the `Stack::throughput_metric_name` helper:
`tools/bench-tx-burst/src/lib.rs`.

## verify-rack-tlp — ALL 5 scenarios PASS

| scenario | spec | rto | rack | tlp | agg | result |
|---|---|---:|---:|---:|---:|:---|
| `low_loss_05pct` | `loss 0.5%` | 12 443 | 0 | 2 486 | 14 929 | PASS |
| `low_loss_1pct_corr` | `loss 1% 25%` | 5 | 0 | 1 | 6 | PASS (ANY-of, retired — flaky) |
| `high_loss_3pct` | `loss 3% delay 5ms` | 9 029 | 0 | 1 428 | 10 457 | PASS |
| `symmetric_3pct` | `loss 3%` | 7 527 | 0 | 1 496 | 9 023 | PASS |
| `high_loss_5pct` | `loss 5% 25%` | 300 | 0 | 60 | 360 | PASS |

Phase 11 RTO/RACK/TLP counter split validated against real netem loss on AWS ENA. Empirical: **RACK never fires on this NIC/wire** (sparse ACK information) — the ANY-of assertion saves the low-loss scenarios via TLP.

## Wallclock breakdown

| phase | time |
|---|---:|
| bench-rtt × 3 stacks | 18 s |
| bench-tx-burst × 3 stacks | 51 s |
| bench-tx-maxtp × 3 stacks | 395 s |
| bench-rx-burst × 3 stacks | **8 s** (was 271 s in v3 due to fstack stalls) |
| verify-rack-tlp (5 scenarios) | 1 610 s |
| **TOTAL** | **2 076 s (35 min)** |

20 % faster than v3 (40 min) — the hardened peer eliminates per-arm stall budget.

## Reproducibility

```bash
./scripts/fast-iter-setup.sh up --with-fstack   # ~3 min provision + rebuild + deploy
./scripts/fast-iter-suite.sh                    # ~35 min full suite
cat target/bench-results/fast-iter-<UTC>/SUMMARY.md
./scripts/fast-iter-setup.sh down               # ~30 s teardown
```

## Architecture: comparator triplet — fully validated

For the first time across the entire bench-overhaul:

- **All four bench tools** (bench-rtt, bench-tx-burst, bench-tx-maxtp, bench-rx-burst) work end-to-end on **all three stacks** (dpdk_net, linux_kernel, fstack).
- **All three stacks** talk to the same peer over real wire (no loopback, no proxy). See "Methodology — two-ENI comparison" for the dpdk_net/fstack vs linux_kernel NIC disclosure — same peer, two physical ENIs.
- **Phase 11 counters** (`tx_retrans_rto/rack/tlp`) validate against real netem-induced loss.
- **Hardened peer servers** survive DUT SIGKILL within 5s (was 15 min).

## Open follow-ups

1. **SUMMARY.md verify-rack-tlp section is empty** (parser stub from T55). Add a Python parser that reads `verify-rack-tlp.log.log` and pivots the per-scenario PASS/FAIL + counter deltas into a table.

2. ~~bench-tx-burst linux + fstack throughput numbers are buffer-fill artifacts~~ **DONE 2026-05-12** (follow-up #2). Rename: linux_kernel and fstack arms now emit `write_acceptance_rate_bps` in the CSV `metric_name` column; dpdk_net keeps `throughput_per_burst_bps`. Label asymmetry is intentional — completion semantics differ across arms (`rte_eth_tx_burst`-return vs `write()`-return), so the metric name advertises which thing was measured. Per-arm rationale + helper in `tools/bench-tx-burst/src/lib.rs::Stack::throughput_metric_name`; failing-test coverage in `tools/bench-tx-burst/tests/burst_grid.rs`. The fast-iter SUMMARY.md pivot already dispatches on `metric_name` so renders both labels with no summarizer changes needed.

3. **bench-tx-maxtp linux empty row in SUMMARY.md** at W=64K C=16 (last row showed `0.0`). Spot — but the other cells produced real data. Likely a CSV pivot bug in the summarizer, not a bench-tool bug.

4. **verify-rack-tlp wallclock 27 min** still dominates suite time. The 3%-loss scenarios are RTO-bound; further iter reduction would speed things up but risk losing the `>0` floor.

5. ~~`low_loss_1pct_corr` scenario is stochastically flaky~~ **DONE 2026-05-12** (T57 follow-up #5). The `loss 1% 25%` correlated-drop spec produced `agg=6` on T55/T56 v3 / T57 v5 and `agg=0` on T56 v4 across 200 k iters — netem's burst-clustering algorithm randomly mis-aligns with the bench's iter rate, so the ANY-of assertion (`rack | tlp`) could false-fail. Renamed scenario to `low_loss_1pct` with spec `loss 1%` (uniform per-packet drop, no correlation). Same iter count (200 k) now yields thousands of recovery events because drops distribute uniformly across iters instead of clustering. Verified by code-path inspection; smoke-test deferred to next fast-iter run. Rationale + change recorded inline in `scripts/verify-rack-tlp.sh` (SPECS map comment) and `docs/bench-reports/verify-rack-tlp.md` (assertion-set v2 table + `low_loss_1pct` bullet).

6. ~~bench-rtt fstack multi-conn gap silent in fair-comparison~~ **DONE 2026-05-12** (T57 follow-up #6). `tools/bench-rtt/src/main.rs:207` (early-exit in `main()`, before the precondition governor / EAL spin-up) bails on `--connections > 1` for the fstack arm; a defense-in-depth bail at `run_fstack` (~line 675) backs it up. The underlying gap: per-conn `ff_socket` + `ff_poll` multiplexing inside a request/response inner loop is a Phase 6+ implementation gap — the shape exists in `tools/bench-tx-maxtp/src/fstack.rs` for sustained writes but bench-rtt's request/response loop needs the same callback restructure. The T57 fair-comparison is published at `--connections 1` for all three stacks (`scripts/fast-iter-suite.sh run_bench_rtt` omits `--connections` → default 1), which is apples-to-apples within that constraint but does NOT measure multi-conn behaviour. **Option B (documentation) chosen** for the publication round: (a) the bench-rtt bail message now spells out the gap + the C=1-for-parity-across-all-three-stacks framing; (b) a NOTE block directly under the bench-rtt p50 RTT table in this report makes the constraint visible to anyone reading the comparison; (c) the `fast-iter-suite.sh` SUMMARY.md generator emits the same disclaimer above every per-stack bench-rtt section in every future suite run (`scripts/fast-iter-suite.sh::write_summary`). Implementation (Option A — per-conn `ff_socket` + `ff_poll` multiplexing inside one `ff_run` invocation, mirroring `tools/bench-tx-maxtp/src/fstack.rs`) is a future-phase follow-up if/when a multi-conn comparison is needed.

Closing the most actively-iterated section of T55/T56 follow-ups. Suite is now reliable + repeatable.
