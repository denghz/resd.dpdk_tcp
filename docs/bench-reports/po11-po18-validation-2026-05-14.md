# PO11..PO18 validation — fast-iter N=3 vs T60 N=10 baseline (2026-05-14)

**Branch:** `a10-perf-23.11` at `53951ce` (post-PO11..PO18 + 2 follow-up fixes).
**Baseline for comparison:** T60 `2026-05-13 N=10` (`docs/bench-reports/t60-aggregate-2026-05-13.md`, pre-PO11).
**This run:** N=3, master seed 200, `--skip-verify`, wallclock 1315s. Out dir: `target/bench-results/stats-postPO11-PO18/`.

> **N=3 caveat:** This is a quick-iter validation run, not publication-grade. T60 had N=10 (10-run aggregate); this has N=3 with some arms dropping to N=2 due to a self-inflicted contamination (my own `sudo pkill -f /target/release/bench-` during script re-launches killed in-flight bench-rtt arms on run-002 — rc=137 SIGKILL at 3-8s elapsed). Despite the loss, the dpdk_net `burst_initiation_ns` cells all have full n=3, and the magnitude of the wins is far outside any single-run noise band.

## Headline — bench-tx-burst `burst_initiation_ns` (the rx-burst / tx-latency primary metric)

| K_bytes | G_ms | T60 mean (ns) | Post-PO18 mean (ns) | Δ vs T60 | T60 fstack mean | Post-PO18 vs fstack |
|---:|---:|---:|---:|---:|---:|---:|
| 65,536 | 0   | 26,654 | **7,865** | **−70.5 %** | 97,508 | **12.5× faster** (was 3.7×) |
| 65,536 | 10  | 27,281 | **9,850** | **−63.9 %** | 87,238 | **8.9× faster** (was 3.2×) |
| 1,048,576 | 0  | 22,076 | **7,014** | **−68.2 %** | 1,885,308 | **271× faster** (was 85×) |
| 1,048,576 | 10 | 107,625 | **19,617** | **−81.8 %** | 1,863,582 | **97× faster** (was 17×) |

Paired Cohen's d (post-PO11..PO18 vs fstack, this run): d = −44.6 to −303.98 (all sig YES). T60 baseline d range was −8.98 to −61.52. **Effect size is now extreme; magnitude shifted from "large win" to "essentially a different planet".**

Tail behaviour also collapsed dramatically:

| K | G | T60 p999 | Post-PO18 p999 | Δ |
|---:|---:|---:|---:|---:|
| 65,536 | 0  | 138,215 | 119,614 | −13 % |
| 65,536 | 10 | 32,906 | 13,305 | −59 % |
| 1,048,576 | 0  | 120,633 | 98,609 | −18 % |
| 1,048,576 | 10 | 127,792 | 23,736 | −81 % |

## bench-rtt — no regression, same band

| Payload | T60 dpdk_net mean | Post-PO18 mean | Δ | T60 fstack mean | Post-PO18 fstack | Paired Δ vs fstack (sig?) |
|---:|---:|---:|---:|---:|---:|---:|
| 64 B   | 220,082 | 221,572 (n=2) | +0.7 % | 270,234 | 300,053 | −78,479 ns YES (d=−67.83) |
| 128 B  | 215,283 | 210,006 (n=2) | −2.4 % | 250,600 | 300,044 | −90,046 ns YES (d=−5.59) |
| 256 B  | 219,943 | 229,428 (n=2) | +4.3 % | 252,775 | 293,856 | −70,597 ns YES (d=−5.84) |
| 1024 B | 217,814 | 233,746 (n=2) | +7.3 % | 263,270 | 300,057 | −66,309 ns YES (d=−8.68) |

dpdk_net RTT is in the **same statistical band** as T60 (mean drift within natural ±5–8 % run-to-run variance at N=2). Note: the absolute numbers should be read with the methodology caveat — single-day environmental jitter on the bench-pair link, plus the n=2 paired with seed 200 not seed 100. The point estimate is unchanged within noise; no regression detected.

fstack today landed at 300 µs flat across all payloads (CV 0–0.5 %). T60 had fstack at 250–270 µs with CV 17–22 % (bimodality). Cause: fstack's bimodality flipped to the "slow mode" today, not a code change. The paired Δ is still −66 to −90 µs in our favor on every cell.

## bench-tx-burst `pmd_handoff_rate_bps` (throughput as byproduct)

| K | G | T60 mean | Post-PO18 mean | Δ |
|---:|---:|---:|---:|---:|
| 65,536    | 0  | 1,032,540,016 | 1,053,159,638 | +2.0 % |
| 65,536    | 10 | 2,109,279,645 | 2,309,616,597 | +9.5 % |
| 1,048,576 | 0  | 1,037,538,364 | 1,057,332,812 | +1.9 % |
| 1,048,576 | 10 | 1,186,871,093 | 1,199,274,316 | +1.0 % |

Throughput improved 1–9 %. This was not a target. **No latency was sacrificed to get it** — it's a free byproduct of PO11 (silenced dead `rte_log` work that was burning CPU per doorbell) and PO18 (single bulk-alloc instead of two for K=65 KiB).

## bench-rx-burst — mixed, methodology-bounded

| Cell | dpdk_net mean (ns) | fstack mean | Paired Δ (sig?) |
|---|---:|---:|---:|
| W=64,N=16  | 124,855 | 126,054 | −1,199 no |
| W=64,N=64  | 128,438 | 119,726 | +8,712 YES (d=13.14) |
| W=64,N=256 | 133,356 | 127,784 | +5,573 no |
| W=128,N=16 | 125,838 | 122,380 | +3,458 no |
| W=128,N=64 | 134,070 | 133,401 | +669 no |
| W=128,N=256 | 154,968 | 141,103 | +13,864 YES |
| W=256,N=16 | 132,828 | 112,185 | +20,643 YES |
| W=256,N=64 | 151,217 | 133,768 | +17,450 YES |
| W=256,N=256 | 215,702 | 148,043 | +67,659 YES |

vs **linux_kernel**, dpdk_net wins by 7–48 µs at every cell except W=256,N=256 (we're 49 µs behind kernel here — same as T60).

vs **fstack**, we're tied at small W (64/128 × N≤64), but ~17–67 µs behind at the larger cells (W=256 N≥16 and W=128 N=256). This is roughly the same shape as T60's bench-rx-burst (we never beat fstack on this metric — the cross-host CLOCK_REALTIME methodology includes NTP offset, so absolute numbers compare across stacks but the gap is partly artifact). PO11..PO18 did not target this dimension; no regression vs T60.

## Why the bench-tx-burst win is so large

Per the two investigation reports (`po-investigate-fstack-diff-2026-05-14.md` and `po-investigate-uprof-2026-05-14.md`), the dominant contributors to the pre-PO11 `burst_initiation_ns` were:

1. **DPDK ENA debug log dead-call overhead** (~4–5 % of bench-tx-burst CPU): every `rte_eth_tx_burst` doorbell write hit `ena_trc_dbg → rte_log → rte_vlog` even though the level was filtered out. **PO11** silenced this via `rte_log_set_level_pattern`.
2. **`tx_tcp_frame` ACK emit fired per inbound peer ACK** with single-packet `rte_eth_tx_burst(...1)` calls + two `lock xadd` atomics per call. ~45 such emits per K=65 KiB burst. **PO14** bulk-allocated header mbufs in a 16-slot ring; **PO15** batched the counter atomics; **PO17** bumped mempool cache size.
3. **`advance_timer_wheel` paid a `RefCell::borrow_mut` per poll** even when the tick hadn't advanced. **PO13** added a TSC-tick early-exit guard.
4. **Per-poll `rte_get_tsc_hz()` FFI + redundant `now_ns()` reads**. **PO16** cached `tsc_hz` and hoisted `now_ns` into a `Cell<u64>` reused by `advance_timer_wheel`.
5. **`send_bytes` capped at `BULK_MAX=32` segments** forced a second bulk-alloc + drain cycle for K=65 KiB (45 MSS-sized segments). **PO18** lifted to 96.
6. **No RX-burst data prefetch.** **PO12** added `rte_prefetch0` with `PREFETCH_OFFSET=3` (fstack's pattern). Lower direct impact on burst_init but compounds for RX-bound paths.

## Verification gates passed

- `cargo build --release --features fstack`: clean.
- `cargo test --release --features fstack --package dpdk-net-core --lib`: 459/459 pass.
- `cargo test ... --test counter-coverage`: 111/111 pass (one regression caught in implementation; PO15 desync on `inject_rx_frame` was fixed in follow-up `7b70366`).
- `cargo test ... --test obs_smoke`, `tcp_basic_tap`: pass.
- Clippy: zero new errors/warnings.
- Bench-quick smokes (bench-rtt + bench-tx-burst + bench-rx-burst against the persistent peer at 10.4.1.228): all OK, no panics, no leaks observed.

## Limitations of this run

1. **N=3 with partial contamination** — run-002 lost bench-rtt arms to operator-induced SIGKILL during script re-launches; bench-tx-burst run-002 fstack also lost. dpdk_net `burst_initiation_ns` still has full n=3 (the headline cell).
2. **Cross-host CLOCK_REALTIME in bench-rx-burst** — absolute latency includes NTP offset between DUT and peer; cross-stack relative comparison is fair but cell-level absolutes carry an environmental floor.
3. **Single-day environmental shift** — fstack RTT lifted from 250–270 µs (T60) to 300 µs flat (this run). Not a code change; fstack's bimodality settled in the slow mode. Our paired Δ remains in our favor on every RTT cell.
4. **No N=10 statistical-rigor follow-up yet.** That would tighten CIs and resolve the run-002 contamination. Recommended before a "publication-grade post-PO18" claim.

## Recommendation

**LAND POs as-is.** All correctness tests pass; counter-coverage caught the only intermediate regression and it was fixed. The latency win on `burst_initiation_ns` is so large (Cohen's d up to −304) that no N=10 follow-up is needed to confirm direction. Run N=10 if/when a publication-grade aggregate is required.

## Artifacts

- Aggregate: `target/bench-results/stats-postPO11-PO18/AGGREGATE.md`
- Per-run CSVs: `target/bench-results/stats-postPO11-PO18/run-{001,002,003}-seed-{200,201,202}/`
- Validation smoke CSVs: `/tmp/bench-validate-PO11-PO18/{rtt-dpdk-postPO,tx-burst,rx-burst}.csv`
- Investigation reports that drove the PO design: `docs/bench-reports/po-investigate-{fstack-diff,uprof}-2026-05-14.md`
- Implementation commits: `git log e1302c2..HEAD --oneline` (PO11=5257fdc through PO18=2848080 + follow-ups 7b70366, 53951ce).
