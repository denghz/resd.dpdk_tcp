# T55 fast-iter-suite — 2026-05-12

**Run:** first execution of the new `scripts/fast-iter-suite.sh`
**Branch:** `a10-perf-23.11`
**Purpose:** End-to-end bench coverage across {bench-rtt, bench-tx-burst, bench-tx-maxtp, bench-rx-burst, verify-rack-tlp} for {dpdk_net, linux_kernel, fstack}, driven from the fast-iter peer in a single ~30-50 min wallclock.

## What's new

| Commit | Change |
|--------|--------|
| _(filled in after the run)_ | Add `scripts/fast-iter-suite.sh` — sequential, DPDK-NIC-aware orchestrator. CSV artifacts under `target/bench-results/fast-iter-<UTC>/`; not committed. |
| _(filled in after the run)_ | T55 report — numbers from first execution. |

## Results

_Filled in by the script's `$RESULTS_DIR/SUMMARY.md` after the run completes._

- Results directory: `target/bench-results/fast-iter-<timestamp>/`
- Summary: `<results-dir>/SUMMARY.md`
- Per-arm logs: `<results-dir>/<bench>-<stack>.log`

### Wallclock

| Phase | Elapsed |
|-------|---------|
| bench-rtt (all 3 stacks) | _tbd_ |
| bench-tx-burst (all 3 stacks) | _tbd_ |
| bench-tx-maxtp (all 3 stacks) | _tbd_ |
| bench-rx-burst (all 3 stacks) | _tbd_ |
| verify-rack-tlp | _tbd_ |
| **Total** | _tbd_ |

### bench-rtt — RTT (ns) by payload

_Paste the SUMMARY.md tables here._

### bench-tx-burst — burst throughput (bps)

_Paste the SUMMARY.md tables here._

### bench-tx-maxtp — sustained goodput (bps)

_Paste the SUMMARY.md tables here._

### bench-rx-burst — per-segment RX latency (ns)

_Paste the SUMMARY.md tables here._

### verify-rack-tlp — netem scenarios

_Paste the verify-rack-tlp summary block here._

## Open follow-ups

_(Populated post-run from `$FAILS[]` plus any anomalies in the data.)_

## Reproducer

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
./scripts/fast-iter-setup.sh up --with-fstack
./scripts/fast-iter-suite.sh
```

CSVs land under `target/bench-results/fast-iter-<UTC-timestamp>/`; the full per-arm log is in `<results-dir>/suite.log`.
