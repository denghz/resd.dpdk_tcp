# Part 8 Cross-Phase Retro Synthesis
**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 8 — AWS infra + benchmark harness + DPDK 24.x + perf cherry-picks (LARGEST)
**Phases:** A10 (incl. PR #9 deferred-fixes + T17-T23 post-phase tickets)
**Inputs:** cross-phase-retro-part-8-claude.md, cross-phase-retro-part-8-codex.md

## Combined verdict

A10 shipped functionally (DPDK 23.11 builds clean, 100k-iter bench-pair runs succeed, deferred items closed) but the largest part also accumulated the most structural debt. Two distinct problem classes dominate. **(1) The pressure-correctness layer between unit tests and benchmarks is missing** (Claude D1) — every T17 bug (TX mempool exhaustion, conn-handle leak, K=1MiB stall) was caught by sustained-throughput AWS runs, never by `cargo test`; the deferred-fixes regression suite is symptom-targeted, not class-targeted. **This is foundational and connects directly to user task #11 (pressure-test gap audit) — fix this layer before A11 perf work compounds the gap.** **(2) The benchmark harness now reaches around the C ABI** (Claude A1-A4, Codex hidden-coupling): 5 tools consume `dpdk_net_core::tcp_events::InternalEvent` / `Engine::events()` / `diag_input_drops()` directly, the `bench-internals` cargo feature legitimises the leak, and each new comparator (T21 bench-vs-mtcp, T22 mTCP, T19 F-Stack) widens the surface. Codex independently surfaced a sharper concrete bug class in the comparator itself: maxtp failure paths drop CSV marker rows (silent disappearance of failed buckets), F-Stack is wired into nightly without its cargo feature being default-on (stub-only coverage), F-Stack bypasses its own `ff_init` precondition, and the T17 TX-mempool divisor uses `mbuf_data_room` while the send path emits one mbuf per MSS-sized segment (formula optimistic when MSS < data-room). These two observation lenses are complementary: Claude sees architecture drift, Codex sees correctness/observability gaps in the new harness — both must be addressed before Stage 2.

## BLOCK A11 (must-fix before next phase)

- **B-A11-1. F-Stack comparator runs without `ff_init` — undefined behavior at every socket call.** [Codex BUG] `tools/bench-vs-mtcp/src/fstack_ffi.rs:34` documents the precondition, `tools/bench-vs-mtcp/src/fstack_ffi.rs:135` provides `ff_init_from_args`, but `tools/bench-vs-mtcp/src/fstack_burst.rs:81` and `tools/bench-vs-mtcp/src/fstack_maxtp.rs:76` call `ff_socket` directly with no init call site found in the comparator. FFI precondition violation; either remove F-Stack from nightly or wire init.
- **B-A11-2. F-Stack burst conflates EINPROGRESS with all connect failures.** [Codex BUG] `tools/bench-vs-mtcp/src/fstack_burst.rs:101-111` returns `Ok(fd)` on any nonzero `ff_connect` return without reading errno. Permanent failures look like successful connections to the harness.
- **B-A11-3. Nightly advertises F-Stack but builds without the `fstack` feature → marker/stub coverage only.** [Codex BUG] `scripts/bench-nightly.sh:110` builds `--workspace` with no features, `tools/bench-vs-mtcp/Cargo.toml:50` excludes `fstack` from defaults, but `scripts/bench-nightly.sh:827` passes `--stacks dpdk,fstack`. Nightly reports lie about coverage breadth.
- **B-A11-4. maxtp failure paths skip CSV marker rows → failed buckets disappear from reports.** [Codex BUG] DPDK open/run failures at `tools/bench-vs-mtcp/src/main.rs:857,860,893,898`; Linux at `:1058,1061,1096`; F-Stack at `:1459,1485,1487`. Invariant "every requested bucket produces an outcome row" violated. This is the strictest observability hole in the new harness.
- **B-A11-5. `bench-stress` CSV merge gated on `bench_stress_csvs[0]` — a missing first scenario blanks the whole stress CSV.** [Codex BUG] `scripts/bench-nightly.sh:589,599`. Failed first scenario hides successful later scenarios.
- **B-A11-6. `BENCH_ITERATIONS=5000` workaround default outlives the `f3139f6` cliff fix; comment lies about cliff being live.** [Claude C1, I1] `scripts/bench-nightly.sh:495-503` vs `docs/superpowers/reports/README.md:63`. Documentation drift + tech debt; pick raise-default or rewrite-comment.
- **B-A11-7. Pressure-correctness layer between unit and benchmark is missing — class-of-bug coverage gap.** [Claude D1, partial Codex SMELL on local-testable arithmetic] T17's three bugs only surfaced via AWS bench-pair. `crates/dpdk-net-core/tests/long_soak_stability.rs` is a single 100k-iter test with no churn / high-conn-count variant. **Foundational: connects to user task #11. Block A11 to prevent the next perf cherry-pick wave from layering on top of the gap.**

## STAGE-2 FOLLOWUP (real concern, deferred)

- **S2-1. T17 TX data-mempool divisor uses `mbuf_data_room`; send path emits one mbuf per MSS-sized segment.** [Codex LIKELY-BUG] `crates/dpdk-net-core/src/engine.rs:1231,1238` ceil(send_buffer_bytes / mbuf_data_room) vs send sites at `:5336,5376,5426,5524`. Optimistic for MSS=1460 with larger data-room; fix when revisiting sizing for higher-conn grids.
- **S2-2. F-Stack maxtp collapses all `ff_write < 0` into transient backoff — no errno discrimination.** [Codex LIKELY-BUG] `tools/bench-vs-mtcp/src/fstack_maxtp.rs:215`. Dead comparator → low-throughput bucket instead of failed row.
- **S2-3. `pub mod` everywhere in `dpdk-net-core` — bench tools couple to internals not C ABI (5 tools and growing).** [Claude A1, Codex hidden-coupling SMELL] `crates/dpdk-net-core/src/lib.rs:4-55` exposes 30+ modules; bench-ab-runner, bench-e2e (×2), layer-h-correctness, bench-vs-mtcp all reach past `dpdk_net.h`. Stage 2 boundary work.
- **S2-4. `bench-internals` cargo feature legitimises the engine-internals leak.** [Claude A2] `crates/dpdk-net-core/Cargo.toml:105-107` + `lib.rs:62-63` `pub use engine::test_support::EngineNoEalHarness`. Names the problem rather than fixing it.
- **S2-5. `test-server` workspace-feature unification trap recurs (tcpreq-runner `9f0ccd0`, layer-h-correctness `8147404`).** [Claude A3] Architectural fix: split test-only paths (`test_server`, `test_tx_intercept`, virtual clock) into a sibling crate so feature unification cannot rewire production binaries.
- **S2-6. `pub fn diag_*` accessor pattern accreting on `Engine` (T17 `tx_data_mempool_size`, `rx_drop_nomem_prev`, T17 `force_close_etimedout`, T21 `diag_input_drops`).** [Claude A4, C2] No `Diag` trait, no boundary; needs consolidation before Stage 2 multi-thread work breaks the borrow-cell single-thread invariant.
- **S2-7. Multi-seg RX L3 invariant gap persists at HEAD (Part 4 codex finding never closed).** [Claude B2] `crates/dpdk-net-core/src/lib.rs:74-78` `mbuf_data_slice` returns segment 0 only; `crates/dpdk-net-core/src/l3_ip.rs:86` rejects chained datagrams as `BadTotalLen`. Jumbo / scatter-RX silently dropped. No A10 commit touches this.
- **S2-8. C ABI doc for `rx_mempool_size` is behind the deferred PR #9 fix.** [Codex SMELL] `crates/dpdk-net/src/api.rs:67` documents old 2x term; `crates/dpdk-net-core/src/engine.rs:1186,1194` implements the newer doubled RX term. Update before next C++ caller integration.
- **S2-9. `EngineConfig.tx_data_mempool_size` (T17, Rust-only knob) not exposed in C ABI.** [Claude C5, G1; Codex hidden-coupling SMELL] `crates/dpdk-net/src/lib.rs:213-218` pins to 0 (formula default); header unchanged. C++ callers cannot configure; asymmetry undocumented.
- **S2-10. `Engine::diag_input_drops` returns Rust struct `InputDropsSnapshot` with no C ABI mirror.** [Claude G3] Bench-vs-mtcp consumes; future C++ caller diagnosing same stall has no equivalent.
- **S2-11. `tcp.tx_data_mempool_avail` is 2nd `AtomicU32` slow-path counter bypassing `ALL_COUNTER_NAMES`, no "intentionally absent" comment (mirrors `tcp.rx_mempool_avail`).** [Claude C3, E1] `crates/dpdk-net-core/src/counters.rs:304`. Future audits cannot distinguish known-omitted vs accidentally-omitted.
- **S2-12. Maxtp comparator-grid coupling: TX data-mempool override pins 32768 based on hard-coded "64 conns × 128" comment.** [Codex SMELL] `tools/bench-vs-mtcp/src/main.rs:1716`. Manual sync needed when grid grows; T17 stall shape can return.
- **S2-13. Stress harness shell↔Rust scenario duplication.** [Codex SMELL, Claude — implied via D2] `scripts/bench-nightly.sh:549` (4 netem) vs `tools/bench-stress/src/scenarios.rs:81,105,112,122` (additional FaultInjector scenarios). Already drifted: FaultInjector rows not in nightly matrix.
- **S2-14. FaultInjector p999-ratio limits informational, not enforced (no idle baseline under FaultInjector).** [Codex SMELL] `tools/bench-stress/src/main.rs:220,333,356`; `tools/bench-stress/src/scenarios.rs:106,123`. Limits don't fail nightly.
- **S2-15. Default `tx_payload_bytes` cross-check is feature-gated.** [Codex SMELL] `tools/bench-vs-mtcp/src/main.rs:628,916`. The most direct "sent exactly what harness expected" invariant is off by default.
- **S2-16. Engine `events()` returns `RefMut<EventQueue>`; bench-vs-mtcp pops in tight loops.** [Claude E4, F2] `crates/dpdk-net-core/src/engine.rs:2483`. Single-thread borrow-cell invariant; first thing to break in Stage 2 multi-core.
- **S2-17. T17 `close_persistent_connections` soft-fail on 5s deadline emits stderr only.** [Claude E2] `tools/bench-vs-mtcp/src/dpdk_maxtp.rs`. Scheduled bench-pair can pass with mid-grid timeouts; signal buried.
- **S2-18. T22 mTCP driver JSON-stderr-only error reporting; no cross-stack counter consistency assertion.** [Claude E3]
- **S2-19. Unit-test gating warnings on `mod a10_diagnostic_counter_tests` (release-build hygiene regression).** [Claude C4] `crates/dpdk-net-core/src/counters.rs:1060` two `unused_imports` warnings — only release-build warnings in workspace.
- **S2-20. Post-phase perf cherry-picks (a10-perf-23.11 series, ~14 commits) not all linked to per-task review summaries.** [Claude C6] `501b33f`, `da31fba`, `779fd55`, `3f1b4a8` reach hot-path engine code; T6.1/T6.2 review summaries exist (`98184b3`, `490a933`) but per-commit messages don't carry the pointer.
- **S2-21. `mbuf_data_slice` doc comment claims "first (and in Stage A2, only) segment" — Stage A2 framing is years stale.** [Claude I3] `crates/dpdk-net-core/src/lib.rs:66-67`. Function still returns seg 0; the framing misleads.
- **S2-22. Spec docs (`2026-04-29-a10-deferred-fixes-design.md`, `2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md`) lack Closure/Resolved-by pointers to commits `cebcb61`, `0cbc8d6`, `010b57b`, `f3139f6`, `bf48196`, `f07ac53`.** [Claude I2, I5]
- **S2-23. T18.1 commit message references "DPDK 20.11 sidecar" with no version-skew doc; `tools/bench-vs-mtcp/README.md` does not exist.** [Claude I4]

## DISPUTED (reviewer disagreement)

- **D-1. PR #9 cliff-fix invariant — both reviewers AGREE preserved at HEAD; not actually disputed.** Claude B3 audits 4 surviving `refcnt_update(-1)` sites (engine.rs:4185, 4232, 6163; tcp_input.rs:1393) as paired-bookkeeping safe; Codex independently confirms same 4 sites + adds tcp_input.rs:1392 / engine.rs:4184 / engine.rs:6162 (line-off-by-one variation, same sites). Not disputed; both rate "safe but fragile."
- **D-2. T17 TX-mempool sizing — Codex LIKELY-BUG vs Claude not flagged as bug (only architectural).** Codex says formula is optimistic when MSS < `mbuf_data_room`; Claude treats T17 as additive non-violation (B1). DISPUTED → routed to **S2-1** (Stage 2 follow-up): bench passes at 100k iters today, but Codex's MSS-vs-data-room arithmetic is plausible and worth verifying before next grid expansion.

## AGREED FYI (both reviewers flagged but not blocking)

- **AF-1. PR #9 cliff-fix held at HEAD.** [Claude B3, Codex FYI] `mempool.rs:271`/`tcp_reassembly.rs:303` use `shim_rte_pktmbuf_free_seg`; remaining `refcnt_update(-1)` are paired rollback paths.
- **AF-2. No A4-A6.7 invariant violations introduced by T17/T21/T22.** [Claude B1, Codex FYI on T17 send arithmetic + timer-wheel discipline]
- **AF-3. Memory ordering: all 222 `Ordering::*` uses are `Relaxed`; defensible for monotonic counters / single-threaded engine, flagged for Stage 2.** [Claude F1-F3, Codex FYI memory-ordering] `crates/dpdk-net-core/src/counters.rs:804,808`; `ena_xstats.rs:75`.
- **AF-4. C ABI shape unchanged since `phase-a10-deferred-fixed`.** [Claude G2, Codex FYI documentation-drift only]

## INDEPENDENT-CLAUDE-ONLY (HIGH/MEDIUM/LOW plausibility)

- **HIGH C-1. Pressure-correctness layer gap (D1).** Promoted to **B-A11-7**. Foundational; connects user task #11.
- **HIGH C-2. `pub mod` engine-internals leak across 5 bench tools (A1, H1, H2).** Promoted to **S2-3**.
- **HIGH C-3. `bench-internals` feature legitimising leak (A2, H4).** Promoted to **S2-4**.
- **HIGH C-4. `pub fn diag_*` accretion on Engine (A4, C2).** Promoted to **S2-6**.
- **HIGH C-5. Multi-seg RX L3 gap persists (B2).** Promoted to **S2-7**. Concrete code path: `lib.rs:74-78` + `l3_ip.rs:86`.
- **HIGH C-6. `BENCH_ITERATIONS=5000` doc drift (C1, I1).** Promoted to **B-A11-6**.
- **MED C-7. test-server feature unification trap recurrence (A3).** Promoted to **S2-5**.
- **MED C-8. `tx_data_mempool_avail` counter bypasses `ALL_COUNTER_NAMES` (C3, E1).** Promoted to **S2-11**.
- **MED C-9. T17 `EngineConfig.tx_data_mempool_size` not in C ABI (C5, G1).** Promoted to **S2-9**.
- **MED C-10. `diag_input_drops` no C ABI mirror (G3).** Promoted to **S2-10**.
- **MED C-11. `Engine::events()` borrow-cell single-thread (E4, F2).** Promoted to **S2-16**.
- **MED C-12. T17 `close_persistent_connections` stderr-only soft-fail (E2).** Promoted to **S2-17**.
- **MED C-13. T22 mTCP driver no cross-stack counter consistency (E3).** Promoted to **S2-18**.
- **MED C-14. `bench-stress` is throughput-under-netem, not correctness-under-stress; existing assertions brittle (D2).** Reinforces B-A11-7.
- **MED C-15. `layer-h-correctness` 17 scenarios single-conn — narrower than maxtp grid (D3, D4).** Reinforces B-A11-7.
- **LOW C-16. `mod a10_diagnostic_counter_tests` release-build warnings (C4).** Promoted to **S2-19**.
- **LOW C-17. Post-phase perf cherry-picks not all linked to review summaries (C6).** Promoted to **S2-20**.
- **LOW C-18. `mbuf_data_slice` "Stage A2, only" stale comment (I3).** Promoted to **S2-21**.
- **LOW C-19. Spec docs lack Closure/Resolved-by pointers (I2, I5).** Promoted to **S2-22**.
- **LOW C-20. T18.1 DPDK 20.11 sidecar undocumented + no `tools/bench-vs-mtcp/README.md` (I4).** Promoted to **S2-23**.
- **FYI C-21. T22 ~1100 LOC subagent-produced C, 3 blockers fixed in same commit (J1).** Quality-of-output signal.
- **FYI C-22. `cargo build --workspace --release` 41.78s, 2 warnings (J6).**
- **FYI C-23. `bench-rx-zero-copy` doesn't depend on `bench-common` (J2).** Justified.
- **FYI C-24. Test counts: 425/116/31/29 (J3).**
- **FYI C-25. 4-way comparator now exists (DPDK / mTCP / Linux / F-Stack via T19) (J5).** Compounds H1.

## INDEPENDENT-CODEX-ONLY (HIGH/MEDIUM/LOW plausibility)

- **HIGH X-1. F-Stack `ff_init` precondition violated (FFI BUG).** Promoted to **B-A11-1**.
- **HIGH X-2. F-Stack burst EINPROGRESS-vs-error conflation (FFI BUG).** Promoted to **B-A11-2**.
- **HIGH X-3. F-Stack feature default-off but nightly invokes `--stacks dpdk,fstack` (BUG).** Promoted to **B-A11-3**.
- **HIGH X-4. maxtp failure paths skip CSV markers (BUG).** Promoted to **B-A11-4**.
- **HIGH X-5. `bench-stress` CSV merge gated on first-scenario presence (BUG).** Promoted to **B-A11-5**.
- **MED X-6. T17 mempool divisor uses `mbuf_data_room` not MSS (LIKELY-BUG).** Routed to **S2-1** / DISPUTED **D-2**.
- **MED X-7. F-Stack maxtp collapses all `ff_write < 0` into backoff (LIKELY-BUG).** Promoted to **S2-2**.
- **MED X-8. `crates/dpdk-net/src/api.rs:67` rx_mempool_size doc behind PR #9 fix (SMELL).** Promoted to **S2-8**.
- **MED X-9. Maxtp `tx_data_mempool_size=32768` pinned to "64 × 128" comment (SMELL).** Promoted to **S2-12**.
- **MED X-10. Shell↔Rust scenario duplication, FaultInjector rows skipped by nightly (SMELL).** Promoted to **S2-13**.
- **MED X-11. FaultInjector p999-ratio limits informational only (SMELL).** Promoted to **S2-14**.
- **MED X-12. Default `tx_payload_bytes` cross-check feature-gated (SMELL).** Promoted to **S2-15**.
- **FYI X-13. AtomicBool stop-flag in `linux_maxtp.rs:350,381,422` is `Relaxed`-acceptable (FYI).**
- **FYI X-14. No lock-ordering bug in reviewed `RefCell::borrow_mut` paths (FYI).**
- **FYI X-15. No SEQ-comparison regression in T17 send arithmetic (FYI).**
- **FYI X-16. No timer-wheel add/cancel discipline violation in T17/T20 paths (FYI).**

## Counts

Total: 56; BLOCK-A11: 7; STAGE-2: 23; DISPUTED: 2; AGREED-FYI: 4; CLAUDE-ONLY: 25 (5 promoted + 20 informational); CODEX-ONLY: 16 (5 promoted + 11 informational)
