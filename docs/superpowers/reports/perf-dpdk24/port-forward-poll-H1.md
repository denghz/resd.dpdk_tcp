# Port forward: poll H1 (`5b8ee71` from a10-perf-23.11) → 24.11

**Original commit on a10-perf-23.11:** `5b8ee71` — `a10-perf-23.11: poll H1 — pre-allocate conn_handles_scratch (mirror Engine::poll_once)`
**Cherry-picked to a10-dpdk24-adopt as:** `fcd344f`

## Cherry-pick path

Clean cherry-pick — no API drift between 23.11 and 24.11 versions of `EngineNoEalHarness`. The change is harness-internal (in the `engine::test_support` inline module under `bench-internals` feature gate), with no DPDK API surface touched.

## A/B results on 24.11

| Bench | T4.5 post-rebase median | Post-port (post-H1) median | Δ | Δ % | Verdict |
|---|---|---|---|---|---|
| bench_poll_empty | 1039.02 ns | ~46 ns | -993 ns | -95.6% | adopted |
| bench_poll_idle_with_timers | 1139.43 ns | 51.51 ns | -1088 ns | -95.5% | adopted |

Criterion verdict: `Performance has improved.` (p < 0.05). Confidence interval: `[-95.938% -95.196% -94.240%]` for `bench_poll_idle_with_timers`. The improvement matches the magnitude observed on 23.11 (T3.1: 1031 → 45.8 ns, 1165 → 50.7 ns).

## Decision

**Keep.** H1 transfers cleanly to DPDK 24.11. No re-implementation needed; no regression introduced.

## §11.2 budget verification

Both poll benches now within budget on 24.11:
- `bench_poll_empty`: ~46 ns ≤ 100 ns upper ✓
- `bench_poll_idle_with_timers`: ~51 ns ≤ 100 ns upper ✓

Same gate-met outcome as 23.11.

## Caveats

Diagnostic baseline (THP=madvise, governor unavailable) — same noise floor as 23.11. The relative -95% delta is robust under that noise. Absolute numbers will tighten on a hardened verification host.

## What this confirms

The harness scratch-reuse optimization is **DPDK-version-agnostic** — it lives in pure-Rust harness code that doesn't touch DPDK APIs. The same change works identically on 23.11 and 24.11. This is expected for harness-internal optimizations and serves as a validation point for Phase 5 port-forward more broadly: harness-only changes cherry-pick cleanly between worktrees with no adaptation cost.
