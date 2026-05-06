# Stage 1 Cross-Phase Retro — Meta-Synthesis

**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Inputs:** cross-phase-retro-part-{1..9}-synthesis.md
**Scope:** Stage 1 phases A1 through A10.5

## Headline numbers

| Part | Phases | Verdict | BLOCK-A11 | STAGE-2 | DISPUTED |
|------|--------|---------|-----------|---------|----------|
| 1 | A1 + A2 | NEEDS-FIX | 3 | 27 | 0 |
| 2 | A3 + A4 | NEEDS-FIX | 3 | 11 | 1 |
| 3 | A5 + A5.5 + A5.6 | CONDITIONAL SHIP | 2 | 9 | 1 |
| 4 | A6 + A6.5 + A6.6 + A6.7 | BLOCK-A11 | 3 | 13 | 0 |
| 5 | A-HW + A-HW+ | NEEDS-FIX | 1 | 21 | 1 |
| 6 | A7 + A8 + A8.5 | PROCEED | 0 | 19 | 0 |
| 7 | A9 | NEEDS-FIX | 2 | 20 | 0 |
| 8 | A10 (incl. T17–T23) | BLOCK-A11 (largest) | 7 | 23 | 2 |
| 9 | A10.5 | PROCEED-WITH-FIXES | 3 | 10 | 0 |

**Total BLOCK-A11 items across Stage 1: 24** (de-duped where one part cross-references another, e.g. Part 7 BLOCK-A11 #1 explicitly defers to Part 6 S2-1)
**Total STAGE-2 items: 153**
**Total DISPUTED: 5** (4 routed; 1 collapsed back to single-reviewer)
**Total findings (all buckets): ~322**

## Pervasive patterns (recurring across ≥2 parts)

### Pattern P1 — Workspace-feature-unification leak via test/inject crates
**Recurrence:** Parts 6, 7, 8, 9 (and originating regressions cited from earlier T11/T20 commits)
**Description:** Tool crates declare `dpdk-net-core` as a non-optional dep with `features = ["test-inject"]` (or analogous test-only feature). Cargo's workspace feature unification rewires every consumer of `dpdk-net-core` in the workspace, silently activating test-only branches in production binaries. The pattern was nominally fixed twice — `tcpreq-runner` in `9f0ccd0` (2026-05-02) and `layer-h-correctness` in `8147404` — but both fixes were incomplete:
- Part 6 S2-1: `tools/scapy-fuzz-runner/Cargo.toml:8` still has the antipattern; not gated, not optional.
- Part 7 BLOCK-A11 #1: same `scapy-fuzz-runner` Cargo.toml leak (cross-references Part 6).
- Part 9 BLOCK-A11 B-1: commit `8147404` patched `Cargo.toml` + `lib.rs` but did NOT touch `scripts/layer-h-{smoke,nightly}.sh`, which still call `cargo build --release --workspace` with no feature flag, so the `required-features = ["test-server"]` binary is silently skipped on a clean tree.
- Part 8 S2-5: explicitly names "test-server workspace-feature unification trap" as a recurring class.

**Root cause:** No structural enforcement. Cargo silently unifies; reviewers cannot grep for absence of a feature gate; CI does not assert "the released `dpdk-net-core` resolves with exactly `default` features."

**Recommendation:** A single workspace-level CI gate (5–10 lines of `cargo metadata | jq`) that asserts the production resolution graph for `dpdk-net-core` contains no `test-server` / `test-inject` / `test-panic-entry` / `fault-injector` / `obs-none` features. This catches every past and future instance mechanically. In addition, structurally split the `test_server` / `test_tx_intercept` / virtual-clock paths into a sibling crate (Part 8 S2-5) so feature-unification cannot reach them.

**Findings cross-referenced:**
- Part 6 S2-1, S2-2 (CI gate), S2-7 (shim-runner non-optional dev-dep)
- Part 7 BLOCK-A11 #1
- Part 8 S2-5
- Part 9 BLOCK-A11 B-1, S2-1, S2-2

---

### Pattern P2 — Engine internals leaking past the C ABI to bench/correctness tools
**Recurrence:** Parts 4, 6, 8, 9
**Description:** The advertised public surface is `include/dpdk_net.h` (21 `extern "C"` symbols). In practice the bench / correctness tools reach into `dpdk-net-core` and consume `InternalEvent`, `Engine::events()` returning `RefMut<EventQueue>`, `engine.poll_once()` directly, `Engine::diag_*` accessors, and `tcp_events::EventKind`. The leak is now deep enough that a `bench-internals` cargo feature exists to legitimise it (Part 4 S6-style finding promoted to Part 8 S2-4).

- Part 4 STAGE-2: `Engine::events()` leaks `RefMut<EventQueue>` (both reviewers); `bench-ab-runner` consumes `InternalEvent`; `layer-h-correctness` bypasses public ABI.
- Part 6 S2-3 / Part 7: divergent `inject_rx_frame` implementations because tools call into engine directly through feature flags.
- Part 8 S2-3 / S2-4 / S2-6 / S2-16: `pub mod` everywhere, 5 tools coupled, `pub fn diag_*` accretion (T17 `tx_data_mempool_size`, `rx_drop_nomem_prev`, `force_close_etimedout`, T21 `diag_input_drops`), `Engine::events()` borrow-cell single-thread invariant.
- Part 9 S2-7 / S2-8: layer-h consumes `InternalEvent` directly; `EventKind::Other` fallthrough loses event timestamps.

**Root cause:** No enforced API boundary on the Rust side. `dpdk-net-core` exposes most internals as `pub mod`, and every new bench tool finds it cheaper to dip into internals than to extend the C ABI. Each new comparator (T19 F-Stack, T21 mTCP-bench, T22 mTCP driver) widens the leak.

**Recommendation:** Introduce a `pub(crate)` discipline pass on `dpdk-net-core/src/lib.rs` and replace today's `pub use engine::test_support::*` re-exports with a tightly-scoped `dpdk-net-test-support` sibling crate gated on `test-server`. Promote `diag_input_drops` and friends into a `Diag` trait with a typed C ABI mirror (`dpdk_net_diag_t`). This is foundational for Stage-2 multi-thread work since the current pattern relies on the single-lcore borrow-cell invariant.

**Findings cross-referenced:**
- Part 4 BLOCK-A11 (READABLE event lifetime), STAGE-2 `Engine::events()` leak, A10 bench-ab-runner, A10.5 layer-h
- Part 6 S2-3 (divergent inject_rx_frame)
- Part 8 S2-3, S2-4, S2-6, S2-9, S2-10, S2-16
- Part 9 S2-7, S2-8

---

### Pattern P3 — `bump_counter_one_shot` covers addressability, not real-path correctness
**Recurrence:** Parts 1, 5, 6 (and indirectly 8 via the missing pressure-correctness layer)
**Description:** The counter-coverage test infrastructure exposes `engine.bump_counter_for_test(...)` (`crates/dpdk-net-core/tests/common/mod.rs:592`) used in ~15+ HW-only counter sites and many A8/A8.5 cases. These tests prove that the counter exists and is registered in `ALL_COUNTER_NAMES`, NOT that it is correctly bumped from the real protocol path. This is exactly why the NIC-BAD `ip.rx_csum_bad` double-bump (Part 1 BLOCK-A11 / Part 5 BLOCK-A11) survived undetected: there is no real-path test that drives a NIC-BAD frame through `ip_decode_offload_aware`.

- Part 1 STAGE-2: "No default-build engine-level test for the NIC-BAD checksum counter" (Codex TP-1).
- Part 1 STAGE-2: `ip.rx_drop_unsupported_proto` only TAP-test covered.
- Part 5 STAGE-2 S-4 / TP-2: explicit — `tests/counter-coverage.rs:391-450` and `:377-388` use synthetic `bump_counter_one_shot`; closest real-path test (`l3_ip.rs:343-353` `bad_csum_dropped_when_verifying`) exercises only software path.
- Part 6 S2-9 (cross-references Part 5 CPI-2): same pattern restated for HW-only and A8/A8.5 counters.
- Part 8 B-A11-7 / pressure-correctness layer: every T17 bug surfaced via AWS bench-pair runs, not `cargo test`. Same root cause: the test pyramid has unit-level addressability and integration-level smoke, but no class-targeted real-path coverage.

**Root cause:** Adding a real-path test for each NIC-BAD / loss / corruption / ENOMEM path is more expensive than `bump_counter_one_shot`, and the per-phase reviewer prompts accept the synthetic harness as "covered." Consequently: the NIC-BAD double-bump introduced in `e2aae95` survived three phases (A-HW → A-HW+ → A6.x) and was first surfaced by the cross-phase retro pass.

**Recommendation:** Two structural changes:
1. Forbid `bump_counter_one_shot` for any counter that has a known real-path drive available (encode the rule in the reviewer prompt + a `#[deny(unused)]`-like lint via test attribute).
2. Add a minimal real-path NIC-BAD / ENOMEM injection layer as part of A11 hardening: a `FaultInjector`-style entry that drives a single NIC-BAD mbuf through `ip_decode_offload_aware` and asserts exact counter increments at every layer.

**Findings cross-referenced:**
- Part 1 BLOCK-A11 #3 (NIC-BAD double-count) + STAGE-2 (no NIC-BAD engine test)
- Part 5 BLOCK-A11 B-1 + STAGE-2 S-4
- Part 6 S2-9
- Part 8 B-A11-7 (pressure-correctness layer)

---

### Pattern P4 — Counter-placement bugs + observability counter divergence
**Recurrence:** Parts 1, 2, 3, 5, 7, 9
**Description:** Counters are bumped at the wrong layer, on the wrong condition, or unconditionally where they should be conditional. This is mechanically-detectable but consistently survives per-phase review.

- Part 1 BLOCK-A11 #3: `ip.rx_csum_bad` double-bumped (l3 wrapper + engine caller).
- Part 1 STAGE-2: `ip.rx_drop_short` collapses two distinct error kinds (`Short` vs `BadTotalLen`); `tcp.rx_syn_ack` bumped on SYN-only segments.
- Part 2 BLOCK-A11 B3 / STAGE-2 S4: close-path counters never populated; `tcp.rx_syn_ack` bogus increment on test-server passive-open path.
- Part 3 STAGE-2 S1 / S2 (BUG-class): TLP and RACK both increment `tcp.tx_tlp` / `tcp.tx_rack_loss` after a void `retransmit()` call, even when the call queued nothing (ENOMEM / stale-entry).
- Part 5 BLOCK-A11 B-1: same NIC-BAD double-bump — the highest-confidence cross-cutting bug.
- Part 7 BLOCK-A11 #2: `fault_injector.rs:276` `corrupts` counter increments on zero-length corruption; the actual write at `:263` is guarded but the counter is not.
- Part 9 BLOCK-A11 B-3: row-14 corruption disjunction omits `tcp.rx_bad_csum`; spurious false-fail under offload-OFF.

**Root cause:** Counter ownership is implicit and per-handler. There is no compile-time invariant that "exactly one site bumps `ip.rx_csum_bad` per dropped packet." When a new offload path or close-state path is added, the author increments the counter "to be safe" and the existing increment is never noticed.

**Recommendation:** Encode counter ownership into the type system: each `L3Drop::CsumBad` / `TcpParseError::Csum` variant should carry a `CounterCharge` token that is consumed exactly once by the dispatcher. Pair with **Pattern P3**'s real-path counter test framework so the counter math is asserted end-to-end.

**Findings cross-referenced:**
- Part 1 BLOCK-A11 #3, STAGE-2 (`ip.rx_drop_short`, `tcp.rx_syn_ack`, OtherDropped|Malformed collapse)
- Part 2 BLOCK-A11 B3, STAGE-2 S4
- Part 3 STAGE-2 S1, S2, S3 (void-return retransmit primitive)
- Part 5 BLOCK-A11 B-1
- Part 7 BLOCK-A11 #2
- Part 9 BLOCK-A11 B-3

---

### Pattern P5 — C-ABI dead/unread fields + spec-vs-header drift
**Recurrence:** Parts 1, 3, 4, 5, 8
**Description:** The C ABI struct grows fields that are documented but unread, defaulted to 0 by `dpdk_net_engine_create`, or whose default substitution order is wrong. Each phase silently adds another knob without an audit pass.

- Part 1 BLOCK-A11 #1: `tcp_min_rto_ms`, `tcp_timestamps`, `tcp_sack`, `tcp_ecn` declared in C ABI but never read by `dpdk_net_engine_create`. SYN options hard-coded.
- Part 3 BLOCK-A11 B1: `tcp_min_rto_us` / `tcp_initial_rto_us` / `tcp_max_rto_us` validation only `debug_assert!`; release builds reach `u32::clamp(min, max)` panic.
- Part 3 BLOCK-A11 B2: `event_queue_soft_cap` documented default 4096 but bound check runs BEFORE default substitution; zero-init C config fails with NULL.
- Part 4 BLOCK-A11 #3: ffi-test `Cfg` mirror missing `rx_mempool_size` tail field — OOB read under `DPDK_NET_TEST_TAP=1`.
- Part 4 STAGE-2: no ABI size/version guard around `dpdk_net_engine_config_t`; `_pad` strategy inconsistent across counter structs.
- Part 4 HIGH (Claude): `tcp_min_rto_ms` C-ABI field still vestigial after `_us` cousins added; cpp-consumer still sets it, silently ignored.
- Part 4 HIGH (Claude): `dpdk_net_poll` accepts `_timeout_ns` (underscore-prefixed) — spec says `timeout_ns`; cbindgen passes underscore through to C signature.
- Part 4 / Part 8 S2-8: `rx_mempool_size` formula doc-comment drift (2× vs 4×) on `crates/dpdk-net/src/api.rs:67`.
- Part 5 STAGE-2 S-6: A-HW+ phase review claims `ena_large_llq_hdr` + `ena_miss_txc_to_sec` are at the end of EngineConfig; A6.6-7 T10 appended `rx_mempool_size` after — review wording stale.
- Part 8 S2-9 / S2-10: `EngineConfig.tx_data_mempool_size` and `Engine::diag_input_drops` are Rust-only; not exposed in C ABI.

**Root cause:** No single audit gate on the C ABI between phases. Each phase adds one field; no per-phase pass enumerates "every field declared, every field read, every default substitution." The compile-time `size_of` assertion at `api.rs:518` only catches total-size drift, not per-field semantics.

**Recommendation:** Add an A11-block C-ABI audit script: walk `crates/dpdk-net/src/api.rs` field-by-field, assert each field is read by `dpdk_net_engine_create` (or has an explicit `#[allow(dead_code)] // documented-deprecated`), and assert default-substitution order is `0 → default → bound check`. One mechanical script, run in CI, ends this entire pattern class. Pair with an explicit `_reserved_for_rust_only_forensics` pad strategy mirroring `_pad: [u64; N]` from existing counter structs.

**Findings cross-referenced:**
- Part 1 BLOCK-A11 #1
- Part 3 BLOCK-A11 B1, B2
- Part 4 BLOCK-A11 #3, STAGE-2 ABI version guard, vestigial `tcp_min_rto_ms`, `_timeout_ns`
- Part 5 STAGE-2 S-6
- Part 8 S2-8, S2-9, S2-10

---

### Pattern P6 — Panic-across-FFI / unwrap on caller-supplied input
**Recurrence:** Parts 1, 3, 9
**Description:** Public entry points or hot config-validation paths use `unwrap()` / `debug_assert!` / unchecked indexing on caller-supplied input. Spec §3 forbids panic across `extern "C"`; this rule keeps drifting.

- Part 1 BLOCK-A11 #2: `engine::eal_init` does `CString::new(*s).unwrap()` (`engine.rs:959`) and `EAL_INIT.lock().unwrap()` (`:933`); panic on malformed argv across FFI.
- Part 3 BLOCK-A11 B1: invalid RTO bounds reach release-build clamp panic because validation is `debug_assert!`-only.
- Part 7 STAGE-2 (Codex BUG ×2): `tools/scapy-fuzz-runner/src/main.rs:80, :88` use unchecked `frames[i]` subscripts on manifest-driven indexing; stale manifest panics the runner instead of returning `anyhow` context.
- Part 9 STAGE-2 S2-10: `--duration-override` arithmetic edge — `Instant + huge Duration` can panic.

**Root cause:** No clippy lint or grep gate on `unwrap()` / `debug_assert!` / unchecked indexing in public-API or test-runner entry surfaces.

**Recommendation:** Add a clippy `unwrap_used` deny on `dpdk-net/src/lib.rs` (the C-ABI surface) and on `tools/*/src/main.rs` (the runner entry points). Replace every `debug_assert!` on caller-supplied numeric input in `Engine::new` / `*::new` constructors with a release-build error return.

**Findings cross-referenced:**
- Part 1 BLOCK-A11 #2
- Part 3 BLOCK-A11 B1
- Part 7 STAGE-2 (scapy-fuzz-runner unchecked indexing)
- Part 9 STAGE-2 S2-10

---

### Pattern P7 — Documentation drift: spec/header/code triplet diverges silently
**Recurrence:** Parts 1, 2, 3, 4, 5, 6, 8, 9 (literally every part)
**Description:** Three sources of truth — design specs in `docs/superpowers/specs/`, generated header `include/dpdk_net.h`, and Rust code — drift apart phase by phase. Symptoms recur:

- Stale crate names (`resd-net-core` vs `dpdk-net-core`): Parts 1, 2.
- Stale phase markers (Part 5 S-6 "tail-append" claim invalidated by later phase).
- Stale comments referencing "Stage A2, only" or "Task 5/6 stubs" (Parts 7, 8 S2-21).
- Spec docs missing entirely from `docs/superpowers/specs/` (Part 5 S-7: A-HW+ spec absent; Part 2 X1: A3/A4 specs only in `plans/`).
- Spec deviation paragraphs duplicated verbatim across two cbindgen-generated headers (Part 6 S2-5).
- Hardcoded historical thresholds (Part 4: `mbuf_refcnt_drop_unexpected` doc-comment "32 handles" vs actual `MBUF_DROP_UNEXPECTED_THRESHOLD = 8`).
- Bench-nightly comment lies (Part 8 B-A11-6: `BENCH_ITERATIONS=5000` workaround default outlives the `f3139f6` cliff fix).

**Root cause:** Documentation is per-phase additive; no end-of-phase doc-sweep gate. The cbindgen drift-check script (commit `c069421`) only diffs regenerated bytes; missing-from-include type silently drops.

**Recommendation:** Add a doc-drift gate that runs at end-of-phase: for every public symbol in `include/dpdk_net.h`, assert (a) it is mentioned in a `specs/` doc, (b) the spec reference matches the current phase tag, and (c) any `Stage A<N>` / `Task <M>` reference in code comments is either current or carries a `[Resolved-by:]` pointer. Mechanical; one Python script.

**Findings cross-referenced:** Too many to enumerate exhaustively; representative items:
- Part 1 STAGE-2 (multiple): `eal.rs` / `l2_eth.rs` brief drift, `arp.rs` module doc drift, A2 mTCP review path drift.
- Part 2 F1, F2, X1.
- Part 3 D2 / CO7 / CO8 / CX2.
- Part 4 AGREED-FYI (`rx_mempool_size` 2× vs 4×), Claude MEDIUM (`mbuf_refcnt_drop_unexpected` threshold).
- Part 5 S-6, S-7, S-12, S-13, S-18.
- Part 6 S2-5, S2-19.
- Part 8 S2-21, S2-22, S2-23, B-A11-6.
- Part 9 CL-HIGH-1 (stale `Engine::state_of` comment), CL-HIGH-2 (`test-server` umbrella feature naming), AF-4 (hardcoded `dev ens6`).

---

### Pattern P8 — `#[allow(dead_code)]` / `#[allow(clippy::too_many_arguments)]` accretion
**Recurrence:** Parts 1, 2, 3, 4, 5, 6, 9
**Description:** Phase-by-phase accumulation of `allow` attributes. Each phase legitimately needs a temporary allow; no phase removes the previous phase's. Result: dead helpers, unread fields, oversized signatures.

- Part 1: `tx_data_frame` (`#[allow(dead_code)]` 5+ phases on, 60+ vestigial lines); `applied_rx_offloads` / `applied_tx_offloads`; `rx_drop_nomem_prev`; `#[allow(unused_variables)]` on `ip_decode_offload_aware` params.
- Part 2 S11: 4 `#[allow(clippy::too_many_arguments)]` sites argue for a `TcpConnConfig` carrier struct.
- Part 3 CO5 / CO9: `tcp_timer_wheel.rs` module-level `#![allow(dead_code)]` stale; `tcp_tlp.rs` `Default::default()` test-only but not `#[cfg(test)]`-gated.
- Part 4 STAGE-2: `#![allow(clippy::missing_safety_doc)]` mutes 13+ `unsafe extern "C" fn` Safety sections.
- Part 5 S-11, S-13: `Engine.driver_name` `#[allow(dead_code)]`; `and_offload_with_miss_counter` stale-justification allow.
- Part 6 CO-C6: redundant inner `#[cfg(feature = "test-server")]` on `conn_peer_mss`.
- Part 7: `tools/scapy-fuzz-runner/src/main.rs:34` `#[allow(dead_code)] flags`.
- Part 9 CL-MEDIUM-3: `#[allow(clippy::useless_conversion)]` on Stage-2 newtype migration intent.

**Root cause:** No grep-gate on `#[allow]` introductions during per-task review.

**Recommendation:** Each `#[allow(...)]` introduced must carry a comment-line `// REMOVE-BY: A<phase>` and CI fails when reaching that phase if the allow still exists.

**Findings cross-referenced:** See list above; same theme across parts.

---

### Pattern P9 — Test-pyramid coverage gaps with documented-but-untested invariants
**Recurrence:** Parts 1, 2, 3, 4, 5, 6, 7, 8, 9
**Description:** Each phase adds invariants to the spec and lands the implementation, but the test-pyramid coverage stops at unit-level + a single integration smoke. The pyramid's middle (real-path counter coverage, FSM-trajectory coverage, offload-on/off matrix coverage, ENOMEM injection) is missing.

- Part 1: TAP-gated dispatcher tests; no default-build counter-coverage for ARP/ICMP/L3 wiring.
- Part 2 S9: no matched-RST length test, no TIME_WAIT stale-seq filtering test, no close-state PAWS test.
- Part 3 CX1: ignored synthetic-peer TAP scenarios don't force ENOMEM around `on_tlp_fire`/RACK fire — the void-return accounting bug invisible without injection.
- Part 4 STAGE-2: `EventQueue` overflow not unit-tested; multi-seg zero-copy test bypasses real RX parser path.
- Part 5 S-3 / S-4 / S-5: `bench-offload-ab` is compile-time matrix only — runtime latch fallback never measured; `ahw_smoke.rs` Task 16 SW-fallback is the only behavioral test for the runtime latch; no offload-on behavioral test on net_tap.
- Part 6 S2-10 / S2-11 / S2-14: tcpreq probe tests are pass-only and short; `state_trans[11][11]` fires only 5 of 121 cells; no disconnect-mid-run timer-cancel test.
- Part 7 STAGE-2: `tcp_options` and `tcp_state_fsm` fuzz targets are no-panic only; `engine_inject` fuzz target is TAP-gated no-op without env var; spec §6 invariant #3 (recv-window monotonicity) unasserted in any A9 proptest.
- Part 8 B-A11-7: pressure-correctness layer between unit and benchmark is missing — every T17 bug surfaced via AWS bench-pair, never `cargo test`.
- Part 9 S2-4 / S2-5 / S2-6: Layer-H covers only netem-stress correctness; `rx_mempool_avail` floor never approached at smoke intensity; offload-OFF arm of row 14 never built/run.

**Root cause:** Each per-phase reviewer prompt evaluates "tests for this phase added" but not "tests that exercise the invariant in concert with prior phases under stress." The pyramid is unit + benchmark; the middle layer that catches T17-class bugs and counter-placement bugs (Pattern P4) is not staffed.

**Recommendation:** Promote the **pressure-correctness layer** (Part 8 B-A11-7) to a Stage-1-finalization deliverable. Concrete shape: a single `crates/dpdk-net-core/tests/pressure_correctness.rs` driver that runs all 17 layer-H scenarios at 100k-conn-per-second intensity, asserts cross-stack counter consistency, and runs the offload-on/off matrix. This is the only realistic way to catch counter-placement bugs that escape per-phase reviewers.

**Findings cross-referenced:** Spans every part; Part 8 B-A11-7 is the most explicit articulation.

---

### Pattern P10 — Cross-phase architectural drift unflagged by per-phase reviewers
**Recurrence:** Parts 1, 2, 3, 4, 5, 6
**Description:** Helper functions, accessor patterns, and module organisation drift across phases without a single moment of architectural review. Per-phase reviewers see only the diff; the cumulative shape never gets a top-down read.

- Part 1: `Engine::new` is 420+ lines (phase-by-phase accumulation); `our_mac` / `gateway_mac()` / `gateway_ip()` accessors use three different patterns.
- Part 2 S6 / S7: `engine.rs` god-object 2104→8141 LOC, 142 methods, ~50 fields; `tcp_input::handle_established` is 770 lines mixing six RFC references.
- Part 2 C1: `tcp_conn.rs` pub-field count 28→68 (4.2× LOC growth).
- Part 3 S7: RACK reaches into `tcp_retrans::RetransEntry` directly (layering inversion).
- Part 4: `Engine::pump_tx_drain` and `Engine::pump_timers` are top-level `pub` but test-only-effective.
- Part 5 D-1 / Codex: A-HW behavior is NOT isolated in an `offloads.rs` module; TX cksum is in `tcp_output.rs`, RX is split between `l3_ip.rs` and `engine.rs`, RSS in `flow_table.rs`, latches in `engine.rs`.
- Part 6 S2-3: two diverging `inject_rx_frame` implementations; S2-13: two separate `ENGINE_SERIALIZE` mutexes.

**Root cause:** Per-phase reviewers are scoped to "did this phase's diff break invariants." Accumulated god-object growth requires a separate cumulative pass. The cross-phase retro IS that pass — but its findings need a feedback loop into Stage-2 refactor budget.

**Recommendation:** Reserve a Stage-2 "architectural-drift cleanup" milestone: extract `tcp_dispatch.rs`, `engine_lifecycle.rs`, `tx_path.rs`, `offloads.rs`. The cleanup is mechanical now (no behavior change) and dramatically cheaper than after Stage-2 multi-thread work.

**Findings cross-referenced:** See list above.

---

### Pattern P11 — Drop-path / mempool-lifetime invariants under-reviewed
**Recurrence:** Parts 3, 4, 7
**Description:** PR #9-class bugs (mbuf double-free / leak on engine drop, retrans rollback, fault-injector chain) are forensically dense and the per-phase reviewers under-weight them. Each surfaces only via ASAN-augmented soak tests that are not part of default CI.

- Part 3 D2: `Drop for Engine` Step 1 comment misrepresents `snd_retrans` mbuf reclamation (Claude flags as misleading; Codex confirms no functional bug).
- Part 4: PR #9 `MbufHandle::Drop` leak fix intact at HEAD (both reviewers explicitly verify).
- Part 4 BLOCK-A11 #1: READABLE event lifetime invalidated across queued polls — a related but distinct lifetime bug.
- Part 7 (cross-reference): Post-A9 UAF fixed in `a0f8f96`. Class-of-bug recurrence: in-tree subagent reviewers under-weight engine-drop/mempool-lifetime invariants; `fault_injector_chain_uaf_smoke.rs` passes silently in `cargo test` without ASAN.

**Root cause:** ASAN is in spec §6 line 372 as a "should-have" but not in default CI. PR #9 was a human-found bug; A9 UAF was a human-found bug.

**Recommendation:** Promote ASAN to a CI matrix axis (Part 7 STAGE-2 explicit). One green-CI run/day under ASAN catches every mempool-lifetime bug class mechanically.

**Findings cross-referenced:**
- Part 3 D2
- Part 4 BLOCK-A11 #1, AGREED-FYI (PR #9 status)
- Part 7 (post-A9 UAF cross-reference)

---

### Pattern P12 — Counter-load Relaxed atomics + ARM-portability deferrals
**Recurrence:** Parts 1, 2, 3, 4, 5, 6, 7, 8, 9
**Description:** Every part confirms `Ordering::Relaxed` is correct under single-lcore engine invariant. Every part also flags ARM-portability / multi-engine future drift. No part takes action.

- Part 1: `compile_error!` x86-only on `clock.rs:39` — whole crate fails to compile on aarch64.
- Part 3 CO6 / CO10: `#[repr(C, align(64))]` hardcodes 64-byte cache line; ARM Neoverse-N1 = 64 B, ThunderX2 / Apple-Silicon = 128 B.
- Part 5 F-1, F-2: explicit AGREED-FYI that single-threaded construction makes `bool` latches sound.
- All parts: `Relaxed` defensible today, flagged for Stage-2 multi-thread.

**Root cause:** Stage-1 is x86_64-only by current scope; ARM unblock is Stage-2.

**Recommendation:** No action for A11 unblock. Bundle into Stage-2 ARM-port milestone. The findings ARE real and ARE consistent across parts — this is a healthy "deferred-by-design" cluster, not drift.

**Findings cross-referenced:** Every part has at least one ARM-deferred FYI.

---

## Single-instance findings worth flagging at meta-level

These appear in only one part but have cross-cutting structural implications:

### M1 — Pressure-correctness layer (Part 8 B-A11-7)
The single most important meta-finding. Connects to Patterns P3, P4, P9. Foundational for Stage-2; explicitly named by Claude as connecting to user task #11. Should be addressed before A11 perf cherry-picks compound the gap.

### M2 — F-Stack comparator runs without `ff_init` (Part 8 B-A11-1)
Pure mechanical FFI precondition violation that the per-phase A10 review missed. Highlights that subagent-produced ~1100 LOC C glue (Part 8 C-21) has different review-rigor needs than Rust core code.

### M3 — Workspace feature unification CI gate absence (Part 6 S2-2)
Solves Pattern P1 mechanically. A 5-line script. Belongs in A11 unblock work because it ends an entire bug class.

### M4 — `Outcome` populate-side has no compile-time guard (Part 2 S5)
Underlies Part 2 BLOCK-A11 B3 (close-path bypass). A builder-pattern / `Required` marker per Outcome field would surface counter-coverage gaps at typecheck time. Would have caught Pattern P4.

### M5 — `retransmit()` returns `()` (Part 3 S3)
Underlies Part 3 STAGE-2 S1 + S2. Single repair (`RetransmitOutcome` enum) closes both BUG-class findings. The void primitive is a textbook example of "API shape begets observability bugs."

### M6 — Layer H build script not feature-gated (Part 9 B-1)
Same root cause as Part 1 cbindgen drift-check (`crates/dpdk-net/cbindgen.toml` whitelist coverage assertion missing): a tooling pass exists but doesn't validate post-conditions. Belongs in M3's CI gate.

---

## Recommended A11 action plan

Group BLOCK-A11 items by theme. Suggested ordering:

### Theme A — C-ABI hardening (Pattern P5 + P6)
*Parallel-safe; mechanical.*
1. Part 1 BLOCK-A11 #1 — Wire or delete dead C-ABI fields (`tcp_min_rto_ms`, `tcp_timestamps`, `tcp_sack`, `tcp_ecn`); drop vestigial `tcp_min_rto_ms` (Part 4 HIGH).
2. Part 1 BLOCK-A11 #2 — Convert `unwrap()` panics in `eal_init` to error returns.
3. Part 3 BLOCK-A11 B1 — Validate RTO bounds at `Engine::new` (release-build).
4. Part 3 BLOCK-A11 B2 — Substitute `event_queue_soft_cap == 0 → 4096` before bound check.
5. Part 4 BLOCK-A11 #3 — Sync ffi-test `Cfg` mirror with `rx_mempool_size` (or replace with cbindgen-generated bindings).
6. **Add CI gate (M3 / Pattern P5 recommendation):** `cargo metadata` assertion that production resolves `dpdk-net-core` with default features only; field-by-field audit script over `api.rs`.

### Theme B — Counter-correctness (Pattern P4)
*Sequential within theme; landed alongside test-coverage backfill (Theme E).*
1. Part 1 BLOCK-A11 #3 / Part 5 BLOCK-A11 B-1 — Fix `ip.rx_csum_bad` double-bump (single repair, two parts cross-reference).
2. Part 2 BLOCK-A11 B1 — Matched-flow RST ACK arithmetic include SYN/FIN sequence length.
3. Part 2 BLOCK-A11 B2 — TIME_WAIT refresh gated on in-window segments only.
4. Part 2 BLOCK-A11 B3 — Close-path PAWS / SACK / Outcome population.
5. Part 7 BLOCK-A11 #2 — Move `corrupts` counter inside `if data_len > 0` guard.
6. Part 9 BLOCK-A11 B-3 — Add `tcp.rx_bad_csum` to row-14 corruption disjunction.

### Theme C — Lifetime / RX-path correctness (Pattern P11)
1. Part 4 BLOCK-A11 #1 — READABLE event lifetime across queued polls (move `poll_once` to be conditional on `events_out` slot availability, OR copy iovec data into the InternalEvent at population time).
2. Part 4 BLOCK-A11 #2 — Multi-seg RX L3 chain walker (use `pkt_len` not head segment `data_len`).
3. Part 8 S2-7 — Same as Part 4 BLOCK-A11 #2 cross-reference; one fix.

### Theme D — Bench/comparator harness (Part 8 cluster)
*Independent-parallel.*
1. Part 8 B-A11-1 — Wire `ff_init` for F-Stack comparator OR remove F-Stack from nightly.
2. Part 8 B-A11-2 — F-Stack burst errno discrimination.
3. Part 8 B-A11-3 — Default-on `fstack` feature OR drop `--stacks fstack` from nightly.
4. Part 8 B-A11-4 — maxtp emit CSV marker rows on every failure path.
5. Part 8 B-A11-5 — `bench-stress` CSV merge handle missing first scenario.
6. Part 8 B-A11-6 — Either raise `BENCH_ITERATIONS` default or rewrite comment.
7. Part 9 BLOCK-A11 B-1 — Add `--features test-server` to layer-h wrapper scripts.
8. Part 9 BLOCK-A11 B-2 — Reorder Layer H `observe_batch` BEFORE `bench-e2e::run_rtt_workload` event drain.

### Theme E — Pressure-correctness layer (Pattern P3 + P9 + M1)
**Foundational; do this in parallel with Themes A–D so it lands before A11 perf cherry-picks.**
1. Part 8 B-A11-7 — Build the pressure-correctness layer. Concrete first step: `crates/dpdk-net-core/tests/pressure_correctness.rs` covering layer-H scenarios at 100k-conn-per-second.
2. Promote ASAN to CI matrix axis (Pattern P11 recommendation).

### Parallelisation strategy
- Themes A, B, C, D, E are largely independent. A single agent can take Theme A; another Theme D; the counter-correctness fixes (Theme B) should land alongside Theme E pressure tests so each fix is verified at intensity.
- Total parallel cost: estimate 3–5 agent-days under `dispatching-parallel-agents` discipline.

---

## Stage-2 follow-up index

Flat list of every STAGE-2 finding with part-number, finding-id, and primary file:line citation. Operator uses this as the running index.

### Part 1 (A1 + A2)
- 1-S1 — `compile_error!` non-x86_64 in `crates/dpdk-net-core/src/clock.rs:39`
- 1-S2 — `maybe_emit_gratuitous_arp` not migrated to timer wheel — `crates/dpdk-net-core/src/engine.rs:6238-6255`
- 1-S3 — GARP / gateway-probe `last_send` updated on failed TX — `engine.rs:6248-6254, :6525-6537`
- 1-S4 — `EngineConfig.mbuf_data_room` overflow on Rust-direct callers — `engine.rs:1205, :1257`
- 1-S5 — Gateway-MAC unicast ARP-REQUEST probe path not in spec — `engine.rs:6512-6538`
- 1-S6 — `tcp_initial_rto_ms` documented but field is `_us` — spec §4 line 98
- 1-S7 — `ip.rx_drop_short` double-bumped on `BadTotalLen` — `engine.rs:3924-3927`
- 1-S8 — `OtherDropped | Malformed` ICMP no-op — `engine.rs:3986`
- 1-S9 — No default-build NIC-BAD checksum counter test
- 1-S10 — TAP integration test gated behind env; no default-build dispatcher
- 1-S11 — `ip.rx_drop_unsupported_proto` only TAP-test covered
- 1-S12 — `tx_data_frame` `#[allow(dead_code)]` 5+ phases on
- 1-S13 — `PortConfigOutcome::applied_*_offloads` `#[allow(dead_code)]`
- 1-S14 — `rx_drop_nomem_prev` accessor `#[allow(dead_code)]`
- 1-S15 — Stale `#[allow(unused_variables)]` on `ip_decode_offload_aware`
- 1-S16 — `/proc/net/arp` parser accepts extra octets — `arp.rs:268-273`
- 1-S17 — `tx_ring_size` zero/undersized — `engine.rs:382, :5486-5494, :6202-6207`
- 1-S18 — `l2_decode` accepts broadcast before ethertype — `l2.rs:38-47`
- 1-S19 — `arp.rs` module doc says static-only but `classify_arp` is partial dynamic — `arp.rs:127-139`
- 1-S20 — Cross-phase brief lists `eal.rs` / `l2_eth.rs` that don't exist
- 1-S21 — ICMP parser comment vs guard mismatch — `icmp.rs:70-74`
- 1-S22 — `Engine::new` is 420+ lines
- 1-S23 — `THREAD_COUNTERS_PTR` not set in `EngineNoEalHarness` — `mempool.rs:21-24`
- 1-S24 — `siphash_4tuple` truncates to u32 — `flow_table.rs:43-61`
- 1-S25 — `our_mac` / `gateway_mac()` / `gateway_ip()` use 3 different patterns
- 1-S26 — `EthCounters.rx_drop_miss_mac` vs spec "MissMac" casing
- 1-S27 — `dpdk_net_eal_init` drops DPDK errno, returns `-libc::EAGAIN`

### Part 2 (A3 + A4)
- 2-S1 — A4 SACK state mutated before ACK validated — `tcp_input.rs:876-892`
- 2-S2 — `handle_inbound_syn_listen` discards `parsed_opts.ws_clamped` — `engine.rs:4106-4107`
- 2-S3 — `TcpConn::new_passive` records `ws_shift_out=0` while SYN-ACK advertises non-zero — `tcp_conn.rs:497-544`
- 2-S4 — `tcp.rx_syn_ack` bumped on SYN-only segment — `engine.rs:4105`
- 2-S5 — `Outcome` populate-side has no compile-time guard — `engine.rs:868-927`
- 2-S6 — `engine.rs` god-object 2104→8141 LOC
- 2-S7 — `tcp_input::handle_established` 770 lines mixes 6 RFCs
- 2-S8 — `SendRetrans::entries` exposed `pub` — `tcp_retrans.rs:46`, `tcp_input.rs:1092`
- 2-S9 — Test-pyramid gaps for B1/B2/B3
- 2-S10 — `proptest_paws.rs` tests local rule wrapper not production gate
- 2-S11 — Four `#[allow(clippy::too_many_arguments)]` sites

### Part 3 (A5 + A5.5 + A5.6)
- 3-S1 — TLP fire records probe success on void retransmit — `engine.rs:3206-3208, :3221-3225`
- 3-S2 — RACK loss accounting same void-retransmit assumption — `engine.rs:4286-4288, :4294-4299`
- 3-S3 — `retransmit()` primitive returns `()` — `engine.rs:5824-5826, :6161-6163`
- 3-S4 — RTT sampler u32-µs scheme sibling defect to bug_008 — `tcp_input.rs:912, :918, :945`
- 3-S5 — `obs.events_queue_high_water` `fetch_max` Relaxed — `tcp_events.rs:175-178`
- 3-S6 — `EventQueue::with_cap` caps `VecDeque` at 4096 — `tcp_events.rs:148`
- 3-S7 — RACK reaches into `tcp_retrans::RetransEntry` — `tcp_rack.rs:6`
- 3-S8 — `tlp_config()` `u32::MAX` sentinel in core not ABI translator — `tcp_conn.rs:551-561`, `lib.rs:919-921`
- 3-S9 — `LossCause` enum lacks `#[repr(u8)]` — `tcp_events.rs:13-25`

### Part 4 (A6 family)
- 4-S1 — `inject_rx_chain` under-reports `eth.rx_pkts` — `engine.rs:6389, :6492`
- 4-S2 — bench-ab-runner consumes `InternalEvent` directly — `tools/bench-ab-runner/src/workload.rs:44-47`
- 4-S3 — layer-h-correctness bypasses public ABI — `tools/layer-h-correctness/src/observation.rs:16, :167-170`
- 4-S4 — `Engine::events()` leaks `RefMut<EventQueue>` — `engine.rs:2483`
- 4-S5 — `Engine::pump_tx_drain`/`pump_timers` test-only-effective — `engine.rs:7097, :7111`
- 4-S6 — `#![allow(clippy::missing_safety_doc)]` mutes 13+ extern fn — `crates/dpdk-net/src/lib.rs:1`
- 4-S7 — `EventQueue::with_cap` clamps at 4096 — `tcp_events.rs:147-150`
- 4-S8 — No ABI size/version guard for `dpdk_net_engine_config_t`
- 4-S9 — `dpdk_net_tcp_counters_t` lacks `_reserved_for_rust_only_forensics` pad
- 4-S10 — `EventQueue` overflow not unit-tested
- 4-S11 — Test-server intercept `Vec::with_capacity` per TX frame — `engine.rs:2159, :2252, :2321, :2763`
- 4-S12 — `dpdk_net_poll` per-event `RefMut<FlowTable>` borrow churn — `lib.rs:509-575`
- 4-S13 — `dpdk_net_rx_mempool_size` getter inlines own raw cast — `lib.rs:632-645`

### Part 5 (A-HW + A-HW+)
- 5-S1 — `port_id == u16::MAX` bypass undocumented — `engine.rs:1303-1311`
- 5-S2 — A-HW+ "M1" overflow constants duplicated — `engine.rs:1678-1680`, `tests/knob-coverage.rs:580-582`
- 5-S3 — `bench-offload-ab` doesn't exercise runtime latch — `tools/bench-offload-ab/src/matrix.rs:66-115`
- 5-S4 — No integration test drives NIC-BAD frame
- 5-S5 — `ahw_smoke.rs` Task 16 only behavioral test for runtime latch
- 5-S6 — A-HW+ tail-append claim historically true
- 5-S7 — A-HW+ spec file missing from `docs/superpowers/specs/`
- 5-S8 — RX timestamp dynflag mask not range-checked — `engine.rs:1396-1404`
- 5-S9 — Half-initialized PMD on bring-up errors after `rte_eth_dev_start` — `engine.rs:1313-1440`
- 5-S10 — `tx_offload_rewrite_cksums` `pseudo_len` `wrapping_add` — `tcp_output.rs:270`
- 5-S11 — `Engine.driver_name` `#[allow(dead_code)]` — `engine.rs:814`
- 5-S12 — Stale comments at `engine.rs:778`
- 5-S13 — `and_offload_with_miss_counter` allow stale — `engine.rs:1081`
- 5-S14 — `dpdk_net_recommended_ena_devargs` lacks platform doc note
- 5-S15 — `xstat_map` private; no resolution-status accessor
- 5-S16 — RX timestamp dynfield steady-state not in counter snapshot
- 5-S17 — `EthCounters._pad` near boundary; document next-bump
- 5-S18 — Spec §6.3 "UDP analog" unresolved
- 5-S19 — `Cargo.toml` RSS feature comment promises consume — `Cargo.toml:48-50`
- 5-S20 — `dpdk_net_recommended_ena_devargs` lacks typed error enum
- 5-S21 — Real-ENA wire-drive correctness test deferred from A-HW Task 18 to A10

### Part 6 (A7 + A8 + A8.5)
- 6-S2-1 — `scapy-fuzz-runner` test-inject Cargo.toml leak — `Cargo.toml:8`
- 6-S2-2 — Missing CI metadata gate for never-default features
- 6-S2-3 — Two diverging `inject_rx_frame` impls — `engine.rs:6302, :6692`
- 6-S2-4 — `wall_timeout` parameter dead code — `tools/packetdrill-shim-runner/src/invoker.rs:18-50`
- 6-S2-5 — `dpdk_net_shutdown` declared in two headers — `dpdk_net.h:772`, `dpdk_net_test.h:108`
- 6-S2-6 — `port_id == u16::MAX` sentinel undocumented — `engine.rs:1299-1311`
- 6-S2-7 — Shim-runner non-optional dev-dep — `Cargo.toml:28`
- 6-S2-8 — A8.5 T9 soak test not continuously verified — `corpus_ligurio.rs:112`
- 6-S2-9 — Counter coverage uses `bump_counter_one_shot` for HW-only
- 6-S2-10 — tcpreq probe tests pass-only and short
- 6-S2-11 — `state_trans[11][11]` covers single FSM trajectory — `obs_smoke.rs:152-196`
- 6-S2-12 — Duplicated `test_eal_args` / `test_server_config`
- 6-S2-13 — Two `ENGINE_SERIALIZE` mutexes
- 6-S2-14 — Disconnect-mid-run timer-cancel coverage gap
- 6-S2-15 — `parse_tcp_seq_ack` stricter preconditions — `test_server.rs:226-231`
- 6-S2-16 — Fault-injector chain UAF detection sanitizer-dependent
- 6-S2-17 — Hidden frame-layout coupling — `tcpreq-runner/src/lib.rs:34`
- 6-S2-18 — External packetdrill binary via `build.sh` from `build.rs`
- 6-S2-19 — Audit A9/A10/A10.5 counters in `EXPECTED_COUNTERS`

### Part 7 (A9)
- 7-S1 — `pub mod fault_injector;` exposes `pub struct FaultInjector` — `lib.rs:13`, `fault_injector.rs:172`
- 7-S2 — `FaultInjectorCounters` always-present contradicts spec §5.2 — `counters.rs:773-786`
- 7-S3 — `FaultInjector::process` ordering collapses dup-then-reorder — `fault_injector.rs:279-336`
- 7-S4 — Spec §6 invariant #3 (recv-window monotonicity) unasserted
- 7-S5 — `tools/scapy-fuzz-runner/src/main.rs:80, :88` unchecked indexing
- 7-S6 — `fuzz_targets/tcp_reassembly.rs:73` insert without pre-bumped refcount
- 7-S7 — `fuzz_targets/tcp_reassembly.rs:40` fake mbuf alignment violation
- 7-S8 — `tcp_options` and `tcp_state_fsm` fuzz no-panic only
- 7-S9 — `engine_inject` fuzz TAP-gated no-op
- 7-S10 — `engine_inject.rs:97` discards `inject_rx_frame` errors
- 7-S11 — `fault_injector_smoke.rs:30` lacks zero-length corrupt assertion
- 7-S12 — `proptest_paws.rs` and `proptest_rack_xmit_ts.rs` 1-line local mirrors
- 7-S13 — Post-A9 UAF class-of-bug recurrence; ASAN as CI axis
- 7-S14 — No counters for reorder-ring eviction / FaultInjector errors
- 7-S15 — `dispatch_one_rx_mbuf` reads `self.fault_injector` via RefCell on RX hot — `engine.rs:3724`
- 7-S16 — `fault_injector.rs:244, :260` corruption bounded to head segment
- 7-S17 — `fault_injector.rs:268` corruption byte XOR no protocol classification
- 7-S18 — Spec §4.2 reorder ring depth 4 vs hardcoded 16 — `fault_injector.rs:177`
- 7-S19 — `fault_injector.rs:31-34` doc-block Task-5/Task-6 stubs stale
- 7-S20 — `fuzz_targets/tcp_reassembly.rs:20` safety note stale

### Part 8 (A10)
- 8-S2-1 — T17 TX data-mempool divisor uses `mbuf_data_room` not MSS — `engine.rs:1231, :1238`
- 8-S2-2 — F-Stack maxtp `ff_write < 0` collapses to backoff — `fstack_maxtp.rs:215`
- 8-S2-3 — `pub mod` everywhere in `dpdk-net-core` — `lib.rs:4-55`
- 8-S2-4 — `bench-internals` cargo feature legitimises leak — `Cargo.toml:105-107`
- 8-S2-5 — `test-server` workspace feature unification trap
- 8-S2-6 — `pub fn diag_*` accessor accretion on `Engine`
- 8-S2-7 — Multi-seg RX L3 invariant gap persists — `lib.rs:74-78`, `l3_ip.rs:86`
- 8-S2-8 — C ABI doc `rx_mempool_size` 2× vs 4× — `api.rs:67`
- 8-S2-9 — `EngineConfig.tx_data_mempool_size` not in C ABI — `lib.rs:213-218`
- 8-S2-10 — `Engine::diag_input_drops` no C ABI mirror
- 8-S2-11 — `tcp.tx_data_mempool_avail` bypasses `ALL_COUNTER_NAMES` — `counters.rs:304`
- 8-S2-12 — Maxtp TX data-mempool override pinned 32768 — `main.rs:1716`
- 8-S2-13 — Stress harness shell↔Rust scenario duplication — `bench-nightly.sh:549`
- 8-S2-14 — FaultInjector p999-ratio limits informational only — `bench-stress/src/main.rs:220, :333`
- 8-S2-15 — Default `tx_payload_bytes` cross-check feature-gated — `bench-vs-mtcp/src/main.rs:628, :916`
- 8-S2-16 — Engine `events()` returns `RefMut<EventQueue>` — `engine.rs:2483`
- 8-S2-17 — T17 `close_persistent_connections` 5s deadline stderr-only — `dpdk_maxtp.rs`
- 8-S2-18 — T22 mTCP driver JSON-stderr-only error reporting
- 8-S2-19 — Release-build warnings on `mod a10_diagnostic_counter_tests` — `counters.rs:1060`
- 8-S2-20 — Post-phase perf cherry-picks not all linked to review summaries
- 8-S2-21 — `mbuf_data_slice` "Stage A2, only" stale comment — `lib.rs:66-67`
- 8-S2-22 — Spec docs lack Closure/Resolved-by pointers
- 8-S2-23 — T18.1 DPDK 20.11 sidecar undocumented; no `bench-vs-mtcp/README.md`

### Part 9 (A10.5)
- 9-S2-1 — `scapy-fuzz-runner` same `dpdk-net-core` non-optional pattern — cross-ref to 6-S2-1
- 9-S2-2 — No CI metadata gate on workspace feature unification — cross-ref to 6-S2-2
- 9-S2-3 — Stale `DPDK_NET_FAULT_INJECTOR` env contaminates pure-netem — `main.rs:185, :187`
- 9-S2-4 — A10.5 covers only netem-stress correctness; max-throughput gap
- 9-S2-5 — `rx_mempool_avail` side-check always-pass at smoke intensity
- 9-S2-6 — Disjunction offload-on-only in practice; offload-OFF arm never run
- 9-S2-7 — `EventKind::Other` fallthrough loses event timestamps — `observation.rs:240-249`
- 9-S2-8 — Layer-h consumes `InternalEvent` directly — `observation.rs:16, :121, :167`
- 9-S2-9 — Bring-up boilerplate ~150 LOC duplicated across `bench-nightly.sh` + layer-h wrappers
- 9-S2-10 — `--duration-override` `Instant + huge Duration` panic — `main.rs:104, :209`

---

## Confidence notes (reviewer disagreement → meta-routing)

Five disputed items across all 9 parts:

| Part | ID | Codex | Claude | Meta-routing |
|------|-----|-------|--------|--------------|
| 2 | D1 | BUG (close-path PAWS) | SMELL (Outcome population) | Promoted to BLOCK-A11 B3 (preserve BUG severity per "do not soften" rule). Both reviewers describe the same control-flow miss. |
| 3 | D2 | No defect (functional) | Documentation drift | Routed to STAGE-2 doc fix (CO7). Both agree no functional issue. |
| 5 | D-1 | SMELL (dispersion / reviewability) | Frames as legible | Routed to STAGE-2. Codex's reviewability concern real; not runtime behavior. |
| 8 | D-1 | (PR #9 confirmed safe) | (PR #9 confirmed safe) | Not actually disputed; both agree. Listed for traceability. |
| 8 | D-2 | LIKELY-BUG (T17 mempool divisor MSS vs data-room) | Architectural-only, not a bug | Routed to STAGE-2 8-S2-1. Codex arithmetic plausible; bench passes today; verify before next grid expansion. |

In every case the disputed routing favored the reviewer with the more concrete file:line evidence. No "BUG" was downgraded to SMELL; one SMELL was upgraded to BLOCK-A11 (Part 2 D1) per project policy.

---

## What the per-phase mTCP / RFC reviews did NOT catch

The following classes of finding ONLY surfaced via this cross-phase retro pass — they would not have been caught by the per-phase mTCP-comparison-reviewer or rfc-compliance-reviewer subagents:

1. **Counter-placement bugs that survive multiple phases** (Pattern P4). The NIC-BAD `ip.rx_csum_bad` double-bump originated in A-HW Task 8 (`e2aae95`) and survived A-HW+, A6, A7, A8, A8.5, A9, A10, A10.5. RFC review checks correctness against an RFC; mTCP review checks behaviour against mTCP. Neither asks "is this counter incremented by exactly one site per event?" — that requires reading the *intersection* of the offload-on path and the offload-off path, which only exists across phases.

2. **Workspace feature unification leaks** (Pattern P1). The leak is invisible from inside any single crate; it manifests only when `cargo build --workspace --release` is observed from the outside. Per-phase reviewers scope to crate/file diffs.

3. **Engine internals leaking past C ABI to bench tools** (Pattern P2). Each new tool in isolation looks fine. The pattern is visible only when 5 tools are listed side-by-side and the C ABI surface is held against `bench-internals` cargo feature legitimacy.

4. **Pressure-correctness layer absence** (M1 / Pattern P9). Per-phase reviewers see "tests pass for this phase." Pressure-correctness is a CROSS-PHASE invariant: each phase passes its own tests, but the system collapses under sustained-throughput at the phase intersection (T17). This is the #1 argument for keeping the cross-phase retro practice in Stage 2.

5. **god-object cumulative growth** (Pattern P10). `engine.rs` 2104 → 8141 LOC happens 1000 LOC per phase; no single phase looks alarming.

6. **C-ABI dead-fields accretion** (Pattern P5). Fields declared by phase N and never wired by phase N+1, N+2, N+3 require a top-down audit pass that no per-phase reviewer schedules.

7. **`#[allow(...)]` accretion** (Pattern P8). Each phase adds one; no phase removes the prior phase's.

8. **Documentation triplet drift** (Pattern P7). Pieces of the spec/header/code triplet update at different rates per phase; only a cross-phase pass catches the cumulative divergence.

The cross-phase retro practice has paid for itself: it surfaced 24 BLOCK-A11 items and 153 STAGE-2 items, of which a substantial majority were missed by per-phase reviews. **Recommendation: keep the cross-phase retro as a hard gate at Stage-2 phase boundaries (every 3–5 phases), not just at Stage end.**
