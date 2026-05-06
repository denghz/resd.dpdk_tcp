# A10-perf — post-master-merge bench suite report

**Date:** 2026-05-02
**Master tip:** `da31fba` (T9 H7) — phase-a10 merged via PR #9 + 14 production-code cherry-picks from `a10-perf-23.11`
**Host:** AWS EC2 dev box, AMD EPYC 7R13 (Zen 3 Milan), KVM, kernel 6.8.0-1052-aws
**Diagnostic baseline:** THP=madvise, no cpufreq governor under KVM (consistent with earlier A10-perf measurements)

## TL;DR

Now that phase-a10 is on master, the full A10 + A10-perf production-code chain is exercisable on `master`. This report covers running every **non-bench-micro** tool delivered by A10. Findings:

- **5 tools run usefully on this KVM dev host** (no bench-pair needed): `bench-vs-linux --stacks linux`, `bench-vs-linux --mode wire-diff`, `bench-rx-zero-copy` (placeholder body), `bench-report`, `bench-stress --list-scenarios`.
- **7 tools require the bench-pair** (real ENA + paired peer): `bench-e2e`, `bench-stress` (run mode), `bench-offload-ab`, `bench-obs-overhead`, `bench-ab-runner`, `bench-vs-linux --stacks dpdk`, `bench-vs-mtcp --stacks dpdk`. All fail-fast cleanly at "gateway ARP did not resolve" — no fabricated numbers.
- **`scripts/bench-nightly.sh`** is the orchestrator that provisions the bench-pair via `resd-aws-infra` CDK (~$10–13/run for 2× c6in.metal). NOT invoked per cost discipline.
- **No bugs found.** Stub stacks (`afpacket`, `mtcp`) behave correctly: drop in lenient mode, error in strict mode. PMD-degradation messages on TAP vdev are informational, not failures.

## Per-tool result table

| Tool | Local on KVM? | Output | Notes |
|---|---|---|---|
| `bench-e2e` | **No** | log only | DPDK + peer ENA echo-server required; bails at gateway ARP. CSV produced is header-only. |
| `bench-stress` (run mode) | **No** | log only | Same DPDK + peer + netem requirement; bails at gateway ARP. |
| `bench-stress --list-scenarios` | **Yes** | 7 scenarios listed | Lists 4 netem + 3 FaultInjector scenarios without DPDK init. |
| `bench-offload-ab` | **No** | header-only CSV | A/B driver — invokes `bench-ab-runner` per config. Inherits DPDK + peer requirement. |
| `bench-obs-overhead` | **No** | header-only CSV | Observability A/B driver. Same DPDK + peer requirement. |
| `bench-vs-linux --stacks linux` | **Yes** | `/tmp/bench-vs-linux-loopback.csv` | **Linux kernel TCP loopback RTT** — see numbers below. |
| `bench-vs-linux --stacks dpdk` | **No** | log only | dpdk_net stack needs DPDK + peer. |
| `bench-vs-linux --stacks afpacket` | **No (T8 stub)** | clean drop | Plan B T8 stub — drops in lenient, errors in strict. |
| `bench-vs-linux --mode wire-diff` | **Yes (with pcaps)** | clean skip | Pure pcap canonicaliser path; bails on missing pcaps. |
| `bench-vs-mtcp --stacks dpdk` | **No** | log only | DPDK + peer. |
| `bench-vs-mtcp --stacks mtcp` | **No (T12 stub)** | clean drop | Plan B T12 stub. |
| `bench-ab-runner` | **No** | log only | Single-config DPDK runner. |
| `bench-rx-zero-copy` | **Yes (placeholder)** | criterion output | T14 placeholder body — measures iovec construction only (1.42 ns / 5.65 ns). Real RX path lands in T14. |
| `bench-report` | **Yes** | empty-input report | Pure CSV → JSON/HTML/MD; empty-input case writes empty report. |
| `scripts/bench-nightly.sh` | **No (cost gated)** | dry-run blocked | Provisions 2× c6in.metal via `resd-aws-infra` CDK; not invoked. |

## Real-data: bench-vs-linux loopback (Linux kernel TCP)

The only **real measurement** that ran end-to-end locally:

```
Workload: 5000 iters / 500 warmup, 127.0.0.1:10002, 64 B request → 64 B response
Stack: Linux kernel TCP (no dpdk_net)

p50:  37.07 µs
p99:  52.75 µs
p999: 67.09 µs
mean: 38.36 µs
```

This is a **reference baseline** for what a kernel-stack RTT looks like on this host's loopback. Useful as the comparator for what dpdk_net would aim to beat on the bench-pair (where real ENA + paired peer would let us measure dpdk_net's RTT and diff against this kernel number).

Numbers reflect kernel TCP overhead + loopback driver + scheduler — not directly comparable to wire-RTT on real ENA (where kernel adds NIC driver + IRQ + softirq overhead and dpdk_net bypasses those entirely). The comparison only becomes meaningful when both stacks run against the same NIC + peer setup.

## Stub-stack behavior verification

Two T-deferred stub stacks behaved correctly:

- **`bench-vs-linux --stacks afpacket`** (Plan B T8 stub): startup error in strict mode, clean drop in lenient mode.
- **`bench-vs-mtcp --stacks mtcp`** (Plan B T12 stub): with `--stacks mtcp` lenient → "no stacks selected (--stacks resolved to empty)" — well-engineered skip, no fabricated comparison.

## bench-pair invocation (for when needed)

The bench-pair is the right route for the 7 tools that bail at gateway ARP. Per `scripts/bench-nightly.md` + `scripts/bench-nightly.sh` (806 lines):

- Provisions 2× c6in.metal via the sister project `resd-aws-infra` (CDK).
- Cost: ~$10–13 per nightly run.
- Covers: bench-e2e, bench-stress (4 netem scenarios), bench-vs-linux mode A + B, bench-offload-ab, bench-obs-overhead, bench-vs-mtcp burst + maxtp.

To execute (operator-side):

```bash
# One-time: install the sister project
pip install -e ~/resd.aws-infra-setup
# AWS creds configured + AMI baked

# Full run (~$10–13)
./scripts/bench-nightly.sh

# Leave fleet up for debug (still bills until tear-down)
SKIP_TEARDOWN=1 ./scripts/bench-nightly.sh

# Cheaper subset
BENCH_ITERATIONS=1000 ./scripts/bench-nightly.sh
```

`scripts/bench-nightly.sh --dry-run` does a prereq-only check and short-circuits before any AWS call. Dry-run on this host fails immediately at the `resd-aws-infra` not-installed step (sister project not on this dev box) — that's expected and harmless.

## Test suite status

`cargo test -p bench-*` ran clean across the bench tools that have unit tests:
- bench-stress: 1 + 9 passed
- bench-e2e: 29 + 3 + 12 passed
- bench-ab-runner / bench-offload-ab / bench-obs-overhead / bench-vs-linux / bench-vs-mtcp: all unit tests pass

No code changes were made by this report's exercise; the 14 cherry-picked production-code commits + the existing master state are sufficient to compile + test every tool.

## Recommendations

1. **Push the 14 local-only production-code commits** to origin (after the user re-confirms; the fast-forward push is safe). This completes the chain on origin so future bench-pair runs use the optimized stack.

2. **Run `scripts/bench-nightly.sh`** from a host with the sister project installed when ready to spend ~$10–13. The bench-pair gives the **real** ENA throughput + RTT numbers that the local KVM benches can't produce. Specifically, the H5+H7 production wins (+18% throughput on tcp_input) need a bench-pair run to confirm they hold under real ENA traffic and aren't an artifact of the test-fixtures harness.

3. **bench-rx-zero-copy** is currently a placeholder body (T14 deferred). When T14 lands real RX path measurement, this becomes a direct comparator for the H5 reorder gate's effect under sustained RX bursts.

4. **bench-vs-linux loopback** is a **standing local reference**: the 37 µs p50 RTT is the kernel-stack number to beat on bench-pair. Dpdk_net's bench-pair RTT goal is < 25 µs p50 (per A10 design spec §11.3 targets); the gap is what justifies the userspace stack.

## Status

DONE. No code changes. No git push. Local master state preserved at `da31fba` with 14 commits ahead of origin/master pending operator confirmation to push.
