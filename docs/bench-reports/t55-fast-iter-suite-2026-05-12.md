# T55 fast-iter-suite — 2026-05-12

**Run:** first execution of the new `scripts/fast-iter-suite.sh`
**Branch:** `a10-perf-23.11`
**Status:** DONE_WITH_CONCERNS
**Purpose:** End-to-end bench coverage across {bench-rtt, bench-tx-burst, bench-tx-maxtp, bench-rx-burst, verify-rack-tlp} for {dpdk_net, linux_kernel, fstack}, driven from the fast-iter peer in a single ~30-45 min wallclock.

## What's new

`scripts/fast-iter-suite.sh` — a single-binary, sequential bench orchestrator
that drives every arm of every tool against the fast-iter peer and emits a
per-run results directory + `SUMMARY.md`. Iteration counts are tuned for the
~30-45 min wallclock target, not nightly-grade statistical depth. Adds:

- timestamped results directory under `target/bench-results/fast-iter-<UTC>/`,
- per-arm timeout (`RUN_ONE_TIMEOUT`, default 300s) so hung benches don't
  block the suite,
- peer `echo-server` restart between every DPDK/fstack arm (releases workers
  that get pinned by FIN-less DPDK teardowns),
- Python-driven SUMMARY.md generator that pivots the spec §14 CSV schema into
  p50/p99/mean tables per stack per bench.

## Results

- **Results directory:** `target/bench-results/fast-iter-2026-05-12T04-30-22Z/`
- **Auto-generated summary:** `<results-dir>/SUMMARY.md`
- **Per-arm logs:** `<results-dir>/<bench>-<stack>.log`

### Wallclock + outcome

| Phase | Outcome | Notes |
|-------|---------|-------|
| bench-rtt (3 stacks) | 1 OK / 1 TIMEOUT / 1 SIGSEGV | linux NAT routing hang; fstack arm SIGSEGV after init |
| bench-tx-burst (3 stacks) | 3 OK | full grid, all stacks |
| bench-tx-maxtp (3 stacks) | 3 OK after retry | linux arm needed `--local-ip` even though it's documented dpdk-only |
| bench-rx-burst (3 stacks) | 2 OK after retry / 1 TIMEOUT | dpdk + fstack arms after peer's burst-echo-server restart; linux arm same NAT hang |
| verify-rack-tlp | 2/5 scenarios PASS, 3 not run | killed at high_loss_3pct (long RTO-driven scenario projected to exceed 1500s outer timeout) |
| **Total wallclock** | ~48 min (script + retries) | original suite run 16 min; second pass to fill gaps |

### bench-rtt — RTT (ns) by payload (dpdk_net only — linux + fstack arms broken in this env)

| payload | p50 (us) | p99 (us) | mean (us) |
|---------|---------:|---------:|----------:|
| 64 B    |    73.65 |    96.99 |     74.40 |
| 128 B   |    75.00 |   102.05 |     76.27 |
| 256 B   |    78.49 |   106.91 |     79.59 |
| 1024 B  |    80.75 |   105.18 |     81.97 |

Stable ~75-80 µs across the 64-1024 B range, consistent with the c7i x86_64
AWS network path. linux_kernel arm hangs on `read_exact` after a handful of
iterations (NAT routing pinned to `vethpxtn0`/`10.99.1.2`); fstack arm
SIGSEGVs after engine init.

### bench-tx-burst — throughput_per_burst_bps (Gbps mean)

| K (KiB) | G (ms) | dpdk_net | linux_kernel | fstack |
|--------:|-------:|---------:|-------------:|-------:|
| 64      | 0      | 1.05     |    65.6      | 5.10   |
| 64      | 10     | 1.36     |    16.4      | 5.71   |
| 1024    | 0      | 1.05     |   ~15        | 4.59   |
| 1024    | 10     | 1.20     |   ~17        | 5.13   |

linux_kernel reports inflated numbers because its `throughput_per_burst_bps`
is "bytes written / time-to-write" — for the kernel, that's a `write_all()`
into the kernel send buffer which fills nearly instantly (NIC bandwidth is
not measured on the kernel arm). dpdk_net and fstack report wire-rate
because the engines run their own send schedulers. The dpdk_net~1 Gbps and
fstack~5 Gbps figures are consistent with prior bench-pair reports (T54
saw dpdk~2.5 Gbps; this run was reduced-grid + only 200 bursts so spread
is larger).

### bench-tx-maxtp — sustained_goodput_bps (Gbps mean)

| W (B) | C  | dpdk_net | linux_kernel | fstack |
|------:|---:|---------:|-------------:|-------:|
| 4096  | 1  | 1.05     |  0.00        | 1.39   |
| 4096  | 4  | 1.05     |  0.00        | 3.01   |
| 4096  | 16 | 1.05     |  -           | 3.85   |
| 16384 | 1  | 1.10     |  0.00        | 2.74   |
| 16384 | 4  | 1.07     |  0.00        | 4.06   |
| 16384 | 16 | 1.06     |  -           | 4.07   |
| 65536 | 1  | 1.07     |  -           | 2.78   |
| 65536 | 4  | 1.05     |  -           | 4.10   |
| 65536 | 16 | 1.05     |  -           | 2.50   |

Pattern matches T54: fstack peaks ~4 Gbps wire rate (echo-counted), dpdk_net
ceilings at ~1.1 Gbps in this reduced 10-second window. linux_kernel arm
(reduced grid: W=4k/16k, C=1/4) reports near-zero because the kernel-side
peer port (`linux-tcp-sink` on :10002) silently drops the buffered data and
the bench's "wire rate" metric ends up matching what came back through the
sink, which is the discard side.

### bench-rx-burst — per-segment latency (us, mean) — reduced grid

| W (B) | N  | dpdk_net | fstack |
|------:|---:|---------:|-------:|
| 64    | 16 | 62.5     | 56.9   |
| 64    | 64 | 156      | 165    |
| 128   | 16 | 62.0     | 56.7   |
| 128   | 64 | 156      | 165    |

fstack RX latency at small N is **slightly lower** than dpdk_net for the
peer-driven burst workload — that's likely the FreeBSD TCP stack's larger
recv-buffer pre-allocation absorbing the burst without a stall. Linux arm
not measured (same NAT hang).

### verify-rack-tlp — netem scenarios (2/5 PASS, 3 not run)

| scenario            | tcp.tx_retrans | _rto  | _rack | _tlp | PASS? |
|---------------------|---------------:|------:|------:|-----:|-------|
| low_loss_05pct      | 14948          | 12459 |     0 | 2489 | PASS  |
| low_loss_1pct_corr  |     6          |     5 |     0 |    1 | PASS  |
| high_loss_3pct      | _not run_      | -     | -     | -    | n/a   |
| symmetric_3pct      | _not run_      | -     | -     | -    | n/a   |
| high_loss_5pct      | _not run_      | -     | -     | -    | n/a   |

Both completed scenarios satisfy the ALL-of (`tx_retrans>0`) AND ANY-of
(`tx_retrans_rack>0 OR tx_retrans_tlp>0`) assertion sets baked into
`verify-rack-tlp.sh`. RACK fires 0 times in both (consistent with the script
header comment that RACK needs dense ACK + low loss to be detectable). TLP
covers the tail-loss case in both. RTO dominates at 0.5% loss (12459) which
the script's calibrated comment treats as expected — 0.5% loss puts ACKs
sparse enough that RACK's reorder window can't fire in time.

The high_loss_3pct/symmetric_3pct/high_loss_5pct scenarios were killed
mid-run because the third scenario's bench-rtt was projected to exceed the
1500s outer timeout (200k iters × 3% loss × heavy RTO recovery → ~11 min
just for that single scenario on this AWS network path).

## Open follow-ups

1. **bench-rtt linux_kernel hang** — the host's data NIC `0000:28:00.0` is
   bound to vfio-pci so kernel TCP traffic goes via the host bridge through
   `vethpxtn0` (`10.99.1.2`) and NAT-translates to `10.2.1.11` from the
   peer's perspective. The TCP handshake completes but `read_exact()` of
   the echo response stalls indefinitely. Plain `nc | peer | nc` works
   (verified). Root cause likely a path-MTU / SACK / reordering edge case
   in this sandboxed network setup. Workaround: run linux_kernel arm from a
   host that has direct (non-NAT'd) routing to the peer NIC, OR add a
   timeout-and-skip path inside the bench's `run_rtt_workload`.

2. **bench-rtt fstack SIGSEGV** — F-Stack inits cleanly (DPDK port comes
   up, FreeBSD bind succeeds, "f-stack-0: Successed to register dpdk
   interface"), then SIGSEGVs (rc=139) inside the workload. The same fstack
   binary handles bench-tx-burst, bench-tx-maxtp, and bench-rx-burst fine.
   Likely a recent regression in the bench-rtt fstack arm; needs gdb
   inspection on a follow-up T-task.

3. **bench-tx-maxtp `--local-ip` precondition** — the linux_kernel arm
   parses `--local-ip` as IPv4 for its peer-rwnd probe, even though the
   `--help` says it's required only for dpdk_net. Suite now passes
   `--local-ip $DUT_LOCAL_IP` for the linux arm too; either the CLI doc
   should be updated or the parse made conditional.

4. **bench-rx-burst dpdk_net "stalled at 0/1024 bytes"** — first attempt
   failed with the dpdk_rx_burst engine reporting no forward progress in 60s.
   After restarting the peer's burst-echo-server, the same invocation
   succeeded. Worker-pool exhaustion on the peer side after the earlier
   bench-tx-burst arms is the leading hypothesis; suite should also restart
   burst-echo-server between rx-burst arms (currently only echo-server is
   restarted between dpdk/fstack arms).

5. **verify-rack-tlp wallclock at high loss** — at 3% loss with 200k iters,
   the run takes 10-15 minutes on this AWS network path (RTO-driven). The
   script's `SCENARIO_ITERS[high_loss_3pct]=200000` is calibrated for a
   higher-throughput physical lab. Either drop the override here to use the
   suite's `ITERS=50000`, or split verify-rack-tlp out of the main suite
   into its own slow-track invocation.

6. **DPDK zombie cleanup** — when fstack rx-burst was killed with `SIGKILL`,
   it left 23 hugepages locked + a zombie process still spinning at 99% CPU
   that prevented `umount /dev/hugepages` and broke subsequent DPDK init
   ("Invalid port_id=0, PortInfo(0, -19)"). The orphan was visible only
   via `ps auxf`. Suite should explicitly clean up the bench subprocess
   tree on timeout (e.g. via `pkill -KILL -P <pid>` or process-group send).

## Reproducer

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
./scripts/fast-iter-setup.sh up --with-fstack      # generates .fast-iter.env
./scripts/fast-iter-suite.sh                       # writes target/bench-results/fast-iter-<UTC>/SUMMARY.md
```

CSVs land under `target/bench-results/fast-iter-<UTC>/`. The full per-arm log
is in `<results-dir>/suite.log`.

Environment overrides:
- `RUN_ONE_TIMEOUT` — per-arm hard cap (default 300s).
- `VERIFY_RACK_TLP_TIMEOUT` — verify-rack-tlp specific cap (default 1800s).
- `SKIP_VERIFY_RACK_TLP=1` — skip the netem matrix entirely.
- `DUT_PCI`, `DUT_LOCAL_IP`, `DUT_GATEWAY`, `DUT_LCORE`, `PEER_NIC` — DUT/peer
  topology defaults that match the a10-perf-23.11 dev host.
