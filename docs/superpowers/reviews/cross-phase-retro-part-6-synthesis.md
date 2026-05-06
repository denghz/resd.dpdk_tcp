# Part 6 Cross-Phase Retro Synthesis
**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 6 — Test infrastructure
**Phases:** A7, A8, A8.5
**Inputs:** cross-phase-retro-part-6-claude.md, cross-phase-retro-part-6-codex.md

## Combined verdict

Both reviewers found the test-infrastructure stack architecturally sound: A8's exact-counter `obs_smoke.rs` gate holds, tcpreq probes use correct wrapping arithmetic, PAWS edges are covered, and no atomic-ordering, RefCell-locking, or mbuf-leak defects were confirmed in scoped Rust at HEAD. Both reviewers independently flagged the same one confirmed mechanical bug: the `wall_timeout` parameter in `tools/packetdrill-shim-runner/src/invoker.rs:25-49` is documented as a hard per-script bound, accepted as an argument, immediately discarded with `let _ = wall_timeout;`, and the resulting outcome hard-codes `timed_out: false` — which makes the corpus tests' `out.timed_out` failure branch unreachable. Claude additionally surfaced architectural drift findings that Codex did not search for (notably the `test-inject` workspace-unification half-fix in `scapy-fuzz-runner/Cargo.toml`, the dual `inject_rx_frame` implementations, and the missing CI metadata gate that would have caught the gateway-ARP regression class). Codex's scoped review confirmed several null findings (no sequence-arithmetic, atomic-ordering, RefCell, or mbuf-leak bugs in scope) that complement Claude's broader architectural sweep.

## BLOCK A11 (must-fix before next phase)

*(none — both reviewers agree the wall_timeout bug is real but mitigated in CI by external `timeout 300/900` wrappers in `scripts/ci-packetdrill-corpus.sh:19-28`; classified as STAGE-2 below per "prefer STAGE-2 when uncertain")*

## STAGE-2 FOLLOWUP (real concern, deferred)

- **S2-1 — `test-inject` workspace-unification half-fix in `scapy-fuzz-runner`.** [Claude AD-1, CPI-1] `tools/scapy-fuzz-runner/Cargo.toml:8` requests `dpdk-net-core = { path = "...", features = ["test-inject"] }` non-optionally and without a feature gate. This is the same architectural pattern that caused the T11 (commit `f6280ab`) and T20 (commits `8147404`/`50a2392`) gateway-ARP regressions. The immediate `tx_frame` reroute symptom does not reproduce (test-inject doesn't reroute tx), but a divergent `inject_rx_frame` implementation gets pulled into every workspace consumer's build. SEVERITY: architectural drift / latent regression class. Fix shape: `optional = true` + non-default `test-inject` feature gate, mirroring the tcpreq-runner / layer-h-correctness fixes.

- **S2-2 — Missing CI metadata guard for "should-never-appear-in-default-build" features.** [Claude CPI-1] No test or CI gate asserts that `cargo build --workspace --release` produces a `dpdk-net-core` resolve WITHOUT `test-server` / `test-inject` / `test-panic-entry` / `fault-injector` / `obs-none`. A 5-line `cargo metadata | jq` check would catch the regression class that landed twice (T11, T20) and is still latent (per S2-1). SEVERITY: process gap.

- **S2-3 — Two diverging `inject_rx_frame` implementations behind feature flags.** [Claude AD-2, HC-4] `crates/dpdk-net-core/src/engine.rs:6302` (test-inject) and `engine.rs:6692` (test-server-only) are subtly different — different mempool source (`test_inject_pool` vs `_rx_mempool`), different error type (`InjectErr` vs `crate::Error`), and counter-bump parity was lost-then-restored at `engine.rs:6360` (A8.5 T10 followup). Out-of-crate callers must use `.expect(..)` to paper over the error-type divergence. SEVERITY: design smell, future-foot-gun.

- **S2-4 — `wall_timeout` parameter dead code in shim invoker.** [Claude TD-1, Codex BUG (verdict + CPI + TPC + DD)] `tools/packetdrill-shim-runner/src/invoker.rs:18-50`: docstring promises a hard wall-timeout bound, signature accepts the argument, line 30 discards it with `let _ = wall_timeout;`, line 44 uses blocking `Command::output()`, line 49 hard-codes `timed_out: false`. The corpus tests `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:73,134` branch on `out.timed_out` for no-crash failure classification — the branch is unreachable. SEVERITY: confirmed mechanical defect with documentation drift. MITIGATED in CI by external `timeout 300/900` in `scripts/ci-packetdrill-corpus.sh:19-28`; local `cargo test` runs unwrapped and can hang. Fix: implement `wait_timeout` + kill, or drop the parameter and the docstring promise.

- **S2-5 — `dpdk_net_shutdown` declared in BOTH `dpdk_net.h` and `dpdk_net_test.h`.** [Claude AD-4, CFI-1, DD-1] `include/dpdk_net.h:772` (production) and `include/dpdk_net_test.h:108` (test header) both declare the same prototype because the test-header cbindgen pass excludes only `dpdk_net_test_*` symbols (`crates/dpdk-net/cbindgen.toml:35-51`). Identical declarations so not an ABI break, but C++ test binaries including both headers get duplicate declarations; deviation paragraph at lines 751-771 / 87-107 is also duplicated verbatim. SEVERITY: header noise + future doc-sync risk.

- **S2-6 — Test-server `port_id == u16::MAX` sentinel is undocumented architecturally.** [Claude AD-5] `crates/dpdk-net-core/src/engine.rs:1299-1311` makes `u16::MAX` a bypass sentinel that disables every `rte_eth_*` call AND zeros offload-active latches. The A7 spec introduces the value but does not name it as the architectural bypass. SEVERITY: doc gap; mirrors Part 5 AD-2.

- **S2-7 — `tools/packetdrill-shim-runner` dev-dep `dpdk-net-core` is non-optional and ungated.** [Claude TD-4] `Cargo.toml:28` carries `dpdk-net-core = { path = "..." }` as a dev-dep without `optional = true` or feature gate. Two self-tests (`tests/shim_inject_drain_roundtrip.rs:17`, `shim_virt_time_rto.rs:18`) rely on cargo workspace feature unification of `test-server` from elsewhere; if the rest of the workspace stopped activating `test-server`, these tests would fail to compile. SEVERITY: brittle test wiring.

- **S2-8 — A8.5 T9 soak test (`#[ignore]` + `LIGURIO_SOAK_ITERS`) is not continuously verified.** [Claude TD-5] `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:112` `ligurio_no_crash_soak` requires opt-in env; no scheduled CI job runs it. The "soak-tested 100×" claim in `tools/packetdrill-shim/SKIPPED.md` is a snapshot, not a continuous gate. SEVERITY: soak coverage drift risk.

- **S2-9 — Counter coverage uses `bump_counter_one_shot` for HW-only and other counter sites.** [Claude TPC-3, OG (one-shot framing); Codex SMELL (TD + Obs gaps)] `crates/dpdk-net-core/tests/common/mod.rs:592` directly bumps via `engine.bump_counter_for_test(...)`; ~15 HW-path counters and many A8/A8.5 cases (e.g. `counter-coverage.rs:1208,1222,1234,1618`) rely on it. Proves addressability / metric registration, not protocol-path increment placement. SEVERITY: test-pyramid debt; mirrors Part 5 CPI-2.

- **S2-10 — tcpreq probe integration tests are pass-only and short; zero negative-shape tests.** [Claude TPC-2] `tools/tcpreq-runner/tests/probe_*.rs` are 24-47 lines each and assert only `ProbeStatus::Pass`. False-positives in probe internals (e.g. counting wrong counters) would still pass. SEVERITY: coverage gap.

- **S2-11 — `state_trans[11][11]` exhaustive table covers only one FSM trajectory.** [Claude CPI-3] `crates/dpdk-net-core/tests/obs_smoke.rs:152-196` enforces zero on 116 unreached cells (good fail-loud) but only 5 of 121 cells fire; `counter-coverage.rs` has 33+ other `Reached` transitions documented separately. The "Stage 1 ship-gate" framing oversells coverage. SEVERITY: doc/scope clarification.

- **S2-12 — Duplicated `test_eal_args` / `test_server_config` between tcpreq-runner and `dpdk-net-core/tests/common`.** [Claude TD-3] `tools/tcpreq-runner/src/lib.rs:181-207` openly comments the duplication. Each new test-server consumer adds another. Should be lifted to a tiny shared `dpdk-test-fixtures` crate. SEVERITY: low-risk drift.

- **S2-13 — Two separate `ENGINE_SERIALIZE` mutexes for the same architectural problem.** [Claude HC-1; Codex FYI mempool/harness serialization at lib.rs:169 and common/mod.rs:441,505] No shared abstraction; comments at `tcpreq-runner/lib.rs:146-159` describe the lock as "binary-wide" — correct per-process but no shared crate. SEVERITY: design duplication.

- **S2-14 — Disconnect-mid-run timer-cancel coverage gap.** [Codex FYI (TPC focus item 8)] No scoped A7/A8/A8.5 test disconnects a test client mid-run and asserts timer-cancel discipline; only fire/reap is exercised at `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:75,80` and `crates/dpdk-net-core/tests/counter-coverage.rs:1538`. SEVERITY: coverage gap.

- **S2-15 — `parse_tcp_seq_ack` helper has stricter caller preconditions than its sibling.** [Codex SMELL] `crates/dpdk-net-core/src/test_server.rs:226-231` indexes `frame[14]` and TCP bytes directly without the bounds checks `parse_syn_ack` performs at lines 201,207,213. Current callers feed harness-drained frames so no runtime defect, but contract is mismatched and a future reuse on untrusted input would fault. SEVERITY: helper-contract smell.

- **S2-16 — Fault-injector chain UAF detection depends on sanitizer/debug-allocator environment.** [Codex FYI] `crates/dpdk-net-core/tests/fault_injector_chain_uaf_smoke.rs:21,74,118`: deterministic only under sanitizer; non-sanitized CI is not a hard proof every injected-mbuf error path is leak-free. SEVERITY: detection coverage gap.

- **S2-17 — Hidden coupling on fixed Ethernet+IPv4+TCP frame layout in tcpreq + test_server helpers.** [Codex SMELL] `tools/tcpreq-runner/src/lib.rs:34`, `tools/tcpreq-runner/src/probes/options.rs:46`, `crates/dpdk-net-core/src/test_server.rs:226` assume layout offsets without self-defending checks against malformed/non-Ethernet frames. SEVERITY: coupling, not correctness in current call sites.

- **S2-18 — External packetdrill binary built via `build.sh` from `build.rs`; behavior hidden behind generated artifacts.** [Codex FYI / SMELL] `tools/packetdrill-shim-runner/build.rs:22,25` delegates to shell; Rust invoker treats result as opaque binary. Time-conversion / shim/engine shared-state assumptions are not Rust-reviewable. SEVERITY: review-surface gap.

- **S2-19 — Audit: every counter added since A8 (A9, A10, A10.5) must be in `EXPECTED_COUNTERS` or expected-zero.** [Claude OG-1] One-time walk needed; A10 perf-instrumentation counters were partially confirmed (`obs.events_dropped`, `obs.events_queue_high_water = 7`) but the A10 limit-detection counter family was not audited against `obs_smoke.rs`. SEVERITY: drift audit task.

## DISPUTED (reviewer disagreement)

*(none material — both reviewers' classifications align where they overlap. The wall_timeout finding is BUG (Codex) vs TD-1 (Claude); collapsed to S2-4 above as STAGE-2 because external CI wrappers mitigate.)*

## AGREED FYI (both reviewers flagged but not blocking)

- **AF-1 — `wall_timeout` parameter dead code.** Both reviewers independently flagged. (Promoted to S2-4 above due to runtime-effect documentation drift; cited here for traceability per prompt note.)

- **AF-2 — A8 obs-gate exact-counter assertions hold.** [Claude CPI-2 + OG framing; Codex Obs gaps verdict] `crates/dpdk-net-core/tests/obs_smoke.rs:72-300` pins 21 expected non-zero counters and walks all declared counters asserting zero on the rest. Both reviewers confirm no drift, no path where an exact-assertion counter is bumped via `bump_counter_one_shot`.

- **AF-3 — `bump_counter_one_shot` covers addressability, not behavior.** [Claude TPC-3, OG-2; Codex SMELL TD + Obs gaps] Promoted to S2-9 above for actionability.

- **AF-4 — Per-process / per-binary harness mutex pattern.** [Claude HC-1; Codex FYI mem-ordering] `tools/tcpreq-runner/src/lib.rs:169,245`, `crates/dpdk-net-core/tests/common/mod.rs:440,455,471,505`. Coarse serialization, ARM-portable, no shared abstraction. (Promoted to S2-13.)

## INDEPENDENT-CLAUDE-ONLY (HIGH/MEDIUM/LOW plausibility)

- **CO-C1 — HIGH — `test-inject` workspace-unification half-fix in `scapy-fuzz-runner`.** [AD-1, CPI-1] Promoted to S2-1.
- **CO-C2 — HIGH — Missing CI metadata gate for never-default features.** [CPI-1] Promoted to S2-2.
- **CO-C3 — HIGH — Two diverging `inject_rx_frame` implementations.** [AD-2, HC-4] Promoted to S2-3.
- **CO-C4 — MEDIUM — `dpdk_net_shutdown` double-declared in two headers.** [AD-4, CFI-1, DD-1] Promoted to S2-5.
- **CO-C5 — MEDIUM — `port_id == u16::MAX` sentinel is undocumented architecturally.** [AD-5] Promoted to S2-6.
- **CO-C6 — MEDIUM — Redundant inner `#[cfg(feature = "test-server")]` on `conn_peer_mss` at `engine.rs:6673`.** [AD-3] SEVERITY: cosmetic; left as FYI.
- **CO-C7 — MEDIUM — Shim-runner non-optional dev-dep on `dpdk-net-core` relies on workspace feature unification.** [TD-4] Promoted to S2-7.
- **CO-C8 — MEDIUM — A8.5 T9 soak test not continuously verified.** [TD-5] Promoted to S2-8.
- **CO-C9 — MEDIUM — tcpreq probe tests are pass-only.** [TPC-2] Promoted to S2-10.
- **CO-C10 — MEDIUM — `state_trans[11][11]` covers single FSM trajectory; framing oversells coverage.** [CPI-3] Promoted to S2-11.
- **CO-C11 — LOW — `clock.rs:79` TODO on `CLOCK_MONOTONIC_RAW` (Instant uses `CLOCK_MONOTONIC`).** [TD-2] Spec deviation, recorded at site only. FYI.
- **CO-C12 — MEDIUM — Duplicated `test_eal_args` / `test_server_config` helpers.** [TD-3] Promoted to S2-12.
- **CO-C13 — LOW — `EXPECTED_COUNTERS_OBS_BYTE` array literal feature-gated; cleaner shape would be a function.** [OG-2] FYI, ergonomics.
- **CO-C14 — LOW — Corpus tests pin counts rather than enumerate scripts; harder triage.** [TPC-4] FYI.
- **CO-C15 — LOW — A8.5 spec docstring references transient `jenkins-ci-migration` branch tip.** [DD-3] FYI, historical citation.
- **CO-C16 — LOW — `obs_smoke.rs:18-22` references "(now-removed)" diagnostic helper; better kept under `#[ignore]`.** [DD-2] FYI.
- **CO-C17 — LOW — `packetdrill-shim-runner/Cargo.toml` bins lack `required-features` but don't link test-server symbols.** [FYI-1] Verified safe; FYI only.
- **CO-C18 — LOW — A10 / A10.5 counters not audited against `obs_smoke.rs::EXPECTED_COUNTERS`.** [OG-1] Promoted to S2-19.
- **CO-C19 — HIGH (negative) — No memory-ordering or x86_64-only gates in test infra; ARM-portable.** [MO-1] FYI / null finding.
- **CO-C20 — HIGH (negative) — No symbol renames since A8.5; ABI stable across two adds (`dpdk_net_shutdown`, `dpdk_net_test_shutdown`).** [CFI-3] FYI / null finding.

## INDEPENDENT-CODEX-ONLY (HIGH/MEDIUM/LOW plausibility)

- **CO-X1 — MEDIUM — Disconnect-mid-run timer-cancel coverage gap.** [TPC focus item 8] Promoted to S2-14.
- **CO-X2 — MEDIUM — `parse_tcp_seq_ack` stricter caller preconditions than `parse_syn_ack`.** [SMELL `test_server.rs:226-231` vs `:201,207,213`] Promoted to S2-15.
- **CO-X3 — MEDIUM — Fault-injector UAF detection sanitizer-dependent.** [Codex FYI `fault_injector_chain_uaf_smoke.rs:21,74,118`] Promoted to S2-16.
- **CO-X4 — MEDIUM — Hidden frame-layout coupling in tcpreq + test_server helpers.** [SMELL] Promoted to S2-17.
- **CO-X5 — MEDIUM — External packetdrill binary hidden behind `build.sh` from `build.rs`; not Rust-reviewable.** [FYI/SMELL] Promoted to S2-18.
- **CO-X6 — LOW — `counter-coverage.rs:921` documents increment site by source line, fragile across phases.** [DD] FYI; assertions are counter-name based, comments only.
- **CO-X7 — LOW — `test_server.rs:7` top comment describes initial passive-listen contract; reads as historical phase-local context.** [DD] FYI.
- **CO-X8 — LOW — `inject_rx_chain_smoke.rs:11,18` notes one stage of counter plan covered elsewhere.** [Obs gaps FYI] Test-plan accounting, not a bug.
- **CO-X9 — HIGH (negative) — No mechanical defect found in tcpreq sequence arithmetic; `wrapping_add` consistently applied.** [Probes mss/options/reserved/urgent/rst_ack] FYI / null finding.
- **CO-X10 — HIGH (negative) — No PAWS edge violation; wrap-boundary deltas explicitly tested in `proptest_paws.rs:132,160,164`.** FYI / null finding.
- **CO-X11 — HIGH (negative) — No atomic / memory-ordering bug in scoped `test_server.rs` (no atomics in scope) or scoped `Relaxed` uses (counters / stop flags only).** [`urgent.rs:104`, `rst_ack.rs:65`, `bench_alloc_hotpath.rs:142,417`, `multiseg_retrans_tap.rs:96,296`] FYI / null finding.
- **CO-X12 — HIGH (negative) — No RefCell borrow-chain lock-ordering risk in scoped `test_server.rs`; file is data + helpers only.** FYI / null finding.
- **CO-X13 — HIGH (negative) — No mempool/mbuf leak in scoped test-only injection paths; harness drops clear pinned mbufs before engine drop.** [`tcpreq-runner/lib.rs:222,228`, `common/mod.rs:471,477`, `multi_seg_chain_pool_drift.rs:391,405`] FYI / null finding.
- **CO-X14 — HIGH (negative) — No `unsafe` / `extern` / raw pointer manipulation in scoped tool crates.** FYI / null finding.

## Counts

Total: 36; BLOCK-A11: 0; STAGE-2: 19; DISPUTED: 0; AGREED-FYI: 4 (1 promoted); CLAUDE-ONLY: 20 (12 promoted to STAGE-2); CODEX-ONLY: 14 (5 promoted to STAGE-2)
