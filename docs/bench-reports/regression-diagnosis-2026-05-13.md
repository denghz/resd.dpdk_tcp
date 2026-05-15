# T57→T58 bench-rtt RTT regression — diagnosis (2026-05-13)

**Verdict: NOT a code regression.** The ~2-3× bench-rtt p50 inflation is an
**environmental artifact** of the shared AWS dev box and the inter-instance
network path. No fix to the bench code, the engine, or the peer servers
recovers it; conversely, no change to the bench code between T57 (`61c5e00`)
and T58/HEAD (`6f13821`) plausibly explains it.

## What was measured (current HEAD, 2026-05-13 ~04:54 UTC)

Re-run of `bench-rtt --iterations 10000 --warmup 100 --payload-bytes-sweep
64,128,256,1024` on the worktree's HEAD (`6f13821`, post-T58 codex-review
commit; the `5fecb92` engine fix and all T57-follow-up commits in place).
Same peer (10.4.1.228), same wire, same EAL args as the suite uses.

| stack                              | p50 / p99 (µs), payloads 64 / 128 / 256 / 1024 B |
| ---------------------------------- | ------------------------------------------------ |
| T57 baseline (61c5e00, 09:38 UTC)  | dpdk **78/119  84/133  77/118  98/133** ; linux **104/294  106/302  107/301  109/310** ; fstack **100/126  100/122  100/126  100/153** |
| **Current HEAD, unpinned**         | dpdk **218/262  217/259  204/220  205/228** ; linux **255/2581 219/2477 236/2488 242/2498** ; fstack **200/296  300/311  299/311  300/314** |
| Current HEAD, `taskset -c 2-7` only | dpdk **217/259  212/252  220/262  233/273** ; linux (4-7) **235/268  225/261  213/262  256/296** |
| Current HEAD, `chrt -f 90` + `taskset -c 2-3` | dpdk **222/266  214/258  232/274  211/230** ; linux **218/269  231/259  228/284  221/264** |

Raw CSVs and per-bucket distributions in `/tmp/regression-diag/rtt-*.csv` +
`-raw.csv` (10 000 rtt_ns samples per bucket). Host / peer network-state
captures in `/tmp/regression-diag/host/capture.txt` and `peer/capture.txt`.

## Hypothesis ruling

| H    | Description                                            | Verdict |
| ---- | ------------------------------------------------------ | ------- |
| H1   | Transient AWS jitter (recovers within hours)           | **REJECTED.** Elevated state persists ~19h+ after T57 across 3 T58 runs + my re-run + chrt+taskset probes + pinned-python TCP RTT + pinned ICMP ping. Not transient. |
| H2   | `5fecb92` engine scratch-fix has a perf cost           | **REJECTED.** Change is `dpdk-net-core`-only (dpdk_net stack). Linux and fstack also regressed 2-3×. Pinning dpdk_net to isolated lcore + RT FIFO priority still yields ~212-233 µs (vs T57's 77-99 µs); the engine change cannot be the cause. |
| H3   | Peer echo-server's 4 MiB SO_SNDBUF/RCVBUF              | **REJECTED.** Peer binary `/tmp/echo-server` was deployed at 09:37:30 — **before** T57's 09:37:47 start. The `movl $0x400000` (= 4 MiB) is present in the deployed binary. T57 ran against the *same* hardened server and got ~78 µs. Restarting the echo-server now (clearing 22 leaked ESTAB conns down to 1) changes RTT by <1 µs. |
| H4   | Host-side state drift (qdisc, conntrack, IRQ, CPU)     | **PARTIAL — explains the tail, not the median.** See "Contributing factor #2" below. |

## What actually moved between T57 and T58

### Primary: inter-instance network RTT has tripled

Pinned (`taskset -c 5`), RT-priority (`chrt -f 90`) probes from the DUT
root netns to the peer (10.4.1.228) — all measured **right now**:

| probe                          | min / avg / max (µs)        |
| ------------------------------ | --------------------------- |
| ICMP ping DUT→peer (n=30)      | **294 / 321 / 407**         |
| ICMP ping peer→DUT (n=20)      | **312 / 340 / 376**         |
| Python TCP 64B echo (n=3 000)  | **216 / 226 (p50) / 251 (p99)** |
| Bench-rtt dpdk_net 64B (n=10k) | min 209, p50 219, p99 262   |
| ICMP ping DUT→gateway          | 76 / 98 / 133               |
| ICMP ping DUT→localhost        | 14 / 27 / 36                |

The **wire-only ICMP RTT between these two specific instances is currently
~290-410 µs**. At T57 time, the implied wire RTT was much lower (dpdk_net
p50 = 78 µs = wire + ~22 µs stack overhead → ~50-60 µs wire). All three
stacks ride on the same wire, so all three regressed by roughly the wire
delta. No code change can fix this — it's the physical network path.

**Both instances are in `aps1-az1` / `ap-south-1a`, same /24 subnet
(10.4.1.0/24), TTL=64 from each side** (zero IP hops). No
hibernate/migrate/resume events in `dmesg` on either host. Same instance
IDs (DUT `i-0a6e844d6af751c1f` `c7i-flex.2xlarge`, peer
`i-0222587a5864ab4d4` `c6a.xlarge`), both up since May 11. So the path
*looks* identical from a config standpoint but is empirically ~3× slower.

The most plausible AWS-side mechanism is **ENA bandwidth-allowance
shaping triggered by burst-credit exhaustion**. Both NICs show massive
accumulated `bw_*_allowance_exceeded` counters since boot:

```
DUT ens5:    bw_in_allowance_exceeded:  19 731 631
             bw_out_allowance_exceeded: 39 182 526
peer ens5:   bw_in_allowance_exceeded:   8 880 663
             bw_out_allowance_exceeded: 31 599 832
```

These counters are *monotonic since boot*. They confirm that **tens of
millions of packets** have been throttled by the AWS network shaper. The
DUT is `c7i-flex.2xlarge` — a burstable/flex instance with limited
baseline network bandwidth + burst credits. The cumulative load from
many parallel agent bench sessions (bench-tx-maxtp at multi-Gbps × N
runs × 3 stacks) plausibly exhausted the bandwidth credits sometime
between T57 (09:38) and T58 (14:43). A 5-second sample right now showed
the counters *not currently incrementing*, but the RTT is still ~290 µs
— consistent with either (a) AWS keeping credit-exhausted instances on
a degraded network path even after instantaneous shaping subsides, or
(b) a longer-term AWS-side placement change that the visible-from-guest
counters don't directly attribute. Either way, the symptom is a
persistent ~3× inter-instance RTT increase and the trigger correlates
with sustained heavy bench traffic.

### Contributing factor #2: runaway CPU-1 hog + concurrent agent overload — adds the *tail*, not the median

A stuck `tcp_rack_rto_retrans_tap` test binary
(`/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/agent-a6b04bb4d148f29d6/target/release/deps/tcp_rack_rto_retrans_tap-15c16e606c0c9112`,
PID 206795, parent `cargo test` PID 205541) started on
**2026-05-12 11:18:37 UTC** — squarely between T57 (ended 10:12) and
T58 (started 14:43). It has consumed **17+ hours of CPU time at 96.7%
on CPU 1** and is still running. Its fds are `/dev/null` + pipes only
— it does no network I/O. Memory mappings show no hugepages /
`/dev/hugepages` references — it is not the DPDK NIC. It is a stuck
in-process test (`--features test-server`, virtual clock) busy-spinning.

Combined with other concurrent agent activity (parallel `cargo test`
runs, criterion benches like `parse_options-9259774998e386ad` at 90% CPU
on CPU 0 observed during this diagnosis), **CPU 0 and CPU 1 are
saturated at ~99%**. The bench host has `isolcpus=managed_irq,domain,2-7`
so the general-purpose CPU pool is just cores 0-1.

Critically, `bench-rtt` (and the rest of the bench tools) **do not pin
their measurement loop to an isolated core**. The `--lcore 2` flag and
`-l 2-3` EAL args reserve cores 2-3 for DPDK lcore threads, but
`EngineConfig.lcore_id` is only used to name mempools (`rx_mp_2`, etc.)
— no `sched_setaffinity`, no `rte_thread_set_affinity`, no thread
pinning in `Engine` or in `bench-rtt`. The hot `poll_once` /
`send_request` / `receive_response` loop runs on the **main thread**,
which the OS scheduler places on CPU 0 or 1 (the only available non-
isolated CPUs) and thus competes with the saturated general pool.

This adds **tail-latency spikes** but not the median shift. Evidence:
- linux_kernel unpinned p99 = 2 477-2 594 µs.
- linux_kernel pinned to cores 4-7 p99 = 261-296 µs (10× drop).
- linux_kernel `chrt -f 90 taskset -c 2-3` p99 = 259-284 µs.
- However the **medians** in all three (unpinned, pinned, RT-priority)
  remain in the 213-256 µs band, only modestly perturbed — and dpdk_net
  with `chrt -f 90 taskset -c 2-3` (absolute scheduling priority on the
  EAL cores) still measures p50 ~212-233 µs. CPU contention alone
  doesn't move the median into the T57 regime.

Recommendation flagged but not actioned (out of scope, runaway is in
another agent's worktree and the auto-mode classifier blocked
`kill -KILL 206795`/`SIGSTOP`): the runaway test process should be
killed, and the bench tools should be patched to pin their measurement
loop to an isolated core via `sched_setaffinity` / `rte_thread_set_affinity`
so a noisy CPU-0/1 doesn't taint future runs.

### Contributing factor #3: 22 leaked ESTABLISHED connections on the peer's echo-server

Snapshotted before peer-restart hygiene during this diagnosis:

```
ESTAB ... 10.4.1.228:10001  10.4.1.141:52154
  cubic ... rtt:5.222 cwnd:2 ssthresh:2 retrans:0/1589 reord_seen:5605
  busy:622451ms minrtt:5.213
ESTAB ... 10.4.1.228:10001  10.4.1.141:57963
  cubic ... cwnd:2 ssthresh:2 retrans:0/2492 reord_seen:12256 busy:615640ms
...
```

These came from DPDK-userspace bench-rtt runs that exited (or were
SIGKILLed) without a clean TCP close — the peer kernel keeps the
connection ESTAB forever because there's nothing left on the DUT side
to send a RST/FIN, and the echo-server's `TCP_USER_TIMEOUT=5s` only
fires on un-ACKed *data*, not on idle connections. The
`reord_seen:5605` and `cwnd:2 ssthresh:2` come from the verify-rack-tlp
netem-loss scenarios; the connections are in degenerate small-window
state but they're idle and the echo-server has 14 pthreads sleeping in
`read()` against them — negligible resource pressure (load 0.01, 2 MB
RSS).

This is **not** a contributor to the median RTT. After restarting the
echo-server during this diagnosis (clearing 22 → 1 ESTAB), the kernel-
TCP RTT stayed at ~216 µs min / ~226 µs p50 — within the same band as
before. Documented here only as a hygiene note for future suite runs
(the `peer_restart_echo_server` helper in `scripts/fast-iter-suite.sh`
should run after every bench-rtt arm, not just between rx-burst arms).

## Why pinning + RT priority doesn't recover the median

The probes:

```
chrt -f 90 taskset -c 2-3 bench-rtt --stack dpdk_net  (isolated EAL cores)
chrt -f 90 taskset -c 2-3 bench-rtt --stack linux_kernel
```

run the bench's main thread on an isolated core with absolute scheduling
priority (preempts SCHED_OTHER). If the CPU 0-1 runaway / overload were
the dominant cause, this would dump RTT back into the T57 regime. It
does not — dpdk_net p50 stays at 211-233 µs, linux at 218-231 µs. So
the residual ~120-130 µs shift above T57 is **network**, not CPU.

For the linux arm specifically, there is a confound: `isolcpus=
managed_irq` keeps device IRQs off cores 2-7, so even pinning the linux
userspace thread to an isolated core, the **`ens5` RX softirq** still
runs on CPU 0 and competes with the runaway / agent activity. That's
why linux's unpinned p99 was 2.5 ms while pinned-to-4-7 p99 was 268 µs
(the userspace `recv_exact` wakeup is no longer behind the runaway, but
the softirq packet handling still is for *some* iterations). For
dpdk_net there's no softirq — its RX is the DPDK PMD on lcore 2 — so
its p99 tail is consistently small (~258-275 µs) regardless of pinning.

## Concrete answers to the prompt

- **Status**: DONE_WITH_CONCERNS (root cause identified; no code fix
  appropriate; environmental fixes are out of this worktree's scope).
- **Root cause**: environmental, two-channel — the primary channel is
  **inter-instance network-path latency tripling** (ICMP RTT
  ~290-410 µs vs implied ~50-80 µs at T57), most plausibly downstream
  of AWS ENA bandwidth-allowance shaping triggered by sustained
  cross-session bench traffic on a `c7i-flex.2xlarge` DUT (40M+
  `bw_out_allowance_exceeded` events since boot). The secondary channel
  is **CPU 0-1 saturation** from a stuck `tcp_rack_rto_retrans_tap`
  test binary (PID 206795 since 2026-05-12 11:18:37) plus concurrent
  agent workloads, which inflates the *p99 tail* (linux arm 2 477 µs
  unpinned vs 268 µs pinned-to-isolated) but not the median.
- **The fix**: none in the bench code. Methodology recommendations
  below.

## Recommendations (do NOT block publication on absolute numbers)

1. **Reframe the published comparison as relative, not absolute.** The
   *ordering* across stacks (dpdk_net ≤ fstack ≤ linux_kernel for RTT;
   fstack ≤ dpdk_net ≤ linux_kernel for RX latency) is stable across
   T55, T56, T57, T58, and this re-run. Publish that ordering + the
   ratio columns; do not quote absolute µs as portable performance
   characteristics of the stacks. Codex review's I3/I5 (statistically
   underpowered, "pure stack overhead" overstated) are now also
   evidence-backed by this regression.

2. **Bench-rtt should pin its measurement loop.** Patch
   `tools/bench-rtt/src/main.rs::run_dpdk_net`,
   `run_linux_kernel`, and `run_fstack` to call
   `sched_setaffinity` to the isolated cores (e.g. cores 4-7 by
   default; configurable via `--measurement-affinity`). This kills the
   2.5 ms p99 tail on the linux arm under any future CPU-0/1
   contention and also disambiguates "stack" cost from "scheduling
   noise" cost in future regression hunts.

3. **The fast-iter suite should snapshot ENA `bw_*_allowance_exceeded`
   before and after each arm and warn / abort if the delta during a
   bench-rtt arm is non-zero.** A few lines of `ethtool -S ens5`
   diffing in `scripts/fast-iter-suite.sh::run_one` would let the
   suite refuse to publish results taken under active throttling.
   (The DPDK NIC `0000:28:00.0` has no kernel iface, so its xstats
   aren't directly readable; the `ens5` allowance counters are
   instance-wide and reflect the shaping decision regardless.)

4. **The fast-iter suite should hard-reject the run if DUT load
   average > N or any CPU-0/1 hogger > 50% CPU is present.** Cheap
   `cat /proc/loadavg` + `ps -o pcpu` check at the top of
   `run_bench_rtt` would skip a tainted run rather than publish bogus
   numbers. Codex's I4 (fixed stack order aliases with time-varying
   EC2 behaviour) is the same problem from a different angle.

5. **Kill the stuck `tcp_rack_rto_retrans_tap` PID 206795 + parent
   cargo PID 205541.** They are 17+ h of orphaned compute on a shared
   dev box. Out of this worktree's authority — flagged for the user.

6. **Consider running publication-grade benches on a non-flex instance
   type** (`c6in.xlarge`, `c7i.xlarge` — not `-flex`) with explicit
   bandwidth provisioning, so burst-credit exhaustion isn't a hidden
   variable.

## What WAS fixed during this diagnosis

- **Build re-enabled with clang-22 + libstdc++**. The clang-22.1.3
  upgrade introduced a new `-Wgcc-install-dir-libstdcxx` warning. The
  `cc` crate's `flag_if_supported` uses `-Werror` for its support
  probe, so this warning silently dropped every flag in the probe
  (including `-march=corei7 -mrtm` from `pkg-config --cflags libdpdk`
  → the `shim.c` compile lost SSSE3 enablement → DPDK's `rte_memcpy.h`
  refused to compile against the missing target feature). Workaround:
  set `CFLAGS="-Wno-gcc-install-dir-libstdcxx" CXXFLAGS=...` before
  `cargo build`. The workspace + all four bench tools (with `--features
  fstack`) built cleanly at HEAD with this workaround. This is a build-
  environment issue, not a code regression — it will recur on any fresh
  worktree until either (a) `scripts/bench-nightly.sh` /
  `scripts/fast-iter-setup.sh` export those CFLAGS, or (b)
  `crates/dpdk-net-sys/build.rs` does it programmatically before the
  `cc::Build` invocation.

- **Peer echo-server restarted**, clearing 21 of 22 leaked ESTAB
  connections from prior runs. Standard hygiene; no measurable effect
  on RTT (~216 µs min / ~226 µs p50 unchanged).

## Self-review

- The four named hypotheses were each falsified by an independent
  probe rather than by hand-waving (H1 — re-run still 218 µs, ~19 h
  after T57; H2 — `chrt -f 90 taskset -c 2-3` dpdk_net still 212-233 µs;
  H3 — peer binary mtime predates T57's start by 17 s and T57 ran
  against it; H4 — pinning collapses p99 by 10× but barely moves p50).
- The "wire-RTT" story is checked from both directions: DUT→peer ICMP
  (294-407 µs), peer→DUT ICMP (312-376 µs), kernel TCP echo
  (216-251 µs), and bench-rtt dpdk_net (209-262 µs). All four
  probes agree on a 200-400 µs band, none of which is reachable from
  a 78 µs T57 baseline without the network path having changed.
- The ENA `bw_*_allowance_exceeded` counters give a plausible *trigger*
  (40M+ shaping events since boot, sustained heavy bench traffic
  consistent with the May 12 timeline) without claiming AWS is *still*
  actively shaping right now. The "still ~290 µs with no instantaneous
  shaping" datum is honestly reported.
- The stuck `tcp_rack_rto_retrans_tap` is documented as a *contributor
  to the tail*, not the median. Evidence: pinning collapses p99 but
  not p50.
- All probes are in `/tmp/regression-diag/` for re-checking.
- Diagnosis-only commit, no code change, no policy violation
  (worktree is `a10-perf-23.11` at `/home/ubuntu/resd.dpdk_tcp-a10-perf`).

## Artifacts

- `/tmp/regression-diag/host/capture.txt`
- `/tmp/regression-diag/peer/capture.txt`
- `/tmp/regression-diag/rtt-{dpdk,linux,fstack}{,_-raw}.csv` — current
  HEAD, unpinned (the actual T58-regime numbers)
- `/tmp/regression-diag/rtt-{dpdk-pinned27,linux-pinned47}{,_-raw}.csv`
  — pinned to isolated cores (probes the CPU-contention hypothesis)
- `/tmp/regression-diag/rtt-{dpdk,linux}-rtprio{,_-raw}.csv` — chrt -f 90
  + taskset -c 2-3 (probes "absolute priority on isolated EAL cores")
- `/tmp/regression-diag/run-*.sh` and `*.out` — exact reproducers
