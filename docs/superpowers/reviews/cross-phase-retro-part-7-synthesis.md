# Part 7 Cross-Phase Retro Synthesis
**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 7 — Property/fuzz + FaultInjector
**Phases:** A9
**Inputs:** cross-phase-retro-part-7-claude.md, cross-phase-retro-part-7-codex.md

## Combined verdict

A9 shipped a working FaultInjector with clean feature gating and a real post-A9 UAF fix already merged (`a0f8f96`). Both reviewers converge on residual drift at HEAD: a workspace-wide `test-inject` feature leak via `tools/scapy-fuzz-runner` (cross-reference to Part 6's identical pattern); spec-vs-implementation drift on `pub(crate) FaultInjector` and on always-present `FaultInjectorCounters`; and weak fuzz/proptest assertion content in several targets. Codex adds three concrete defects Claude missed: a counter-placement bug on zero-length corruption, unchecked manifest indexing in scapy-fuzz-runner, and a fake-mbuf precondition (refcount + alignment) violation in `tcp_reassembly` fuzz target. Claude adds breadth on architectural drift, RefCell-on-RX-hot-path coupling, and the unasserted spec §6 invariants (#3 recv-window monotonicity, #6 counter-load-no-panic).

## BLOCK A11 (must-fix before next phase)

- **scapy-fuzz-runner unconditionally enables `dpdk-net-core/test-inject`** (`tools/scapy-fuzz-runner/Cargo.toml:8`, root `Cargo.toml:21` no exclusion) — Claude. Workspace-wide feature leak; same antipattern that `tcpreq-runner` 2026-05-02 fix (`9f0ccd0`) corrected. Fix template already on hand. **CROSS-REFERENCE TO PART 6** — Part 6 already covered the scapy-fuzz-runner test-inject Cargo.toml leak class. Carry the Part-6 disposition (block until Cargo.toml gating + `default-members` exclusion mirror `tcpreq-runner` pattern).

- **`fault_injector.rs:276` `corrupts` counter increments on zero-length corruption decision** (`crates/dpdk-net-core/src/fault_injector.rs:276`) — Codex, BUG. The actual write at line 263 is guarded by `if data_len > 0`, but the counter at line 276 fires unconditionally inside the corrupt branch. Observable counter then reports a corruption fault was applied when no byte was mutated. Direct contradiction to `feedback_observability_primitives_only.md` (counter must reflect the event it claims to count).

## STAGE-2 FOLLOWUP (real concern, deferred)

- **`pub mod fault_injector;` exposes `pub struct FaultInjector` on Rust public API** (`crates/dpdk-net-core/src/lib.rs:13`, `fault_injector.rs:172`) — Claude. Spec §5.2 said `pub(crate)`. External Rust callers can construct + feed mbufs without env-var path. Spec amendment OR `pub(crate)` retrofit needed.

- **`FaultInjectorCounters` always-present contradicts spec §5.2** (`crates/dpdk-net-core/src/counters.rs:773-786`, `include/dpdk_net.h:403-407`) — Claude. Deliberate decision in `a7ca84e` to keep C ABI stable across feature flips, but spec was never amended. Also confirmed FYI by Codex (`counters.rs:779`, `repr(C, align(64))`, no ARM concern). Pure spec/doc drift.

- **`FaultInjector::process` ordering: dup-then-reorder collapses two faults into a serial pipeline** (`fault_injector.rs:279-336`) — Claude. Spec §4.2 modeled drop/dup/reorder/corrupt as independent enum variants. Either document in module header or amend spec.

- **Spec §6 invariant #3 (recv-window monotonicity) unasserted in any A9 proptest/fuzz target** — Claude. `engine_inject` only asserts #2 + #4 (six promised, two delivered). Spec needs to drop #3 (and #6 counter-load-no-panic) OR A9 has unfinished assertion work.

- **`tools/scapy-fuzz-runner/src/main.rs:80` and `:88` use unchecked `frames[i]` subscripts on manifest-driven indexing** — Codex, BUG (×2). Stale/hand-edited manifest panics the runner instead of returning `anyhow` context. Mechanical numeric error-path defect in a corpus replay tool. Two independent occurrences (chain path + single-frame path).

- **`fuzz_targets/tcp_reassembly.rs:73` calls `ReorderQueue::insert` without pre-bumped mbuf refcount** — Codex, LIKELY-BUG. Callee contract at `tcp_reassembly.rs:136` says caller "MUST have bumped the mbuf refcount by 1". Target validates structural invariants while exercising invalid refcount state, so it can miss the exact leak/free imbalance class A6.5/A6.7/A9 were trying to harden.

- **`fuzz_targets/tcp_reassembly.rs:40` fake mbuf backed by `Vec<u8>` (byte alignment only) cast to `*mut rte_mbuf`** — Codex, LIKELY-BUG. `rte_mbuf` alignment assumptions not held; precondition violated even if most allocators happen to return aligned memory.

- **`tcp_options` and `tcp_state_fsm` fuzz targets are no-panic only; do not share property body with proptest siblings** (`fuzz_targets/tcp_options.rs:5-11`, `fuzz_targets/tcp_state_fsm.rs:1-21`) — Claude. The proptest+fuzz pairing is meant to give the property-checker more iters via libFuzzer; current pairing tests strictly different (overlapping) properties. `tcp_state_fsm`'s "Once a richer pure transition helper lands…" TODO has been outstanding since `23417c2`.

- **`engine_inject` fuzz target is TAP-gated no-op without `DPDK_NET_TEST_TAP=1`** (`fuzz_targets/engine_inject.rs:88-89`) — Claude. CI runners that don't set the gate burn libFuzzer hours over a `return;` body; green check signals false confidence. Either `fuzz-smoke.sh` refuses without TAP, or target gets a fallback path.

- **`engine_inject.rs:97` discards every `inject_rx_frame` error, treating frame-too-large == mempool-exhausted** — Codex, SMELL. Repeated `MempoolExhausted` is the observable symptom of an mbuf leak; swallowing it removes a cheap leak signal from the fuzz target.

- **`fault_injector_smoke.rs:30` lacks zero-length / short-frame `corrupt=1.0` assertion** — Codex, SMELL. Counter-placement bug (BLOCK-A11 above) would not be caught by current smoke coverage. Direct test gap for the new BLOCK item.

- **proptest_paws.rs and proptest_rack_xmit_ts.rs model their gates via 1-line local mirrors, not the actual code** (`tests/proptest_paws.rs:74-76`, `tests/proptest_rack_xmit_ts.rs:29`) — Claude. Maintenance trap: if gate ever refactored, proptest silently keeps proving the wrong rule. Right fix: extract pure helper, share between dispatch path and proptest.

- **Post-A9 UAF in `Drop for Engine` / `Drop for FaultInjector`** — Claude (cross-reference; already fixed in `a0f8f96`). Class-of-bug recurrence: in-tree subagent reviewers under-weight engine-drop/mempool-lifetime invariants; `fault_injector_chain_uaf_smoke.rs` passes silently in `cargo test` without ASAN. Stage-2 ask: add ASAN as a CI matrix axis (already in spec §6 line 372 as a should-have).

- **No counter for `reorder-ring-evicted-on-engine-drop`, no counter for `reorder ring full → evicted`, no counter for `InjectErr::MempoolExhausted` / `FrameTooLarge`** — Claude. Future-improvement; cheap to add; unlocks soak-test diagnostics.

- **Engine `dispatch_one_rx_mbuf` reads `self.fault_injector` via `RefCell::borrow_mut` on the RX hot path** (`engine.rs:3724`) — Claude. Even the feature-on-but-injector-None case pays a RefCell borrow per dispatch. Mitigation: `Cell<bool>` or `Option<RefCell<...>>` shape. Low priority pending fault-injector ever shipping in benchmark configuration. Codex independently inspected the same line and found no nested-borrow panic risk under current call shape (FYI).

- **`fault_injector.rs:244, :260` corruption is bounded to head segment's data room, not packet chain / `pkt_len`** — Codex. SMELL × 2 (architectural drift + hidden coupling). For `inject_rx_chain`/LRO inputs, tail segments never selected. Single-segment assumption hides inside post-PMD "packet" middleware.

- **`fault_injector.rs:268` corruption is generic byte XOR with no protocol-field classification** — Codex, SMELL. TCP seq/ack/window/checksum corruption indistinguishable from Ethernet-header / payload-only corruption via the single `corrupts` counter.

- **Spec §4.2 reorder ring "default depth 4, configurable"; implementation hardcodes `ArrayVec<_, 16>`** (`fault_injector.rs:177`) — Claude. Memory-bound stricter, but tunability promised in spec didn't ship.

- **`fault_injector.rs:31-34` doc-block still talks about Task-5/Task-6 stubs** — Claude. Cosmetic; misleads new readers post-A9.

- **`fuzz_targets/tcp_reassembly.rs:20` safety note references `shim_rte_mbuf_refcnt_update`, but HEAD drop path uses `shim_rte_pktmbuf_free_seg` (`tcp_reassembly.rs:312`)** — Codex, SMELL. Documented unsafe surface stale.

- **`fault_injector.rs:265` local comment explains XOR-forced-nonzero but misses the outer zero-length case** — Codex, SMELL. Same root cause as the BLOCK item; comment fix should ship with the counter fix.

- **`tools/scapy-fuzz-runner/src/main.rs:34` `#[allow(dead_code)] flags`** — Claude. Drop the field or wire it into a per-frame-flags assertion.

## DISPUTED (reviewer disagreement)

(none — the two reviews touch overlapping but largely orthogonal surfaces; no classification mismatch found on shared findings.)

## AGREED FYI (both reviewers flagged but not blocking)

- **FaultInjector counters use `Ordering::Relaxed` and are sound on ARM** — Claude (memory-ordering section) + Codex (`fault_injector.rs:240` FYI, `counters.rs:779` FYI). Telemetry-only monotonic counters; no Release/Acquire edge needed.

- **`Engine::dispatch_one_rx_mbuf` RefCell borrow on RX path** — Claude (Hidden coupling, low priority) + Codex (`engine.rs:3724` FYI, no nested-borrow panic at HEAD). Both agree: not a bug today; visible if call shape ever nests.

- **Post-A9 UAF chain (`a0f8f96`) is fixed at HEAD** — Claude (cross-phase invariant violation, retro context) + Codex (`fault_injector.rs:295` FYI, "current code has the per-segment bump shape expected for chain balance"). Codex explicitly defers to Claude's coverage.

- **FaultInjector retains no timer-wheel state; only the reorder ring, drained in `Drop`** — Codex (`fault_injector.rs:342` FYI). Claude noted the `try_borrow_mut` skip-on-borrowed at `engine.rs:6583` (silent leak by design) — same drop-path topic, complementary observation.

## INDEPENDENT-CLAUDE-ONLY (HIGH/MEDIUM/LOW plausibility)

(All Claude-only items already triaged above into BLOCK-A11 / STAGE-2 / AGREED-FYI. No additional independent items remain unclassified.)

## INDEPENDENT-CODEX-ONLY (HIGH/MEDIUM/LOW plausibility)

(All Codex-only items already triaged above into BLOCK-A11 / STAGE-2 / AGREED-FYI. No additional independent items remain unclassified.)

## Counts
Total: 26; BLOCK-A11: 2 (1 cross-reference to Part 6, 1 new); STAGE-2: 20; DISPUTED: 0; AGREED-FYI: 4; CLAUDE-ONLY: 0 unclassified; CODEX-ONLY: 0 unclassified
