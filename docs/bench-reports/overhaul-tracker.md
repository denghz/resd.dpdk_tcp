# Bench overhaul tracker — 2026-05-09

Plan: docs/superpowers/plans/2026-05-09-bench-suite-overhaul.md

Baseline: T50 report at `docs/bench-reports/t50-bench-pair-2026-05-08.md` and T54 wire-rate report at `docs/bench-reports/t54-...` (most recent dpdk/fstack data). Plan Task 1.1 (re-run nightly to capture pre-overhaul baseline) is **skipped** — T50/T54 serve as the regression reference.

## Tool fate map

| Tool | Action | Phase |
|---|---|---|
| `bench-e2e` (binary) | replaced by `bench-rtt` | 4 |
| `bench-e2e/peer/echo-server` | extended (burst-echo) | 8 |
| `bench-stress` | deleted | 4 |
| `bench-vs-linux` mode A | folded into `bench-rtt --stack linux_kernel` | 4 |
| `bench-vs-linux` mode B | retained | — |
| `bench-vs-mtcp` burst | replaced by `bench-tx-burst` | 5 |
| `bench-vs-mtcp` maxtp | replaced by `bench-tx-maxtp` | 5 |
| `bench-vs-mtcp/src/mtcp.rs` | deleted | 2 |
| `bench-rx-zero-copy` | deleted | 2 |
| `bench-stress/scenarios.rs::pmtu_blackhole_STAGE2` | deleted | 2 |
| `bench-vs-linux/src/afpacket.rs` | deleted | 2 |
| `bench-offload-ab` | retained (subprocess re-target) | 4 |
| `bench-obs-overhead` | retained (subprocess re-target) | 4 |
| `bench-ab-runner` | retained then deleted (post-consolidation) | 4, 12 |
| `bench-micro` | retained | — |
| `bench-common` | augmented (`raw_samples`) | 3 |
| `bench-report` | updated for new tool names | 10 |
| `bench-tx-burst` | NEW | 5 |
| `bench-tx-maxtp` | NEW | 5 |
| `bench-rx-burst` | NEW | 8 |
| `bench-rtt` | NEW | 4 |

## Phase status

- [x] Phase 1 — Pre-work + tracker
- [x] Phase 2 — Remove dead bench arms
- [x] Phase 3 — Raw-sample CSV writer in bench-common
- [x] Phase 4 — Consolidate RTT benches into bench-rtt
- [x] Phase 5 — Split bench-vs-mtcp into bench-tx-burst + bench-tx-maxtp
- [x] Phase 6 — Per-segment send→ACK latency
- [x] Phase 7 — Bidirectional netem via peer IFB ingress
  - Tasks 7.1, 7.2 done 2026-05-09
  - Netem matrix expanded from `4 scenarios × 1 direction = 4 buckets`
    to `4 scenarios × 3 directions {egress, ingress, bidir} = 12 buckets`
    in `scripts/bench-nightly.sh` step [8/12]. Direction `ingress`
    redirects peer ens6 ingress traffic to ifb0 via `peer-ifb-setup.sh`
    so netem applies to packets DUT sent (DUT-TX-data-loss path);
    `egress` keeps the previous peer-root qdisc (DUT-RX path); `bidir`
    applies both simultaneously for symmetric loss. Per-scenario CSVs
    are now keyed `bench-stress-${scenario}-${direction}` and merged
    into `bench-stress.csv` as before.
  - Wallclock-budget impact: nightly's [8/12] block grows by 3× on
    the netem axis (composed multiplicatively with the Phase 10
    iter-count expansion).
- [x] Phase 8 — bench-rx-burst tool
- [x] Phase 9 — HW-TS attribution validation on c7i (code-validated; live c7i
  validation **permanently deferred** 2026-05-11 — HW-TS not supported in
  ap-south-1 for any EC2 instance type incl. c7i. Empirical confirmation:
  bench-rtt's DPDK init logs `RX timestamp dynfield/dynflag unavailable on
  port 0 (ENA steady state)`. Code path remains correct + tested; live
  validation would require region or NIC family migration. See t51 §c7i
  validation.)
- [x] Phase 10 — Nightly script rewire + scenario expansion
  - Task 10.1 done 2026-05-09: bench-rtt invocations now sweep
    `$BENCH_RTT_PAYLOADS` (default `64,128,256,1024`) instead of the
    legacy hard-coded `--payload-bytes-sweep 128`. Closes C-C1.
  - Task 10.2 done 2026-05-09: netem matrix grows from
    `4 scenarios × 3 directions = 12 buckets` (post-Phase-7) to
    `7 scenarios × 3 directions = 21 buckets`. Three new scenarios
    (`high_loss_3pct`, `high_loss_5pct`, `symmetric_3pct`) push the
    burst-tail past the 200ms RTO floor — first time in this
    bench suite the RTO recovery path is exercised. Closes C-D2.
    `bench-tx-burst` and `bench-rx-burst` now run under the netem
    matrix (dpdk_net only) in addition to bench-rtt: 21 cells × 3
    tools = 63 sub-runs per nightly. Closes C-C3.
  - Task 10.3 done 2026-05-09: per-scenario iter override
    (`SCENARIO_ITERS`) lifts low-loss buckets to 1M iters so p999 of
    loss-affected events is statistically meaningful. Closes C-D1.

## Phase 10 wallclock impact

The netem matrix now has 7 scenarios × 3 directions = 21 cells per
tool. With per-scenario iter overrides:
  - random_loss_01pct_10ms: 1M iters (3 directions ≈ 3 × 30 min = 90 min)
  - correlated_burst_loss_1pct: 200k iters (3 × 6 min = 18 min)
  - reorder_depth_3: 20k iters (3 × 1 min = 3 min)
  - duplication_2x: 20k iters (3 × 1 min = 3 min)
  - high_loss_3pct: 200k iters (3 × 6 min = 18 min)
  - high_loss_5pct: 100k iters (3 × 3 min = 9 min)
  - symmetric_3pct: 200k iters (3 × 6 min = 18 min)

Total bench-rtt wallclock under netem: ~3 hours
Plus bench-tx-burst + bench-rx-burst per direction: ~2 hours combined.
Plus the existing clean-wire passes: ~1.5 hours.

Estimated nightly total: ~6.5 hours (was ~2 hours pre-Phase-10).
- [x] Phase 11 — Counters + observability
- [x] Phase 12 — Cleanup, c7i validation deferral, t51 report
  - Task 12.1 done 2026-05-09: bench-ab-runner crate deleted (workspace
    leaf since Phase 4); doc comments + bail messages in bench-offload-ab
    + bench-obs-overhead refreshed to point at bench-rtt.
  - Task 12.2 deferred: c7i live HW-TS validation requires fleet
    provisioning + ~6.5h nightly run — operator follow-up. See
    `docs/bench-reports/t51-bench-overhaul-2026-05-09.md` §c7i validation
    for the runbook + grep cookbook.
  - Task 12.3 done 2026-05-09: t51 final report at
    `docs/bench-reports/t51-bench-overhaul-2026-05-09.md`. All 22
    catalogued claims (C-A1..C-F2) closed or explicitly deferred.
  - Task 12.4 done 2026-05-09: tag `bench-overhaul-2026-05` (local only,
    not pushed).

## Claims (from 2026-05-09 audit)

| ID | Claim | Phase |
|---|---|---|
| C-A1 | mTCP arm permanent stub | 2, 5 |
| C-A2 | afpacket stub | 2 |
| C-A3 | bench-rx-zero-copy placeholder body | 2, 8 |
| C-A4 | pmtu_blackhole_STAGE2 placeholder | 2 |
| C-A5 | RTT bench overlap (e2e/stress/vs-linux mode A) | 4 |
| C-B1 | maxtp single-mean | 5 |
| C-B2 | no raw samples emitted by any bench | 3, 4, 5, 8 |
| C-B3 | no per-RX-segment latency | 8, 9 |
| C-B4 | no send→ACK CDF | 6 |
| C-B5 | single-conn bias in RTT benches | 4, 5 |
| C-C1 | no payload sweep at 64/128/256 B | 4, 10 |
| C-C2 | no peer-burst RX workload | 8 |
| C-C3 | no burst×netem bucket | 10 |
| C-C4 | no bidirectional netem (DUT-TX-data-loss) | 7 |
| C-D1 | iter count 5k too low for p999 at 0.1% loss | 10 |
| C-D2 | RTO never fires at current loss profiles | 10, 11 |
| C-D3 | lost-iter terminal (single timeout kills scenario) | 4 |
| C-E1 | no queue-depth time series in CSV | 5, 11 |
| C-E2 | no RTO/RACK/TLP retransmit split | 11 |
| C-E3 | HW-TS attribution dead on ENA | 9, 12 |
| C-F1 | mTCP comparator scope (per user direction) | 2, 5 |
| C-F2 | linux maxtp peer port should be linux-tcp-sink | 5 |
