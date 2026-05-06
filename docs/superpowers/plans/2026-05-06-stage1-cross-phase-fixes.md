# Stage 1 Cross-Phase Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to execute this plan task-by-task — one fresh subagent per task, two-stage review per project memory `feedback_per_task_review_discipline.md`. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply all 24 BLOCK-A11 fixes surfaced by the Stage 1 cross-phase retrospective review (`docs/superpowers/reviews/cross-phase-retro-summary.md`) before A11 perf cherry-picks compound the test-pyramid + counter-correctness gaps.

**Architecture:** Five parallel themes (A–E). Themes A–D land in this plan; Theme E (pressure-correctness layer) is delegated to the existing `docs/superpowers/plans/2026-05-05-pressure-test-plan.md` (T0–T18, 18 tasks). Each task = one fresh subagent dispatch (opus 4.7) with TDD discipline (write-failing-test → implement → green → commit) followed by two-stage review (spec-compliance + code-quality). Per project memory `feedback_test_timeouts.md`, every `cargo test`/`cargo bench` invocation must carry an explicit per-command timeout.

**Tech Stack:** Rust 2024 edition (stable), DPDK 23.11 (with 24.x compat), cbindgen, cargo workspaces, AMD x86_64 (ARM deferred to Stage 2 per Pattern P12).

---

## Inputs to read before executing any task

1. `docs/superpowers/reviews/cross-phase-retro-summary.md` — meta-synthesis: 12 cross-cutting patterns + Stage-2 follow-up index.
2. `docs/superpowers/reviews/cross-phase-retro-part-{1..9}-synthesis.md` — per-part synthesis with file:line citations.
3. `docs/superpowers/specs/2026-05-05-pressure-test-plan-design.md` — Theme E design (FINAL, codex-reviewed).
4. `docs/superpowers/plans/2026-05-05-pressure-test-plan.md` — Theme E implementation tasks T0–T18.
5. Per-task subagents must read the relevant Part-N synthesis and any cited per-phase mTCP/RFC review.

## Phase ordering (top-level)

| Phase | Scope | Parallel? | Gate |
|-------|-------|-----------|------|
| **A11.0** | Theme E T0–T2 (CI gate + `pressure-test` cargo feature + failure-bundle helper) **AND** Theme A6 (C-ABI audit script) | Sequential within phase, parallel across themes | CI green on master with both gates active |
| **A11.1** | Theme A items 1–5 (C-ABI hardening), Theme B items 1–6 (counter-correctness), Theme C items 1–2 (lifetime/RX) | Parallel within theme; B-block sequential (touch shared engine.rs paths) | All 24 BLOCK-A11 fixed, default `cargo test --release` green with explicit per-command 300s timeout |
| **A11.2** | Theme D items 1–8 (bench/comparator harness fixes) | Fully parallel — independent tools | bench-nightly.sh green end-to-end on master |
| **A11.3** | Theme E T3–T18 (pressure-test suites) | Per linked plan | Per-PR pressure-max-throughput + pressure-counter-parity-offload-matrix Tier-1 buckets green |
| **A11-FINAL** | Cross-phase retro re-run (gate before declaring Stage 1 complete) | n/a | Diff against current retro shows BLOCK-A11 = 0 |

A11.0 must complete before A11.1, A11.2, A11.3 can begin (every fix needs the CI gate to prevent re-introducing Pattern P1, and every counter fix wants the pressure-test infra to verify under intensity).

A11.1, A11.2, A11.3 can overlap. Aim for 5 concurrent agents at peak (one per theme).

## Execution model (subagent-driven)

For every task below the operator (or top-level orchestrator agent) does:

1. Read the task brief + cited files.
2. Dispatch ONE primary subagent (opus 4.7, `general-purpose` type) with a self-contained prompt that includes: bug citation (file:line + finding ID), the fix-shape decision, the TDD steps, the test command + timeout, the commit message template.
3. After the primary subagent reports the file written + test passing, dispatch the two reviewers IN PARALLEL:
   - `general-purpose` (opus) acting as **spec-compliance reviewer**: confirm fix matches the cited finding's intent and does not introduce regressions on cited cross-phase invariants.
   - `general-purpose` (opus) acting as **code-quality reviewer**: confirm clippy clean, no new `#[allow(...)]` without `// REMOVE-BY:` marker, no new `unwrap()` on caller input, test asserts on observable state not internals.
4. If both reviewers pass: mark task complete, commit. If either flags a blocker: dispatch a fix subagent with the reviewer's diff, then re-review.

Per project memory `feedback_subagent_model.md`, ALL subagents are opus 4.7. Use `model: opus` on every Agent dispatch.

---

## Theme A — C-ABI hardening (Patterns P5 + P6)

**Theme verdict from retro:** 6 BLOCK-A11 items, mostly mechanical, parallel-safe, foundational for FFI stability.

### Task A1 — Wire or delete dead C-ABI fields (Part 1 BLOCK-A11 #1)

**Source:** Part 1 BLOCK-A11 #1 + Part 4 HIGH (vestigial `tcp_min_rto_ms`)
**Pattern:** P5 (dead/unread C-ABI fields)
**Open operator decision:** wire the four fields OR delete them. Default: **wire `tcp_timestamps` / `tcp_sack` / `tcp_ecn` (they map to SYN options); delete `tcp_min_rto_ms` (the `_us` cousin obsoletes it).** Confirm with operator before dispatch.

**Files:**
- Modify: `crates/dpdk-net/src/api.rs` (field declarations around the `dpdk_net_engine_config_t` struct + `dpdk_net_engine_create` body)
- Modify: `crates/dpdk-net-core/src/engine.rs` (find `build_connect_syn_opts` — currently emits SACK-permitted + timestamps unconditionally)
- Modify: `crates/dpdk-net/cbindgen.toml` if needed (header regeneration)
- Regen: `include/dpdk_net.h` via `cargo run --bin cbindgen` or whatever the existing drift-check script does
- Test: `crates/dpdk-net-core/tests/cabi_field_wired.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test**

```rust
// tests/cabi_field_wired.rs
use dpdk_net_core::engine::{Engine, EngineConfig};

#[test]
fn tcp_timestamps_false_omits_ts_option() {
    let mut cfg = EngineConfig::default_for_test();
    cfg.tcp_timestamps = false;
    let opts = Engine::build_connect_syn_opts_for_test(&cfg);
    assert!(!opts.has_timestamps(), "TS option emitted despite tcp_timestamps=false");
}

#[test]
fn tcp_sack_false_omits_sack_permitted() {
    let mut cfg = EngineConfig::default_for_test();
    cfg.tcp_sack = false;
    let opts = Engine::build_connect_syn_opts_for_test(&cfg);
    assert!(!opts.has_sack_permitted(), "SACK-permitted emitted despite tcp_sack=false");
}
```

- [ ] **Step 2: Run test (should FAIL).** `timeout 60 cargo test -p dpdk-net-core --test cabi_field_wired`. Expected: FAIL on "TS option emitted" or compile-error on missing helpers.

- [ ] **Step 3: Implement** — thread `tcp_timestamps`/`tcp_sack`/`tcp_ecn` from `EngineConfig` into `build_connect_syn_opts` so each option is conditional. Delete `tcp_min_rto_ms` from `EngineConfig` and the C ABI struct (or leave with `#[deprecated]` + `// REMOVE-BY: A12` if cpp-consumer compatibility required this cycle). Regenerate header.

- [ ] **Step 4: Run test (should PASS).** Same command.

- [ ] **Step 5: Run full crate test suite to verify no regression.** `timeout 600 cargo test -p dpdk-net-core --release`.

- [ ] **Step 6: Verify cbindgen drift-check green.** Run the existing drift-check script (look in `scripts/`).

- [ ] **Step 7: Commit.**
```bash
git add crates/dpdk-net/src/api.rs crates/dpdk-net-core/src/engine.rs include/dpdk_net.h crates/dpdk-net-core/tests/cabi_field_wired.rs
git commit -m "fix(c-abi): wire tcp_timestamps/tcp_sack/tcp_ecn into SYN options; drop vestigial tcp_min_rto_ms (Part 1 BLOCK-A11 #1)"
```

**Two-stage review:** dispatch spec-compliance reviewer + code-quality reviewer in parallel.

**Estimate:** 0.5 agent-day.
**Dependencies:** A11.0 complete (CI gate active).

---

### Task A2 — Convert `unwrap()` panics in `eal_init` to error returns (Part 1 BLOCK-A11 #2)

**Source:** Part 1 BLOCK-A11 #2
**Pattern:** P6 (panic-across-FFI on caller-supplied input)

**Files:**
- Modify: `crates/dpdk-net-core/src/engine.rs:933` (`EAL_INIT.lock().unwrap()`), `:959` (`CString::new(*s).unwrap()`)
- Modify: `crates/dpdk-net/src/lib.rs` (the `dpdk_net_eal_init` extern wrapper — return error code on failure)
- Test: `crates/dpdk-net-core/tests/eal_init_error_paths.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test** that passes argv containing an interior NUL byte and asserts `dpdk_net_eal_init` returns a documented error code (e.g. `-libc::EINVAL`) instead of panicking. Use `std::panic::catch_unwind` to convert any panic into a test failure with diagnostic.

- [ ] **Step 2: Run test (should FAIL with panic).** `timeout 30 cargo test -p dpdk-net-core --test eal_init_error_paths`.

- [ ] **Step 3: Implement** — replace each `.unwrap()` with `?` propagation; map `CString::new` errors to `EalInitError::ArgvNul`; map mutex poisoning to `EalInitError::Reentrant`; convert all `EalInitError` to negative errno at the FFI boundary. Document errno mapping in the doc-comment.

- [ ] **Step 4: Run test (should PASS).**

- [ ] **Step 5: Run full crate test suite.** `timeout 600 cargo test -p dpdk-net-core --release`.

- [ ] **Step 6: Commit.**
```bash
git commit -m "fix(eal): convert unwrap() to error returns in eal_init (Part 1 BLOCK-A11 #2, Pattern P6)"
```

**Two-stage review.**

**Estimate:** 0.5 agent-day. **Dependencies:** A11.0 complete.

---

### Task A3 — Validate RTO bounds at `Engine::new` (release-build) (Part 3 BLOCK-A11 B1)

**Source:** Part 3 BLOCK-A11 B1
**Pattern:** P6 (`debug_assert!`-only validation reaches release-build panic)

**Files:**
- Modify: `crates/dpdk-net-core/src/engine.rs` — find the `Engine::new` body where RTO bounds are validated. Currently uses `debug_assert!`; release builds reach `u32::clamp(min, max)` which panics if `min > max`.
- Test: `crates/dpdk-net-core/tests/engine_new_validation.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test** with `cfg.tcp_min_rto_us = 1_000_000; cfg.tcp_max_rto_us = 100;` (inverted bounds). Assert `Engine::new` returns `Err(EngineConfigError::InvalidRtoBounds { min, max })` instead of panicking.

- [ ] **Step 2: Run test (should FAIL with panic in release build).** `timeout 30 cargo test -p dpdk-net-core --test engine_new_validation --release`.

- [ ] **Step 3: Implement** — replace `debug_assert!(min <= max)` with `if min > max { return Err(InvalidRtoBounds { ... }); }`. Add the same shape for any other `_us`/`_ms` pair. Add the error variant to the existing `EngineConfigError` enum (or create one if absent).

- [ ] **Step 4: Run test (should PASS).**

- [ ] **Step 5: Commit.**
```bash
git commit -m "fix(engine): validate RTO bounds in release builds (Part 3 BLOCK-A11 B1, Pattern P6)"
```

**Two-stage review.**

**Estimate:** 0.3 agent-day. **Dependencies:** A11.0.

---

### Task A4 — Substitute `event_queue_soft_cap == 0 → default` before bound check (Part 3 BLOCK-A11 B2)

**Source:** Part 3 BLOCK-A11 B2

**Files:**
- Modify: `crates/dpdk-net-core/src/engine.rs` `Engine::new` config-validation path
- Test: `crates/dpdk-net-core/tests/engine_new_zero_init.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test** with `let cfg: EngineConfig = unsafe { std::mem::zeroed() };` and assert `Engine::new(cfg)` succeeds with the documented default `event_queue_soft_cap = 4096`. Currently the bound check (`> 65536` etc.) runs BEFORE default substitution so 0 is rejected.

- [ ] **Step 2: Run test (should FAIL with NULL/error from zero-init).**

- [ ] **Step 3: Implement** — invert the order: `cfg.event_queue_soft_cap = if cfg.event_queue_soft_cap == 0 { 4096 } else { cfg.event_queue_soft_cap };` then bound-check. Apply the same pattern to every other `_size`/`_cap` field whose default substitution had the same bug.

- [ ] **Step 4: Run test (should PASS).**

- [ ] **Step 5: Commit.**
```bash
git commit -m "fix(engine): default-substitute zero-init config fields before bound-check (Part 3 BLOCK-A11 B2)"
```

**Two-stage review.**

**Estimate:** 0.3 agent-day. **Dependencies:** A11.0.

---

### Task A5 — Sync ffi-test `Cfg` mirror with `rx_mempool_size` (Part 4 BLOCK-A11 #3)

**Source:** Part 4 BLOCK-A11 #3
**Pattern:** P5 (FFI struct drift)

**Files:**
- Modify: `tests/ffi-test/tests/ffi_smoke.rs` (the hand-rolled `Cfg` mirror is missing the `rx_mempool_size` tail field added in A6.6/A6.7 — causes OOB read under `DPDK_NET_TEST_TAP=1`)
- OR delete the hand-rolled mirror and replace with cbindgen-generated bindings included via `bindgen` in `build.rs`

**TDD steps:**

- [ ] **Step 1: Write failing test** that runs `dpdk_net_engine_create` from `ffi-test` with `DPDK_NET_TEST_TAP=1` and verifies `rx_mempool_size` round-trips correctly (i.e. read back the active config via `dpdk_net_rx_mempool_size_get` if available, or via TAP introspection).

- [ ] **Step 2: Run test (should FAIL with OOB read or wrong rx_mempool_size value).** `timeout 60 cargo test -p ffi-test --test ffi_smoke -- --ignored` if TAP is gated.

- [ ] **Step 3: Implement** — operator decision:
  - (a) Add `rx_mempool_size` field to the hand-rolled `Cfg` struct in field-order matching `dpdk_net_engine_config_t`. Add a `static_assert` (compile-time) that `size_of::<Cfg>() == size_of::<dpdk_net_engine_config_t>()`.
  - (b) Replace the hand-rolled mirror with `bindgen`-generated bindings of `include/dpdk_net.h` so future drift is structurally impossible.
  Default: (b) — eliminates the bug class.

- [ ] **Step 4: Run test (should PASS).**

- [ ] **Step 5: Commit.**
```bash
git commit -m "fix(ffi-test): replace hand-rolled Cfg mirror with cbindgen-driven bindings (Part 4 BLOCK-A11 #3, Pattern P5)"
```

**Two-stage review.**

**Estimate:** 0.5 agent-day (if (b)) or 0.2 agent-day (if (a)). **Dependencies:** A11.0.

---

### Task A6 — C-ABI field-by-field audit script (Pattern P5 prerequisite)

**Source:** Meta-synthesis Pattern P5 recommendation; ends the dead-field accretion class mechanically.
**This task IS A11.0** along with Theme E T0–T2.

**Files:**
- Create: `scripts/check-cabi-fields.sh` (or `.py`)
- Create: CI workflow step that runs the script on every push

**Behavior:** parse `crates/dpdk-net/src/api.rs` `#[repr(C)] pub struct dpdk_net_engine_config_t { ... }` field-by-field. For each field, grep the codebase for any read site. If a field has no reader and no `#[allow(dead_code)] // REMOVE-BY: A<N>` comment, fail the build with a list of dead fields.

**TDD steps:**

- [ ] **Step 1: Write the script.** Use `cargo expand` or a small `syn`-based Rust binary if regex parsing is too brittle.

- [ ] **Step 2: Run script on master.** Expected: dead fields surface (A1 should have addressed `tcp_min_rto_ms` etc.; if A1 ran first, this script confirms zero dead fields).

- [ ] **Step 3: Add a synthetic dead field to a throwaway branch and confirm the script fails.**

- [ ] **Step 4: Wire into CI.**

- [ ] **Step 5: Commit.**
```bash
git commit -m "ci: add C-ABI field-by-field dead-field audit script (Pattern P5 mechanical gate)"
```

**Two-stage review.**

**Estimate:** 0.5 agent-day. **Dependencies:** none — A11.0 prerequisite, can start immediately.

---

### Task A7 — Fix `scapy-fuzz-runner` Cargo.toml workspace-feature leak (Part 7 BLOCK-A11 #1)

**Source:** Part 7 BLOCK-A11 #1 (cross-references Part 6 S2-1, promoted to BLOCK by Part 7)
**Pattern:** P1 (workspace-feature-unification leak via test/inject crates)

**Files:**
- Modify: `tools/scapy-fuzz-runner/Cargo.toml:7-8` — `dpdk-net-core = { path = "...", features = ["test-inject"] }` is non-optional, which causes cargo workspace feature unification to activate `test-inject` in EVERY production binary that depends on `dpdk-net-core`. Same antipattern as the `tcpreq-runner` regression fixed in `9f0ccd0` and the `layer-h-correctness` regression fixed in `8147404`. Apply the SAME fix shape: make `dpdk-net-core` an optional dep, gate behind a non-default `test-inject` feature on this crate, document.
- Verify: `scripts/check-workspace-features.sh` (added in Task A6) goes from RED → GREEN after this fix lands.

**TDD steps:**

- [ ] **Step 1: Run T0 CI gate (after A6 lands).** Expected: RED — script reports `test-inject` enabled in production resolution graph for `dpdk-net-core`.

- [ ] **Step 2: Apply the fix.** Make `dpdk-net-core` optional (`optional = true`) and add `default = []` + `test-inject = ["dpdk-net-core/test-inject", "dep:dpdk-net-core"]` features. Update `tools/scapy-fuzz-runner/src/main.rs` if any `use dpdk_net_core::*` needs the feature gate.

- [ ] **Step 3: Re-run T0 CI gate.** Expected: GREEN.

- [ ] **Step 4: Verify scapy-fuzz-runner still works** with explicit `cargo run -p scapy-fuzz-runner --features test-inject -- <args>`.

- [ ] **Step 5: Commit.**
```bash
git commit -m "fix(scapy-fuzz-runner): gate test-inject behind crate feature (Part 7 BLOCK-A11 #1, Pattern P1)"
```

**Two-stage review** with explicit ask: did the gate go RED → GREEN? Are there any OTHER tools/* crates with the same antipattern that A6 surfaces?

**Estimate:** 0.3 agent-day. **Dependencies:** A6 (CI gate must exist to verify the fix).

---

## Theme B — Counter-correctness (Pattern P4)

**Theme verdict:** 6 BLOCK-A11 items. Sequential within theme — fixes touch overlapping engine.rs control flow. Land each fix with the Theme E pressure-counter-parity-offload-matrix suite (T6 in pressure plan) so the assertion catches regressions at intensity.

### Task B1 — Fix `ip.rx_csum_bad` double-bump (Part 1 BLOCK-A11 #3 + Part 5 BLOCK-A11 B-1)

**Source:** Part 1 BLOCK-A11 #3, confirmed origin in Part 5 BLOCK-A11 B-1 (commit `e2aae95`, A-HW Task 8).
**Pattern:** P4 (counter-placement bug); the canonical example.

**Files:**
- Modify: `crates/dpdk-net-core/src/l3_ip.rs:213-220` (one bump site)
- Modify: `crates/dpdk-net-core/src/engine.rs:3928-3931` (the OTHER bump site)
- Decide: which site is the "canonical" owner. Per Pattern P4 recommendation, the dispatcher owns the counter. Default: keep `engine.rs` increment, drop `l3_ip.rs` increment.
- Test: `crates/dpdk-net-core/tests/counter_parity_offload.rs` (NEW — also feeds Theme E T6)

**TDD steps:**

- [ ] **Step 1: Write failing test** that injects exactly one NIC-BAD-cksum frame via `inject_rx_frame` (after Theme E T1 lands `pressure-test` feature gate; if T1 not yet landed, gate this test under `#[cfg(test)]` + `RTE_MBUF_F_RX_IP_CKSUM_BAD` set via direct mbuf-flag manipulation in test harness). Assert `delta(ip.rx_csum_bad) == 1` and `delta(eth.rx_drop_cksum_bad) == 1` (or whichever single-owner counter is canonical per spec §9.1).

- [ ] **Step 2: Run test (should FAIL with `delta == 2`).**

- [ ] **Step 3: Implement** — drop the redundant increment per the dispatcher-owns-counter rule. Add a doc-comment on the surviving site: `// Owns ip.rx_csum_bad — see Part 1 BLOCK-A11 #3, Pattern P4`.

- [ ] **Step 4: Run test (should PASS).**

- [ ] **Step 5: Run full crate test suite + integration.** `timeout 600 cargo test -p dpdk-net-core --release`.

- [ ] **Step 6: Commit.**
```bash
git commit -m "fix(counter): drop redundant ip.rx_csum_bad bump (Part 1+5 BLOCK-A11, Pattern P4 canonical)"
```

**Two-stage review** with explicit reviewer brief: confirm `ALL_COUNTER_NAMES` still lists the field, no other tests broke.

**Estimate:** 0.3 agent-day. **Dependencies:** A11.0 (the test feature gate).

---

### Task B2 — Matched-flow RST ACK arithmetic (Part 2 BLOCK-A11 B1)

**Source:** Part 2 BLOCK-A11 B1
**Files:** `crates/dpdk-net-core/src/engine.rs:4822` (`emit_rst` helper omits SYN/FIN sequence-length when computing the ACK)
**Test:** `crates/dpdk-net-core/tests/test_server_rst_ack_arithmetic.rs` (NEW; needs `test-server` feature)

**TDD steps:**

- [ ] **Step 1: Write failing test** — drive the test-server through SYN_RECEIVED → dup-SYN → emit_rst path. Assert RST's ACK == seg.SEQ + 1 (SYN consumes 1 byte) per RFC 9293 §3.5.2.
- [ ] **Step 2: Run test (FAIL).**
- [ ] **Step 3: Implement** — fix `emit_rst` to add SYN/FIN-flag sequence-length contribution when computing ACK.
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Add packetdrill-style negative scenario** in `crates/dpdk-net-core/tests/` covering the same path on FIN-during-SYN_RECEIVED.
- [ ] **Step 6: Commit.** `fix(tcp): include SYN/FIN sequence length in matched-flow RST ACK (Part 2 BLOCK-A11 B1)`.

**Two-stage review.** **Estimate:** 0.3 agent-day. **Dependencies:** A11.0; sequential after B1 (same engine.rs path edits).

---

### Task B3 — TIME_WAIT refresh gating (Part 2 BLOCK-A11 B2)

**Source:** Part 2 BLOCK-A11 B2
**Files:** `crates/dpdk-net-core/src/engine.rs:4476-4484` (TIME_WAIT refresh keyed on coarse `TxAction::Ack`) + `crates/dpdk-net-core/src/tcp_input.rs:1500-1504` (returns Ack early before close-path window check)
**Test:** `crates/dpdk-net-core/tests/time_wait_stale_refresh.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test** that establishes a connection, closes it, then sends a stale (out-of-window) segment matching the 4-tuple. Assert TIME_WAIT timer NOT extended past 2×MSL.
- [ ] **Step 2: Run test (FAIL — timer extended).**
- [ ] **Step 3: Implement** — gate TIME_WAIT refresh on in-window check; or move the refresh below the close-path window validation in tcp_input.
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Commit.** `fix(tcp): gate TIME_WAIT refresh on in-window segments only (Part 2 BLOCK-A11 B2)`.

**Two-stage review.** **Estimate:** 0.4 agent-day. **Dependencies:** A11.0; sequential after B2.

---

### Task B4 — Close-path PAWS / SACK / Outcome population (Part 2 BLOCK-A11 B3)

**Source:** Part 2 BLOCK-A11 B3 (DISPUTED → BLOCK-A11 per "do not soften" rule)
**Files:** `crates/dpdk-net-core/src/engine.rs` close-state inbound block; `crates/dpdk-net-core/src/tcp_input.rs` close-state branches that return `Ack` early
**Test:** `crates/dpdk-net-core/tests/close_state_paws_sack.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing tests** — three scenarios:
  1. PAWS rejection in CLOSE_WAIT: stale TS stamps a segment; assert it's dropped + `paws_rejected` counter +1.
  2. SACK block decode in FIN_WAIT_2: send segment with valid SACK; assert SACK is decoded and `dup_ack` counter behavior matches ESTABLISHED equivalent.
  3. Outcome populated for close-state segment (urgent_dropped, rx_zero_window if applicable).
- [ ] **Step 2: Run tests (FAIL — close-state paths return Ack before checks).**
- [ ] **Step 3: Implement** — propagate the close-path checks into the early-return path. Consider extracting a `validate_inbound_segment` helper invoked from both ESTABLISHED and close states. (Pattern M4 / P10 — this is also a small architectural cleanup; keep scoped.)
- [ ] **Step 4: Run tests (PASS).**
- [ ] **Step 5: Commit.** `fix(tcp): extend PAWS/SACK/Outcome population to close-state inbound (Part 2 BLOCK-A11 B3)`.

**Two-stage review** with explicit ask: did any new bug get introduced into the close-state ACK path? Run packetdrill close-state corpus.

**Estimate:** 0.7 agent-day (largest counter fix, touches multiple states). **Dependencies:** A11.0; sequential after B3.

---

### Task B5 — Move `corrupts` counter inside `if data_len > 0` guard (Part 7 BLOCK-A11 #2)

**Source:** Part 7 BLOCK-A11 #2
**Files:** `crates/dpdk-net-core/src/fault_injector.rs:276` (counter bumps even when `data_len == 0`); `:263` (the actual write IS guarded)
**Test:** `crates/dpdk-net-core/tests/fault_injector_zero_len.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test** that drives a zero-length segment through FaultInjector with corruption rate 100%; assert `delta(corrupts) == 0` (no byte was actually mutated).
- [ ] **Step 2: Run test (FAIL — counter bumped despite no write).**
- [ ] **Step 3: Implement** — move the `corrupts.fetch_add(1, Relaxed)` inside the `if data_len > 0` block.
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Commit.** `fix(fault-injector): only count actual byte mutations (Part 7 BLOCK-A11 #2)`.

**Two-stage review.** **Estimate:** 0.2 agent-day. **Dependencies:** A11.0.

---

### Task B6 — Add `tcp.rx_bad_csum` to row-14 corruption disjunction (Part 9 BLOCK-A11 B-3)

**Source:** Part 9 BLOCK-A11 B-3
**Files:** `tools/layer-h-correctness/src/matrix.rs` (or wherever the disjunctive corruption-counter assertion for row 14 lives)
**Test:** `tools/layer-h-correctness/tests/row14_offload_off.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test** that runs the row-14 scenario under an `offload-off` mock that bumps only `tcp.rx_bad_csum` (TCP-payload corruption path); assert the disjunctive group fires.
- [ ] **Step 2: Run test (FAIL — none of the existing disjunction members fire).**
- [ ] **Step 3: Implement** — extend the disjunctive group from `[eth.rx_drop_cksum_bad, ip.rx_csum_bad]` to `[eth.rx_drop_cksum_bad, ip.rx_csum_bad, tcp.rx_bad_csum]`.
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Commit.** `fix(layer-h): include tcp.rx_bad_csum in row-14 corruption disjunction (Part 9 BLOCK-A11 B-3)`.

**Two-stage review.** **Estimate:** 0.2 agent-day. **Dependencies:** A11.0.

---

## Theme C — Lifetime / RX-path correctness (Pattern P11)

### Task C1 — READABLE event lifetime across queued polls (Part 4 BLOCK-A11 #1)

**Source:** Part 4 BLOCK-A11 #1
**Pattern:** P11 (drop-path / mempool-lifetime)

**Files:** `crates/dpdk-net-core/src/engine.rs` (top of `poll_once` clears `readable_scratch_iovecs` before `drain_events` runs; READABLE event refs become invalid mid-poll across `dpdk_net_poll` boundaries)
**Test:** `tests/ffi-test/tests/readable_event_uaf.rs` (NEW; ASAN-enabled if available)

**Fix-shape (operator decision):**
- (a) Move scratch-clear AFTER `drain_events` — but events that span polls still see stale data.
- (b) Copy iovec data into the InternalEvent at population time so the event is self-contained.
- Default: (b) — eliminates the lifetime hazard entirely. Cost: per-event copy of up to N iovecs. Acceptable per per-conn-mempool sizing.

**TDD steps:**

- [ ] **Step 1: Write failing test** that calls `dpdk_net_poll` repeatedly, queues events across N polls, then reads READABLE event payload after the next poll has run. Under ASAN this crashes; without ASAN it returns garbage. Assert payload bytes match expected.
- [ ] **Step 2: Run test (FAIL).** `timeout 60 RUSTFLAGS="-Z sanitizer=address" cargo +nightly test --target x86_64-unknown-linux-gnu --test readable_event_uaf` (or fallback to plain test if ASAN unavailable; stage 2 will promote ASAN to CI matrix per Pattern P11 recommendation).
- [ ] **Step 3: Implement** — store a copy of the iovec data inside `InternalEvent::Readable { payload: Vec<u8> }` (or a `bytes::Bytes` if zero-copy is preserved into a refcounted owned region). Update event consumer to read from the owned payload.
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Run full FFI smoke + a 5000-iter loop variant** to verify no regression in throughput.
- [ ] **Step 6: Commit.** `fix(engine): own READABLE event payload to eliminate cross-poll UAF (Part 4 BLOCK-A11 #1, Pattern P11)`.

**Two-stage review.** **Estimate:** 0.7 agent-day (touches hot path; verify perf regression < 5% via bench-e2e). **Dependencies:** A11.0.

---

### Task C2 — Multi-seg RX L3 chain walker (Part 4 BLOCK-A11 #2 + Part 8 S2-7)

**Source:** Part 4 BLOCK-A11 #2 (cross-references Part 8 S2-7)
**Files:** `crates/dpdk-net-core/src/lib.rs:74-78` (`mbuf_data_slice` returns head-only); `crates/dpdk-net-core/src/l3_ip.rs:86` (rejects multi-seg with `BadTotalLen` before the chain walker runs)
**Test:** `crates/dpdk-net-core/tests/multi_seg_rx_l3.rs` (NEW)

**TDD steps:**

- [ ] **Step 1: Write failing test** that injects a 2-segment chain (head 100B + tail 1400B) — assert L3 parser uses `pkt_len` (1500B) not head `data_len` (100B), so the IP total-length check passes. Use the test-inject path or fault_injector.
- [ ] **Step 2: Run test (FAIL — `BadTotalLen`).**
- [ ] **Step 3: Implement** — change `l3_ip.rs:86` to use `mbuf_pkt_len` (whole-chain) for the total-length validation; if the IP header parsing needs to read past head segment, walk the chain via `rte_pktmbuf_read` or copy into a scratch buffer for the IP header only (TCP/payload stays zero-copy).
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Add an integration test for 16-segment chain with TCP payload to verify TCP processing on multi-seg.**
- [ ] **Step 6: Commit.** `fix(l3): use pkt_len for multi-seg RX validation (Part 4 BLOCK-A11 #2 / Part 8 S2-7)`.

**Two-stage review** with bench-e2e regression check.

**Estimate:** 0.7 agent-day. **Dependencies:** A11.0; sequential after C1 (both touch RX hot path).

---

## Theme D — Bench/comparator harness (Part 8 cluster + Part 9)

**Theme verdict:** 8 BLOCK-A11 items, fully parallel. Each is independent.

### Task D1 — Wire `ff_init` for F-Stack comparator (Part 8 B-A11-1)

**Files:** `tools/bench-vs-mtcp/` or wherever F-Stack lives — find the missing `ff_init` call site.
**Fix:** Add `ff_init(argc, argv)` to F-Stack comparator startup before any `ff_socket` / `ff_connect` call. If F-Stack feature is currently default-off (see D3), gate the test accordingly.

**TDD steps:**

- [ ] **Step 1: Write failing test** that runs F-Stack comparator end-to-end with `--stacks fstack`; assert stdout shows F-Stack `ff_init` printed banner + at least one valid TCP exchange occurred.
- [ ] **Step 2: Run test (FAIL — no ff_init).**
- [ ] **Step 3: Implement** — call `ff_init` once at process start; pass argv per F-Stack docs. Wire into the existing `T19: F-Stack as 3rd comparator` infrastructure.
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Commit.** `fix(bench): wire ff_init for F-Stack comparator (Part 8 B-A11-1)`.

**Two-stage review.** **Estimate:** 0.3 agent-day. **Dependencies:** A11.0.

---

### Task D2 — F-Stack burst errno discrimination (Part 8 B-A11-2)

**Files:** `tools/bench-vs-mtcp/src/fstack_maxtp.rs:215` (or wherever F-Stack burst path collapses `ff_write < 0` to backoff)
**Fix:** Discriminate `EINPROGRESS` (legitimate connect-in-progress; do NOT count as failure) from EAGAIN/EWOULDBLOCK (back-pressure; back off) from real failures (count as connect/send error).

**TDD steps:**

- [ ] **Step 1: Write failing test** that simulates `EINPROGRESS` and asserts the bench treats it as in-progress, not as error. Mock `ff_errno` per the F-Stack API.
- [ ] **Step 2: Run test (FAIL).**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Commit.** `fix(bench): discriminate F-Stack errno (EINPROGRESS != error) (Part 8 B-A11-2)`.

**Two-stage review.** **Estimate:** 0.3 agent-day. **Dependencies:** A11.0.

---

### Task D3 — Default-on `fstack` feature OR drop from nightly (Part 8 B-A11-3)

**Files:** `tools/bench-vs-mtcp/Cargo.toml` (the `fstack` feature) + `scripts/bench-nightly.sh` (the `--stacks fstack` invocation)
**Decision:** Operator chooses (a) make `fstack` default-on so nightly invokes a real F-Stack binary, OR (b) drop `--stacks fstack` from nightly so we don't advertise stub coverage as real.
**Default:** (a). The F-Stack AMI is now ready (per project memory `project_fstack_ami_complete.md`), so default-on is feasible.

**TDD steps:**

- [ ] **Step 1: Write failing test** that runs `bench-nightly.sh --stacks fstack` against a known-good AWS bench-pair AMI and asserts F-Stack TCP exchange completed (not stub).
- [ ] **Step 2: Run test (FAIL — feature default-off, stub used).**
- [ ] **Step 3: Implement** — flip `default = ["fstack"]` in Cargo.toml; verify gate in T0 (workspace-feature CI gate) does not regress.
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Commit.** `fix(bench): default-on fstack feature now AMI ready (Part 8 B-A11-3)`.

**Two-stage review.** **Estimate:** 0.3 agent-day. **Dependencies:** A11.0 (verify CI gate still green); D1+D2.

---

### Task D4 — maxtp emit CSV marker rows on every failure path (Part 8 B-A11-4)

**Files:** `tools/bench-vs-mtcp/src/main.rs` (or wherever maxtp output runs; failed buckets currently disappear silently)
**Fix:** every failure path emits a CSV row with `status=FAIL,reason=<errno|stderr-tag>` so analysts can see failed buckets.

**TDD steps:**

- [ ] **Step 1: Write failing test** that injects a failed bucket (e.g. via T19 stub-driven failure) and asserts CSV output contains a `FAIL` row.
- [ ] **Step 2: Run test (FAIL).**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run test (PASS).**
- [ ] **Step 5: Commit.** `fix(bench): emit CSV marker rows on maxtp failure paths (Part 8 B-A11-4)`.

**Two-stage review.** **Estimate:** 0.3 agent-day. **Dependencies:** A11.0.

---

### Task D5 — `bench-stress` CSV merge handle missing first scenario (Part 8 B-A11-5)

**Files:** `tools/bench-stress/src/csv_merge.rs` (or equivalent)
**Fix:** when first scenario is missing, emit blank cells with `status=MISSING` rather than blanking the entire row.

**TDD steps:** standard TDD pattern (write test, fail, implement, pass, commit).

**Estimate:** 0.2 agent-day. **Dependencies:** A11.0.

---

### Task D6 — Either raise `BENCH_ITERATIONS` default or rewrite stale comment (Part 8 B-A11-6)

**Files:** `scripts/bench-nightly.sh:495-503` (comment claims iter-7051 cliff is unfixed despite `f3139f6` resolving it)
**Fix-shape:** operator decision — raise `BENCH_ITERATIONS=5000` to a higher value (e.g. 100000) AND update the comment, OR keep low + rewrite comment to describe the current default rationale.
**Default:** raise to 100000 + delete the cliff-workaround comment; reference the post-fix commit.

**TDD steps:** test that bench-nightly.sh runs to completion with the new default; verify `f3139f6` cliff doesn't reappear.

**Estimate:** 0.2 agent-day. **Dependencies:** A11.0.

---

### Task D7 — Add `--features test-server` to layer-h wrapper scripts (Part 9 BLOCK-A11 B-1)

**Files:** `scripts/layer-h-smoke.sh:94`, `scripts/layer-h-nightly.sh:101` (both run `cargo build --release --workspace` without `--features test-server`; binary `target/release/layer-h-correctness` is silently skipped because `Cargo.toml` has `required-features = ["test-server"]`)
**Fix:** Add `--features test-server` to both `cargo build` invocations. Add a post-build existence check (script fails if binary missing).

**TDD steps:**

- [ ] **Step 1: Write failing CI step** that runs `scripts/layer-h-smoke.sh` on a fresh tree; assert exit code 0 AND `target/release/layer-h-correctness` exists.
- [ ] **Step 2: Run (FAIL — binary not built).**
- [ ] **Step 3: Implement** — append `--features test-server -p layer-h-correctness` to the cargo build line in both scripts. Add `[[ -x target/release/layer-h-correctness ]] || { echo "ERR: binary missing"; exit 1; }` after the build.
- [ ] **Step 4: Run (PASS).**
- [ ] **Step 5: Commit.** `fix(layer-h): wire --features test-server in wrapper scripts (Part 9 BLOCK-A11 B-1)`.

**Two-stage review.** **Estimate:** 0.2 agent-day. **Dependencies:** A11.0.

---

### Task D8 — Reorder Layer H `observe_batch` BEFORE `bench-e2e::run_rtt_workload` event drain (Part 9 BLOCK-A11 B-2)

**Files:** `tools/layer-h-correctness/src/main.rs` (or the orchestrator that calls bench-e2e workload runner) — currently `bench-e2e::run_rtt_workload` drains `engine.events()` before Layer H's `observe_batch` snapshots it, starving FSM/event oracle of `StateChange`/`TcpRetrans`/`TcpLossDetected` records.

**TDD steps:**

- [ ] **Step 1: Write failing test** that runs the layer-h scenario, then asserts `observe_batch` received at least N `StateChange` events (where N = expected per scenario).
- [ ] **Step 2: Run (FAIL — events drained by bench-e2e first).**
- [ ] **Step 3: Implement** — reorder so `observe_batch` runs FIRST, then bench-e2e drains. Or: add a non-draining peek API (`engine.events_peek()`) and use it from layer-h.
- [ ] **Step 4: Run (PASS).**
- [ ] **Step 5: Commit.** `fix(layer-h): observe_batch before bench-e2e drains events (Part 9 BLOCK-A11 B-2)`.

**Two-stage review.** **Estimate:** 0.4 agent-day. **Dependencies:** A11.0.

---

## Theme E — Pressure-correctness layer

**Delegated entirely to:** `docs/superpowers/plans/2026-05-05-pressure-test-plan.md`

That plan ships 18 tasks (T0–T18) across 4 sub-phases (A11.0 → A11.4):
- T0–T2 = A11.0 prerequisites (CI gate, `pressure-test` cargo feature, failure-bundle helper). T0+T1+T2 land alongside Theme A6 in the same A11.0 phase of THIS plan.
- T3–T18 = pressure-test suites in 17 targeted scenarios.

The pressure-test plan is independently scoped; THIS master plan only adds:
- A11-FINAL gate: pressure-test Tier-1 (T3–T7 in linked plan) must be green before declaring Stage 1 cross-phase fixes complete.
- The existing pressure-test plan already has its own two-stage review discipline + estimates.

Total Theme E estimate: ~16.5 agent-days per linked plan; ~6–7 calendar days under parallel dispatch.

---

## Stage-2 follow-up index (informational; NOT in scope for this plan)

153 STAGE-2 items remain after this plan completes. The full index is in `docs/superpowers/reviews/cross-phase-retro-summary.md` §"Stage-2 follow-up index" (lines 372–544). They cluster into:

- **Pattern P7 (doc drift)** ~24 items — clean up at end of Stage 2 with a single doc-sweep gate.
- **Pattern P8 (`#[allow]` accretion)** ~12 items — bundle into a Stage-2 cleanup milestone with `// REMOVE-BY:` markers.
- **Pattern P10 (god-object growth)** ~8 items — Stage-2 architectural-drift cleanup (extract `tcp_dispatch.rs`, `engine_lifecycle.rs`, `tx_path.rs`, `offloads.rs`).
- **Pattern P12 (ARM portability)** ~9 items — Stage-2 ARM-port milestone bundle.
- **Pattern P2 (engine-internals leak)** ~6 items — Stage-2 `dpdk-net-test-support` sibling crate extraction.
- The remainder ~94 items are spread across all parts; triage during A11-FINAL.

These are tracked but explicitly **NOT** addressed in this plan. Do not let scope creep pull them in.

---

## A11-FINAL — Cross-phase retro re-run (Stage 1 ship gate)

Once A11.0 → A11.3 land:

- [ ] **Re-run cross-phase retro** using `docs/superpowers/reviews/cross-phase-retro-review-prompts.md` against the new HEAD.
- [ ] **Diff against current retro:** every BLOCK-A11 from this plan should be RESOLVED (cite the fix commit in the new retro).
- [ ] **No NEW BLOCK-A11 items** introduced by Theme A–E work.
- [ ] **STAGE-2 count delta:** acceptable if some Theme work surfaced new STAGE-2 items, but each new BLOCK-A11 is a regression that must be fixed before declaring Stage 1 done.

Per project memory `feedback_phase_mtcp_review.md` + `feedback_phase_rfc_review.md`, A11 also gets per-phase mTCP + RFC review subagent gates as final ship checks.

---

## Self-review checklist (operator-side)

After all Theme A–D tasks land:

- [ ] **Spec coverage:** every BLOCK-A11 ID from `cross-phase-retro-summary.md` has a matching task in this plan or in the linked pressure-test plan.
- [ ] **Type consistency:** counter names cited in tasks match `crates/dpdk-net-core/src/counters.rs` `ALL_COUNTER_NAMES`.
- [ ] **No placeholders:** every task has actual code in TDD steps.
- [ ] **Project-memory compliance:** all subagent dispatches use opus 4.7; all `cargo test` invocations have explicit per-command timeouts; no new hot-path counters without compile-time feature gate; no `Ordering::Relaxed` on flags read for ordering decisions.

---

## Estimates summary

| Theme | Tasks | Est. agent-days | Parallel? |
|-------|-------|------|-----------|
| A | 7 (A1–A7) | 2.8 | yes (parallel; A7 depends on A6) |
| B | 6 (B1–B6) | 2.1 | mostly sequential (B1→B2→B3→B4 chain; B5+B6 independent) |
| C | 2 (C1–C2) | 1.4 | sequential |
| D | 8 (D1–D8) | 2.2 | yes (parallel) |
| E | 18 (T0–T18 in linked plan) | 16.5 | per linked plan |
| **Total** | **41** | **~25.0** | **6–8 calendar days at 4–5 concurrent agents** |

A11.0 prerequisite (Theme A6 + Theme E T0+T1+T2) ≈ 1.5 agent-days. A11.0 starts immediately; everything else follows.

---

## Status

**READY-FOR-EXECUTION** — plan written 2026-05-06.
- Theme E linked plan (`docs/superpowers/plans/2026-05-05-pressure-test-plan.md`) is independently READY-FOR-EXECUTION.
- Theme A–D tasks here all carry concrete file:line citations + TDD steps + two-stage review.
- A11.0 (Theme A6 + Theme E T0+T1+T2) can start IMMEDIATELY.
