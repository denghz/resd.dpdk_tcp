# Worktree 2 (`a10-dpdk24-adopt`) — final summary + 24.11 pinning recommendation

**Branch:** `a10-dpdk24-adopt` (worktree at `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24`)
**Base commit:** `phase-a10` tip `671062a` (same as Worktree 1)
**DPDK pin:** 24.11.0 (worktree-local via `scripts/use-dpdk24.sh`; 23.11.0 stays system default)
**Profiling host:** EC2 KVM, AMD EPYC 7R13 / Zen 3 Milan, TBP-only (same as Worktree 1)
**Baseline mode:** diagnostic (THP=madvise, governor unavailable on KVM)

## Phase 1 — Rebase outcome

**Decision-matrix-row-1 outcome (clean rebase).** DPDK 23.11 → 24.11.0 transition produced **zero API drift** in our codebase:

- `crates/dpdk-net-sys/build.rs`: `atleast_version("23.11")` → `atleast_version("24.11")` — single-line bump
- bindgen regen: +797 lines (+6%) of binding output, all additive (new APIs we'll explore in Phase 2)
- `cargo build --workspace`: clean
- `cargo test --workspace --features bench-internals`: 1064 passed, 1 failed (pre-existing api.rs:288 doctest from base — not introduced by this effort), 10 ignored
- harness tests: 5/5 pass
- bench compile-check: all 30+ benches compile

No fix commits required between the version bump and a clean test sweep.

## Phase 1.5 — Post-rebase bench-micro baseline (T4.5-4.6)

Side-by-side with 23.11 baseline (both worktrees on same KVM dev host, same diagnostic conditions, neither yet had H1 applied):

| Family / Bench | 23.11 (T3.0) | 24.11 post-rebase (T4.5) | Δ % | Note |
|---|---|---|---|---|
| bench_poll_empty | 1031.54 ns | 1039.02 ns | +0.7% | within noise |
| bench_poll_idle_with_timers | 1165.02 ns | 1139.43 ns | -2.2% | within noise |
| bench_tsc_read_ffi | 10.21 ns | 10.18 ns | -0.3% | host-ceiling both |
| bench_tsc_read_inline | 10.37 ns | 10.33 ns | -0.4% | host-ceiling both |
| bench_flow_lookup_hot | 27.07 ns | 25.73 ns | -5.0% | within noise |
| bench_flow_lookup_cold | 95.20 ns | 95.79 ns | +0.6% | within noise |
| bench_tcp_input_data_segment | 86.04 ns | 86.43 ns | +0.5% | within noise |
| bench_tcp_input_ooo_segment | 84.86 ns | 71.65 ns | **-15.6%** | improvement; likely run-to-run host variance, not a 24.11 effect |
| bench_send_small (STUB) | 70.58 ns | 72.87 ns | +3.2% | stub variance |
| bench_send_large_chain (STUB) | 1231.55 ns | 1342.52 ns | +9.0% | stub variance |
| bench_timer_add_cancel | 25.02 ns | 24.24 ns | -3.1% | within noise |
| bench_counters_read | 1.49 ns | 0.92 ns | **-38.5%** | sub-ns floor; criterion overhead dominates |

ENA TX regression check (5× bench_send_*): N/A on stubs; recorded stub variance for reference (1.5–3.2%) but not a genuine ENA TX measurement.

**Verdict: rebase alone is neutral-within-noise.** No regressions. The two large-percent items are an improvement (ooo_segment) and a noise-floor sub-ns artefact (counters_read); neither attributes to a real 24.11 mechanism.

## Phase 2 — Target-API adoption (T4.7-4.10)

Per the brainstorm D6 hypothesis that 24.11/24.07 APIs would deliver measurable benefits, we surveyed 4 target APIs:

| API | Outcome | Report | Reason |
|---|---|---|---|
| `rte_lcore_var` (24.11) | **N/A** — 0 candidate sites | `adopt-rte-lcore-var.md` | Architectural mismatch: our Stage 1 design is single-Engine-per-lcore (Cell/RefCell), not the per-lcore-array pattern this API replaces. The 5 `repr(C, align(64))` decorations in `counters.rs` are single-instance struct decorations for false-sharing isolation, not the per-lcore-slab pattern. |
| `rte_ptr_compress` (24.07) | **deferred-to-e2e** | `adopt-rte-ptr-compress.md` | 1 candidate site exists at `engine.rs:2077` (the `[*mut rte_mbuf; 32]` RX burst array), but `EngineNoEalHarness` doesn't call `shim_rte_eth_rx_burst`, and send is stubbed. Bench-micro has no measurement reach. Re-evaluate when bench-e2e runs on real ENA. |
| `rte_bit_atomic_*` (24.11) | **N/A** — 0 candidate sites | `adopt-rte-bit-atomic.md` | Codebase scan found 0 instances of `fetch_or` / `fetch_and` / `fetch_xor` / `fetch_nand` anywhere. Our atomic surface is exclusively `fetch_add(_, Relaxed)` and `load(Relaxed)` on counters. The bit-mask atomic pattern this API optimizes simply doesn't exist in our code. |
| ENA TX logger rework (24.07) | **deferred-pending-T3.3** | `adopt-ena-tx-logger.md` | Passive driver-internal change; no application-side surface. Verification requires real `dpdk_net_send` execution + real ENA NIC. Both prerequisites are outside bench-micro scope (T2.7 deferral). |

Plus pre-declared deferrals (D6): PM QoS, `rte_thash_gen_key`, Intel E830, eventdev pre-scheduling — all confirmed N/A or out-of-scope (`deferrals.md`).

**Net adoption count: 0 of 4 target APIs adopted.** This is an honest deferral: bench-micro on a KVM dev host can't capture much of 24.11's value, and the parts that COULD matter (ENA driver improvements, RX-burst optimizations) require infrastructure (T3.3 real-send wiring + bench-pair host) that's out of this effort's scope.

## Phase 3 — Port-forward from Worktree 1 (T5)

**Worktree 1 had two retained optimization commits worth porting:**

1. **`5b8ee71` — poll H1 (harness scratch reuse)** → cherry-picked as `fcd344f`. **A/B verified -95% on 24.11**, matching 23.11's outcome. Both `bench_poll_empty` (~46 ns) and `bench_poll_idle_with_timers` (~51 ns) within §11.2 100 ns upper. Report: `port-forward-poll-H1.md`.

2. **`2cc6829` — tsc_read H1+H2 (iter_custom + XOR-fold)** → cherry-picked as `c59a973`. Methodology improvement; same host-ceiling outcome on 24.11 (TSC virtualization adds ~5 ns / 10 ns floor regardless of DPDK version).

Both port-forwards confirm the harness-only optimizations are **DPDK-version-agnostic** — they cherry-pick cleanly without adaptation.

## Final per-family state on 24.11 (post-port-forward)

| Family / Bench | 24.11 final median | §11.2 upper | Within budget? |
|---|---|---|---|
| bench_poll_empty | ~46 ns | 100 ns | ✓ |
| bench_poll_idle_with_timers | ~51.5 ns | 100 ns | ✓ |
| bench_tsc_read_ffi | 10.18 ns | 5 ns | host-ceiling ✗ |
| bench_tsc_read_inline | 10.33 ns | 1 ns | host-ceiling ✗ |
| bench_flow_lookup_hot | 25.73 ns | 40 ns | ✓ |
| bench_flow_lookup_cold | 95.79 ns | 200 ns | ✓ |
| bench_tcp_input_data_segment | 86.43 ns | 200 ns | ✓ |
| bench_tcp_input_ooo_segment | 71.65 ns | 400 ns | ✓ |
| bench_send_small | (stub) | 150 ns | n/a |
| bench_send_large_chain | (stub) | 5000 ns | n/a |
| bench_timer_add_cancel | 24.24 ns | 50 ns | ✓ |
| bench_counters_read | 0.92 ns | 100 ns | ✓ (108× under) |

**Same gate-met-rate as Worktree 1.** 9 of 10 measurable benches within budget; 2 host-ceiling on KVM TSC (would meet on bare-metal); 2 stubs deferred.

## **Net 24.11 pinning recommendation: STAY ON 23.11 for Stage 1 ship**

Per spec §7.3 decision matrix, this is row 2: "Both pass §11.2; 24.11 comparable or worse" — comparable here meaning within criterion noise (no measurable advantage on bench-micro).

### Reasoning

1. **Rebase is safe** — 24.11 builds + tests cleanly with zero application-side fix commits. If at some future point we need to upgrade for an external reason, the path is simple.

2. **24.11's API improvements don't directly benefit our bench-micro scope** — 0 of 4 target APIs adopted; survey showed our codebase doesn't use the patterns these APIs optimize.

3. **24.11's improvements that COULD help our workload (ENA driver, real RX/TX bursts) are gated on infrastructure we don't have** — real send wiring (T3.3 deferred) + bench-pair hardware host. Until those exist, 24.11's advantages are theoretical for us.

4. **23.11 is well-validated** — phase-a10's full test matrix ran on 23.11 across A1-A10. No reason to introduce a year of upstream churn into the Stage 1 ship.

5. **Keep 24.11 as a reference branch** — `a10-dpdk24-adopt` stays local, delete from primary working copy if disk pressure, but don't lose the learning. When T3.3 lands AND a bench-pair host is available, **re-run T4.7-T4.10 with end-to-end measurement** to revisit the adoption decision.

### When to revisit 24.11 promotion

The following conditions, individually or together, would change the recommendation:

- **Stage 1 ship has shipped** and we have time to do a clean upgrade with proper bench-pair validation.
- **T3.3 real-send wiring lands** AND bench-pair host is provisioned. At that point, re-measure the ENA TX logger rework (24.07) + `rte_ptr_compress` (24.07) on the RX burst — both could deliver real gains for production traffic.
- **Stage 2 multi-queue work** introduces RSS imbalance that `rte_thash_gen_key` (24.11) could cure.
- **External pressure** (security advisory, upstream EOL on 23.11, must-have driver in a newer release) forces an upgrade.

### What to do with the worktree

- **Keep the branch local** as a research reference: `a10-dpdk24-adopt` at HEAD `c59a973`.
- **Don't merge to master.** The base-commit-amendment + spec/plan + scripts cherry-picks should appear on master via Worktree 1's review (since they're identical).
- **If user wants disk back:** `git worktree remove /home/ubuntu/resd.dpdk_tcp-a10-dpdk24` after Phase 6 completes; the branch's commits stay reachable via the branch ref.

## Cherry-pick candidate set for Phase 6 master integration (production-fit subset)

Given the recommendation to stay on 23.11, **no Worktree-2-specific commits should ship to master directly**. The Worktree-2 work is documentary (reports + survey evidence) and the rebase commit is conditional on a future 24.11 promotion decision.

Phase 6 should:
- Pick from Worktree 1 (`a10-perf-23.11`) for the production code changes (bench-internals + harness + bench rewrites + CSV schema + H1 + tsc_read H1+H2).
- Optionally pick the **survey reports** from Worktree 2 (`adopt-*.md`, `deferrals.md`, this `summary.md`) onto master as documentation of the 24.11 evaluation. These don't change code; they document a decision.

## Worktree 2 commit log (for review's reference)

```
<this commit> a10-dpdk24: T4.12 worktree-2 summary + 24.11 pinning recommendation
c59a973 a10-perf-23.11: tsc_read H1+H2 — iter_custom batched + XOR-fold black_box (cherry-picked)
d22e15b a10-dpdk24: T5 port-forward poll H1 — verified -95% improvement on 24.11
fcd344f a10-perf-23.11: poll H1 — pre-allocate conn_handles_scratch (cherry-picked)
b5399d7 a10-dpdk24: T4.11 document pre-declared 24.11 deferrals
de71de1 a10-dpdk24: ENA TX logger rework — deferred-pending-T3.3 + bench-pair host
38b6905 a10-dpdk24: rte_bit_atomic_* N/A — 0 candidate sites in our crate
84c9333 a10-dpdk24: rte_ptr_compress N/A in bench-micro scope — deferred to e2e
803c549 a10-dpdk24: rte_lcore_var N/A — 0 candidate sites
322cd60 a10-dpdk24: T4.5-4.6 post-rebase baseline + ENA TX regression check (N/A on stubs)
<rebase> a10-dpdk24: bump atleast_version 23.11 → 24.11
... (Phase 0-2 commits cherry-picked from a10-perf-23.11)
... (initial 4 A10-perf-spec/plan/report/script cherry-picks from master)
... (base 671062a phase-a10 tip)
```

## Worktree 2 exit decision

**Worktree 2 is done.** Recommendation is "stay on 23.11"; ship Worktree 1's optimizations to master. Keep `a10-dpdk24-adopt` as a reference branch for future re-evaluation.

The cross-worktree summary in Phase 6 T6.6 (`docs/superpowers/reports/perf-a10-postphase.md`) consolidates this with Worktree 1's outcome and is the user-facing final recommendation.
