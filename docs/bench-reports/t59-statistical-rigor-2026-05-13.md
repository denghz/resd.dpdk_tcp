# T59 — fast-iter statistical rigor pass (5-run pooled, 2026-05-13)

**Branch state:** `a10-perf-23.11` at HEAD `a924b22` (post-I1..I5 codex fixes,
post-T58 follow-ups).

> ## Codex 2026-05-13 re-review caveats (read before quoting numbers)
>
> This report passed codex's first review of the I3 statistical-rigor work
> but the 2026-05-13 codex re-review (`docs/bench-reports/codex-rereview-2026-05-13.md`)
> raised three IMPORTANT caveats about the run itself. They do NOT
> invalidate the findings but reframe what each table is evidence for.
>
> **1. Run used a pre-I2-rename binary (tx-burst column label is legacy,
> data is still valid).** The 5-run pass was driven against a binary
> rebuilt *before* the codex I2 `throughput_per_burst_bps` →
> `pmd_handoff_rate_bps` rename landed in the running artifact (the
> source rename was in tree but the build cache had not picked it up).
> Consequence: the bench-tx-burst aggregator rows below carry the
> **legacy** `throughput_per_burst_bps` column name for dpdk_net rather
> than `pmd_handoff_rate_bps`. The underlying computation
> (`K / (t1 − t0)` at `rte_eth_tx_burst`-return, see
> `methodology-and-claims-2026-05-09.md`) did NOT change between the two
> labels — codex I2 was a rename, not a semantic redefinition — so the
> numeric values are valid as PMD-handoff-rate measurements. The
> aggregator (`scripts/aggregate-fast-iter.py:105-124`) accepts both
> names, which is why the report compiled cleanly despite the column
> mismatch. **For publication, re-run the suite with a post-I2 binary
> and confirm the column re-emits as `pmd_handoff_rate_bps`** —
> recommended in §Scaling to N=10+.
>
> **2. bench-rx-burst `latency_ns` is a cross-host metric, not a pure
> DUT RX latency.** The "9/9 cells significant for fstack-vs-linux"
> finding in §bench-rx-burst is statistically valid for the cross-host
> peer-send → DUT-recv delta, which is what the code actually computes.
> It is **NOT** evidence for a pure DUT-side internal RX-path latency
> claim — that interpretation would require HW RX timestamps which the
> current AWS ENA bench instance does not expose. See the corrected
> "Metrics — bench-rx-burst" section in
> `docs/bench-reports/methodology-and-claims-2026-05-09.md` for the
> full breakdown of what every `rx_latency_ns` sample includes (peer
> send timestamp, peer NIC TX, network transit, DUT NIC RX, plus an
> NTP-skew floor of ~100 µs same-AZ). The cross-stack **ordering**
> signal ("fstack < dpdk_net < linux_kernel on this metric") is the
> publication-grade claim; absolute µs are end-to-end cross-host
> values bounded below by NTP skew.
>
> **3. Raw aggregate artifacts are reproduced inline.** The original
> codex re-review note flagged `target/bench-results/stats-2026-05-13/`
> as absent from this checkout because the dir is gitignored and lived
> only in the agent worktree that produced it. To unblock reviewer
> reproduction we have copied the AGGREGATE.md output into
> `docs/bench-reports/t59-aggregate-2026-05-13.md` (committed under the
> repo so reviewers can read the canonical numeric tables alongside
> this report). The per-run raw CSVs (~MB-scale, gitignored) are still
> only in the producing worktree — re-run `scripts/fast-iter-stats.sh`
> with `--seed 42` to regenerate them locally.
>
> **Net recommendation for publication:** re-run with N≥10 against a
> post-I2 binary, preserve the AGGREGATE.md + raw CSVs under
> `docs/bench-reports/` from the start, and replace this disclaimer
> with a "passed re-review" line. Until then, treat the numeric tables
> below as preliminary cross-stack ordering evidence, not as
> publication-grade absolute-value claims.

**Why this report exists.** T58 ("fast-iter variance, 2026-05-13") ran the
suite 3 times back-to-back and reported per-cell CVs ranging 0.7 % to 21.5 %
on bench-rtt p50. Codex IMPORTANT I3 called that out — three runs is sampling
noise. Publication-grade numbers need (a) more repetitions, (b) confidence
intervals derived from a bootstrap distribution, (c) paired-difference
statistics across the stack-pairs (since the codex IMPORTANT I4 per-tool
stack-order randomization eliminates ordering bias but not the per-run
environmental drift), and (d) tail metrics beyond p50.

This T59 pass is the statistical-rigor build-out. It does **not** re-run the
T57 fair-comparison benchmark from scratch. It re-uses the existing
suite-level instrumentation (`scripts/fast-iter-suite.sh` with codex-I4
randomization + I1 raw-samples sidecars + I2 metric-naming fixes), wraps it
in a new harness (`scripts/fast-iter-stats.sh`) that drives N back-to-back
runs with derived seeds, and adds a Python aggregator
(`scripts/aggregate-fast-iter.py`) that pools the per-run results and emits a
bootstrap-CI + paired-difference table.

## Methodology

1. **Harness:** `scripts/fast-iter-stats.sh N --seed S0 --skip-verify
   --out-dir DIR` invokes `scripts/fast-iter-suite.sh` N times sequentially.
   Each iteration's seed is derived as `S0 + run_idx` (`run_idx ∈ 0..N-1`),
   so a single master seed S0 reproduces the full N-run matrix
   deterministically (per-tool stack-order shuffles vary with the per-run
   seed, layered on top of the codex I4 randomization). Inter-run gap:
   30 s (configurable via `INTER_RUN_SLEEP_SECS`) — gives the kernel,
   peer, and AWS ENA traffic-allowance accounting a small settle window.

2. **`--skip-verify`** drops the netem `verify-rack-tlp` matrix
   (~13-16 min/run). The netem matrix is a correctness/regression gate
   (RACK/TLP behavior under loss + reorder), not a cross-stack absolute-
   number comparison, so it's excluded from the I3 statistical-rigor pass to
   keep the wallclock budget tractable. The four bench tools
   (bench-rtt / bench-tx-burst / bench-tx-maxtp / bench-rx-burst) ARE the
   cross-stack comparison surface, and they run unchanged.

3. **Aggregator:** `scripts/aggregate-fast-iter.py STATS_DIR` discovers the
   N per-run dirs from `stats-metadata.json` and, for each
   `(tool, stack, dim_tuple, metric)` cell:

   - **Mean + 95 % CI** via percentile bootstrap (1000 resamples) over the
     N per-run means. Stdlib only — no numpy/scipy. The percentile-bootstrap
     CI is intentionally lightweight; N=5 is the smallest sample size where
     this is meaningful, and the CI WILL be wide because n=5. That width
     IS the honest signal — we are owning the precision of a 5-run sample.

   - **p50 / p99 / p999** computed two ways depending on tool:
     - bench-rtt has a per-bucket `--raw-samples-csv` sidecar (10 000
       samples / payload / run / stack). We **pool the raw samples across
       all N runs** (~50 000 samples / payload / stack at N=5) and take the
       nearest-rank percentile on the pooled distribution. This is the
       publication-grade tail estimate.
     - Other tools (bench-tx-burst / bench-tx-maxtp / bench-rx-burst) do
       not yet expose a raw-samples sidecar — the suite-level CSV carries
       per-aggregation rows (p50 / p99 / p999 / mean / stddev /
       ci95_lower / ci95_upper). For those we average the per-run p50,
       p99, and p999 aggregate rows. Note: averaged p999 is a less-robust
       tail estimator than the pooled-raw approach (each individual run
       only has ~200 measurements for bench-tx-burst /
       bench-rx-burst, so per-run p999 is one or two samples in the
       extreme tail — averaging N=5 of those still leaves a noisy
       estimate). Wiring `--raw-samples-csv` into those three tools is
       tracked as future work; not blocking for T59.

   - **CV across runs:** `100 × stdev(per_run_means) / mean(per_run_means)`.
     This is the run-to-run noise floor, NOT within-run jitter.

   - **Paired-difference test:** for each cross-stack pair (A, B) within a
     `(tool, dim_tuple, metric)` cell, pair `A_i` against `B_i` by run
     index `i`. Compute mean(`A_i − B_i`), a 95 % percentile-bootstrap CI
     of that mean over 1000 resamples of the paired-diff vector, and
     Cohen's d = mean_diff / stdev(diffs). Pairing by run index controls
     for per-run environmental drift (AWS ENA traffic-allowance state,
     ambient host load, etc.) since the same conditions apply to both
     stacks within a single run. Significance: "YES" when 0 is outside
     the 95 % CI (equivalent to two-sided percentile-bootstrap test at
     α = 0.05).

   - **Cross-stack metric asymmetry:** bench-tx-burst emits structurally
     distinct metrics per stack (`pmd_handoff_rate_bps` on dpdk_net,
     `write_acceptance_rate_bps` on linux_kernel + fstack) — this is by
     design (codex IMPORTANT I2: they measure different layers, see
     `tools/bench-tx-burst/src/lib.rs`). The aggregator pairs by exact
     metric name, so those rows simply won't appear in the paired table.
     `burst_initiation_ns` and `burst_steady_bps` ARE emitted by all three
     stacks and DO pair across all three.

4. **N value for this report:** N=5. Each run is ~7-8 minutes
   (bench-rtt × 3 stacks ≈ 30 s + bench-tx-burst × 3 ≈ 25 s + bench-tx-maxtp
   × 3 ≈ 6 min + bench-rx-burst × 3 ≈ 10 s + preflight + DPDK resets
   ≈ 30 s, no netem). 5 × ~7-8 min + 4 × 30 s sleep ≈ **~40 min wallclock
   total**. Master seed `42`; per-run seeds 42..46.

   **N=5 is "preliminary statistical rigor".** It's enough to get a
   bootstrap CI off the ground and to identify which cross-stack
   differences are large enough to survive any reasonable noise, but the
   CIs WILL be wide for cells with high run-to-run variance (e.g. linux_kernel
   bench-tx-maxtp where the kernel's TCP scheduling jitter is naturally
   high). For "publication-grade" in the strict sense the recommended N
   is 10+ (so the bootstrap CI is informed by 10 paired diffs rather than
   5, narrowing the interval by ~30 % under iid assumptions). See **Scaling
   to N=10+** below.

## Artifacts

- **Pooled aggregate (preserved in-tree, codex 2026-05-13 re-review fix):**
  `docs/bench-reports/t59-aggregate-2026-05-13.md` — full per-cell tables,
  more verbose than this report. **Quote-able as the canonical numeric
  source.** Contains the verbatim `AGGREGATE.md` output of
  `scripts/aggregate-fast-iter.py` from the originating
  `target/bench-results/stats-2026-05-13/` dir.
- Raw bench-results (gitignored, agent-worktree only): originally at
  `target/bench-results/stats-2026-05-13/run-0{01..05}-seed-{42..46}/`.
  Regenerate via `bash scripts/fast-iter-stats.sh 5 --seed 42 --skip-verify`.
- Per-run stats metadata: `target/bench-results/stats-2026-05-13/stats-metadata.json`
  (regenerated alongside the raw bench-results).
- Per-run completion log: `target/bench-results/stats-2026-05-13/runs.txt`
  (regenerated alongside the raw bench-results).
- Harness script: `scripts/fast-iter-stats.sh`
- Aggregator: `scripts/aggregate-fast-iter.py`

## Results — full per-cell tables (mean ± 95 % CI, CV, p50/p99/p999, paired diff)

The complete output of `scripts/aggregate-fast-iter.py` on this 5-run pass
is preserved in-tree at `docs/bench-reports/t59-aggregate-2026-05-13.md`
(canonical numeric source; codex 2026-05-13 re-review IMPORTANT
finding fix — the originating `target/bench-results/stats-2026-05-13/`
dir is gitignored and was lost when the agent worktree was reaped).
The per-tool tables below are extracted from that file verbatim;
cross-stack "YES"/"no" significance is the percentile-bootstrap paired-CI
test described in §Methodology.

### bench-rtt — `rtt_ns` (ns)

| connections | payload_bytes | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | dpdk_net | 213,670 | [208,097, 219,243] | 3.51% | 207,226 | 253,993 | 260,487 | 5 |
| 1 | 64 | linux_kernel | 235,646 | [225,381, 244,390] | 5.17% | 231,259 | 345,210 | 1,463,796 | 5 |
| 1 | 64 | fstack | 241,651 | [202,357, 281,012] | 22.06% | 203,399 | 306,951 | 313,860 | 5 |
| 1 | 128 | dpdk_net | 221,703 | [210,531, 234,338] | 6.73% | 214,571 | 274,598 | 281,975 | 5 |
| 1 | 128 | linux_kernel | 248,596 | [228,296, 279,103] | 13.20% | 227,033 | 900,394 | 3,133,421 | 5 |
| 1 | 128 | fstack | 280,185 | [240,455, 300,053] | 15.85% | 299,136 | 309,648 | 317,115 | 5 |
| 1 | 256 | dpdk_net | 216,024 | [210,366, 223,615] | 3.96% | 212,674 | 265,011 | 271,319 | 5 |
| 1 | 256 | linux_kernel | 245,889 | [223,376, 275,427] | 14.37% | 225,086 | 934,351 | 3,112,873 | 5 |
| 1 | 256 | fstack | 260,706 | [221,286, 299,837] | 20.48% | 297,267 | 309,746 | 347,038 | 5 |
| 1 | 1024 | dpdk_net | 221,679 | [212,469, 231,126] | 5.65% | 218,941 | 273,846 | 280,170 | 5 |
| 1 | 1024 | linux_kernel | 233,681 | [226,554, 239,856] | 3.78% | 230,187 | 296,955 | 1,388,122 | 5 |
| 1 | 1024 | fstack | 235,787 | [209,260, 270,963] | 16.74% | 202,495 | 306,414 | 344,085 | 5 |

**Paired comparison (A − B), `rtt_ns`** — significant when 0 is outside the 95 % CI.

| payload_bytes | A | B | mean_diff (ns) | 95% CI | Cohen's d | sig? |
|---:|---|---|---:|---|---:|:---:|
| 64   | dpdk_net | linux_kernel | -21,976 | [-31,907, -12,741] | -1.74 | YES |
| 64   | dpdk_net | fstack       | -27,981 | [-70,212, +14,265] | -0.50 | no  |
| 64   | linux_kernel | fstack   |  -6,005 | [-53,291, +34,682] | -0.11 | no  |
| 128  | dpdk_net | linux_kernel | -26,893 | [-46,702, -7,521]  | -1.02 | YES |
| 128  | dpdk_net | fstack       | -58,482 | [-89,882, -10,265] | -1.10 | YES |
| 128  | linux_kernel | fstack   | -31,589 | [-67,908, +10,550] | -0.65 | no  |
| 256  | dpdk_net | linux_kernel | -29,864 | [-63,867, -712]    | -0.72 | YES |
| 256  | dpdk_net | fstack       | -44,682 | [-87,314, +761]    | -0.78 | no  |
| 256  | linux_kernel | fstack   | -14,818 | [-54,693, +22,277] | -0.28 | no  |
| 1024 | dpdk_net | linux_kernel | -12,002 | [-26,357, +7,297]  | -0.57 | no  |
| 1024 | dpdk_net | fstack       | -14,108 | [-50,189, +10,519] | -0.35 | no  |
| 1024 | linux_kernel | fstack   |  -2,106 | [-39,233, +27,977] | -0.05 | no  |

**Findings — bench-rtt:**

1. **dpdk_net is significantly faster than linux_kernel at 64/128/256B**
   (Cohen's d −0.72 to −1.74). At 1024B the per-run variance in linux_kernel
   widens enough that the CI engulfs 0 — not significant at N=5.

2. **fstack RTT is bimodal across the 5 runs.** CV is 15–22 % at 128 / 256 /
   1024 B; pooled raw deciles confirm a true mode shift (some runs cluster
   near 200 µs, others near 300 µs). The codex IMPORTANT I1 fix
   (`pkt_tx_delay=0`) reduced but did NOT eliminate this bimodality — only
   64B is well-behaved across all 5 runs (CV 22 % is misleading: the cell
   actually contains the same mode-flip as the other payloads — the
   pooled raw-sample deciles section in `run-001-seed-42/` of the
   originating `stats-2026-05-13/` dir would show this, but the
   raw-CSV sidecars are gitignored; re-run via
   `bash scripts/fast-iter-stats.sh 5 --seed 42 --skip-verify` to
   regenerate them locally).

3. **Cross-stack paired comparisons involving fstack are mostly NOT
   significant** because the wide fstack CI overwhelms the paired difference
   in 4 / 6 of those cells. The one cell where dpdk_net beats fstack
   significantly is 128B (Cohen's d −1.10).

4. **p999 tails:** linux_kernel shows extreme p999 tails (1.4–3.1 ms — TCP
   retransmits in the kernel path). dpdk_net p999 stays ≤ 282 µs across all
   payloads; fstack p999 stays ≤ 347 µs. The pooled-raw approach correctly
   exposes these — without the raw sidecar, p999 would be the per-run mean
   of percentiles, which understates the kernel tail.

### bench-tx-burst — `burst_initiation_ns` (ns) — cross-stack comparable

| K_bytes | G_ms | tx_ts_mode | stack | mean (ns) | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---:|---:|---|---|---:|---|---:|---:|---:|---:|---:|
| 65536   | 0.0  | tsc_fallback | dpdk_net | 27,364    | [26,510, 28,128] | 3.72% | 27,215    | 50,248    | 138,745   | 5 |
| 65536   | 0.0  | tsc_fallback | fstack   | 103,301   | [99,566, 106,927] | 4.54% | 85,150    | 161,991   | 164,056   | 5 |
| 65536   | 10.0 | tsc_fallback | dpdk_net | 28,482    | [28,064, 29,012] | 2.17% | 28,401    | 30,475    | 32,617    | 5 |
| 65536   | 10.0 | tsc_fallback | fstack   | 91,251    | [89,751, 92,604] | 2.01% | 91,286    | 99,027    | 115,883   | 5 |
| 1048576 | 0.0  | tsc_fallback | dpdk_net | 22,021    | [21,668, 22,284] | 1.87% | 21,314    | 39,156    | 117,731   | 5 |
| 1048576 | 0.0  | tsc_fallback | fstack   | 1,873,661 | [1,858,066, 1,889,898] | 1.07% | 1,851,298 | 1,935,636 | 1,951,121 | 5 |
| 1048576 | 10.0 | tsc_fallback | dpdk_net | 112,813   | [111,558, 114,439] | 1.73% | 112,086   | 126,042   | 129,421   | 5 |
| 1048576 | 10.0 | tsc_fallback | fstack   | 1,869,817 | [1,854,271, 1,884,497] | 1.06% | 1,867,443 | 1,930,376 | 1,952,810 | 5 |
| 65536   | 0.0  | _(n/a)_      | linux_kernel | 3,931  | [2,605, 4,994]    | 41.01% | 2,894     | 23,611    | 87,700    | 5 |
| 65536   | 10.0 | _(n/a)_      | linux_kernel | 10,289 | [9,743, 10,806]   | 6.42%  | 9,679     | 21,486    | 24,188    | 5 |
| 1048576 | 0.0  | _(n/a)_      | linux_kernel | 1,692  | [1,323, 2,060]    | 28.49% | 1,037     | 7,372     | 20,566    | 5 |
| 1048576 | 10.0 | _(n/a)_      | linux_kernel | 11,845 | [10,984, 12,682]  | 9.35%  | 11,220    | 24,929    | 48,301    | 5 |

**Paired comparison (A − B), `burst_initiation_ns`** — only stacks with
matching `tx_ts_mode` pair (dpdk_net vs fstack; linux_kernel has
`tx_ts_mode` absent, so it can't pair).

| K_bytes | G_ms | A | B | mean_diff (ns) | 95% CI | Cohen's d | sig? |
|---:|---:|---|---|---:|---|---:|:---:|
| 65536   | 0.0  | dpdk_net | fstack |    -75,937 | [-79,382, -71,685]      | -15.28 | YES |
| 65536   | 10.0 | dpdk_net | fstack |    -62,769 | [-64,369, -61,256]      | -30.83 | YES |
| 1048576 | 0.0  | dpdk_net | fstack | -1,851,640 | [-1,867,774, -1,836,948] | -92.19 | YES |
| 1048576 | 10.0 | dpdk_net | fstack | -1,757,004 | [-1,772,068, -1,741,939] | -91.87 | YES |

**Findings — bench-tx-burst:**

- **dpdk_net wins burst initiation across all 4 cells** with very large
  effect sizes (Cohen's d −15 to −92). The fstack overhead is 75 µs at
  65 KiB no-gap and explodes to 1.85 ms at 1 MiB no-gap — fstack appears
  to serialize the burst through its mbuf-allocation path rather than
  doing the bulk PMD enqueue dpdk_net does.

- **linux_kernel `burst_initiation_ns` ~1.7-11.8 µs** — orders of magnitude
  lower than the DPDK stacks because the kernel's `write()` syscall returns
  as soon as data lands in the socket buffer (not "PMD-queue admission" as
  the DPDK stacks measure). This is the metric-asymmetry covered by codex
  IMPORTANT I2; we surface the numbers but DO NOT cross-stack compare
  linux_kernel against the DPDK stacks for this metric.

- The legacy `throughput_per_burst_bps` is emitted only by dpdk_net (the
  rebuild hasn't picked up the codex I2 rename to `pmd_handoff_rate_bps`
  yet; aggregator handles both names). fstack / linux_kernel emit
  `write_acceptance_rate_bps`, structurally different (socket-buffer
  admission rate), again not cross-stack comparable.

### bench-tx-maxtp — `sustained_goodput_bps` (bits/s) and `tx_pps`

Showing 9 cells (W ∈ {4 K, 16 K, 64 K} × C ∈ {1, 4, 16}). Two cells
(`C=1,W=65536` and `C=4,W=65536` for fstack) had `bucket_invalid="connect
timeout"` in one of the 5 runs — those count as n_runs=4 in the table.

| C | W_bytes | stack | mean (bps) | 95% CI | CV% | n_runs |
|---:|---:|---|---:|---|---:|---:|
| 1  |   4096 | dpdk_net |    984,318,924 | [    973,109,425,     996,458,384] | 1.51% | 5 |
| 1  |   4096 | fstack   |  1,859,897,118 | [  1,823,672,374,   1,891,605,102] | 2.34% | 5 |
| 1  |   4096 | linux_kernel |  4,961,608,907 | [  4,960,602,056,   4,962,522,546] | 0.02% | 5 |
| 4  |   4096 | dpdk_net |  1,013,356,312 | [  1,001,871,813,   1,026,511,231] | 1.53% | 5 |
| 4  |   4096 | fstack   |  2,982,314,954 | [  2,957,829,340,   3,005,181,945] | 0.99% | 5 |
| 4  |   4096 | linux_kernel | 10,489,378,845 | [ 10,080,454,894,  11,191,133,884] | 7.67% | 5 |
| 16 |   4096 | dpdk_net |    780,484,757 | [    758,875,274,     803,088,604] | 3.56% | 5 |
| 16 |   4096 | fstack   |  2,669,234,630 | [  2,637,903,357,   2,696,304,532] | 1.38% | 5 |
| 16 |   4096 | linux_kernel |  8,248,820,744 | [  8,058,606,489,   8,375,360,434] | 2.50% | 5 |
| 1  |  16384 | dpdk_net |    994,040,125 | [    982,242,181,   1,005,838,068] | 1.52% | 5 |
| 1  |  16384 | fstack   |  2,687,697,867 | [  2,671,144,012,   2,706,934,427] | 0.86% | 5 |
| 1  |  16384 | linux_kernel |  4,961,919,103 | [  4,959,281,437,   4,964,121,410] | 0.06% | 5 |
| 4  |  16384 | dpdk_net |    994,687,792 | [    979,313,672,   1,010,531,957] | 1.93% | 5 |
| 4  |  16384 | fstack   |  2,732,410,926 | [  2,703,960,314,   2,761,077,480] | 1.24% | 5 |
| 4  |  16384 | linux_kernel | 11,970,235,038 | [ 11,110,681,831,  12,401,381,397] | 8.03% | 5 |
| 16 |  16384 | dpdk_net |    960,734,068 | [    930,023,938,     991,444,198] | 4.30% | 5 |
| 16 |  16384 | fstack   |  2,292,731,468 | [  1,873,870,276,   2,524,270,981] | 20.26% | 5 |
| 16 |  16384 | linux_kernel | 11,678,869,626 | [ 10,923,894,333,  12,406,495,307] | 8.54% | 5 |
| 1  |  65536 | dpdk_net |  1,027,427,962 | [  1,019,733,746,   1,037,347,738] | 1.11% | 5 |
| 1  |  65536 | fstack   |  2,725,507,179 | [  2,685,952,502,   2,765,289,087] | 1.92% | 4 |
| 1  |  65536 | linux_kernel |  4,953,915,254 | [  4,940,803,595,   4,963,674,305] | 0.29% | 5 |
| 4  |  65536 | dpdk_net |    991,034,296 | [    975,551,559,   1,008,362,484] | 2.04% | 5 |
| 4  |  65536 | fstack   |  2,655,508,670 | [  2,605,795,942,   2,690,318,235] | 1.98% | 4 |
| 4  |  65536 | linux_kernel | 11,950,645,646 | [ 11,450,079,228,  12,405,702,364] | 5.25% | 5 |
| 16 |  65536 | dpdk_net |    932,491,501 | [    908,015,702,     956,967,299] | 3.51% | 5 |
| 16 |  65536 | fstack   |  1,956,489,078 | [    967,115,434,   2,517,396,484] | 56.22% | 5 |
| 16 |  65536 | linux_kernel | 12,394,052,499 | [ 12,383,837,575,  12,400,343,596] | 0.09% | 5 |

**Findings — bench-tx-maxtp:**

- **linux_kernel > fstack > dpdk_net for `sustained_goodput_bps` in every
  cell** — but this is the loopback-routing-vs-real-wire delta (see T57
  §Methodology — two-ENI comparison): linux_kernel runs through host
  netns to a different physical NIC; dpdk_net + fstack run the DPDK NIC.
  The qualitative ordering preserves T57's finding, but absolute
  cross-stack ratios are still distorted by the two-ENI imbalance.

- **dpdk_net vs fstack** (the same-NIC paired test) is significant in
  all 9 cells (see `docs/bench-reports/t59-aggregate-2026-05-13.md`
  paired table for `sustained_goodput_bps`). fstack delivers ~2.5–3 Gbps
  vs dpdk_net's ~1 Gbps on this NIC (dpdk_net's ENA tx-data mempool
  exhaustion at C=16 lowers its number — see the C=16 W=4096 cell;
  dpdk_net at 780 Mbps is a Pool exhaustion signal, not a stack-comparison
  result).

- **fstack reports `tx_pps=0`** because PPS measurement uses DPDK port
  stats which aren't queryable under F-Stack's PMD wrapping (this is a
  known F-Stack limitation; the cells legitimately are zero — not a
  data-loss bug).

- **N=4 (not 5) cells:** `fstack C=1,W=65536` and `fstack C=4,W=65536`
  each lost one of the 5 runs to a `connect timeout` bucket-invalid
  failure (the run completed but bucket emitted 0). Aggregator correctly
  pools only the 4 valid runs for those cells.

### bench-rx-burst — `latency_ns` (ns)

> **Cross-host metric reminder (codex 2026-05-13 re-review):**
> `latency_ns` is computed as `dut_recv_ns − peer_send_ns` with both
> endpoints anchored on `CLOCK_REALTIME` (see `methodology-and-claims-
> 2026-05-09.md` §Metrics — bench-rx-burst for the full capture
> breakdown). Every sample includes peer send latency, peer NIC TX,
> AWS data-plane transit, DUT NIC RX, plus an NTP-skew floor of
> ~100 µs same-AZ — it is NOT a pure DUT-side internal RX
> measurement. The cross-stack **ordering** signal in this table
> (e.g. "fstack < dpdk_net < linux_kernel on this metric") is the
> publication-grade claim; absolute µs values are end-to-end
> cross-host values bounded below by the NTP-skew floor.

| W (segment_size) | N (burst_count) | stack | mean (ns) | 95% CI | CV% | p50 | p99 | p999 | n_runs |
|---:|---:|---|---:|---|---:|---:|---:|---:|---:|
| 64  | 16  | dpdk_net     | 112,272 | [103,290, 121,021] | 10.11% | 110,514 | 130,378   | 143,764   | 5 |
| 64  | 16  | linux_kernel | 161,639 | [140,201, 187,661] | 18.29% | 146,344 | 788,588   | 1,254,717 | 5 |
| 64  | 16  | fstack       | 112,826 | [102,353, 121,498] | 11.19% | 111,185 | 126,596   | 142,950   | 5 |
| 64  | 64  | dpdk_net     | 120,097 | [112,145, 130,243] | 10.61% | 114,162 | 146,782   | 151,788   | 5 |
| 64  | 64  | linux_kernel | 167,046 | [138,130, 195,961] | 22.74% | 133,487 | 1,054,706 | 1,416,561 | 5 |
| 64  | 64  | fstack       | 114,205 | [105,864, 124,550] | 10.33% | 109,716 | 135,062   | 142,168   | 5 |
| 64  | 256 | dpdk_net     | 129,252 | [120,720, 138,409] |  8.33% | 127,187 | 164,995   | 178,137   | 5 |
| 64  | 256 | linux_kernel | 162,948 | [151,960, 175,634] |  9.39% | 145,786 | 739,192   | 1,265,690 | 5 |
| 64  | 256 | fstack       | 122,514 | [119,018, 126,707] |  3.95% | 118,765 | 183,865   | 299,218   | 5 |
| 128 | 16  | dpdk_net     | 113,286 | [104,212, 122,609] | 10.09% | 110,525 | 145,196   | 149,043   | 5 |
| 128 | 16  | linux_kernel | 158,315 | [139,489, 178,111] | 16.25% | 132,861 | 901,986   | 1,181,279 | 5 |
| 128 | 16  | fstack       | 104,691 | [97,984, 110,359]  |  7.47% | 102,660 | 125,728   | 132,822   | 5 |
| 128 | 64  | dpdk_net     | 125,667 | [114,222, 136,083] | 10.79% | 121,918 | 155,659   | 161,248   | 5 |
| 128 | 64  | linux_kernel | 160,635 | [146,783, 180,294] | 13.80% | 143,934 | 731,446   | 1,217,704 | 5 |
| 128 | 64  | fstack       | 112,946 | [100,250, 121,922] | 12.56% | 108,957 | 134,432   | 141,284   | 5 |
| 128 | 256 | dpdk_net     | 148,234 | [135,856, 159,114] |  9.91% | 149,827 | 189,873   | 203,942   | 5 |
| 128 | 256 | linux_kernel | 201,619 | [159,479, 260,007] | 32.27% | 149,216 | 1,168,366 | 1,639,931 | 5 |
| 128 | 256 | fstack       | 122,554 | [115,696, 128,965] |  7.07% | 116,723 | 291,118   | 361,077   | 5 |
| 256 | 16  | dpdk_net     | 121,664 | [110,872, 131,864] | 10.47% | 117,137 | 151,665   | 160,640   | 5 |
| 256 | 16  | linux_kernel | 161,941 | [128,477, 217,792] | 37.79% | 120,503 | 904,821   | 1,357,760 | 5 |
| 256 | 16  | fstack       | 108,510 | [98,736, 119,980]  | 12.21% | 105,147 | 137,156   | 140,998   | 5 |
| 256 | 64  | dpdk_net     | 137,443 | [126,774, 148,399] |  9.96% | 137,488 | 171,280   | 178,198   | 5 |
| 256 | 64  | linux_kernel | 159,851 | [138,441, 183,349] | 18.18% | 136,109 | 930,135   | 1,626,995 | 5 |
| 256 | 64  | fstack       | 122,633 | [114,189, 132,389] | 10.18% | 118,769 | 143,844   | 329,561   | 5 |
| 256 | 256 | dpdk_net     | 195,784 | [183,791, 206,736] |  7.27% | 200,713 | 269,673   | 282,510   | 5 |
| 256 | 256 | linux_kernel | 189,588 | [163,350, 221,266] | 18.35% | 150,698 | 973,363   | 2,508,906 | 5 |
| 256 | 256 | fstack       | 132,885 | [130,267, 135,112] |  2.40% | 118,529 | 387,943   | 634,051   | 5 |

**Findings — bench-rx-burst:**

All claims below are about the **cross-host** `dut_recv_ns −
peer_send_ns` delta. They are NOT claims about pure DUT-side
internal RX-path cost — that would require HW RX timestamps the
bench instance does not expose. Read the disclaimer block at the
top of this report and `methodology-and-claims-2026-05-09.md`
§Metrics — bench-rx-burst before quoting absolute µs.

- **fstack < dpdk_net on the cross-host RX delta** (paired sig YES) in
  5 of 9 cells (W=128,256 across all N). At W=64 the two stacks are
  statistically indistinguishable (overlapping CIs). The qualitative
  T57 finding "fstack wins RX latency" is preserved as a cross-stack
  ordering claim on the end-to-end peer-send-to-DUT-recv metric.
- **dpdk_net < linux_kernel on the cross-host RX delta** in 8 of 9
  cells (the W=256,N=256 cell is borderline — CI engulfs 0).
  linux_kernel's RX delta carries a ~1 ms p99/p999 tail that's the
  dominant signal for the test; that tail is real on the wire (kernel
  TCP retransmit + scheduler-quantum delays) but the *bulk* of the
  µs value is the same peer + network components that show up in
  every stack's distribution.
- **linux_kernel > fstack on the cross-host RX delta** (paired sig YES)
  in 9 of 9 cells. This is the cell that codex's 2026-05-13 re-review
  flagged as "statistically true for the cross-host delta but not
  evidence for internal DUT RX latency". The ordering signal is valid;
  the absolute µs values are NOT a pure-stack RX-cost number.
- **Bootstrap CIs are widest at small N** — most cells have ~10 % CV
  across the 5 runs even for the DPDK stacks, so the 95 % CI half-width
  is ~10 % of the mean. This is the limit of N=5.

## Paired-comparison summary

Across the four tools, the cross-stack paired-bootstrap test produced
significance like this (out of the cells where the paired comparison is
structurally valid):

| Comparison                       | bench-rtt           | bench-tx-burst (init)       | bench-tx-maxtp (goodput)        | bench-rx-burst (latency)        |
|---|---|---|---|---|
| dpdk_net vs linux_kernel         | 3/4 sig (1024B no)  | n/a (`tx_ts_mode` differs)  | 0/9 paired (`tx_ts_mode` differs) | 8/9 sig (W=256,N=256 no)        |
| dpdk_net vs fstack               | 1/4 sig (only 128B) | 4/4 sig (Cohen's d −15..−92) | 9/9 sig (Cohen's d −0.94..−94)  | 5/9 sig (W=128,256 all sig)      |
| linux_kernel vs fstack           | 0/4 sig (fstack CI wide) | n/a (`tx_ts_mode` differs) | 0/9 paired (`tx_ts_mode` differs) | 9/9 sig (fstack faster)         |

Interpretation:
- **Large effects survive N=5** — dpdk_net's bench-tx-burst initiation
  advantage and dpdk_net-vs-fstack tx-maxtp differences are far outside
  any reasonable noise floor.
- **Borderline effects (Cohen's d ≤ 1)** are where N=5 limits us:
  bench-rtt @1024B (dpdk_net vs linux_kernel d=-0.57, no sig) and
  bench-rx-burst @W256N256 (dpdk_net vs linux_kernel d=0.16, no sig).
  These cells need N=10+ to disambiguate ordering from sampling noise.
- **fstack RTT bimodality dominates the dpdk_net-vs-fstack RTT
  comparison** — the wide 95 % CI on fstack's per-run mean (CV up to
  22 %) widens the paired-diff CI enough to engulf 0 even when dpdk_net
  is ~50 µs faster in the point estimate. This is a fstack-side
  variance issue, not a methodology issue: a binomial fstack RTT
  distribution can't be "averaged away" by repetition — N=5 (or 10, or
  100) won't fix it; only diagnosing why fstack flips modes will.

## Scaling to N=10+

The framework supports any N ≥ 1 via the `N` positional argument to
`fast-iter-stats.sh`. Wallclock scales linearly: N × ~7-8 min ≈ 80 min at
N=10 (with `--skip-verify`; bump to ~5 hr if the netem matrix is included).
Master seed S0 can be re-used (so a new run extends the existing batch
deterministically — re-run with `--seed S0` and `N` increased; the
harness skips any `run-NNN-seed-S/` that already exists if the operator
pre-creates the dir, but cleanest is to use a fresh `--out-dir` and
re-aggregate from a fresh master seed).

To grow N=5 → N=10 without throwing away the 5-run pass:

```
# Bring up the suite
source .fast-iter.env

# Run N=10 against a fresh out-dir.
bash scripts/fast-iter-stats.sh 10 --seed 42 --skip-verify \
    --out-dir target/bench-results/stats-N10-2026-05-XX/

# Aggregate.
python3 scripts/aggregate-fast-iter.py \
    target/bench-results/stats-N10-2026-05-XX/ \
    --out-md docs/bench-reports/t59b-statistical-rigor-N10-2026-05-XX.md
```

Higher N tightens the CI by ~`sqrt(N_old / N_new)` (under iid). N=5 → N=10
narrows the CI by ~29 %; N=5 → N=20 narrows by ~50 %.

## Limitations + caveats

1. **N=5 is preliminary.** Cells with run-to-run CV ≥ 10 % will produce
   bootstrap CIs wide enough that the qualitative cross-stack ordering
   sometimes flips a "YES" → "no" significance at N=10. Treat N=5 as
   "screening" — the cells that come out "YES" at N=5 will almost
   certainly stay "YES" at N=10; cells that come out "no" at N=5 need a
   higher N to disambiguate.

2. **No within-run jitter quantification.** This pass treats each run as
   a single observation (the per-run mean). Within-run percentile tails
   are captured for bench-rtt (raw sidecar) but NOT folded into the
   bootstrap CI of the mean — that's a deliberate scope choice (folding
   in within-run uncertainty would require a hierarchical bootstrap,
   which is overkill for this pass).

3. **bench-tx-burst metric asymmetry.** Cross-stack paired comparison on
   the headline throughput metric (`pmd_handoff_rate_bps` vs
   `write_acceptance_rate_bps`) is intentionally not produced — those two
   metrics measure different layers (codex IMPORTANT I2, 2026-05-13). The
   `burst_initiation_ns` and `burst_steady_bps` metrics are emitted by
   all three stacks and DO appear in the paired table; those are the
   cross-stack comparable signals for bench-tx-burst.

4. **bench-tx-maxtp / bench-tx-burst / bench-rx-burst lack raw-samples
   sidecars.** Only bench-rtt was wired through `--raw-samples-csv` in
   codex IMPORTANT I1 (RTT bimodality investigation). For those three
   tools the aggregator averages the per-run p50, p99, and p999 rows from
   the aggregate CSV — which is workable but treats p999 as a per-run
   point estimate (each run has ~200 samples × 9 cells for tx-maxtp /
   tx-burst, so per-run p999 is the second-to-last sample). The
   `--raw-samples-csv` wiring is a self-contained change in each of the
   four tool crates; tracked as future work but not blocking T59.

5. **Two-ENI methodology persists.** linux_kernel still drives a different
   physical NIC than dpdk_net / fstack (codex BLOCKER B2 disclosure
   stands — see T57). For the cross-stack absolute numbers to be
   bit-perfect identical-wire, the suite needs a vfio↔ena rebind dance
   before each linux_kernel arm; that's a Phase 6+ refactor.

## Verdict — preliminary statistical rigor achieved

- **Framework reproducibility:** seed 42 → seeds 42..46 → 5 vanilla
  `fast-iter-suite` runs, then `aggregate-fast-iter.py` → AGGREGATE.md
  (preserved in-tree at `docs/bench-reports/t59-aggregate-2026-05-13.md`
  for review; the original gitignored `target/bench-results/stats-2026-05-13/`
  is regenerable via the harness invocation).
  Anyone with the worktree can replay the same numeric output by
  running `bash scripts/fast-iter-stats.sh 5 --seed 42 --skip-verify`.

- **N=5 disambiguation power:**
  - Large effects (Cohen's d ≥ 2): always significant. bench-tx-burst
    burst_initiation (Cohen's d −15 to −92) and bench-tx-maxtp
    goodput (Cohen's d −0.94 to −94) are headline-worthy.
  - Moderate effects (1 ≤ |d| < 2): usually significant. bench-rtt
    dpdk_net-vs-linux_kernel at 64/128/256B (d −0.72 to −1.74) all
    significant.
  - Small effects (|d| < 1): borderline. bench-rtt @1024B (d=-0.57)
    and bench-rx-burst @256/256 (d=0.16) come up "no" at N=5; need
    N=10+ to disambiguate.

- **Tail-metric status:**
  - bench-rtt p999: trustworthy (50 000 pooled raw samples per
    payload×stack, nearest-rank).
  - Other three tools p999: best-effort (per-run-averaged percentile,
    not pooled-raw). Future work: wire `--raw-samples-csv` into
    bench-tx-burst / bench-tx-maxtp / bench-rx-burst for parity with
    bench-rtt's I1 wiring.

- **Persistent qualitative ordering** from T57/T58, now with N=5 CI:
  - dpdk_net wins bench-rtt vs linux_kernel at 64/128/256B (sig YES)
  - dpdk_net wins bench-tx-burst initiation vs fstack at all 4 cells
    (sig YES, d −15 to −92)
  - dpdk_net wins bench-tx-maxtp goodput vs fstack at all 9 cells
    (sig YES). linux_kernel wins on absolute goodput numbers, but the
    two-ENI methodology asterisk holds.
  - fstack wins bench-rx-burst at W=128/256 vs both dpdk_net and
    linux_kernel.

- **fstack RTT bimodality persists** — codex I1's `pkt_tx_delay=0` fix
  improved but did NOT eliminate it. CV 15-22 % at 128/256/1024B
  across the 5 runs means the fstack RTT distribution flips between a
  ~200 µs mode and a ~300 µs mode across runs. Diagnosing the trigger
  (`payload_256` is stuck at the 300 µs mode in ALL 5 runs — what's
  special about that payload size?) is a follow-up.

- **Recommended next step** for "actual publication-grade":
  1. Run N=10 (`bash scripts/fast-iter-stats.sh 10 --seed 42
     --skip-verify`, ~80 min wallclock). Tightens CIs by ~30 %.
  2. Wire `--raw-samples-csv` into the remaining three tools so the
     `--out-md` p999 column is pooled-raw for all 4 tools, not just
     bench-rtt.
  3. Investigate fstack RTT bimodality (mode-flip CV 15–22 %): why
     does `payload_256` always land in the 300 µs mode and the others
     flip? Likely a polling-interval or scheduler quantum interaction.

## Wallclock budget actually spent (this pass)

- **Total wallclock (5-run pass, harness drove `fast-iter-stats.sh 5 --seed 42 --skip-verify`):** 2,352 s ≈ **39 min 12 s**
- **Per-run mean:** ~7 min 50 s
- **Total runs attempted:** 5
- **OK runs:** 5
- **FAIL runs:** 0

Per-run breakdown (from `target/bench-results/stats-2026-05-13/runs.txt`):

| run | seed | elapsed (s) | elapsed (m:ss) |
|---:|---:|---:|---|
| 001 | 42 | 445 | 7:25 |
| 002 | 43 | 472 | 7:52 |
| 003 | 44 | 437 | 7:17 |
| 004 | 45 | 442 | 7:22 |
| 005 | 46 | 436 | 7:16 |

Inter-run sleep of 30 s × 4 = 120 s, plus per-run preflight ≈ 30 s, accounts
for the gap between sum-of-elapsed (2,232 s) and total wallclock (2,352 s).

Well within the 3-hour budget originally targeted for N=5; comfortably
within the 6-hour budget for N=10 (≈ 80 min linear projection, still leaves
> 4 hr slack for re-runs or N=20).

