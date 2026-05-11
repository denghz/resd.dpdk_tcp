# A10-perf-23.11 — Opportunity Matrix (baseline)

**Baseline mode:** diagnostic — `check-perf-host.sh` exit=1; THP=`always [madvise] never`
(target: `never`); cpufreq governor not exposed (`unknown` — KVM); isolcpus / nohz_full /
rcu_nocbs absent (dev box). Re-baseline after THP=never lands.
**Run timestamp:** 2026-04-28T12:30:23Z (bench start) — TBP collect 2026-04-28T13:01–13:04Z
**Worktree HEAD:** acedb33db9616584ffc03e912e3da2b12e51bb77 (branch `a10-perf-23.11`)
**DPDK version:** 23.11.0 (libdpdk pkg-config)
**Host:** AWS EC2 (KVM), AMD EPYC 7R13 (Family 0x19 / Model 0x1, Zen 3 Milan), 8 vCPU,
kernel 6.8.0-1052-aws.
**uProf:** 5.2.606.0 — TBP-only (IBS / Core PMC / DF unavailable per
`docs/superpowers/reports/perf-host-capabilities.md`).
**Bench source:** `profile/full-suite-baseline/results.csv` (criterion summarize).
**TBP source:** `profile/<family>-baseline/tbp.csv` (per-family).

## Per-family criterion summary

All values in nanoseconds (`ns_per_iter`). "Within-budget?" compares median vs §11.2 upper.

| Family / Bench | §11.2 upper | Median (p50) | p99 | Gap (p50 − upper) | Within-budget? |
|---|---|---|---|---|---|
| bench_poll_empty | 100 ns (tens ns) | 1031.54 | 1414.11 | +931.54 | ✗ |
| bench_poll_idle_with_timers | 100 ns (tens ns) | 1165.02 | 4649.59 | +1065.02 | ✗ |
| bench_tsc_read_ffi | 5 ns | 10.21 | 10.38 | +5.21 | ✗ |
| bench_tsc_read_inline | 1 ns | 10.37 | 11.02 | +9.37 | ✗ |
| bench_flow_lookup_hot | 40 ns | 27.07 | 31.36 | −12.93 | ✓ |
| bench_flow_lookup_cold | 200 ns | 95.20 | 145.23 | −104.80 | ✓ |
| bench_tcp_input_data_segment | 200 ns | 86.04 | 113.68 | −113.96 | ✓ |
| bench_tcp_input_ooo_segment | 400 ns | 84.86 | 127.97 | −315.14 | ✓ |
| bench_send_small (STUB) | 150 ns | 70.58 (stub) | 106.82 (stub) | n/a (stub) | n/a |
| bench_send_large_chain (STUB) | 1–5 µs (5000 ns) | 1231.55 (stub) | 1396.63 (stub) | n/a (stub) | n/a |
| bench_timer_add_cancel | 50 ns | 25.02 | 40.03 | −24.98 | ✓ |
| bench_counters_read | 100 ns | 1.49 | 1.91 | −98.51 | ✓ |

Notes on `bench_tsc_read_inline`: header-inline FFI variant does not exist; bench currently
proxies with `dpdk_net_core::clock::now_ns()` (raw RDTSC + ns conversion). The 10.37 ns p50
reflects RDTSC latency on Zen 3 KVM, not a function-call boundary. The "1 ns" target
remains aspirational pending T5 introducing `dpdk_net_now_ns_inline`.

Notes on `bench_poll_*`: both gaps are dominated by criterion-harness setup cost wrapped
around an `EngineNoEalHarness::poll_once_*` proxy; see §"Observations" for how T2.5 Criterion
overhead bounds what TBP can attribute.

## Per-family top hotspots (TBP)

Source: `profile/<family>-baseline/tbp.csv`, `10 HOTTEST FUNCTIONS` block.
TBP samples at 1 ms intervals — only "hot in wall time" code resolves; very-short benches
(e.g. `tsc_read`) are dominated by criterion measurement scaffolding.

| Family | Top hotspot fn | TBP s | Plausible cause |
|---|---|---|---|
| poll | `criterion::bencher::Bencher::iter` | 12.589 | criterion measurement loop — workload too small to dominate sample budget |
| poll | `rayon::iter::plumbing::bridge_producer_consumer::helper` | 8.997 | criterion's parallel bootstrap analyzer (post-bench) |
| poll | `__ieee754_exp_fma` (libm) | 3.928 | criterion's KDE / regression statistics |
| tsc_read | `rayon::iter::plumbing::bridge_producer_consumer::helper` | 9.048 | criterion stats analyzer |
| tsc_read | `criterion::bencher::Bencher::iter` × 2 | 6.394 + 6.208 | RDTSC bench loop |
| tsc_read | `__ieee754_exp_fma` (libm) | 3.912 | criterion KDE |
| flow_lookup | `core::hash::BuildHasher::hash_one` | 2.768 | hash computation per lookup — under test |
| flow_lookup | `dpdk_net_core::flow_table::FlowTable::lookup_by_tuple` | 2.160 | flow lookup body — under test |
| flow_lookup | `core::hash::sip::Hasher::write` | 1.696 | SipHash internal — under test |
| tcp_input | `criterion::*` + `rayon::*` + libm | (combined ~14 s) | bench too fast — workload < harness overhead |
| tcp_input | `_int_malloc` / `_int_free` (glibc) | 0.605 / 0.489 | per-iter bench setup allocations (stub mbuf) |
| tcp_input | `do_user_addr_fault` (kernel) | 0.264 | per-iter page faults on fresh allocations |
| send | `__memcpy_avx_unaligned_erms` (glibc) | 0.957 | mbuf payload copy in stub send path |
| send | `sysmalloc` / `_int_malloc` / `brk` | 0.649 + … | stub allocates per iter (real wiring is T3.3) |
| send | `do_user_addr_fault` / `_raw_spin_unlock_irqrestore` | 0.690 / 0.388 | kernel allocator path on first-touch |
| timer | `criterion::bencher::Bencher::iter` | 2.309 | timer add/cancel core loop |
| timer | `do_user_addr_fault` / `clear_page_rep` (kernel) | 0.841 / 0.761 | per-iter wheel-slot allocator first-touch |
| timer | `__memcpy_avx_unaligned_erms` | 0.518 | timer state copy |
| counters | `criterion::bencher::Bencher::iter` | 6.972 | counter-read loop dominates (1 ns/iter) |
| counters | `rayon::*` + libm | (combined ~7 s) | criterion stats |

For `<not resolvable from TBP samples>` rows: poll, tsc_read, tcp_input, counters all show
the workload-under-test inlined small enough that TBP's 1 ms sampling cannot resolve it
above the criterion harness. IBS would; not available on this host.

## Estimated optimization opportunity

Plan-priority order: poll → tcp_input → send → flow_lookup → timer → tsc_read → counters
(as recorded in §"Next steps" of the master plan).

| Family | Plan-priority | Current gap | Top hotspot opportunity | Est. difficulty | Expected iterations |
|---|---|---|---|---|---|
| poll | 1 (highest) | +931 / +1065 ns over 100 ns | Workload buried under criterion-harness overhead. Real fix: skinnier hot path inside `EngineNoEalHarness::poll_once`; possible per-iter alloc churn (see kernel page-fault hits in `timer`/`send` profiles — same harness). | medium | 3–5 |
| tcp_input | 2 | within budget (−114 / −315 ns) | Already inside §11.2 envelope. Opportunity is variance reduction (p99 113.68 vs p50 86.04 = 32 % tail). Allocator hits in TBP (`_int_malloc`/`_int_free`/`do_user_addr_fault`) suggest preallocate-bench-state opportunity rather than algorithmic change. | low | 1–2 |
| send | 3 (T3.3 wires real EAL) | n/a (stub) | Cannot iterate on perf until T3.3 lands real `dpdk_net_send_*`. TBP confirms current cost is glibc malloc + `memcpy` + first-touch faults — pure stub artifact. | n/a | 0 (deferred) |
| flow_lookup | 4 | within budget hot (−13 ns) / cold (−105 ns) | Hottest workload-attributable: `BuildHasher::hash_one` 2.77 s + `lookup_by_tuple` 2.16 s + `sip::Hasher::write` 1.70 s. Hot path is hash-dominated; opportunity is faster hasher (xxh3 / aHash) for the 5-tuple key. | medium | 2–3 |
| timer | 5 | within budget (−25 ns) | Already inside. Hottest visible cost is `do_user_addr_fault` + `clear_page_rep` — bench-harness allocator first-touch, not the timer wheel. Real optimization: harness-side preallocation. | low | 1 |
| tsc_read | 6 | +5 ns ffi / +9 ns inline | Both bench variants land at ~10 ns — bottleneck is RDTSC latency on Zen 3 KVM, not the FFI call (no measurable ffi vs inline gap → call path already inlined by LTO). The 1 ns "inline" target requires a non-RDTSC clock or HLE/RDTSCP rework — out of scope. Re-target: ≤ 11 ns. | high | 0 (target revision) |
| counters | 7 (lowest) | within budget (−98 ns) | At 1.49 ns p50 already 67× better than 100 ns budget. No iteration warranted; family stays as a regression sentinel. | low | 0 |

## Observations + caveats

- **Diagnostic baseline.** THP=`madvise` (target `never`), cpufreq governor not exposed
  under KVM (target `performance`). Variance in `bench_poll_idle_with_timers` (p99 = 4×
  p50) consistent with THP-induced jitter. Re-baseline numbers after host re-tune.
- **Criterion-harness overhead dominates short benches.** For `tsc_read_*` (~10 ns/iter),
  `counters_read` (1 ns/iter), and most of `poll_empty` (1 µs / iter), the top TBP hotspots
  are `criterion::Bencher::iter` + `rayon::*` + libm `exp` (criterion's KDE statistics) —
  not the workload-under-test. TBP's 1 ms sampling cannot resolve workload symbols at this
  granularity. IBS would, but is not exposed to KVM guests on this host. Accept this and
  use criterion p50/p99 as the canonical signal; use TBP for the longer benches
  (flow_lookup, send) where real symbols do appear.
- **bench_tsc_read_inline target unreachable as-specified.** Header-inline FFI variant
  does not exist; bench proxies a pure-Rust path. Even with zero FFI overhead the floor is
  RDTSC latency itself (~10 ns Zen 3 KVM). Recommend revising §11.2 target to ≤ 11 ns and
  treating this row as a sentinel until T5 introduces `dpdk_net_now_ns_inline` (if ever).
- **bench_send_* are stubs.** Tagged `feature_set=stub` in CSV. TBP top hotspots
  (`__memcpy_avx_unaligned_erms`, `sysmalloc`, `do_user_addr_fault`, `_raw_spin_unlock_irqrestore`)
  are pure stub-side allocator work, not the real DPDK send path. T3.3 wires real EAL
  before this family can be iterated.
- **No call-graph data.** TBP collected without `--call-graph` (call-stack sampling adds
  overhead for already-noisy short benches). Future iterations can add `--call-graph dwarf`
  for the longer benches (flow_lookup, send) where stack attribution would refine the
  hotspot picture.
- **No PMC counters.** No IPC, no L1/L2/L3 misses, no DRAM bandwidth — KVM does not
  virtualize PMCs on this AMI. All claims in this matrix are wall-time only.
- **AMDuProfCLI flag corrections (vs plan §3.0 step 4).** Actual flags used:
  `-o, --output-dir` (not `--output`), `-i, --input-dir` (not `--import-dir`),
  `--report-output` writes CSV by extension (no HTML; no `--type text`; no `--section hot`).
  Hotspots extracted by parsing the `10 HOTTEST FUNCTIONS` block from the CSV report.
  `--profile-time 30` on the bench binary requires `--bench` (cargo-bench style) — without
  it, criterion's `BENCH_BENCHMARK_FILTER` short-circuits and exits in 0.05 s. Capture
  duration was therefore the bench's own `measurement_time` (5 s × N benches per family).

## Next steps

Per plan priority: poll → tcp_input → send (T3.3 wires real EAL) → flow_lookup → timer →
tsc_read → counters.

Top 3 to attack first (gap × plan-priority):
1. **poll** (T3.1) — both `bench_poll_empty` and `bench_poll_idle_with_timers` are 10×
   their §11.2 budget. First step: reproduce locally outside criterion (custom timing
   harness) to confirm the gap is in the workload not the measurement.
2. **tcp_input** (T3.2) — within budget but variance investigation; `_int_malloc` /
   `do_user_addr_fault` in TBP suggests bench-state preallocation will tighten p99.
3. **send** (T3.3) — must land real EAL wiring before any meaningful perf iteration.
   Treat current numbers as "stub baseline" for invariance checks only.

Re-run this baseline (with same TBP procedure) after THP=never lands; record diff in this
file.
