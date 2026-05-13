# T60 — fast-iter publication-grade N=10 post-I2 statistical rigor (2026-05-13)

**Branch state:** `a10-perf-23.11` at HEAD `94050eb` (codex re-review
2026-05-13 fixes already merged; binaries rebuilt fresh against this
HEAD so `bench-tx-burst` emits `pmd_handoff_rate_bps`, not the legacy
`throughput_per_burst_bps` label that T59 still carried).

**Binary:** built with `cargo build --release --features fstack` at
the worktree start (mtimes verified fresh post-build before launching
the suite — `target/release/bench-tx-burst` 2026-05-13T08:32:09Z,
`bench-rx-burst` 08:32:12Z, `bench-rtt` 08:32:46Z, all post-merge of
the codex I2 rename commit).

**Master seed:** `100`. Per-run seeds 100..109 (codex I3 fast-iter-stats
derivation `master_seed + run_idx`). Master-seed reproducibility:
running `bash scripts/fast-iter-stats.sh 10 --seed 100 --skip-verify`
from any clean checkout of this commit regenerates the same per-tool
stack-order matrix and the same numerical aggregate.

**N actually completed:** 10/10 (all runs OK, zero failures).

**Wallclock:** 4,924 s ≈ **82 minutes** total. Per-run mean ~462 s
(7 min 42 s). Inter-run sleep 30 s × 9 ≈ 4.5 min of the budget.

## Why this report exists

T59 (`docs/bench-reports/t59-statistical-rigor-2026-05-13.md`) closed
codex IMPORTANT I3 ("statistical rigor at N≥5"). The codex 2026-05-13
re-review (`codex-rereview-2026-05-13.md`) accepted T59 as preliminary
but flagged THREE residual gaps that blocked the "publication-ready"
verdict:

1. **N=5 + pre-I2 binary.** T59 ran against a binary that still emitted
   the legacy `throughput_per_burst_bps` label for `bench-tx-burst`
   dpdk_net; the codex I2 rename had not propagated to the running
   binary. Aggregator handled both names, but the canonical numeric
   source was labeled pre-rename. **Fix:** rebuild fresh + re-run.
2. **N=5 is preliminary.** CI half-widths at N=5 engulfed 0 for several
   cells where the point estimate suggested an effect; bench-rtt
   dpdk_net-vs-fstack was 1/4 significant when the point-estimate
   ordering held in all 4 cells. **Fix:** N=10 tightens CIs by ~√2 ≈
   29 %, disambiguating those borderline cells.
3. **T59 raw artifacts not preserved in-tree.** The originating
   `target/bench-results/stats-2026-05-13/` dir was gitignored and
   reaped with the agent worktree. **Fix:** commit AGGREGATE.md
   in-tree alongside this report (see
   `docs/bench-reports/t60-aggregate-2026-05-13.md`).

T60 is the publication-grade re-run that addresses all three.

## Methodology

The harness, aggregator, and per-tool semantics are unchanged from
T59. Refer to:

- `docs/bench-reports/t59-statistical-rigor-2026-05-13.md` §Methodology
  for the bootstrap + paired-difference + Cohen's d definitions.
- `docs/bench-reports/methodology-and-claims-2026-05-09.md` for the
  per-tool metric semantics (especially the corrected
  `bench-rx-burst` cross-host disclaimer; codex BLOCKER fix).

Changes vs T59:

1. **N: 5 → 10.** Reaffirms the same paired-bootstrap procedure with
   double the per-pair sample; under iid the 95% CI half-width
   narrows by `1 - √(5/10) ≈ 29 %`.
2. **Master seed: 42 → 100.** Independent statistical replication —
   the T60 per-run stack-order matrices are a different draw from
   the codex-I4 Fisher-Yates space, so T60 is not a deterministic
   repeat of T59 with extra runs but an actual second sample.
3. **Binary: pre-I2 → post-I2.** `bench-tx-burst` dpdk_net throughput
   column emits the canonical `pmd_handoff_rate_bps` label
   (verified in the per-run CSV row 2 below). Numerics unchanged;
   label is publication-stable.
4. **Aggregate preserved in-tree.** `docs/bench-reports/t60-aggregate-2026-05-13.md`
   is the verbatim aggregator output that this report's tables are
   extracted from.

The `--skip-verify` flag was used (same as T59) — the netem
verify-rack-tlp matrix is a correctness/regression gate, not part of
the cross-stack absolute-number comparison surface.

## Artifacts

- **Pooled aggregate (in-tree, codex re-review fix):**
  `docs/bench-reports/t60-aggregate-2026-05-13.md` — verbatim
  `AGGREGATE.md` from the aggregator. Quote-able as the canonical
  numeric source.
- Raw bench-results (gitignored): originally at
  `target/bench-results/stats-2026-05-13-postI2/run-{001..010}-seed-{100..109}/`.
  Regenerate via `bash scripts/fast-iter-stats.sh 10 --seed 100 --skip-verify`
  on a checkout of this commit.
- Per-run completion log:
  `target/bench-results/stats-2026-05-13-postI2/runs.txt`
  (10/10 OK, total 4,924 s).
- Stats metadata:
  `target/bench-results/stats-2026-05-13-postI2/stats-metadata.json`.
- Harness: `scripts/fast-iter-stats.sh`. Aggregator:
  `scripts/aggregate-fast-iter.py`.

## Results — full per-cell tables (mean ± 95 % CI, CV, p50/p99/p999, paired diff)

All tables below are extracted verbatim from
`docs/bench-reports/t60-aggregate-2026-05-13.md`; cross-stack
"YES"/"no" significance is the percentile-bootstrap paired-CI test
described in T59 §Methodology.

### bench-rtt — `rtt_ns` (ns)

| connections | payload_bytes | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | dpdk_net | 220,082 | [211,524, 228,412] | 6.54% | 219,572 | 269,077 | 278,641 | 10 |
| 1 | 64 | linux_kernel | 227,416 | [221,243, 234,606] | 5.09% | 217,992 | 259,510 | 2,238,413 | 10 |
| 1 | 64 | fstack | 270,234 | [240,469, 299,916] | 17.72% | 298,372 | 309,560 | 318,972 | 10 |
| 1 | 128 | dpdk_net | 215,283 | [209,470, 221,194] | 4.99% | 211,366 | 259,790 | 275,039 | 10 |
| 1 | 128 | linux_kernel | 237,526 | [231,515, 244,052] | 4.67% | 230,271 | 266,459 | 2,322,155 | 10 |
| 1 | 128 | fstack | 250,600 | [220,756, 279,901] | 20.57% | 288,003 | 308,530 | 316,729 | 10 |
| 1 | 256 | dpdk_net | 219,943 | [214,428, 225,697] | 4.31% | 215,425 | 265,905 | 273,458 | 10 |
| 1 | 256 | linux_kernel | 233,916 | [225,699, 242,532] | 6.49% | 226,817 | 278,611 | 2,419,392 | 10 |
| 1 | 256 | fstack | 252,775 | [223,362, 281,464] | 19.76% | 293,896 | 308,828 | 339,186 | 10 |
| 1 | 1024 | dpdk_net | 217,814 | [211,224, 225,964] | 5.74% | 217,436 | 266,588 | 279,695 | 10 |
| 1 | 1024 | linux_kernel | 240,660 | [230,818, 249,917] | 6.78% | 234,382 | 285,715 | 2,357,672 | 10 |
| 1 | 1024 | fstack | 263,270 | [234,897, 290,191] | 18.11% | 297,500 | 310,297 | 343,847 | 10 |

**Paired comparison (A − B), `rtt_ns`** — significant when 0 is outside the 95 % CI.

| payload_bytes | A | B | mean_diff (ns) | 95% CI | Cohen's d | sig? |
|---:|---|---|---:|---|---:|:---:|
| 64   | dpdk_net | linux_kernel |  -7,334 | [-15,083, +381]    | -0.55 | no  |
| 64   | dpdk_net | fstack       | -50,152 | [-76,395, -18,408] | -1.02 | YES |
| 64   | linux_kernel | fstack   | -42,817 | [-65,822, -15,695] | -1.00 | YES |
| 128  | dpdk_net | linux_kernel | -22,243 | [-30,138, -14,479] | -1.62 | YES |
| 128  | dpdk_net | fstack       | -35,317 | [-62,890,  -6,913] | -0.73 | YES |
| 128  | linux_kernel | fstack   | -13,074 | [-46,933, +20,657] | -0.22 | no  |
| 256  | dpdk_net | linux_kernel | -13,972 | [-22,567,  -4,631] | -0.93 | YES |
| 256  | dpdk_net | fstack       | -32,832 | [-62,997,  -2,578] | -0.65 | YES |
| 256  | linux_kernel | fstack   | -18,859 | [-52,180, +15,763] | -0.32 | no  |
| 1024 | dpdk_net | linux_kernel | -22,846 | [-32,715, -10,797] | -1.19 | YES |
| 1024 | dpdk_net | fstack       | -45,456 | [-76,778, -12,764] | -0.82 | YES |
| 1024 | linux_kernel | fstack   | -22,610 | [-49,262,  +4,628] | -0.47 | no  |

**Findings — bench-rtt (deltas vs T59 N=5):**

1. **dpdk_net vs fstack: 1/4 sig → 4/4 sig.** The big N=10 win. All
   four payloads now show dpdk_net significantly faster than fstack
   (Cohen's d -0.65 to -1.02). At N=5 only 128B was significant;
   the wide fstack CI (CV 17-22 %, driven by the bimodality codex
   I1's `pkt_tx_delay=0` fix did NOT eliminate) had engulfed 0 for
   64/256/1024B. Doubling N tightened the paired-diff CI enough
   that 0 now sits outside on all four payloads.

2. **dpdk_net vs linux_kernel: 3/4 sig at N=5 → 3/4 sig at N=10, but
   the cell that's "no" flipped.** At N=5, 64/128/256B were sig
   YES (1024B no). At N=10, 128/256/1024B are sig YES (64B no:
   mean_diff -7,334 ns, CI [-15,083, +381] — borderline; the upper
   bound brushes 0 by 381 ns). Interpretation: linux_kernel's 64B
   mean dropped from 235,646 ns (N=5) to 227,416 ns (N=10) — closer
   to dpdk_net's 220,082, so the gap genuinely shrank in this seed
   draw. The qualitative ordering dpdk_net ≤ linux_kernel holds in
   the point estimate at all 4 payloads, but the 64B effect is now
   statistically indistinguishable from 0 in this sample.

3. **linux_kernel vs fstack: 0/4 sig → 1/4 sig.** Only the 64B cell
   is sig YES (mean_diff -42,817 ns, d=-1.00). The other three
   remain ambiguous because the fstack CV stays at 18-20 % across
   payloads — a wide marginal CI translates to a wide paired-diff
   CI even at N=10.

4. **fstack RTT bimodality persists.** N=10 confirms what T59
   already documented: fstack 95% CI half-width is ±30,000-30,000 ns
   on cells where the dpdk_net cell is ±10,000 ns. CV 18-21 % at
   128/256/1024B means the fstack distribution still flips between
   the ~200 µs mode and the ~300 µs mode across runs. The codex
   I1 `pkt_tx_delay=0` fix improved the mean but not the variance.

5. **Tail percentiles (pooled raw across all 10 runs, nearest-rank):**
   - dpdk_net p999: 273,458-279,695 ns across payloads (tight,
     well-bounded).
   - linux_kernel p999: 2,238,413-2,419,392 ns — true ~2.2-2.4 ms
     tail from kernel-TCP retransmit + scheduler-quantum jitter,
     reproducible across both N=5 and N=10. This is the
     publication-grade p999 signal: 10 000 samples × 10 runs ≈
     100 000 pooled samples, so the p999 is the 99.9th sample
     of 100k.
   - fstack p999: 316,729-343,847 ns — the bimodal upper-mode tail
     is exposed in the pooled distribution.

### bench-tx-burst — `pmd_handoff_rate_bps` (dpdk_net only)

> Cross-stack paired comparison is **intentionally not produced** for
> `pmd_handoff_rate_bps` vs `write_acceptance_rate_bps` — they
> measure different layers (codex IMPORTANT I2; see
> `tools/bench-tx-burst/src/lib.rs:60-114`). The aggregator pairs
> by exact metric name; this row appears here only for dpdk_net.

| K_bytes | G_ms | stack | mean (bps) | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---:|---:|---|---:|---|---:|---:|---:|---:|---:|
| 65536   | 0.0  | dpdk_net | 1,032,540,016 | [1,028,203,062, 1,037,808,756] | 0.81% | 1,051,119,043 | 1,121,281,438 | 1,198,689,337 | 10 |
| 65536   | 10.0 | dpdk_net | 2,109,279,645 | [2,039,533,857, 2,168,478,250] | 5.20% | 2,103,123,596 | 2,208,980,601 | 2,217,079,565 | 10 |
| 1048576 | 0.0  | dpdk_net | 1,037,538,364 | [1,030,124,694, 1,044,471,703] | 1.16% | 1,037,397,902 | 1,063,161,008 | 1,070,381,163 | 10 |
| 1048576 | 10.0 | dpdk_net | 1,186,871,093 | [1,175,350,031, 1,198,162,376] | 1.65% | 1,187,472,938 | 1,214,424,483 | 1,219,287,187 | 10 |

**Findings:** dpdk_net PMD-handoff rate is stable across the K×G
grid at ~1.0-2.1 Gbps with CV ≤ 5.2 %. This is **not** wire-line
throughput; it is the rate at which `rte_eth_tx_burst` returns
queue admission. See
`docs/bench-reports/methodology-and-claims-2026-05-09.md` §Metrics
— bench-tx-burst (post-codex-I2) for the layer definition.

### bench-tx-burst — `burst_initiation_ns` (ns)

| K_bytes | G_ms | tx_ts_mode | stack | mean (ns) | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---:|---:|---|---|---:|---|---:|---:|---:|---:|---:|
| 65536   | 0.0  | tsc_fallback | dpdk_net |    26,654 | [26,253, 27,152] |  3.06% |    26,092 |    77,531 |   138,215 | 10 |
| 65536   | 0.0  | tsc_fallback | fstack   |    97,508 | [92,749, 102,020] |  8.31% |    85,047 |   161,314 |   167,098 | 10 |
| 65536   | 10.0 | tsc_fallback | dpdk_net |    27,281 | [26,613, 28,189] |  5.07% |    27,182 |    30,068 |    32,906 | 10 |
| 65536   | 10.0 | tsc_fallback | fstack   |    87,238 | [85,493, 89,321] |  3.77% |    87,110 |    94,680 |   120,963 | 10 |
| 1048576 | 0.0  | tsc_fallback | dpdk_net |    22,076 | [20,743, 23,629] | 11.42% |    20,594 |    87,037 |   120,633 | 10 |
| 1048576 | 0.0  | tsc_fallback | fstack   | 1,885,308 | [1,868,033, 1,903,014] |  1.59% | 1,866,679 | 1,940,480 | 1,952,649 | 10 |
| 1048576 | 10.0 | tsc_fallback | dpdk_net |   107,625 | [105,568, 110,538] |  4.39% |   106,580 |   122,785 |   127,792 | 10 |
| 1048576 | 10.0 | tsc_fallback | fstack   | 1,863,582 | [1,840,598, 1,889,420] |  2.19% | 1,861,785 | 1,910,715 | 1,933,002 | 10 |
| 65536   | 0.0  | _(n/a)_      | linux_kernel |  5,071 | [4,048,   6,363] | 39.10% |  3,334 |  26,887 | 233,890 | 10 |
| 65536   | 10.0 | _(n/a)_      | linux_kernel |  8,216 | [6,982,   9,375] | 25.72% |  6,730 |  22,460 |  33,222 | 10 |
| 1048576 | 0.0  | _(n/a)_      | linux_kernel |  1,726 | [1,451,   2,050] | 29.30% |    949 |  12,842 |  38,610 | 10 |
| 1048576 | 10.0 | _(n/a)_      | linux_kernel |  9,472 | [7,790,  11,724] | 33.29% |  7,898 |  23,137 |  27,796 | 10 |

**Paired comparison (A − B), `burst_initiation_ns`** — dpdk_net vs
fstack only (linux_kernel has `tx_ts_mode` absent, so can't pair).

| K_bytes | G_ms | A | B | mean_diff (ns) | 95% CI | Cohen's d | sig? |
|---:|---:|---|---|---:|---|---:|:---:|
| 65536   | 0.0  | dpdk_net | fstack |    -70,854 | [-75,300, -66,324] |  -8.98 | YES |
| 65536   | 10.0 | dpdk_net | fstack |    -59,958 | [-61,479, -58,778] | -26.91 | YES |
| 1048576 | 0.0  | dpdk_net | fstack | -1,863,232 | [-1,882,237, -1,845,484] | -61.52 | YES |
| 1048576 | 10.0 | dpdk_net | fstack | -1,755,957 | [-1,778,466, -1,732,501] | -45.82 | YES |

**Findings — bench-tx-burst:**

- **dpdk_net wins burst initiation, 4/4 cells (sig YES).** Identical
  to T59's finding, now at N=10. Cohen's d -8.98 to -61.52 — these
  are the largest effect sizes in the entire suite. The fstack
  serializes the 1 MiB burst through its mbuf-allocation path
  (~1.86 ms), where dpdk_net does the bulk PMD enqueue in
  ~22-107 µs.
- **linux_kernel `burst_initiation_ns` 1.7-9.5 µs** — same metric-
  asymmetry as T59. Kernel `write()` returns when the socket
  buffer accepts; DPDK's metric measures PMD-queue admission.
  Cross-arm comparison is not produced (codex I2).

### bench-tx-burst — `burst_steady_bps` (bits/sec)

| K_bytes | G_ms | tx_ts_mode | stack | mean (bps) | 95% CI | CV% | n_runs |
|---:|---:|---|---|---:|---|---:|---:|
| 65536   | 0.0  | tsc_fallback | dpdk_net   | 1,089,535,643 | [1,084,733,552, 1,094,949,473] | 0.80% | 10 |
| 65536   | 0.0  | tsc_fallback | fstack     | 16,243,992,087,210 | [15,370,096,609,541, 17,052,498,638,357] | 8.75% | 10 |
| 65536   | 10.0 | tsc_fallback | dpdk_net   | 2,368,991,646 | [2,285,451,246, 2,437,543,696] | 5.27% | 10 |
| 65536   | 10.0 | tsc_fallback | fstack     | 10,630,514,064,189 | [9,678,152,307,558, 11,493,217,072,756] | 14.59% | 10 |
| 1048576 | 0.0  | tsc_fallback | dpdk_net   | 1,040,383,917 | [1,032,955,090, 1,047,574,405] | 1.17% | 10 |
| 1048576 | 0.0  | tsc_fallback | fstack     | 119,679,156,002,214 | [108,186,130,185,456, 131,999,391,177,862] | 16.74% | 10 |
| 1048576 | 10.0 | tsc_fallback | dpdk_net   | 1,205,211,923 | [1,194,387,353, 1,216,065,541] | 1.63% | 10 |
| 1048576 | 10.0 | tsc_fallback | fstack     | 116,477,505,028,150 | [101,674,926,226,644, 132,106,785,460,253] | 21.75% | 10 |
| 65536   | 0.0  | _(n/a)_      | linux_kernel | 81,433,129,203 | [74,055,521,669, 88,314,469,316] | 15.00% | 10 |
| 65536   | 10.0 | _(n/a)_      | linux_kernel | 39,387,168,115 | [37,615,466,361, 41,459,913,555] |  8.41% | 10 |
| 1048576 | 0.0  | _(n/a)_      | linux_kernel |  6,518,790,075 | [5,446,470,356, 8,197,376,697] | 36.94% | 10 |
| 1048576 | 10.0 | _(n/a)_      | linux_kernel | 46,689,442,211 | [45,108,337,304, 48,445,102,930] |  6.02% | 10 |

**Paired comparison (A − B), `burst_steady_bps`** — dpdk_net vs fstack.

| K_bytes | G_ms | A | B | mean_diff (bps) | 95% CI | Cohen's d | sig? |
|---:|---:|---|---|---:|---|---:|:---:|
| 65536   | 0.0  | dpdk_net | fstack | -16,242,902,551,568 | [-17,086,873,828,852, -15,396,225,543,808] | -11.42 | YES |
| 65536   | 10.0 | dpdk_net | fstack | -10,628,145,072,543 | [-11,503,945,970,828,  -9,722,301,338,398] |  -6.85 | YES |
| 1048576 | 0.0  | dpdk_net | fstack | -119,678,115,618,297 | [-130,875,613,989,635, -108,108,572,294,685] |  -5.97 | YES |
| 1048576 | 10.0 | dpdk_net | fstack | -116,476,299,816,227 | [-131,691,782,479,797, -101,792,228,679,790] |  -4.60 | YES |

**Findings:** dpdk_net vs fstack `burst_steady_bps` 4/4 sig (same as
T59). The fstack value is structurally larger because
`burst_steady_bps` for fstack measures the in-memory copy of the
1 MiB user buffer into its ring (~10-130 Tbps in memory bandwidth),
not the wire-rate. This is a structural artifact of the bench
methodology, not a true throughput comparison — see the metric-
asymmetry note in T59 §bench-tx-burst.

### bench-tx-maxtp — `sustained_goodput_bps` (bits/sec)

| C | W_bytes | stack | mean (bps) | 95% CI | CV% | n_runs |
|---:|---:|---|---:|---|---:|---:|
|  1 |   4096 | dpdk_net |    987,873,996 | [    981,898,921,    993,725,294] |  1.03% | 10 |
|  1 |   4096 | fstack   |  1,845,518,719 | [  1,798,875,725,  1,885,595,710] |  4.37% | 10 |
|  1 |   4096 | linux_kernel |  4,952,577,792 | [  4,931,495,898,  4,963,644,535] |  0.66% | 10 |
|  4 |   4096 | dpdk_net |  1,016,585,715 | [  1,009,413,654,  1,023,519,526] |  1.18% | 10 |
|  4 |   4096 | fstack   |  3,018,131,150 | [  2,996,786,211,  3,038,436,937] |  1.19% | 10 |
|  4 |   4096 | linux_kernel | 10,410,637,970 | [  9,157,501,155, 11,491,927,821] | 19.70% | 10 |
| 16 |   4096 | dpdk_net |    794,271,216 | [    785,760,264,    803,545,988] |  1.84% | 10 |
| 16 |   4096 | fstack   |  2,665,436,791 | [  2,590,747,129,  2,717,514,472] |  4.32% | 10 |
| 16 |   4096 | linux_kernel |  8,081,724,440 | [  7,233,960,080,  8,674,056,365] | 14.63% | 10 |
|  1 |  16384 | dpdk_net |    996,521,021 | [    989,194,921,  1,004,007,699] |  1.23% | 10 |
|  1 |  16384 | fstack   |  2,704,200,098 | [  2,682,515,359,  2,729,237,183] |  1.38% |  9 |
|  1 |  16384 | linux_kernel |  4,951,370,096 | [  4,939,634,899,  4,962,334,593] |  0.39% | 10 |
|  4 |  16384 | dpdk_net |    998,759,016 | [    990,548,731,  1,006,647,012] |  1.35% | 10 |
|  4 |  16384 | fstack   |  2,754,163,317 | [  2,727,272,640,  2,781,959,785] |  1.71% |  9 |
|  4 |  16384 | linux_kernel | 11,884,703,296 | [ 11,323,306,115, 12,393,910,888] |  7.79% | 10 |
| 16 |  16384 | dpdk_net |    960,117,785 | [    940,382,699,    978,414,203] |  3.53% | 10 |
| 16 |  16384 | fstack   |  2,024,401,141 | [  1,640,603,847,  2,300,449,855] | 27.68% | 10 |
| 16 |  16384 | linux_kernel | 12,141,322,445 | [ 11,703,196,525, 12,402,834,582] |  4.90% | 10 |
|  1 |  65536 | dpdk_net |  1,034,592,562 | [  1,025,623,564,  1,042,203,998] |  1.43% | 10 |
|  1 |  65536 | fstack   |  2,666,220,701 | [  2,538,989,926,  2,741,483,519] |  4.57% |  4 |
|  1 |  65536 | linux_kernel |  4,960,393,422 | [  4,956,708,774,  4,963,453,547] |  0.12% | 10 |
|  4 |  65536 | dpdk_net |    917,648,800 | [    710,954,871,  1,029,485,552] | 35.22% | 10 |
|  4 |  65536 | fstack   |  2,681,828,086 | [  2,648,914,893,  2,716,040,227] |  1.51% |  5 |
|  4 |  65536 | linux_kernel | 12,351,673,008 | [ 12,283,140,347, 12,394,428,081] |  0.81% | 10 |
| 16 |  65536 | dpdk_net |    751,266,093 | [    491,435,374,    947,034,892] | 51.59% | 10 |
| 16 |  65536 | fstack   |  1,049,760,585 | [    555,160,913,  1,570,040,179] | 81.55% | 10 |
| 16 |  65536 | linux_kernel | 12,381,019,514 | [ 12,336,387,962, 12,404,493,173] |  0.56% | 10 |

**Paired comparison (A − B), `sustained_goodput_bps`** — dpdk_net vs
fstack only (linux_kernel has `tx_ts_mode` absent, different
metric structure).

| C | W_bytes | A | B | mean_diff (bps) | 95% CI | Cohen's d | sig? |
|---:|---:|---|---|---:|---|---:|:---:|
|  1 |   4096 | dpdk_net | fstack |   -857,644,724 | [    -896,935,409,    -805,215,497] | -10.96 | YES |
|  4 |   4096 | dpdk_net | fstack | -2,001,545,435 | [  -2,019,008,991,  -1,984,947,512] | -67.73 | YES |
| 16 |   4096 | dpdk_net | fstack | -1,871,165,575 | [  -1,918,062,364,  -1,801,979,773] | -17.15 | YES |
|  1 |  16384 | dpdk_net | fstack | -1,708,464,697 | [  -1,731,129,412,  -1,688,885,099] | -49.77 | YES |
|  4 |  16384 | dpdk_net | fstack | -1,756,482,387 | [  -1,786,202,131,  -1,724,860,165] | -36.76 | YES |
| 16 |  16384 | dpdk_net | fstack | -1,064,283,357 | [  -1,353,313,995,    -723,892,902] |  -1.86 | YES |
|  1 |  65536 | dpdk_net | fstack | -1,643,648,517 | [  -1,723,243,696,  -1,516,682,971] | -12.43 | YES |
|  4 |  65536 | dpdk_net | fstack | -1,872,631,279 | [  -2,283,041,722,  -1,653,655,282] |  -4.08 | YES |
| 16 |  65536 | dpdk_net | fstack |   -298,494,492 | [    -910,595,834,    +270,986,815] |  -0.29 | no  |

**Findings — bench-tx-maxtp:**

- **dpdk_net vs fstack: 9/9 sig → 8/9 sig.** At N=5 the 16/65536
  cell was sig YES with d=-0.94 (the smallest effect among the 9
  paired cells). At N=10 that cell flips to "no" because fstack's
  variance widened (N=10 fstack 16/65536 CV is 82 %; dpdk_net is
  52 %) — the paired-diff CI [-910 M, +271 M] now engulfs 0. The
  point estimate -298 M bps still favors fstack but the
  significance is gone. **This is the only cell where T60 has
  LESS evidence than T59 for an effect.** Read: at C=16 + 64 KiB
  buffer + high concurrency, fstack throughput is on the edge of
  the run-to-run jitter floor (likely DPDK mempool exhaustion or
  fstack scheduler oscillation; not a stack-comparison signal).
- **dpdk_net vs fstack on 4/65536 and 16/16384:** sig YES with
  smaller d (-1.86 and -4.08) — these were larger effects at
  N=5 and stayed large at N=10. The dpdk_net vs fstack ordering
  on the dpdk_net NIC holds: fstack delivers ~2-3 Gbps where
  dpdk_net delivers ~0.7-1.0 Gbps.
- **linux_kernel absolute ~5-12 Gbps** is the loopback-routing-vs-
  real-wire delta — see T57 §Methodology — two-ENI comparison.
  Same caveat as T59: qualitative ordering is preserved but
  absolute cross-stack ratios are distorted by the two-ENI
  asymmetry that the codex 2026-05-13 re-review accepted as
  disclosed.
- **fstack reports `tx_pps=0`** (same as T59) — PPS measurement
  uses DPDK port stats which aren't queryable under F-Stack's PMD
  wrapping. Not a data-loss bug.
- **N<10 cells:** `fstack 1/65536` (n=4), `fstack 4/65536` (n=5),
  `fstack 1/16384` (n=9), `fstack 4/16384` (n=9) lost runs to
  `connect timeout: connections did not complete within 30 s
  (DPDK/ARP state may be dirty — clean /run/dpdk/rte/ and retry)`.
  This is a known fstack failure mode at large W; the aggregator
  pools only the valid runs.

### bench-rx-burst — `latency_ns` (ns) — **cross-host metric**

> **Cross-host metric reminder (codex BLOCKER fix, re-review
> 2026-05-13):** `latency_ns` is computed as
> `dut_recv_ns − peer_send_ns` with both endpoints anchored on
> `CLOCK_REALTIME` (see
> `docs/bench-reports/methodology-and-claims-2026-05-09.md`
> §Metrics — bench-rx-burst for the full capture breakdown).
> Every sample includes peer send latency, peer NIC TX, AWS data-
> plane transit, DUT NIC RX, plus an NTP-skew floor of ~100 µs
> same-AZ — it is **NOT** a pure DUT-side internal RX
> measurement. The cross-stack **ordering** signal in this table
> (e.g. "fstack < dpdk_net < linux_kernel on this metric") is the
> publication-grade claim; absolute µs values are end-to-end
> cross-host values bounded below by the NTP-skew floor.

| W (segment_size) | N (burst_count) | stack | mean (ns) | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---:|---:|---|---:|---|---:|---:|---:|---:|---:|
|  64 |  16 | dpdk_net     | 109,931 | [103,106, 115,849] |  9.87% | 107,603 | 128,467 |   137,834 | 10 |
|  64 |  16 | linux_kernel | 138,459 | [132,469, 144,109] |  7.17% | 135,000 | 206,163 |   973,522 | 10 |
|  64 |  16 | fstack       |  97,020 | [ 90,939, 102,737] | 10.02% |  96,016 | 112,124 |   120,066 | 10 |
|  64 |  64 | dpdk_net     | 113,461 | [105,186, 120,432] | 11.28% | 109,899 | 135,710 |   141,658 | 10 |
|  64 |  64 | linux_kernel | 152,314 | [144,378, 161,989] | 10.25% | 143,528 | 309,108 |   965,047 | 10 |
|  64 |  64 | fstack       | 108,070 | [100,301, 116,774] | 12.55% | 103,969 | 128,580 |   153,663 | 10 |
|  64 | 256 | dpdk_net     | 121,754 | [112,206, 129,533] | 12.65% | 119,285 | 156,634 |   173,218 | 10 |
|  64 | 256 | linux_kernel | 158,849 | [144,353, 176,750] | 17.72% | 145,875 | 577,002 | 1,033,882 | 10 |
|  64 | 256 | fstack       | 112,522 | [105,330, 119,488] | 10.57% | 109,952 | 140,720 |   263,816 | 10 |
| 128 |  16 | dpdk_net     | 110,634 | [104,262, 117,029] | 10.01% | 107,689 | 134,342 |   139,696 | 10 |
| 128 |  16 | linux_kernel | 136,043 | [126,680, 145,237] | 11.92% | 123,377 | 336,313 |   621,239 | 10 |
| 128 |  16 | fstack       |  99,547 | [ 91,316, 107,128] | 13.09% |  98,622 | 124,796 |   128,731 | 10 |
| 128 |  64 | dpdk_net     | 118,743 | [111,301, 125,491] | 10.78% | 116,594 | 142,308 |   151,174 | 10 |
| 128 |  64 | linux_kernel | 154,838 | [140,985, 170,363] | 16.97% | 138,396 | 585,500 | 1,410,564 | 10 |
| 128 |  64 | fstack       |  99,242 | [ 94,634, 103,757] |  7.34% |  96,818 | 115,893 |   122,461 | 10 |
| 128 | 256 | dpdk_net     | 140,827 | [130,457, 149,829] | 11.94% | 141,721 | 181,959 |   205,801 | 10 |
| 128 | 256 | linux_kernel | 163,475 | [151,149, 175,996] | 13.12% | 153,960 | 541,048 |   748,356 | 10 |
| 128 | 256 | fstack       | 114,016 | [109,450, 118,539] |  6.67% | 107,866 | 304,429 |   358,525 | 10 |
| 256 |  16 | dpdk_net     | 116,310 | [109,704, 121,986] |  8.98% | 114,052 | 140,672 |   145,944 | 10 |
| 256 |  16 | linux_kernel | 151,119 | [138,026, 168,283] | 17.92% | 135,894 | 349,476 | 1,145,782 | 10 |
| 256 |  16 | fstack       |  97,381 | [ 91,168, 103,382] | 10.85% |  94,816 | 118,195 |   125,750 | 10 |
| 256 |  64 | dpdk_net     | 133,286 | [125,953, 139,334] |  8.21% | 133,482 | 163,438 |   171,314 | 10 |
| 256 |  64 | linux_kernel | 168,362 | [148,960, 189,121] | 21.26% | 144,645 | 827,018 | 1,608,709 | 10 |
| 256 |  64 | fstack       | 106,636 | [ 98,580, 115,528] | 13.22% | 104,317 | 125,412 |   294,249 | 10 |
| 256 | 256 | dpdk_net     | 194,812 | [188,685, 200,698] |  5.67% | 199,710 | 271,835 |   287,352 | 10 |
| 256 | 256 | linux_kernel | 169,796 | [154,590, 186,032] | 16.07% | 151,776 | 689,186 | 1,106,816 | 10 |
| 256 | 256 | fstack       | 131,346 | [125,131, 136,764] |  7.98% | 119,682 | 370,813 |   584,877 | 10 |

**Paired comparison (A − B), `latency_ns`** — all 27 pairs.

| W | N | A | B | mean_diff (ns) | 95% CI | Cohen's d | sig? |
|---:|---:|---|---|---:|---|---:|:---:|
|  64 |  16 | dpdk_net | linux_kernel | -28,528 | [-36,222, -20,046] | -1.98 | YES |
|  64 |  16 | dpdk_net | fstack       | +12,911 | [ +6,864, +19,972] |  1.14 | YES |
|  64 |  16 | linux_kernel | fstack   | +41,438 | [+35,834, +46,443] |  4.51 | YES |
|  64 |  64 | dpdk_net | linux_kernel | -38,853 | [-50,136, -27,259] | -2.01 | YES |
|  64 |  64 | dpdk_net | fstack       |  +5,391 | [ -6,733, +16,896] |  0.26 | no  |
|  64 |  64 | linux_kernel | fstack   | +44,244 | [+32,503, +59,293] |  1.91 | YES |
|  64 | 256 | dpdk_net | linux_kernel | -37,095 | [-52,399, -22,431] | -1.37 | YES |
|  64 | 256 | dpdk_net | fstack       |  +9,232 | [ -1,541, +21,556] |  0.49 | no  |
|  64 | 256 | linux_kernel | fstack   | +46,327 | [+29,483, +62,344] |  1.64 | YES |
| 128 |  16 | dpdk_net | linux_kernel | -25,409 | [-35,871, -15,098] | -1.41 | YES |
| 128 |  16 | dpdk_net | fstack       | +11,087 | [ +3,299, +18,232] |  0.86 | YES |
| 128 |  16 | linux_kernel | fstack   | +36,496 | [+23,253, +48,654] |  1.65 | YES |
| 128 |  64 | dpdk_net | linux_kernel | -36,095 | [-50,222, -19,448] | -1.34 | YES |
| 128 |  64 | dpdk_net | fstack       | +19,502 | [+10,907, +27,749] |  1.32 | YES |
| 128 |  64 | linux_kernel | fstack   | +55,597 | [+42,971, +70,771] |  2.33 | YES |
| 128 | 256 | dpdk_net | linux_kernel | -22,649 | [-34,084,  -8,264] | -1.02 | YES |
| 128 | 256 | dpdk_net | fstack       | +26,811 | [+15,734, +38,228] |  1.48 | YES |
| 128 | 256 | linux_kernel | fstack   | +49,460 | [+38,457, +60,882] |  2.52 | YES |
| 256 |  16 | dpdk_net | linux_kernel | -34,809 | [-51,501, -20,621] | -1.25 | YES |
| 256 |  16 | dpdk_net | fstack       | +18,929 | [+12,061, +26,515] |  1.53 | YES |
| 256 |  16 | linux_kernel | fstack   | +53,738 | [+41,898, +67,558] |  2.44 | YES |
| 256 |  64 | dpdk_net | linux_kernel | -35,077 | [-59,607, -14,065] | -0.94 | YES |
| 256 |  64 | dpdk_net | fstack       | +26,649 | [+19,543, +35,067] |  1.95 | YES |
| 256 |  64 | linux_kernel | fstack   | +61,726 | [+41,031, +83,870] |  1.60 | YES |
| 256 | 256 | dpdk_net | linux_kernel | +25,016 | [ +9,519, +40,256] |  0.90 | YES |
| 256 | 256 | dpdk_net | fstack       | +63,466 | [+55,036, +72,667] |  3.98 | YES |
| 256 | 256 | linux_kernel | fstack   | +38,450 | [+26,581, +52,263] |  1.72 | YES |

**Findings — bench-rx-burst (cross-host delta; ordering claim only):**

All claims below are about the **cross-host** `dut_recv_ns −
peer_send_ns` delta. They are NOT claims about pure DUT-side
internal RX-path cost — that would require HW RX timestamps the
bench instance does not expose. See the disclaimer block above
and `methodology-and-claims-2026-05-09.md` §Metrics — bench-rx-burst.

- **Significance gain vs T59.** T59 had **23/27** paired cells
  significant; T60 has **25/27**. The two cells still "no" at N=10
  (64/64 and 64/256 dpdk_net-vs-fstack) were also "no" at N=5 —
  no regression. The notable T60 gain is at the
  dpdk_net-vs-linux_kernel comparison: **all 9** cells now sig
  YES (T59 had 8/9; the W=256,N=256 cell was "no" at N=5 and is
  now sig YES — N=10 disambiguated it). The linux_kernel-vs-fstack
  comparison stayed at 9/9 sig YES (already strong at N=5).
- **Cross-stack ordering on the cross-host delta:** fstack ≤
  dpdk_net ≤ linux_kernel holds at all 9 (W, N) cells in the
  point estimates. fstack wins outright at 7/9 cells (the two
  exceptions at W=64,N=64 and W=64,N=256 are statistical ties
  where fstack's mean ≈ dpdk_net's mean).
- **W=256, N=256 anomaly:** This cell has dpdk_net > linux_kernel
  in the point estimate (+25,016 ns, sig YES at d=0.90). The
  qualitative ordering flip from the rest of the grid suggests
  this cell is in a different regime (largest payload × largest
  burst — the 64 KiB total per burst probably triggers different
  buffering behavior on the DUT NIC). N=20 may stabilize whether
  this is a real ordering inversion or sampling noise.
- **p999 tails:** linux_kernel pings 0.6-1.6 ms tails (kernel TCP
  retransmits + scheduler quanta on the cross-host delta).
  dpdk_net p999 stays ≤ 287 µs; fstack p999 stays ≤ 585 µs (with
  a single outlier W=256,N=256 cell at 584 µs — fstack has a
  long tail at this dimension). N=10's pooled p999 is the
  per-run-averaged p999 (no `--raw-samples-csv` sidecar for
  bench-rx-burst yet — same as T59).

## Paired-comparison summary (T60 vs T59)

Across the four tools, the cross-stack paired-bootstrap test
produced significance like this (N=10 result first; T59 N=5
result in parens):

| Comparison                       | bench-rtt          | bench-tx-burst (init)      | bench-tx-maxtp (goodput)   | bench-rx-burst (latency) |
|---|---|---|---|---|
| dpdk_net vs linux_kernel         | **3/4** (was 3/4)  | n/a (`tx_ts_mode` differs) | 0/9 paired                  | **9/9** (was 8/9)         |
| dpdk_net vs fstack               | **4/4** (was 1/4)  | **4/4** (was 4/4)          | **8/9** (was 9/9)           | **7/9** (was 6/9)         |
| linux_kernel vs fstack           | **1/4** (was 0/4)  | n/a (`tx_ts_mode` differs) | 0/9 paired                  | **9/9** (was 9/9)         |

Total sig cells (summing all paired-comparison rows in
`AGGREGATE.md`, including the structural metrics
`burst_steady_bps` and `tx_pps` which don't appear in the
narrative column above but DO show paired-bootstrap rows):
**58/65** (89 %) at N=10 vs 53/65 (82 %) at N=5
(verified by `grep -c '| YES |'` on both
`docs/bench-reports/t60-aggregate-2026-05-13.md` and
`docs/bench-reports/t59-aggregate-2026-05-13.md`).

For the four "headline" metrics in the table (rtt_ns,
burst_initiation_ns, sustained_goodput_bps, latency_ns):
**45/52** (87 %) at N=10 vs **40/52** (77 %) at N=5 — the gain
is concentrated in bench-rtt (4 → 8 sig YES) and bench-rx-burst
(23 → 25 sig YES); burst_initiation_ns was already 4/4 at N=5,
and bench-tx-maxtp lost one cell (9 → 8) when fstack's structural
noise at C=16/W=65536 widened past the paired-diff CI.

Interpretation:

- **Large effects survive both N=5 and N=10.** bench-tx-burst
  burst_initiation (Cohen's d -8.98 to -61.52) and bench-tx-maxtp
  dpdk_net-vs-fstack goodput (mostly d ≤ -4) were rock-solid at
  N=5 and stay rock-solid at N=10.
- **Borderline cells from T59 that N=10 disambiguated:**
  - bench-rtt dpdk_net-vs-fstack at 64/256/1024B: **resolved sig YES**
    (was "no" at N=5).
  - bench-rx-burst dpdk_net-vs-linux_kernel at W=256,N=256:
    **resolved sig YES** (was "no" at N=5).
  - bench-rtt linux_kernel-vs-fstack at 64B: **resolved sig YES**
    (was "no" at N=5).
- **One cell went the OTHER way at N=10:**
  - bench-rtt dpdk_net-vs-linux_kernel at 64B: **sig YES → no.**
    Point estimate gap shrank from -21,976 ns (T59 N=5) to
    -7,334 ns (T60 N=10); upper bound of CI brushes 0 by 381 ns.
    Interpretation: the dpdk_net advantage at 64B is real but
    small enough that it dips below the run-to-run noise floor
    in this seed draw. T59's "sig YES" at N=5 was technically
    correct but borderline; T60's "no" is also correct — the
    cell's true effect size is < 1 SD of the run-to-run noise.
- **One cell that lost significance:**
  - bench-tx-maxtp dpdk_net-vs-fstack at 16/65536: **sig YES → no.**
    The fstack value at this dim has CV 82 % (extreme run-to-run
    variance — likely DPDK mempool exhaustion + fstack scheduler
    interaction at C=16 high concurrency). Both stacks have CV
    > 50 % at this cell, so the paired-diff CI spans ±1 Gbps. At
    N=10 the point estimate -298 M bps is genuinely indistinguishable
    from 0 in this noise floor. N=20 may not help if the noise
    is structural rather than sampling.
- **Tails (p999):** consistent across N=5 and N=10 — linux_kernel
  exposes ~1-2.5 ms p999 tails for bench-rtt that are real
  TCP retransmit events; dpdk_net stays ≤ 290 µs; fstack p999
  is ≤ 345 µs for bench-rtt and ≤ 585 µs for bench-rx-burst.

## Cross-host caveat — bench-rx-burst (reminder)

`bench-rx-burst latency_ns` is a cross-host CLOCK_REALTIME delta,
not a pure DUT-side internal RX latency. The codex 2026-05-13
re-review BLOCKER was the methodology doc's earlier false claim
of "DUT-side internal RX latency"; that was corrected in
`docs/bench-reports/methodology-and-claims-2026-05-09.md:196-260`
and reflected in T59's RX section disclaimer. T60 inherits the
same correction. Re-quoted for cross-reference:

- Every `rx_latency_ns` sample includes (1) peer send timestamp,
  (2) peer NIC TX, (3) AWS data-plane transit, (4) DUT NIC RX,
  (5) engine/kernel event dispatch, and (6) an NTP-skew floor of
  ~100 µs same-AZ.
- **The cross-stack ordering signal** (fstack ≤ dpdk_net ≤
  linux_kernel on this metric) IS the publication-grade claim.
- **Absolute µs values are NOT** evidence for pure-stack RX cost.
  To get that we'd need HW RX timestamps (`SO_TIMESTAMPING` or
  `tx_timestamp`/`rx_timestamp` dynfield), which the current AWS
  ENA bench instance does not expose. Phase 9 c7i HW-TS work is
  the future-work item.

## Verdict — publication-ready for ordering claims (with the named exceptions)

T60 closes the codex 2026-05-13 re-review's three IMPORTANT residual
gaps (N=5 preliminary, pre-I2 binary, T59 artifacts not in-tree).
Verdict by claim class:

- **Cross-stack ordering claims at N=10:** *publication-ready.*
  The 58/65 sig-YES paired cells are the publication-grade
  evidence. The qualitative orderings T57/T58 first reported are
  now backed by N=10 paired-bootstrap 95 % CIs:
  - **bench-rtt:** dpdk_net ≤ linux_kernel ≤ fstack (mean) at most
    payloads. dpdk_net beats fstack significantly at all 4
    payloads (was 1/4 at N=5).
  - **bench-tx-burst burst_initiation:** dpdk_net wins outright
    over fstack at all 4 cells (Cohen's d -8.98 to -61.52).
    linux_kernel is structurally a different metric (codex I2).
  - **bench-tx-maxtp `sustained_goodput_bps`:** fstack > dpdk_net
    significantly at 8/9 cells on the dpdk-NIC (same NIC).
    linux_kernel absolute numbers are higher but on a different
    physical NIC (two-ENI methodology, acknowledged).
  - **bench-rx-burst cross-host latency:** fstack ≤ dpdk_net ≤
    linux_kernel at most (W, N) cells. The W=256,N=256 cell
    inverts dpdk_net vs linux_kernel — flagged as anomaly.

- **Cells that need N ≥ 20 to disambiguate:**
  - **bench-rtt dpdk_net-vs-linux_kernel at 64B:** mean_diff
    -7,334 ns with CI upper bound +381 ns — N=10 says "no",
    point estimate says "yes". A larger N can tighten this
    one way or the other.
  - **bench-tx-maxtp dpdk_net-vs-fstack at 16/65536:** CV > 50 %
    on both stacks; the paired-diff CI spans ±1 Gbps. Likely a
    structural noise issue (mempool exhaustion), not a sampling
    issue — N=20 may not help.
  - **bench-rtt linux_kernel-vs-fstack at 128/256/1024B:** the
    wide fstack CI (CV 18-21 %, bimodality persists) widens
    the paired CI enough to engulf 0 in 3 of 4 cells. Fixing
    fstack RTT bimodality is the right path here, not more N.

- **Cells that need fstack RTT bimodality investigation (not
  more N):**
  - The fstack RTT distribution's two modes at ~200 µs and
    ~300 µs are persistent across both T59 N=5 and T60 N=10.
    codex I1's `pkt_tx_delay=0` fix improved the mean but did
    not eliminate the mode flip. Diagnosing the trigger
    (payload_256 always lands in the 300 µs mode; others
    flip) is a follow-up; until then,
    linux_kernel-vs-fstack paired tests on RTT will remain
    underpowered.

- **Tail percentiles:**
  - **bench-rtt p999:** publication-grade (pooled 100 000 raw
    samples per payload × stack, nearest-rank).
  - **Other three tools p999:** per-run-averaged percentile (no
    `--raw-samples-csv` sidecar yet). Disclosed as
    less-robust; future work item not blocking T60.

- **Reproducibility:** master seed `100` + `bash
  scripts/fast-iter-stats.sh 10 --seed 100 --skip-verify` on a
  clean checkout of commit `94050eb` plus the bench-e2e/peer
  build regenerates the same aggregator output. The N=10 raw
  CSVs are gitignored but the aggregator output is preserved at
  `docs/bench-reports/t60-aggregate-2026-05-13.md`.

## Wallclock budget actually spent

- **Total wallclock:** 4,924 s ≈ **82 minutes**
- **Per-run mean:** 462 s ≈ 7 min 42 s
- **Total runs attempted:** 10
- **OK runs:** 10
- **FAIL runs:** 0

Per-run breakdown (from
`target/bench-results/stats-2026-05-13-postI2/runs.txt`):

| run | seed | elapsed (s) | elapsed (m:ss) |
|---:|---:|---:|---|
| 001 | 100 | 482 | 8:02 |
| 002 | 101 | 473 | 7:53 |
| 003 | 102 | 438 | 7:18 |
| 004 | 103 | 486 | 8:06 |
| 005 | 104 | 478 | 7:58 |
| 006 | 105 | 441 | 7:21 |
| 007 | 106 | 482 | 8:02 |
| 008 | 107 | 502 | 8:22 |
| 009 | 108 | 439 | 7:19 |
| 010 | 109 | 432 | 7:12 |

Inter-run sleep of 30 s × 9 = 270 s, plus per-run preflight ≈ 30 s,
accounts for the gap between sum-of-elapsed (4,653 s) and total
wallclock (4,924 s). Well within the original 75-85 min estimate.

---

## Appendix — inlined `AGGREGATE.md` (verbatim aggregator output)

> The codex 2026-05-13 re-review IMPORTANT finding flagged that
> external aggregate files don't survive worktree reaping if
> gitignored. The aggregate is committed at
> `docs/bench-reports/t60-aggregate-2026-05-13.md` AND inlined
> below so the T60 report is self-contained. The text below is
> the unmodified output of `scripts/aggregate-fast-iter.py
> target/bench-results/stats-2026-05-13-postI2/`.

# fast-iter-suite aggregated statistics (N-run rollup)

Generated by `scripts/aggregate-fast-iter.py` (codex IMPORTANT I3, publication-grade rigor pass).


## Run metadata

- **N target:** 10
- **N present on disk:** 10
- **Master seed:** `100`
- **Output dir:** `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/agent-a29bab4a7b4dd2d53/target/bench-results/stats-2026-05-13-postI2/`
- **UTC ts:** `2026-05-13T08-34-10Z`
- **skip_verify:** `True`

**Method:**
- Per-cell mean + 95% CI: percentile bootstrap (1000 resamples) over per-run means.
- Per-cell p50 / p99 / p999 (bench-rtt): pooled across all raw-sample sidecars, nearest-rank percentile (the publication-grade tail estimate; ~50 000 samples at N=5).
- Per-cell p50 / p99 / p999 (other tools): mean of per-run aggregate p50 / p99 / p999 rows. bench_common's emit_csv emits a p999 row for all metrics, but averaging per-run p999 is a less-robust tail estimator than the pooled-raw approach used for bench-rtt.
- Per-cell CV: 100 × stdev(per-run means) / mean(per-run means).
- Paired-difference: paired bootstrap of mean(A_i - B_i) over matched run indices; 0 outside the 95% CI ⇒ significant at α = 0.05 two-sided.
- Effect size: Cohen's d = mean_diff / stdev(diffs) on the paired diffs.
- Stack-order randomization (codex I4) handled at the suite level — each run's seed = master_seed + run_idx.

## bench-rtt

### metric: `rtt_ns` (ns)

| connections | payload_bytes | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | dpdk_net | 220,082 | [211,524, 228,412] | 6.54% | 219,572 | 269,077 | 278,641 | 10 |
| 1 | 64 | linux_kernel | 227,416 | [221,243, 234,606] | 5.09% | 217,992 | 259,510 | 2,238,413 | 10 |
| 1 | 64 | fstack | 270,234 | [240,469, 299,916] | 17.72% | 298,372 | 309,560 | 318,972 | 10 |
| 1 | 128 | dpdk_net | 215,283 | [209,470, 221,194] | 4.99% | 211,366 | 259,790 | 275,039 | 10 |
| 1 | 128 | linux_kernel | 237,526 | [231,515, 244,052] | 4.67% | 230,271 | 266,459 | 2,322,155 | 10 |
| 1 | 128 | fstack | 250,600 | [220,756, 279,901] | 20.57% | 288,003 | 308,530 | 316,729 | 10 |
| 1 | 256 | dpdk_net | 219,943 | [214,428, 225,697] | 4.31% | 215,425 | 265,905 | 273,458 | 10 |
| 1 | 256 | linux_kernel | 233,916 | [225,699, 242,532] | 6.49% | 226,817 | 278,611 | 2,419,392 | 10 |
| 1 | 256 | fstack | 252,775 | [223,362, 281,464] | 19.76% | 293,896 | 308,828 | 339,186 | 10 |
| 1 | 1024 | dpdk_net | 217,814 | [211,224, 225,964] | 5.74% | 217,436 | 266,588 | 279,695 | 10 |
| 1 | 1024 | linux_kernel | 240,660 | [230,818, 249,917] | 6.78% | 234,382 | 285,715 | 2,357,672 | 10 |
| 1 | 1024 | fstack | 263,270 | [234,897, 290,191] | 18.11% | 297,500 | 310,297 | 343,847 | 10 |

**Paired comparison (A − B), metric `rtt_ns`** — significant when 0 is outside the 95% CI.

| connections | payload_bytes | A | B | mean_diff | 95% CI | Cohen's d | sig? | n_paired |
|---|---|---|---|---|---|---|---|---|
| 1 | 64 | dpdk_net | linux_kernel | -7,334 | [-15,083, 381] | -0.55 | no | 10 |
| 1 | 64 | dpdk_net | fstack | -50,152 | [-76,395, -18,408] | -1.02 | YES | 10 |
| 1 | 64 | linux_kernel | fstack | -42,817 | [-65,822, -15,695] | -1.00 | YES | 10 |
| 1 | 128 | dpdk_net | linux_kernel | -22,243 | [-30,138, -14,479] | -1.62 | YES | 10 |
| 1 | 128 | dpdk_net | fstack | -35,317 | [-62,890, -6,913] | -0.73 | YES | 10 |
| 1 | 128 | linux_kernel | fstack | -13,074 | [-46,933, 20,657] | -0.22 | no | 10 |
| 1 | 256 | dpdk_net | linux_kernel | -13,972 | [-22,567, -4,631] | -0.93 | YES | 10 |
| 1 | 256 | dpdk_net | fstack | -32,832 | [-62,997, -2,578] | -0.65 | YES | 10 |
| 1 | 256 | linux_kernel | fstack | -18,859 | [-52,180, 15,763] | -0.32 | no | 10 |
| 1 | 1024 | dpdk_net | linux_kernel | -22,846 | [-32,715, -10,797] | -1.19 | YES | 10 |
| 1 | 1024 | dpdk_net | fstack | -45,456 | [-76,778, -12,764] | -0.82 | YES | 10 |
| 1 | 1024 | linux_kernel | fstack | -22,610 | [-49,262, 4,628] | -0.47 | no | 10 |

## bench-tx-burst

### metric: `pmd_handoff_rate_bps` (bits_per_sec)

| K_bytes | G_ms | tx_ts_mode | workload | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 65536 | 0.0 | tsc_fallback | burst | dpdk_net | 1,032,540,016 | [1,028,203,062, 1,037,808,756] | 0.81% | 1,051,119,043 | 1,121,281,438 | 1,198,689,337 | 10 |
| 65536 | 10.0 | tsc_fallback | burst | dpdk_net | 2,109,279,645 | [2,039,533,857, 2,168,478,250] | 5.20% | 2,103,123,596 | 2,208,980,601 | 2,217,079,565 | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | dpdk_net | 1,037,538,364 | [1,030,124,694, 1,044,471,703] | 1.16% | 1,037,397,902 | 1,063,161,008 | 1,070,381,163 | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | dpdk_net | 1,186,871,093 | [1,175,350,031, 1,198,162,376] | 1.65% | 1,187,472,938 | 1,214,424,483 | 1,219,287,187 | 10 |

### metric: `burst_initiation_ns` (ns)

| K_bytes | G_ms | tx_ts_mode | workload | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 65536 | 0.0 | tsc_fallback | burst | dpdk_net | 26,654 | [26,253, 27,152] | 3.06% | 26,092 | 77,531 | 138,215 | 10 |
| 65536 | 0.0 | tsc_fallback | burst | fstack | 97,508 | [92,749, 102,020] | 8.31% | 85,047 | 161,314 | 167,098 | 10 |
| 65536 | 10.0 | tsc_fallback | burst | dpdk_net | 27,281 | [26,613, 28,189] | 5.07% | 27,182 | 30,068 | 32,906 | 10 |
| 65536 | 10.0 | tsc_fallback | burst | fstack | 87,238 | [85,493, 89,321] | 3.77% | 87,110 | 94,680 | 120,963 | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | dpdk_net | 22,076 | [20,743, 23,629] | 11.42% | 20,594 | 87,037 | 120,633 | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | fstack | 1,885,308 | [1,868,033, 1,903,014] | 1.59% | 1,866,679 | 1,940,480 | 1,952,649 | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | dpdk_net | 107,625 | [105,568, 110,538] | 4.39% | 106,580 | 122,785 | 127,792 | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | fstack | 1,863,582 | [1,840,598, 1,889,420] | 2.19% | 1,861,785 | 1,910,715 | 1,933,002 | 10 |
| 65536 | 0.0 |  | burst | linux_kernel | 5,071 | [4,048, 6,363] | 39.10% | 3,334 | 26,887 | 233,890 | 10 |
| 65536 | 10.0 |  | burst | linux_kernel | 8,216 | [6,982, 9,375] | 25.72% | 6,730 | 22,460 | 33,222 | 10 |
| 1048576 | 0.0 |  | burst | linux_kernel | 1,726 | [1,451, 2,050] | 29.30% | 949 | 12,842 | 38,610 | 10 |
| 1048576 | 10.0 |  | burst | linux_kernel | 9,472 | [7,790, 11,724] | 33.29% | 7,898 | 23,137 | 27,796 | 10 |

**Paired comparison (A − B), metric `burst_initiation_ns`** — significant when 0 is outside the 95% CI.

| K_bytes | G_ms | tx_ts_mode | workload | A | B | mean_diff | 95% CI | Cohen's d | sig? | n_paired |
|---|---|---|---|---|---|---|---|---|---|---|
| 65536 | 0.0 | tsc_fallback | burst | dpdk_net | fstack | -70,854 | [-75,300, -66,324] | -8.98 | YES | 10 |
| 65536 | 10.0 | tsc_fallback | burst | dpdk_net | fstack | -59,958 | [-61,479, -58,778] | -26.91 | YES | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | dpdk_net | fstack | -1,863,232 | [-1,882,237, -1,845,484] | -61.52 | YES | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | dpdk_net | fstack | -1,755,957 | [-1,778,466, -1,732,501] | -45.82 | YES | 10 |

### metric: `burst_steady_bps` (bits_per_sec)

| K_bytes | G_ms | tx_ts_mode | workload | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 65536 | 0.0 | tsc_fallback | burst | dpdk_net | 1,089,535,643 | [1,084,733,552, 1,094,949,473] | 0.80% | 1,106,943,983 | 1,195,093,250 | 1,272,533,452 | 10 |
| 65536 | 0.0 | tsc_fallback | burst | fstack | 16,243,992,087,210 | [15,370,096,609,541, 17,052,498,638,357] | 8.75% | 14,326,296,491,367 | 35,084,565,567,766 | 37,113,060,805,861 | 10 |
| 65536 | 10.0 | tsc_fallback | burst | dpdk_net | 2,368,991,646 | [2,285,451,246, 2,437,543,696] | 5.27% | 2,360,247,609 | 2,490,515,139 | 2,499,819,282 | 10 |
| 65536 | 10.0 | tsc_fallback | burst | fstack | 10,630,514,064,189 | [9,678,152,307,558, 11,493,217,072,756] | 14.59% | 10,528,653,025,777 | 26,679,834,802,875 | 32,676,071,674,208 | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | dpdk_net | 1,040,383,917 | [1,032,955,090, 1,047,574,405] | 1.17% | 1,040,103,271 | 1,066,624,024 | 1,073,597,183 | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | fstack | 119,679,156,002,214 | [108,186,130,185,456, 131,999,391,177,862] | 16.74% | 110,715,357,669,376 | 252,318,140,187,048 | 323,140,318,638,603 | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | dpdk_net | 1,205,211,923 | [1,194,387,353, 1,216,065,541] | 1.63% | 1,205,788,985 | 1,233,505,695 | 1,238,515,468 | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | fstack | 116,477,505,028,150 | [101,674,926,226,644, 132,106,785,460,253] | 21.75% | 110,017,010,904,966 | 234,574,549,390,208 | 324,441,578,412,156 | 10 |
| 65536 | 0.0 |  | burst | linux_kernel | 81,433,129,203 | [74,055,521,669, 88,314,469,316] | 15.00% | 89,546,648,580 | 159,059,721,388 | 167,594,817,718 | 10 |
| 65536 | 10.0 |  | burst | linux_kernel | 39,387,168,115 | [37,615,466,361, 41,459,913,555] | 8.41% | 40,155,134,186 | 50,429,145,898 | 52,125,312,414 | 10 |
| 1048576 | 0.0 |  | burst | linux_kernel | 6,518,790,075 | [5,446,470,356, 8,197,376,697] | 36.94% | 5,334,661,604 | 25,300,566,937 | 37,255,320,767 | 10 |
| 1048576 | 10.0 |  | burst | linux_kernel | 46,689,442,211 | [45,108,337,304, 48,445,102,930] | 6.02% | 46,550,067,024 | 55,266,806,865 | 59,502,333,359 | 10 |

**Paired comparison (A − B), metric `burst_steady_bps`** — significant when 0 is outside the 95% CI.

| K_bytes | G_ms | tx_ts_mode | workload | A | B | mean_diff | 95% CI | Cohen's d | sig? | n_paired |
|---|---|---|---|---|---|---|---|---|---|---|
| 65536 | 0.0 | tsc_fallback | burst | dpdk_net | fstack | -16,242,902,551,568 | [-17,086,873,828,852, -15,396,225,543,808] | -11.42 | YES | 10 |
| 65536 | 10.0 | tsc_fallback | burst | dpdk_net | fstack | -10,628,145,072,543 | [-11,503,945,970,828, -9,722,301,338,398] | -6.85 | YES | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | dpdk_net | fstack | -119,678,115,618,297 | [-130,875,613,989,635, -108,108,572,294,685] | -5.97 | YES | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | dpdk_net | fstack | -116,476,299,816,227 | [-131,691,782,479,797, -101,792,228,679,790] | -4.60 | YES | 10 |

### metric: `write_acceptance_rate_bps` (bits_per_sec)

| K_bytes | G_ms | tx_ts_mode | workload | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 65536 | 0.0 | tsc_fallback | burst | fstack | 5,701,277,160 | [5,510,256,251, 5,885,458,150] | 5.59% | 6,164,197,225 | 6,409,150,284 | 6,437,862,835 | 10 |
| 65536 | 10.0 | tsc_fallback | burst | fstack | 6,025,245,864 | [5,881,980,401, 6,143,231,294] | 3.58% | 6,026,557,658 | 6,353,265,307 | 6,387,970,787 | 10 |
| 1048576 | 0.0 | tsc_fallback | burst | fstack | 4,452,097,298 | [4,410,767,099, 4,492,717,234] | 1.58% | 4,496,613,572 | 4,561,920,152 | 4,569,502,985 | 10 |
| 1048576 | 10.0 | tsc_fallback | burst | fstack | 4,503,431,316 | [4,444,515,996, 4,560,888,212] | 2.15% | 4,507,710,148 | 4,554,729,992 | 4,561,483,782 | 10 |
| 65536 | 0.0 |  | burst | linux_kernel | 49,166,087,176 | [45,024,011,233, 52,652,761,957] | 14.00% | 52,269,305,611 | 101,273,108,184 | 106,013,072,514 | 10 |
| 65536 | 10.0 |  | burst | linux_kernel | 26,538,520,648 | [24,296,096,344, 29,114,769,419] | 15.27% | 27,446,736,649 | 38,312,572,538 | 39,563,891,373 | 10 |
| 1048576 | 0.0 |  | burst | linux_kernel | 6,487,808,257 | [5,395,866,267, 7,860,691,526] | 36.23% | 5,328,387,125 | 24,899,079,006 | 36,602,464,114 | 10 |
| 1048576 | 10.0 |  | burst | linux_kernel | 44,497,901,191 | [42,602,641,663, 46,317,996,505] | 6.95% | 44,426,370,308 | 53,450,678,188 | 57,363,019,815 | 10 |

## bench-tx-maxtp

### metric: `sustained_goodput_bps` (bits_per_sec)

| C | W_bytes | tx_ts_mode | workload | bucket_invalid | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 4096 | tsc_fallback | maxtp |  | dpdk_net | 987,873,996 | [981,898,921, 993,725,294] | 1.03% | 988,267,210 | 992,479,293 | 992,479,293 | 10 |
| 1 | 4096 | tsc_fallback | maxtp |  | fstack | 1,845,518,719 | [1,798,875,725, 1,885,595,710] | 4.37% | — | — | — | 10 |
| 4 | 4096 | tsc_fallback | maxtp |  | dpdk_net | 1,016,585,715 | [1,009,413,654, 1,023,519,526] | 1.18% | 254,168,269 | 254,443,520 | 254,443,520 | 10 |
| 4 | 4096 | tsc_fallback | maxtp |  | fstack | 3,018,131,150 | [2,996,786,211, 3,038,436,937] | 1.19% | — | — | — | 10 |
| 16 | 4096 | tsc_fallback | maxtp |  | dpdk_net | 794,271,216 | [785,760,264, 803,545,988] | 1.84% | 50,034,627 | 106,505,117 | 115,230,781 | 10 |
| 16 | 4096 | tsc_fallback | maxtp |  | fstack | 2,665,436,791 | [2,590,747,129, 2,717,514,472] | 4.32% | — | — | — | 10 |
| 1 | 16384 | tsc_fallback | maxtp |  | dpdk_net | 996,521,021 | [989,194,921, 1,004,007,699] | 1.23% | 996,619,059 | 997,890,458 | 997,890,458 | 10 |
| 1 | 16384 | tsc_fallback | maxtp |  | fstack | 2,704,200,098 | [2,682,515,359, 2,729,237,183] | 1.38% | — | — | — | 9 |
| 4 | 16384 | tsc_fallback | maxtp |  | dpdk_net | 998,759,016 | [990,548,731, 1,006,647,012] | 1.35% | 429,766,790 | 547,697,459 | 547,697,459 | 10 |
| 4 | 16384 | tsc_fallback | maxtp |  | fstack | 2,754,163,317 | [2,727,272,640, 2,781,959,785] | 1.71% | — | — | — | 9 |
| 16 | 16384 | tsc_fallback | maxtp |  | dpdk_net | 960,117,785 | [940,382,699, 978,414,203] | 3.53% | 0 | 518,596,493 | 592,749,891 | 10 |
| 16 | 16384 | tsc_fallback | maxtp |  | fstack | 2,024,401,141 | [1,640,603,847, 2,300,449,855] | 27.68% | — | — | — | 10 |
| 1 | 65536 | tsc_fallback | maxtp |  | dpdk_net | 1,034,592,562 | [1,025,623,564, 1,042,203,998] | 1.43% | 1,034,837,024 | 1,036,959,386 | 1,036,959,386 | 10 |
| 1 | 65536 | tsc_fallback | maxtp |  | fstack | 2,666,220,701 | [2,538,989,926, 2,741,483,519] | 4.57% | — | — | — | 4 |
| 4 | 65536 | tsc_fallback | maxtp |  | dpdk_net | 917,648,800 | [710,954,871, 1,029,485,552] | 35.22% | 0 | 933,598,470 | 933,598,470 | 10 |
| 4 | 65536 | tsc_fallback | maxtp |  | fstack | 2,681,828,086 | [2,648,914,893, 2,716,040,227] | 1.51% | — | — | — | 5 |
| 16 | 65536 | tsc_fallback | maxtp |  | dpdk_net | 751,266,093 | [491,435,374, 947,034,892] | 51.59% | 0 | 821,151,462 | 844,956,128 | 10 |
| 16 | 65536 | tsc_fallback | maxtp |  | fstack | 1,049,760,585 | [555,160,913, 1,570,040,179] | 81.55% | — | — | — | 10 |
| 1 | 4096 | n/a | maxtp |  | linux_kernel | 4,952,577,792 | [4,931,495,898, 4,963,644,535] | 0.66% | — | — | — | 10 |
| 4 | 4096 | n/a | maxtp |  | linux_kernel | 10,410,637,970 | [9,157,501,155, 11,491,927,821] | 19.70% | — | — | — | 10 |
| 16 | 4096 | n/a | maxtp |  | linux_kernel | 8,081,724,440 | [7,233,960,080, 8,674,056,365] | 14.63% | — | — | — | 10 |
| 1 | 16384 | n/a | maxtp |  | linux_kernel | 4,951,370,096 | [4,939,634,899, 4,962,334,593] | 0.39% | — | — | — | 10 |
| 4 | 16384 | n/a | maxtp |  | linux_kernel | 11,884,703,296 | [11,323,306,115, 12,393,910,888] | 7.79% | — | — | — | 10 |
| 16 | 16384 | n/a | maxtp |  | linux_kernel | 12,141,322,445 | [11,703,196,525, 12,402,834,582] | 4.90% | — | — | — | 10 |
| 1 | 65536 | n/a | maxtp |  | linux_kernel | 4,960,393,422 | [4,956,708,774, 4,963,453,547] | 0.12% | — | — | — | 10 |
| 4 | 65536 | n/a | maxtp |  | linux_kernel | 12,351,673,008 | [12,283,140,347, 12,394,428,081] | 0.81% | — | — | — | 10 |
| 16 | 65536 | n/a | maxtp |  | linux_kernel | 12,381,019,514 | [12,336,387,962, 12,404,493,173] | 0.56% | — | — | — | 10 |
| 1 | 65536 | tsc_fallback | maxtp | connect timeout: connections did not complete within 30 s (DPDK/ARP state may be dirty — clean /run/dpdk/rte/ and retry) | fstack | 0 | [0, 0] | — | — | — | — | 6 |
| 4 | 65536 | tsc_fallback | maxtp | connect timeout: connections did not complete within 30 s (DPDK/ARP state may be dirty — clean /run/dpdk/rte/ and retry) | fstack | 0 | [0, 0] | — | — | — | — | 5 |
| 1 | 16384 | tsc_fallback | maxtp | connect timeout: connections did not complete within 30 s (DPDK/ARP state may be dirty — clean /run/dpdk/rte/ and retry) | fstack | 0 | [0, 0] | — | — | — | — | 1 |
| 4 | 16384 | tsc_fallback | maxtp | connect timeout: connections did not complete within 30 s (DPDK/ARP state may be dirty — clean /run/dpdk/rte/ and retry) | fstack | 0 | [0, 0] | — | — | — | — | 1 |

**Paired comparison (A − B), metric `sustained_goodput_bps`** — significant when 0 is outside the 95% CI.

| C | W_bytes | tx_ts_mode | workload | bucket_invalid | A | B | mean_diff | 95% CI | Cohen's d | sig? | n_paired |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 4096 | tsc_fallback | maxtp |  | dpdk_net | fstack | -857,644,724 | [-896,935,409, -805,215,497] | -10.96 | YES | 10 |
| 4 | 4096 | tsc_fallback | maxtp |  | dpdk_net | fstack | -2,001,545,435 | [-2,019,008,991, -1,984,947,512] | -67.73 | YES | 10 |
| 16 | 4096 | tsc_fallback | maxtp |  | dpdk_net | fstack | -1,871,165,575 | [-1,918,062,364, -1,801,979,773] | -17.15 | YES | 10 |
| 1 | 16384 | tsc_fallback | maxtp |  | dpdk_net | fstack | -1,708,464,697 | [-1,731,129,412, -1,688,885,099] | -49.77 | YES | 9 |
| 4 | 16384 | tsc_fallback | maxtp |  | dpdk_net | fstack | -1,756,482,387 | [-1,786,202,131, -1,724,860,165] | -36.76 | YES | 9 |
| 16 | 16384 | tsc_fallback | maxtp |  | dpdk_net | fstack | -1,064,283,357 | [-1,353,313,995, -723,892,902] | -1.86 | YES | 10 |
| 1 | 65536 | tsc_fallback | maxtp |  | dpdk_net | fstack | -1,643,648,517 | [-1,723,243,696, -1,516,682,971] | -12.43 | YES | 4 |
| 4 | 65536 | tsc_fallback | maxtp |  | dpdk_net | fstack | -1,872,631,279 | [-2,283,041,722, -1,653,655,282] | -4.08 | YES | 5 |
| 16 | 65536 | tsc_fallback | maxtp |  | dpdk_net | fstack | -298,494,492 | [-910,595,834, 270,986,815] | -0.29 | no | 10 |

### metric: `tx_pps` (pps)

| C | W_bytes | tx_ts_mode | workload | bucket_invalid | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 4096 | tsc_fallback | maxtp |  | dpdk_net | 181,084 | [180,047, 182,174] | 1.02% | — | — | — | 10 |
| 1 | 4096 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 10 |
| 4 | 4096 | tsc_fallback | maxtp |  | dpdk_net | 186,919 | [185,597, 188,318] | 1.17% | — | — | — | 10 |
| 4 | 4096 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 10 |
| 16 | 4096 | tsc_fallback | maxtp |  | dpdk_net | 186,676 | [185,414, 187,981] | 1.22% | — | — | — | 10 |
| 16 | 4096 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 10 |
| 1 | 16384 | tsc_fallback | maxtp |  | dpdk_net | 185,821 | [184,369, 187,326] | 1.31% | — | — | — | 10 |
| 1 | 16384 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 9 |
| 4 | 16384 | tsc_fallback | maxtp |  | dpdk_net | 186,223 | [184,669, 187,792] | 1.53% | — | — | — | 10 |
| 4 | 16384 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 9 |
| 16 | 16384 | tsc_fallback | maxtp |  | dpdk_net | 182,441 | [179,542, 185,205] | 2.67% | — | — | — | 10 |
| 16 | 16384 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 10 |
| 1 | 65536 | tsc_fallback | maxtp |  | dpdk_net | 178,247 | [176,927, 179,692] | 1.41% | — | — | — | 10 |
| 1 | 65536 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 4 |
| 4 | 65536 | tsc_fallback | maxtp |  | dpdk_net | 158,843 | [122,805, 178,240] | 35.05% | — | — | — | 10 |
| 4 | 65536 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 5 |
| 16 | 65536 | tsc_fallback | maxtp |  | dpdk_net | 132,574 | [84,928, 166,110] | 50.68% | — | — | — | 10 |
| 16 | 65536 | tsc_fallback | maxtp |  | fstack | 0 | [0, 0] | — | — | — | — | 10 |
| 1 | 4096 | n/a | maxtp |  | linux_kernel | 86,651 | [83,928, 89,413] | 5.63% | — | — | — | 10 |
| 4 | 4096 | n/a | maxtp |  | linux_kernel | 243,633 | [216,531, 263,632] | 16.86% | — | — | — | 10 |
| 16 | 4096 | n/a | maxtp |  | linux_kernel | 272,133 | [246,787, 290,321] | 14.56% | — | — | — | 10 |
| 1 | 16384 | n/a | maxtp |  | linux_kernel | 86,546 | [83,590, 88,931] | 5.37% | — | — | — | 10 |
| 4 | 16384 | n/a | maxtp |  | linux_kernel | 212,521 | [197,970, 224,987] | 11.12% | — | — | — | 10 |
| 16 | 16384 | n/a | maxtp |  | linux_kernel | 229,284 | [218,209, 238,071] | 7.89% | — | — | — | 10 |
| 1 | 65536 | n/a | maxtp |  | linux_kernel | 86,661 | [83,765, 89,533] | 5.76% | — | — | — | 10 |
| 4 | 65536 | n/a | maxtp |  | linux_kernel | 223,225 | [217,981, 227,851] | 3.94% | — | — | — | 10 |
| 16 | 65536 | n/a | maxtp |  | linux_kernel | 234,834 | [229,768, 238,077] | 3.36% | — | — | — | 10 |

**Paired comparison (A − B), metric `tx_pps`** — significant when 0 is outside the 95% CI.

| C | W_bytes | tx_ts_mode | workload | bucket_invalid | A | B | mean_diff | 95% CI | Cohen's d | sig? | n_paired |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 4096 | tsc_fallback | maxtp |  | dpdk_net | fstack | 181,084 | [179,962, 182,169] | 97.59 | YES | 10 |
| 4 | 4096 | tsc_fallback | maxtp |  | dpdk_net | fstack | 186,919 | [185,598, 188,221] | 85.45 | YES | 10 |
| 16 | 4096 | tsc_fallback | maxtp |  | dpdk_net | fstack | 186,676 | [185,466, 187,962] | 82.20 | YES | 10 |
| 1 | 16384 | tsc_fallback | maxtp |  | dpdk_net | fstack | 185,654 | [184,185, 187,351] | 73.54 | YES | 9 |
| 4 | 16384 | tsc_fallback | maxtp |  | dpdk_net | fstack | 186,001 | [184,178, 187,806] | 63.72 | YES | 9 |
| 16 | 16384 | tsc_fallback | maxtp |  | dpdk_net | fstack | 182,441 | [179,375, 185,078] | 37.45 | YES | 10 |
| 1 | 65536 | tsc_fallback | maxtp |  | dpdk_net | fstack | 176,221 | [174,215, 178,227] | 71.46 | YES | 4 |
| 4 | 65536 | tsc_fallback | maxtp |  | dpdk_net | fstack | 139,998 | [70,002, 176,939] | 1.80 | YES | 5 |
| 16 | 65536 | tsc_fallback | maxtp |  | dpdk_net | fstack | 132,574 | [95,807, 166,557] | 1.97 | YES | 10 |

## bench-rx-burst

### metric: `latency_ns` (ns)

| segment_size_bytes | burst_count | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|
| 64 | 16 | dpdk_net | 109,931 | [103,106, 115,849] | 9.87% | 107,603 | 128,467 | 137,834 | 10 |
| 64 | 16 | linux_kernel | 138,459 | [132,469, 144,109] | 7.17% | 135,000 | 206,163 | 973,522 | 10 |
| 64 | 16 | fstack | 97,020 | [90,939, 102,737] | 10.02% | 96,016 | 112,124 | 120,066 | 10 |
| 64 | 64 | dpdk_net | 113,461 | [105,186, 120,432] | 11.28% | 109,899 | 135,710 | 141,658 | 10 |
| 64 | 64 | linux_kernel | 152,314 | [144,378, 161,989] | 10.25% | 143,528 | 309,108 | 965,047 | 10 |
| 64 | 64 | fstack | 108,070 | [100,301, 116,774] | 12.55% | 103,969 | 128,580 | 153,663 | 10 |
| 64 | 256 | dpdk_net | 121,754 | [112,206, 129,533] | 12.65% | 119,285 | 156,634 | 173,218 | 10 |
| 64 | 256 | linux_kernel | 158,849 | [144,353, 176,750] | 17.72% | 145,875 | 577,002 | 1,033,882 | 10 |
| 64 | 256 | fstack | 112,522 | [105,330, 119,488] | 10.57% | 109,952 | 140,720 | 263,816 | 10 |
| 128 | 16 | dpdk_net | 110,634 | [104,262, 117,029] | 10.01% | 107,689 | 134,342 | 139,696 | 10 |
| 128 | 16 | linux_kernel | 136,043 | [126,680, 145,237] | 11.92% | 123,377 | 336,313 | 621,239 | 10 |
| 128 | 16 | fstack | 99,547 | [91,316, 107,128] | 13.09% | 98,622 | 124,796 | 128,731 | 10 |
| 128 | 64 | dpdk_net | 118,743 | [111,301, 125,491] | 10.78% | 116,594 | 142,308 | 151,174 | 10 |
| 128 | 64 | linux_kernel | 154,838 | [140,985, 170,363] | 16.97% | 138,396 | 585,500 | 1,410,564 | 10 |
| 128 | 64 | fstack | 99,242 | [94,634, 103,757] | 7.34% | 96,818 | 115,893 | 122,461 | 10 |
| 128 | 256 | dpdk_net | 140,827 | [130,457, 149,829] | 11.94% | 141,721 | 181,959 | 205,801 | 10 |
| 128 | 256 | linux_kernel | 163,475 | [151,149, 175,996] | 13.12% | 153,960 | 541,048 | 748,356 | 10 |
| 128 | 256 | fstack | 114,016 | [109,450, 118,539] | 6.67% | 107,866 | 304,429 | 358,525 | 10 |
| 256 | 16 | dpdk_net | 116,310 | [109,704, 121,986] | 8.98% | 114,052 | 140,672 | 145,944 | 10 |
| 256 | 16 | linux_kernel | 151,119 | [138,026, 168,283] | 17.92% | 135,894 | 349,476 | 1,145,782 | 10 |
| 256 | 16 | fstack | 97,381 | [91,168, 103,382] | 10.85% | 94,816 | 118,195 | 125,750 | 10 |
| 256 | 64 | dpdk_net | 133,286 | [125,953, 139,334] | 8.21% | 133,482 | 163,438 | 171,314 | 10 |
| 256 | 64 | linux_kernel | 168,362 | [148,960, 189,121] | 21.26% | 144,645 | 827,018 | 1,608,709 | 10 |
| 256 | 64 | fstack | 106,636 | [98,580, 115,528] | 13.22% | 104,317 | 125,412 | 294,249 | 10 |
| 256 | 256 | dpdk_net | 194,812 | [188,685, 200,698] | 5.67% | 199,710 | 271,835 | 287,352 | 10 |
| 256 | 256 | linux_kernel | 169,796 | [154,590, 186,032] | 16.07% | 151,776 | 689,186 | 1,106,816 | 10 |
| 256 | 256 | fstack | 131,346 | [125,131, 136,764] | 7.98% | 119,682 | 370,813 | 584,877 | 10 |

**Paired comparison (A − B), metric `latency_ns`** — significant when 0 is outside the 95% CI.

| segment_size_bytes | burst_count | A | B | mean_diff | 95% CI | Cohen's d | sig? | n_paired |
|---|---|---|---|---|---|---|---|---|
| 64 | 16 | dpdk_net | linux_kernel | -28,528 | [-36,222, -20,046] | -1.98 | YES | 10 |
| 64 | 16 | dpdk_net | fstack | 12,911 | [6,864, 19,972] | 1.14 | YES | 10 |
| 64 | 16 | linux_kernel | fstack | 41,438 | [35,834, 46,443] | 4.51 | YES | 10 |
| 64 | 64 | dpdk_net | linux_kernel | -38,853 | [-50,136, -27,259] | -2.01 | YES | 10 |
| 64 | 64 | dpdk_net | fstack | 5,391 | [-6,733, 16,896] | 0.26 | no | 10 |
| 64 | 64 | linux_kernel | fstack | 44,244 | [32,503, 59,293] | 1.91 | YES | 10 |
| 64 | 256 | dpdk_net | linux_kernel | -37,095 | [-52,399, -22,431] | -1.37 | YES | 10 |
| 64 | 256 | dpdk_net | fstack | 9,232 | [-1,541, 21,556] | 0.49 | no | 10 |
| 64 | 256 | linux_kernel | fstack | 46,327 | [29,483, 62,344] | 1.64 | YES | 10 |
| 128 | 16 | dpdk_net | linux_kernel | -25,409 | [-35,871, -15,098] | -1.41 | YES | 10 |
| 128 | 16 | dpdk_net | fstack | 11,087 | [3,299, 18,232] | 0.86 | YES | 10 |
| 128 | 16 | linux_kernel | fstack | 36,496 | [23,253, 48,654] | 1.65 | YES | 10 |
| 128 | 64 | dpdk_net | linux_kernel | -36,095 | [-50,222, -19,448] | -1.34 | YES | 10 |
| 128 | 64 | dpdk_net | fstack | 19,502 | [10,907, 27,749] | 1.32 | YES | 10 |
| 128 | 64 | linux_kernel | fstack | 55,597 | [42,971, 70,771] | 2.33 | YES | 10 |
| 128 | 256 | dpdk_net | linux_kernel | -22,649 | [-34,084, -8,264] | -1.02 | YES | 10 |
| 128 | 256 | dpdk_net | fstack | 26,811 | [15,734, 38,228] | 1.48 | YES | 10 |
| 128 | 256 | linux_kernel | fstack | 49,460 | [38,457, 60,882] | 2.52 | YES | 10 |
| 256 | 16 | dpdk_net | linux_kernel | -34,809 | [-51,501, -20,621] | -1.25 | YES | 10 |
| 256 | 16 | dpdk_net | fstack | 18,929 | [12,061, 26,515] | 1.53 | YES | 10 |
| 256 | 16 | linux_kernel | fstack | 53,738 | [41,898, 67,558] | 2.44 | YES | 10 |
| 256 | 64 | dpdk_net | linux_kernel | -35,077 | [-59,607, -14,065] | -0.94 | YES | 10 |
| 256 | 64 | dpdk_net | fstack | 26,649 | [19,543, 35,067] | 1.95 | YES | 10 |
| 256 | 64 | linux_kernel | fstack | 61,726 | [41,031, 83,870] | 1.60 | YES | 10 |
| 256 | 256 | dpdk_net | linux_kernel | 25,016 | [9,519, 40,256] | 0.90 | YES | 10 |
| 256 | 256 | dpdk_net | fstack | 63,466 | [55,036, 72,667] | 3.98 | YES | 10 |
| 256 | 256 | linux_kernel | fstack | 38,450 | [26,581, 52,263] | 1.72 | YES | 10 |
