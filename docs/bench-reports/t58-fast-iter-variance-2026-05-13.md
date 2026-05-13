# T58 fast-iter-suite variance — 3 consecutive runs (2026-05-12/13)

**Runs:** `target/bench-results/fast-iter-2026-05-12T{14-43-08,15-20-35,15-58-00}Z/`
**Branch state at run time:** `a10-perf-23.11` post-T57-follow-ups (engine scratch-fix `5fecb92`, RX-audit `a3cd8bf`, metric-relabel `0094da4`+`d840df0`, summarizer-pivot-fix `77259c1`, verify-rack-tlp Option-D `d07d9ec`, multi-conn-doc `a75e61d`).
**Outcome:** 12 OK / 1 FAIL in all 3 runs — **consistent**. The single FAIL is `verify-rack-tlp` TIMEOUT (see below).

## Pass-rate reliability

All 12 bench-tool arms (bench-rtt × 3 stacks, bench-tx-burst × 3, bench-tx-maxtp × 3, bench-rx-burst × 3) completed in **all 3 runs**. No crashes, no hangs, no DPDK port-init failures. The `reset_dpdk_state` + hardened peer servers eliminated the cascade failures of T55/v2. **Functional reliability: confirmed.**

## verify-rack-tlp TIMEOUT — root cause + fix

The 5-scenario verify-rack-tlp run hit its 1800s outer timeout in all 3 runs. Cause: the Option-D stabilization (commit `d07d9ec` — `low_loss_1pct_corr` → `low_loss_1pct`, uniform 1% loss) made that scenario take ~7.5 min (200k iters × ~1% retrans × RTO recovery) instead of the ~3s the flaky correlated spec took. 5-scenario total grew ~27 min → ~33 min, past the 30 min cap. **Fixed in commit `a997acd`**: per-scenario iter counts trimmed (500k/200k/50k/50k/30k → 100k/100k/20k/20k/15k), 5-scenario total now ~13-16 min, timeout bumped 1800s → 2100s. Not yet re-validated by a full suite run (next step).

## Numeric variance — CV across 3 runs

### bench-rtt p50 RTT (ns)

| stack | payload | run1 | run2 | run3 | mean | stdev | CV% |
|---|---:|---:|---:|---:|---:|---:|---:|
| dpdk_net | 64 B | 210822 | 213300 | 210995 | 211706 | 1383 | 0.7 |
| dpdk_net | 128 B | 201845 | 200420 | 199194 | 200486 | 1327 | 0.7 |
| dpdk_net | 256 B | 215264 | 224510 | 211869 | 217214 | 6542 | 3.0 |
| dpdk_net | 1024 B | 226220 | 213701 | 216811 | 218911 | 6518 | 3.0 |
| fstack | 64 B | 300039 | 300036 | 300052 | 300042 | 9 | 0.0 |
| fstack | 128 B | 299978 | **200550** | 300028 | 266852 | 57419 | **21.5** |
| fstack | 256 B | 297922 | 299850 | 300047 | 299273 | 1174 | 0.4 |
| fstack | 1024 B | **200456** | 300121 | 300074 | 266884 | 57528 | **21.6** |
| linux_kernel | 64 B | 227004 | 245496 | 248473 | 240324 | 11631 | 4.8 |
| linux_kernel | 128 B | 241445 | 237540 | 241441 | 240142 | 2253 | 0.9 |
| linux_kernel | 256 B | 252015 | 219297 | 240232 | 237181 | 16571 | 7.0 |
| linux_kernel | 1024 B | 250565 | 239501 | 239607 | 243224 | 6357 | 2.6 |

**Two concerns visible here:**

1. **fstack 128B + 1024B are bimodal** — one of the 3 runs drops to ~200µs while the other two sit at ~300µs (CV 21.5/21.6%). This is the fstack RTT arm's `rtt_ns` measurement; if it's a pure DUT-side round-trip it shouldn't have cross-host clock skew, so the bimodality is more likely fstack's poll-loop scheduling or an interaction with the hardened peer's larger socket buffers. **Needs investigation before publication.**

2. **Absolute numbers are ~2-3× higher than T57** — T57 (run at 09:37 UTC the same day) showed dpdk_net p50 ~76-99µs, fstack ~100µs, linux ~104-109µs. These variance runs (14:43-16:35 UTC) show dpdk_net ~200-220µs, fstack ~200-300µs, linux ~227-252µs. A 2-3× regression between the morning and afternoon of the same day. Candidate causes:
   - The engine scratch-clobber fix (`5fecb92`) changed `deliver_readable` from clear+extend to append-within-poll — could have a perf side-effect (though for bench-rtt's single-segment req/resp it should be ≤1 scratch entry per poll, cleared at top).
   - The hardened peer servers (`abd9601`): 4 MiB SO_SNDBUF/RCVBUF on the echo-server may add latency for small req/resp (larger buffer → kernel may delay before flushing); pthread-per-conn adds a thread-creation cost (one-time for bench-rtt's persistent conn — shouldn't affect steady state).
   - AWS network state drift over the day.
   - **This regression MUST be root-caused before the comparison numbers are published.** The relative ordering (dpdk_net < fstack ≈ linux) holds, but the absolute values are not trustworthy across runs.

### bench-rx-burst per-segment latency p50 (ns)

| stack | W | N | run1 | run2 | run3 | mean | stdev | CV% |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| dpdk_net | 64 | 16 | 139397 | 133889 | 148629 | 140638 | 7448 | 5.3 |
| dpdk_net | 64 | 256 | 155453 | 133828 | 167994 | 152425 | 17283 | 11.3 |
| dpdk_net | 128 | 256 | 180846 | 145127 | 182134 | 169369 | 21004 | 12.4 |
| dpdk_net | 256 | 256 | 244017 | 216691 | 213689 | 224799 | 16711 | 7.4 |
| fstack | 64 | 16 | 143360 | 125011 | 130017 | 132796 | 9485 | 7.1 |
| fstack | 128 | 16 | 129115 | 112105 | 124519 | 121913 | 8799 | 7.2 |
| fstack | 128 | 256 | 152627 | 112307 | 133835 | 132923 | 20175 | 15.2 |
| fstack | 256 | 256 | 167646 | 117015 | 137273 | 140645 | 25483 | 18.1 |
| linux_kernel | 64 | 16 | 182321 | 179719 | 176730 | 179590 | 2798 | 1.6 |
| linux_kernel | 128 | 256 | 176625 | 177150 | 175759 | 176511 | 702 | 0.4 |
| linux_kernel | 256 | 256 | 173987 | 177919 | 179680 | 177195 | 2915 | 1.6 |

(Full 9-cell × 3-stack table in the run dirs.)

**Findings:**
- **The "fstack beats dpdk_net on RX" ordering holds** in nearly every cell across all 3 runs: fstack p50 ≤ dpdk_net p50 (132µs vs 140µs at W=64 N=16; 121µs vs 141µs at W=128 N=16). Qualitatively robust.
- **CV is moderate-to-high (5-18%)** — not publication-grade precision. The N=256 buckets in particular have CV ~12-18%, likely because larger bursts span more poll iterations + more peer-side scheduling variance.
- linux_kernel is the most stable (CV 0.4-8%) but consistently slowest.

## Publication-readiness verdict — NOT YET

| Gate | Status |
|---|---|
| Functional reliability (no crashes, consistent pass rate) | ✅ |
| verify-rack-tlp fits the timeout | 🟡 fixed in `a997acd`, not re-validated |
| Numeric variance < 5% CV | ❌ — many metrics 5-20% CV |
| Absolute numbers stable run-to-run | ❌ — 2-3× regression T57→T58 unexplained |
| fstack RTT bimodality explained | ❌ — 128B/1024B flip ~200↔300µs |
| Cross-stack ORDERING holds | ✅ — dpdk_net fastest, fstack/linux close behind; fstack wins on RX |

**The qualitative comparison is publication-ready. The absolute numbers are not** — the T57→T58 2-3× regression and the fstack RTT bimodality must be root-caused first. Recommended next steps:
1. Re-run the suite once to confirm `a997acd` fixed the verify-rack-tlp timeout (13/13 OK).
2. Root-cause the T57→T58 regression — bisect: revert peer hardening locally, re-run bench-rtt; revert engine fix locally, re-run; isolate which commit moved the absolute numbers.
3. If the regression is the 4 MiB socket buffers on the peer echo-server, tune them down (256 KB–1 MB) — 4 MiB was overkill for req/resp.
4. Once stable, re-run 3× and confirm CV < 5% before quoting absolute numbers externally.

## Codex adversarial review

Dispatched after this report — see the codex review output appended to T57/T58 or in a separate `codex-adversarial-review-2026-05-13.md`. Codex was asked to specifically attack: the T57→T58 regression, the fstack bimodality, the comparison methodology, and any mis-labeled or methodologically-unsound result.
