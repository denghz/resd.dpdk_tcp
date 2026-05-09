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
- [ ] Phase 2 — Remove dead bench arms
- [ ] Phase 3 — Raw-sample CSV writer in bench-common
- [ ] Phase 4 — Consolidate RTT benches into bench-rtt
- [ ] Phase 5 — Split bench-vs-mtcp into bench-tx-burst + bench-tx-maxtp
- [ ] Phase 6 — Per-segment send→ACK latency
- [ ] Phase 7 — Bidirectional netem via peer IFB ingress
- [ ] Phase 8 — bench-rx-burst tool
- [ ] Phase 9 — HW-TS attribution validation on c7i
- [ ] Phase 10 — Nightly script rewire + scenario expansion
- [ ] Phase 11 — Counters + observability
- [ ] Phase 12 — Cleanup, c7i validation, t51 report

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
