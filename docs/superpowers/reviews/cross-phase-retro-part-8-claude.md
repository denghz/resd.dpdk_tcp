# Part 8 Cross-Phase Retro Review (Claude)
**Reviewer:** general-purpose subagent (opus 4.7) — covering for superpowers:code-reviewer
**Reviewed at:** 2026-05-05
**Part:** 8 — AWS infra + benchmark harness + DPDK 24.x + perf cherry-picks (largest part)
**Phases:** A10 (incl. PR #9 deferred-fixes + T17-T23 post-phase tickets)

## Verdict

A10 (largest part — ~80 commits over ~3 weeks across IaC, harness, perf, deferred-fixes, and seven post-phase fixup tickets) shipped functionally — DPDK 23.11 builds clean, workspace builds clean, deferred items closed per `docs/superpowers/reports/README.md`, and bench-pair runs at 100k iterations succeed. The architectural debt, however, is now load-bearing on the bench harness rather than the engine: 11 bench-* tools share `bench-common` (good), but they reach across `pub mod`-everything in `dpdk-net-core` (e.g. `engine.events()`, `tcp_events::InternalEvent`, `engine::diag_input_drops()`, `EngineNoEalHarness`) — a tight coupling that the C ABI was supposed to insulate them from. The user's "max throughput bench surfaces many bugs" observation is structural: T17 fixed three issues (TX mempool sizing, conn-handle leaks, K=1MB stall) that pre-existed at phase-a10-complete and were caught only by sustained-throughput AWS runs, not by `cargo test`; the post-phase regression suite added (`long_soak_stability`, `tx_mempool_no_leak_under_retrans`, `multi_seg_chain_pool_drift`) is targeted at the symptoms found, not at the missing layer. The Part 4 finding that `mbuf_data_slice` returns only segment 0 of an RX chain — and L3 thus rejects multi-seg jumbo frames as `BadTotalLen` — remains at HEAD; no A10 work touched the L3 ingest path. `scripts/bench-nightly.sh` still defaults `BENCH_ITERATIONS=5000` with a stale comment claiming the cliff is unfixed (it was, by `f3139f6`, per `docs/superpowers/reports/README.md`).

## Architectural drift

**A1. `pub mod` everywhere in `dpdk-net-core` — bench tools couple to internals not C ABI.** `crates/dpdk-net-core/src/lib.rs:4-55` exposes 30+ modules as `pub mod` (engine, tcp_input, tcp_options, tcp_reassembly, mempool, flow_table, counters, tcp_events, ...). The C ABI surface (`crates/dpdk-net/include/dpdk_net.h`) was meant to be the stable contract; in practice the bench harness goes around it. Examples at HEAD:
  - `tools/bench-ab-runner/src/workload.rs:47` — `use dpdk_net_core::tcp_events::InternalEvent;`
  - `tools/bench-ab-runner/src/workload.rs:211, 356` — `engine.events()` returning `RefMut<EventQueue>`.
  - `tools/bench-e2e/src/workload.rs:29, 185, 283` — same pattern.
  - `tools/layer-h-correctness/src/observation.rs` — same.
  - `tools/bench-vs-mtcp/src/dpdk_burst.rs` (post-T21) — `engine.diag_input_drops()` reading `tcp.rx_paws_rejected/bad_option/bad_seq/bad_ack/urgent_dropped` directly.
This is the same Part 4 finding now wider; T17 / T21 / T22 each added one more internals reach rather than narrowing the surface.

**A2. The `bench-internals` cargo feature legitimises the leak.** `crates/dpdk-net-core/Cargo.toml:105-107` adds `bench-internals = []` (post-A10) which gates `pub use engine::test_support::EngineNoEalHarness` (`crates/dpdk-net-core/src/lib.rs:62-63`). bench-micro now composes against an in-process engine harness that is a `pub` re-export of an internal helper. Architecturally this names a problem rather than fixes it: any future bench-foo tool that needs "just one more" engine knob will gain a feature flag and a `pub fn` accessor. RFC-stable surface drift is now feature-gated drift.

**A3. `test-server` workspace-feature unification trap recurs.** First fix `9f0ccd0 fix(tcpreq-runner): gate test-server feature to prevent workspace-wide unification`, then again at `8147404 fix: gate layer-h-correctness behind test-server feature (gateway ARP regression)` — same pattern, different tool. `tools/tcpreq-runner/Cargo.toml:11-25` and `tools/layer-h-correctness/Cargo.toml:19-44` document the trap inline. Architectural fix would be to move the test-only paths in `dpdk-net-core` (`test_server`, `test_tx_intercept`, virtual clock) into a separate sibling crate so feature unification cannot rewire production binaries; the per-tool gates are workarounds.

**A4. `pub fn diag_input_drops()` (T21) is the engine's accessor #N for diagnostic state.** `crates/dpdk-net-core/src/engine.rs:2472`. The pattern repeats from A10 (`tx_data_mempool_size()`, `rx_drop_nomem_prev()`, T17's `force_close_etimedout` dump): every time a bench-arm needs to attribute a stall, a new `pub fn diag_*` is added on `Engine`. There's no `Diag` trait, no boundary; the engine slowly grows a "look inside" surface that's neither C-ABI-stable nor internally encapsulated.

## Cross-phase invariant violations

**B1. None of T17/T21/T22 violate A4-A6.7 invariants — they're additive.** T17 (`8b25f8f`) adds a config knob + sizing formula in `Engine::new`, slow-path `tx_data_mempool_avail` counter, and a maxtp inter-bucket drain helper outside engine. Verified diff at `crates/dpdk-net-core/src/engine.rs` shows no SEQ-arithmetic, no retransmit-state, no hot-path-alloc changes; the only deletion is the `4096` hardcode. T21 (`e2dddf1`) adds `InputDropsSnapshot` POD + accessor (38 lines). T22 (`72a2214`) is in `peer/mtcp-driver.c` — not in engine code at all.

**B2. Multi-seg RX L3 invariant gap persists at HEAD (Part 4 codex finding NOT addressed).** `crates/dpdk-net-core/src/lib.rs:74-78` `mbuf_data_slice` returns `from_raw_parts(shim_rte_pktmbuf_data, shim_rte_pktmbuf_data_len)` — segment 0 only. `crates/dpdk-net-core/src/engine.rs:3756` feeds that slice to `rx_frame → handle_ipv4`. `crates/dpdk-net-core/src/l3_ip.rs:86` rejects with `Err(L3Drop::BadTotalLen)` when `total_len > pkt.len()`. Effect: any RX mbuf chain whose IPv4 datagram crosses into segment 1+ (jumbo frames > 2 KiB data-room, or scatter-RX on a NIC configured for split-header) is silently dropped at the L3 length check. tcp_input's a6.6-task-5 multi-seg walk only fires *after* L3/L4 decode succeeded, which it cannot for chained datagrams. No A10 commit touches this.

**B3. `engine.rs:4185, 4232` use `refcnt_update(-1)` rollback — defensible only because RX-burst owns +1.** Same primitive that the PR #9 cliff fix replaced in `MbufHandle::Drop`. Audit of every remaining `refcnt_update(-1)` site:
  - `engine.rs:4185, 4232` — rollback of pre-dispatch +1 bump; RX-burst owner still holds +1, so post-rollback refcount ≥ 1 → `shim_rte_pktmbuf_free` at end of `dispatch_one_real_mbuf` (line 3791) frees correctly. **Safe** by paired-bookkeeping but **fragile**: any future change that frees the RX-burst ref earlier would re-introduce the leak class.
  - `engine.rs:6163` — chain-fail rollback on retransmit; data_mbuf was held in `snd_retrans` queue (refcount ≥ 1) prior to +1, so -1 leaves it at queue baseline. **Safe**.
  - `tcp_input.rs:1393` — multi-seg-walk link rollback when OOO insert refused; head still owns +1, so safe.
  - `tcp_reassembly.rs:248` — `+1 × extra` insert bump; positive delta, no leak class.
  None of these need the `pktmbuf_free_seg` switch, but the audit is implicit. A drift-detection comment on each surviving `-1` site naming the holder of the surviving ref would be a cheap insurance against re-introducing the cliff class.

## Tech debt accumulated

**C1. `BENCH_ITERATIONS=5000` workaround default in orchestrator outlives the fix.** `scripts/bench-nightly.sh:495-503` — comment block claims "deterministic TCP retransmit-budget exhaustion at iteration ~7051" mandates lowering the default. `docs/superpowers/reports/README.md:63` says the cliff is fixed by commit `f3139f6` and 100k-iter runs complete cleanly. Bench-nightly default still 5000. **Documentation drift + tech debt simultaneously**: future operator runs at 5k think the cliff is live; the comment lies.

**C2. T17 + T21 introduced one diagnostic surface each on `Engine`; no consolidation.** `tx_data_mempool_size()`, `rx_drop_nomem_prev()`, `diag_input_drops()`. None are in `dpdk-net-core::counters::ALL_COUNTER_NAMES` (T21's `InputDropsSnapshot` reads counters that ARE in the list, but the snapshot accessor is invisible to `lookup_counter`). bench-vs-mtcp is the only consumer.

**C3. `tcp.tx_data_mempool_avail` (T17 counter) — 2nd `AtomicU32` slow-path counter that bypasses `ALL_COUNTER_NAMES`.** `crates/dpdk-net-core/src/counters.rs:304`. Mirrors `tcp.rx_mempool_avail` (which has an explicit "intentionally absent from this list" comment at line 547-550). The new one has no such comment in the table; future M2/M1 audits will not flag it as known-omitted vs. accidentally-omitted. Same shape rule, second instance, no abstraction.

**C4. Unit-test gating warnings on `mod a10_diagnostic_counter_tests`.** `crates/dpdk-net-core/src/counters.rs:1060`. `cargo build -p dpdk-net-core --release` emits two `unused_imports` warnings because the mod isn't `#[cfg(test)]`-gated. Two warnings on a release build is small but is the only release-build warning in the workspace — it's a regression of clean-build hygiene.

**C5. T17 added `tx_data_mempool_size` to `EngineConfig` — pinned to 0 on the C ABI side (`crates/dpdk-net/src/lib.rs` post-T17 hunk).** No deprecation note, no FFI follow-up ticket. C-ABI users will receive the formula default; Rust users can override. The asymmetry isn't documented in `crates/dpdk-net/include/dpdk_net.h`.

**C6. Two A10 perf-23.11 commits merged unreviewed via the post-phase track (`501b33f poll H1 — pre-allocate conn_handles_scratch`, `da31fba T9 H7 — fast-path TS-only parse_options`, `779fd55 T9 H5 — gate reorder.drain on is_empty`, `3f1b4a8 T7.7 tcp_input H2 — #[inline] parse_options`).** These reach into hot-path engine code; the per-task spec/code-review discipline (per `feedback_per_task_review_discipline.md`) appears to have been applied via T6.1/T6.2 review summaries (`98184b3`, `490a933`), but the individual commits don't carry the review pointer in their messages. Rapid post-phase perf cherry-picks risk side-stepping the gate.

## Test-pyramid concerns

**D1. The gap the user named is real: pressure-test layer is missing between unit + benchmark.** A10's bench-* tools are throughput-measurement, not correctness-under-pressure. T17's three bugs (TX mempool exhaustion at K=1MiB sustained 8050+ bursts, conn-handle leak across maxtp buckets, send_buf wedge) were all surfaced by AWS bench-pair, not by `cargo test`. The deferred-fixes wave added targeted regression tests (`crates/dpdk-net-core/tests/long_soak_stability.rs`, `tx_mempool_no_leak_under_retrans.rs`, `multi_seg_chain_pool_drift.rs`, `rx_mempool_no_leak_ooo_netem.rs`) — all are TAP-loopback tests with one `#[test]` each (`grep '#\[test\]' long_soak_stability.rs` returns 1 match at line 154). Pressure scenarios that match maxtp grid shape (W ≥ 4096 conns, K=1MiB sustained bursts, conn churn between buckets) are not in `cargo test`. Class-of-bug coverage gap.

**D2. `bench-stress` is throughput-under-netem, not correctness-under-stress.** `tools/bench-stress/src/main.rs` measures p50/p99/p999 RTT under packet loss / reorder / dup. There's no assertion that the workload completes successfully under stress; the post-T16 commit `fa25bfd bench-stress: relax correlated_burst_loss_1pct assertion to tx_retrans` shows the existing assertions are themselves brittle.

**D3. `layer-h-correctness` (A10.5) is the closest thing to a pressure-correctness suite, but its scope is narrower (17 scenarios, single-conn).** Doesn't exercise the maxtp grid shape that surfaced T17.

**D4. `crates/dpdk-net-core/tests/long_soak_stability.rs` is a single 100k-iter test.** No connection-churn variant, no high-conn-count variant. T17's #3 (mid-bucket InvalidConnHandle on conn-handle leak) would not be caught by the current soak test.

## Observability gaps

**E1. `tcp.tx_data_mempool_avail` is sampled-only (per-second) and excluded from `ALL_COUNTER_NAMES`.** Counter-coverage tests (`crates/dpdk-net-core/tests/counter-coverage.rs`) cannot exercise it. T17's promise "watch eth.tx_drop_nomem + tcp.tx_data_mempool_avail trends" only works if the bench harness reads the value via direct `pub` accessor — which is exactly the architectural-coupling issue (A1 above).

**E2. T17's per-bucket close-and-poll cleanup (`close_persistent_connections` in `dpdk_maxtp.rs`) emits no event the operator can subscribe to.** Soft-fail on 5s deadline → counter? Log line? `grep -n soft_fail tools/bench-vs-mtcp/src/dpdk_maxtp.rs` shows it's stderr-printed only. A scheduled bench-pair could pass with mid-grid timeouts and the only signal is buried in stderr.

**E3. T22's mTCP driver — JSON-only error reporting on stderr.** No counter consistency assertion between mTCP and dpdk_net arms. bench-vs-mtcp comparator wrappers don't assert on tcp.tx_pkts == observed-on-wire-pkts cross-stack.

**E4. `Engine::events()` returns `RefMut<EventQueue>` directly.** `crates/dpdk-net-core/src/engine.rs:2483`. Bench harnesses pop events and then drop the RefMut. Any `borrow_mut` panic during that window crashes the bench. There is no event-queue overflow assertion; bench-vs-mtcp `dpdk_burst.rs` calls `engine.events()` in tight loops and may starve other engine borrows.

## Memory-ordering / ARM-portability concerns

**F1. 222 `Ordering::*` uses in `dpdk-net-core/src/`, ALL `Relaxed`.** Verified by `grep -rn "Ordering::Acquire\|Ordering::Release\|Ordering::SeqCst" crates/dpdk-net-core/src/` returning empty. Single-threaded engine model; counters are written by one thread (lcore) and read by another (operator app). **For pure counter monotonic-increment-and-read** Relaxed is RFC-defensible; ARM weak ordering still respects intra-thread program order, and cross-thread counter reads have no causal precondition. **Not a bug, but a portability assumption** — future Stage 2 work that introduces multiple engine threads, or any cross-thread synchronization (e.g. event queue between the lcore and the application thread, which is `RefCell<EventQueue>` today and inherently single-threaded), will need to revisit.

**F2. `Engine::events()` returns `RefCell::borrow_mut`-derived `RefMut`, not an atomic queue.** Borrow-cell single-thread invariant — when Stage 2 lifts the engine across cores this is the first thing to break. Not an ARM-specific concern but compounding the F1 portability assumption.

**F3. T17's `tx_data_mempool_avail` AtomicU32 — per-second sample, written by lcore, read potentially from operator thread for diag dump. Relaxed write+read.** Same defensibility as F1; flagging for Stage 2 audit, not a bug today.

## C-ABI / FFI

**G1. T17 added `EngineConfig.tx_data_mempool_size` (Rust-only knob). C ABI did not gain a corresponding field.** `crates/dpdk-net/src/lib.rs:213-218` (T17 hunk) pins it to `0` (formula default). `crates/dpdk-net/include/dpdk_net.h` unchanged in this part (verified by `git diff phase-a10-deferred-fixed..HEAD -- crates/dpdk-net/include/`). No ABI break, but C++ callers cannot configure the new knob — the asymmetry is undocumented in the header.

**G2. No C-ABI shape change since `phase-a10-deferred-fixed`.** `dpdk_net.h` is byte-identical at HEAD. Good.

**G3. `Engine::diag_input_drops` (T21) returns `InputDropsSnapshot` — Rust struct, no C ABI mirror.** Bench-vs-mtcp consumes it. Any future C++ caller diagnosing the same stall has no equivalent surface.

## Hidden coupling

**H1. (Part 4 finding still at HEAD, expanded.)** `dpdk_net_core::tcp_events::InternalEvent` consumed by 4 tools (`tools/bench-ab-runner/src/workload.rs`, `tools/bench-e2e/src/workload.rs`, `tools/bench-e2e/src/hw_task_18.rs`, `tools/layer-h-correctness/src/observation.rs`). T21's `InputDropsSnapshot` adds bench-vs-mtcp as a 5th tool reaching past the C ABI.

**H2. `tools/bench-vs-mtcp/src/dpdk_burst.rs` post-T21 reads 5 specific tcp.* counters by name through the `Engine::diag_input_drops` accessor.** Any tcp.rx_*-counter rename in `dpdk-net-core/src/counters.rs` silently breaks the bench-arm's diagnostic message without a compile error (it'd compile because the Rust struct field names are stable; it'd fail at the next bench-pair run).

**H3. `bench-vs-mtcp` peer-side mTCP driver (T22, `peer/mtcp-driver.c`, ~1100 lines) is a separate process — no shared state coupling.** Good.

**H4. `EngineNoEalHarness` (post-A10 `bench-internals` feature) re-exposes engine internals for bench-micro.** `crates/dpdk-net-core/src/lib.rs:62-63` `pub use engine::test_support::EngineNoEalHarness;`. Tests in `crates/dpdk-net-core/tests/engine_no_eal_harness.rs` use it; bench-micro uses it. The "test_support" module name is a fig leaf — the consumer is the perf harness, not test code.

## Documentation drift

**I1. `scripts/bench-nightly.sh:495-502` comment block contradicts `docs/superpowers/reports/README.md:63`.** Comment says iteration-7051 cliff is the reason for the 5000 default; README says the cliff is fixed by `f3139f6`. The default was never raised. **Pick one**: raise the default to 100000 (per `docs/superpowers/specs/2026-04-29-a10-deferred-fixes-design.md:228`) and rewrite the comment to "kept low for fast nightly cycle" — or amend the comment to "5000 is fast-iteration default; cliff fixed in `f3139f6`, run with `BENCH_ITERATIONS=100000` for full validation". Currently the comment misleads.

**I2. `docs/superpowers/specs/2026-04-29-a10-deferred-fixes-design.md` is post-fact at HEAD: items closed in `docs/superpowers/reports/README.md` but the spec doc itself doesn't link the closure commits.** `git log` shows commits `cebcb61`, `0cbc8d6`, `010b57b`, `f3139f6` resolved the three items. Spec at line 75-78 has a "Validation" section but no "Closure" / "Resolved by" pointer. Operators reading the design from latest tag don't see resolution.

**I3. `crates/dpdk-net-core/src/lib.rs:66-67` comment claims `mbuf_data_slice` returns "first (and in Stage A2, only) segment".** Stage A2 was years ago; multi-seg RX shipped in A6.6 task 5. Comment is stale; the function still returns seg 0 only, but the "Stage A2" framing implies callers see all segments. Misleading at HEAD.

**I4. T18.1 commit message mentions "DPDK 20.11 sidecar"; project elsewhere references DPDK 23.11 / DPDK 24.11.** No version-skew doc explaining the mTCP comparator's frozen-DPDK-20.11 boundary. `docs/superpowers/reports/perf-a10-mtcp-rebuild-investigation.md` exists but isn't linked from `tools/bench-vs-mtcp/README.md` (none exists).

**I5. `docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md` covers the 23.11→24.x adoption.** Per the commit log (`a10-dpdk24:` series, T4.11 / T4.12 / T5 / T6.1-6.4), the conclusion was "ship 23.11, defer 24.11 promotion" — `bf48196 perf-a10-postphase: cross-worktree summary + 23.11 ship recommendation`, `f07ac53 a10-dpdk24: T4.12 worktree-2 summary + 24.11 pinning recommendation`. The spec doc itself doesn't carry the deferral note at HEAD.

## FYI / informational

**J1. T22 (`72a2214`) ships ~1100 LOC of C in `peer/mtcp-driver.c`.** Subagent-produced + 2-stage reviewed; 3 blockers fixed in same commit. The blocker class (#2: stack-frame size 16-32 KB → events[1]; #4: spinning drain on dead socket) suggests the produce-then-review loop is necessary. Per-task review discipline working, but C-on-stack hazards in subagent output is a quality-of-output signal worth tracking.

**J2. `tools/bench-rx-zero-copy` doesn't depend on `bench-common`.** Justified — pure criterion microbench; no CSV output. Listed for completeness.

**J3. Test counts at HEAD per T17 commit message: 425 dpdk-net-core, 116 bench-vs-mtcp, 31 bench-obs-overhead, 29 bench-offload-ab.** Robust unit footprint; the gap (D1) is in the layer above unit, below benchmark.

**J4. Post-A10 commit log includes ~14 perf cherry-picks (a10-perf-23.11 series).** None observed at HEAD that violate A4-A6.7 invariants (read sample of 4 above). Comprehensive review of all 14 not performed in this part; per-commit two-stage review pointers should be cross-checked in any future audit.

**J5. `phase-a10-postphase-throughput.md` exists and contains the Linux kernel-TCP comparator capture (commit `38fc296`) — bench infrastructure now produces 3-way (DPDK / mTCP / Linux) and 4-way (+ F-Stack via T19) comparisons.** Architectural coverage is wide; the H1 (engine-internals reach) costs scale with each new comparator.

**J6. `cargo build --workspace --release` succeeds at HEAD against DPDK 23.11.0 (verified, 41.78s)** with two warnings on `a10_diagnostic_counter_tests` (C4). DPDK 24.x adopt deferred per I5.

## Verification trace

- `git tag | grep -E "phase-a10|phase-a9"` → `phase-a9-complete`, `phase-a10-complete`, `phase-a10-deferred-fixed`, `phase-a10-5-complete`.
- `git log --oneline phase-a9-complete..phase-a10-complete | wc -l` → ~52 commits (A10 main).
- `git log --oneline phase-a10-complete..phase-a10-deferred-fixed | wc -l` → 16 commits (PR #9 deferred-fixes).
- `git log --oneline phase-a10-deferred-fixed..HEAD | wc -l` → 190 commits (post-A10: T17-T23 + a10-perf-23.11 + a10-dpdk24 + A10.5).
- `cargo build --workspace --release` (DPDK 23.11.0 via pkg-config) → success, 41.78s, 2 warnings (counters.rs:1060-1062 unused_imports).
- `grep -rn "rte_mbuf_refcnt_update.*-1\|refcnt_update.*, -" crates/dpdk-net-core/src/` → 4 production sites (engine.rs:4185, 4232, 6163; tcp_input.rs:1393); each reviewed in B3.
- `grep -rln "InternalEvent" tools/ --include="*.rs"` → 4 files (bench-ab-runner, bench-e2e workload, bench-e2e hw_task_18, layer-h-correctness).
- `grep -nE "^pub mod" crates/dpdk-net-core/src/lib.rs | wc -l` → 30 modules.
- `grep -rn "Ordering::Acquire\|Release\|SeqCst" crates/dpdk-net-core/src/` → empty (all 222 ordering uses are Relaxed).
- `grep -n "bench_common" tools/*/Cargo.toml` → 9 of 11 bench-* tools depend on bench-common (bench-rx-zero-copy + bench-common itself excluded).
- `bash scripts/bench-nightly.sh --dry-run` → exits at prereq check (`resd-aws-infra` missing); orchestrator structurally OK.
- `git diff phase-a10-deferred-fixed..HEAD -- crates/dpdk-net/include/dpdk_net.h` → empty (no C ABI shape changes since deferred-fixed).
- Reviewed T17 (`8b25f8f`), T21 (`e2dddf1`), T22 (`72a2214`) commit diffs for SEQ-arithmetic / retransmit / hot-path-alloc / zero-copy invariant violations — none found (B1).
- Cross-checked Part 4 codex finding "chained-mbuf head-only L3 rejection" via `crates/dpdk-net-core/src/lib.rs:74-78` + `crates/dpdk-net-core/src/l3_ip.rs:86` — confirmed still at HEAD (B2).
- Cross-checked Part 4 codex finding `Engine::events()` / `drain_events()` exposure → still at HEAD; expanded by T21 (H1, H2).
- `docs/superpowers/reports/README.md:63` cross-checked against `scripts/bench-nightly.sh:495-503` — confirmed documentation drift (I1).
