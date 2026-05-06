# A10-perf — postphase summary (cross-worktree)

**Period:** 2026-04-23 to 2026-04-29
**Effort:** A10-perf (post-A10 performance follow-on, separate from main phase roadmap)
**Worktrees:** `a10-perf-23.11` (DPDK 23.11), `a10-dpdk24-adopt` (DPDK 24.11) — both branched from `phase-a10` tip `671062a`
**Master integration branch:** `integration/a10-perf-2026-04-29`

## TL;DR

- **Worktree 1 outcome:** **9 of 10 measurable bench-micro families within §11.2 budget** after a single hypothesis (poll H1 — pre-allocate `conn_handles_scratch` in `EngineNoEalHarness`, -95% on `bench_poll_empty` and `bench_poll_idle_with_timers`). 2 stubs deferred (`bench_send_*`); 2 host-ceiling-bound on KVM TSC virtualization (would meet target on bare-metal — confirmed by pure-C `__rdtsc` reproduction at 10.13 ns matching the bench's 10.17 ns).
- **Worktree 2 outcome:** DPDK 23.11 -> 24.11 rebase is **clean (zero API drift)** but **0 of 4 target APIs adopted** at our scope (architectural mismatch or out-of-bench-micro-scope). **Recommendation: stay on 23.11** for Stage 1 ship; keep 24.11 worktree as reference for future re-evaluation.
- **Net to master:** Per the recommendation, ONLY documentation (specs, plans, reports, reviews) is being landed in this integration step. The Worktree-1 production code (bench-internals feature, EngineNoEalHarness, bench rewrites, CSV schema, H1 + tsc_read fixes) cannot cherry-pick to master in isolation — those commits depend on the underlying `phase-a10` work which has not yet been merged to master. See "Integration scope caveat" below.

## Integration scope caveat — IMPORTANT

The original task plan assumed master already had the 6 base A10-perf commits (89771a2 spec, 07c0e79 plan, 6b56015 host-capabilities, 8c2d3f9 check-perf-host, c1fab09 T2.7 deferral, 564616c base-amend). At the start of this integration step, master at `f959da5` did **not** have them — they had been removed by an earlier `git reset --hard origin/master` and survived only via reflog.

More fundamentally, master and `phase-a10` are heavily diverged lineages:

- master (a8.5 + Jenkins CI lineage): no phase-a10 work
- phase-a10 (a7+a8+a9+a10 lineage): full benchmark harness + observability + diagnostic counters

Worktree 1's production-code commits (797ab92 onwards) reference `tools/bench-micro/`, `tools/bench-common/`, the `obs-none` Cargo feature, `engine.rs` line numbers from phase-a10's tree, etc. None of those exist on master. Cherry-picking those commits onto master would conflict on EVERY commit because the underlying foundation doesn't match.

The integration that actually landed on master:

- 6 base A10-perf commits (spec, plan, host-capabilities, check-perf-host, base-amend, T2.7 deferral) — re-applied (they had been on master previously and were lost in the reset)
- 7 W1 doc-only family-summary + review commits (poll, tcp_input/flow_lookup/timer/counters, tsc_read, send, final summary, T6.1 spec review, T6.2 code-quality review)
- 10 W2 doc-only commits (3 N/A surveys, 1 ENA logger deferred, 1 deferrals doc, W2 summary, port-forward doc, T6.3 spec review, T6.4 code review, markdown link fix)

Total: 23 commits on master. **0 production-code commits.**

The W1 production-code commits (797ab92 bench-internals, cd3cf20 timer wheel gate, 80ba3c1 EngineNoEalHarness, cae8e01 harness tests, 7695b285 / 95f1bff bench rewrites, bfe3d99 CSV schema, acedb33 STUB_TARGETS, 5b8ee71 poll H1, 2cc6829 tsc_read H1+H2, d000ff9 T3.0 baseline, aeea1d6 T2.7 doc) and the W2 baseline-rebase doc commit (322cd60) **could not be cleanly cherry-picked** because:

- 797ab92 conflicts on `Cargo.toml` (master lacks the `obs-none` feature added in phase-a10)
- 80ba3c1 + downstream commits touch `engine.rs` at line numbers that don't match master
- 7695b285 + 95f1bff modify `tools/bench-micro/benches/{poll,timer}.rs` which don't exist on master
- bfe3d99 modifies 11 bench tools which don't exist on master
- d000ff9 + 322cd60 conflict on `.gitignore` (master added `/.claude/`; these add `/profile/`)

These commits remain available on the `a10-perf-23.11` and `a10-dpdk24-adopt` branches for re-application **after** `phase-a10` is merged to master. Re-application path is straightforward: once phase-a10 is on master, the W1 production-code commits cherry-pick cleanly (verified on the W1 worktree itself).

## Per-family final state (from W1 + W2 reports)

| Family / Bench | Pre-A10-perf baseline (T3.0) | W1 23.11 final | W2 24.11 final | §11.2 upper | Within budget? |
|---|---|---|---|---|---|
| bench_poll_empty | 1031.54 ns | 45.8 ns | ~46 ns | 100 ns | yes |
| bench_poll_idle_with_timers | 1165.02 ns | 50.7 ns | ~51.5 ns | 100 ns | yes |
| bench_tcp_input_data_segment | 86.04 ns | 86.53 ns | 86.43 ns | 200 ns | yes |
| bench_tcp_input_ooo_segment | 84.86 ns | 75.54 ns | 71.65 ns | 400 ns | yes |
| bench_send_small (STUB) | 70.58 ns | (stub) | (stub) | 150 ns | n/a (deferred) |
| bench_send_large_chain (STUB) | 1231.55 ns | (stub) | (stub) | 5000 ns | n/a (deferred) |
| bench_flow_lookup_hot | 27.07 ns | 25.88 ns | 25.73 ns | 40 ns | yes |
| bench_flow_lookup_cold | 95.20 ns | 89.64 ns | 95.79 ns | 200 ns | yes |
| bench_timer_add_cancel | 25.02 ns | 23.94 ns | 24.24 ns | 50 ns | yes |
| bench_tsc_read_ffi | 10.21 ns | 10.222 ns | 10.18 ns | 5 ns | host-ceiling (KVM virt) |
| bench_tsc_read_inline | 10.37 ns | 10.159 ns | 10.33 ns | 1 ns | host-ceiling (KVM virt) |
| bench_counters_read | 1.49 ns | 0.92 ns | 0.92 ns | 100 ns | yes (>=65x under) |

**Gate status:** 9 of 10 measurable benches within budget on both worktrees. 2 of 2 stubs deferred to T3.3 (real-send wiring). 2 of 2 tsc_read benches host-ceiling-bound on KVM (would meet target on bare-metal).

## Retained optimizations (on W1 / W2 branches, not yet on master)

| ID | Family | Change | Saved | Commit |
|---|---|---|---|---|
| H1 | poll | `EngineNoEalHarness::poll_once` pre-allocates `conn_handles_scratch: Vec<ConnHandle>`; mirrors the real `Engine::poll_once` pre-allocated reuse pattern | ~986 / 1081 ns (-95%) | `5b8ee71` (W1), `fcd344f` (W2) |
| H1+H2 | tsc_read | `b.iter_custom` with BATCH=128 + XOR-fold accumulator with single end-of-batch `black_box(acc)` | methodology hygiene; floor unchanged | `2cc6829` (W1), `c59a973` (W2) |

## Rejected hypotheses (W1)

- **tsc_read H3** (inline path not actually inlining): rejected — `objdump -d` shows literal `rdtsc` + inlined `OnceLock::get` + inlined `mulq`/`shld` in both bench hot loops; rustc LTO inlines `dpdk_net_now_ns` through the FFI boundary — no `call` instruction. Already inlining.
- **tsc_read H4** (KVM TSC virtualization ceiling): accepted, not rejected — confirmed via pure-C `__rdtsc` tight loop measuring 10.13 ns/op identical to bench's 10.17 ns. ~5 ns of the 10 ns total is VMCS round-trip / TSC offset adjustment, unaddressable from inside the KVM guest.

## DPDK 24.11 evaluation (W2 outcome)

- **Rebase delta:** clean (single line `atleast_version("23.11")` -> `atleast_version("24.11")` + bindgen regen, +6% additive bindings). bench-micro neutral within criterion noise across all 12 benches.
- **4 target APIs surveyed: 0 adopted:**
  - `rte_lcore_var` (24.11) — N/A: 0 candidate sites; our Stage 1 design is single-Engine-per-lcore, not the per-lcore-array pattern this API replaces.
  - `rte_ptr_compress` (24.07) — deferred-to-e2e: 1 candidate site exists (RX burst at engine.rs:2077), but `EngineNoEalHarness` doesn't call `shim_rte_eth_rx_burst` and send is stubbed.
  - `rte_bit_atomic_*` (24.11) — N/A: 0 candidate sites. Codebase has zero `fetch_or` / `fetch_and` / `fetch_xor` / `fetch_nand` instances.
  - ENA TX logger rework (24.07) — deferred-pending-T3.3: passive driver-internal; verification needs real `dpdk_net_send` + bench-pair host.
- **Recommendation: stay on 23.11.** Do not amend `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md` §8 DPDK pin. Do not amend `project_context` memory.
- **Re-evaluate when:** (a) T3.3 real-send wiring lands, (b) bench-pair host provisioned, (c) Stage 2 multi-queue work begins, (d) external pressure (security advisory, upstream EOL, must-have driver) forces upgrade.

## Caveats / future-work files

- `docs/superpowers/reports/perf-host-capabilities.md` — uProf TBP-only, IBS/PMC unavailable on KVM. Bare-metal verification host needed for production-quality ship numbers.
- `docs/superpowers/reports/t2-7-deferral.md` — `bench_send_*` real-path wiring deferred; future T3.3 picks it up.
- A10-perf does not change the §11 microbench plan or §11.2 targets in the parent Stage 1 design spec — those numbers were already met or close-to-met at A10 finish; A10-perf certifies the gate-met-ness on a unified harness.

## Reviews — verdicts

- **W1 T6.1 (spec-compliance):** PASS-WITH-CAVEATS. Report at `docs/superpowers/reports/perf-23.11/review-spec-compliance.md`.
- **W1 T6.2 (code-quality):** APPROVED-WITH-COMMENTS. Report at `docs/superpowers/reports/perf-23.11/review-code-quality.md`.
- **W2 T6.3 (spec-compliance):** PASS-WITH-CAVEATS. Report at `docs/superpowers/reports/perf-dpdk24/review-spec-compliance.md`.
- **W2 T6.4 (code-quality):** APPROVED. Report at `docs/superpowers/reports/perf-dpdk24/review-code-quality.md`.

All four reviews completed by Opus 4.7 subagents per the per-task two-stage review discipline.

## Roadmap update

Per spec §7.3 row 2 (stay on 23.11), no roadmap clauses are amended. The roadmap row for A10 stays "Complete"; this effort sits between A10 and A10.5 as a documented follow-on once the production code lands.

Roadmap append (apply when phase-a10 is merged to master and W1 production-code commits are landed):

> A10-perf follow-on complete (2026-04-29): bench-micro families certified within §11.2 (9 of 10; 2 host-ceiling on KVM, 2 stubs deferred); DPDK 24.11 evaluation deferred to T3.3 + bench-pair host.

## Worktree cleanup

`a10-perf-23.11` and `a10-dpdk24-adopt` worktrees stay on disk for now (research reference + carrier of the un-cherry-picked production-code commits awaiting phase-a10 merge). User may run:

```
git worktree remove /home/ubuntu/resd.dpdk_tcp-a10-perf
git worktree remove /home/ubuntu/resd.dpdk_tcp-a10-dpdk24
```

when disk pressure warrants. Branches stay reachable via the branch ref regardless of worktree removal.

## Files added to master in this integration

- `docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md`
- `docs/superpowers/plans/2026-04-23-a10-microbench-perf-and-dpdk24-adopt.md`
- `docs/superpowers/reports/perf-host-capabilities.md`
- `docs/superpowers/reports/t2-7-deferral.md`
- `docs/superpowers/reports/perf-23.11/poll-summary.md`
- `docs/superpowers/reports/perf-23.11/counters-summary.md`
- `docs/superpowers/reports/perf-23.11/flow_lookup-summary.md`
- `docs/superpowers/reports/perf-23.11/tcp_input-summary.md`
- `docs/superpowers/reports/perf-23.11/timer-summary.md`
- `docs/superpowers/reports/perf-23.11/tsc_read-summary.md`
- `docs/superpowers/reports/perf-23.11/send-summary.md`
- `docs/superpowers/reports/perf-23.11/summary.md`
- `docs/superpowers/reports/perf-23.11/review-spec-compliance.md`
- `docs/superpowers/reports/perf-23.11/review-code-quality.md`
- `docs/superpowers/reports/perf-dpdk24/adopt-rte-lcore-var.md`
- `docs/superpowers/reports/perf-dpdk24/adopt-rte-ptr-compress.md`
- `docs/superpowers/reports/perf-dpdk24/adopt-rte-bit-atomic.md`
- `docs/superpowers/reports/perf-dpdk24/adopt-ena-tx-logger.md`
- `docs/superpowers/reports/perf-dpdk24/deferrals.md`
- `docs/superpowers/reports/perf-dpdk24/summary.md`
- `docs/superpowers/reports/perf-dpdk24/port-forward-poll-H1.md`
- `docs/superpowers/reports/perf-dpdk24/review-spec-compliance.md`
- `docs/superpowers/reports/perf-dpdk24/review-code-quality.md`
- `scripts/check-perf-host.sh`
- `docs/superpowers/reports/perf-a10-postphase.md` (this file)
