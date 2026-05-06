# Part 9 Cross-Phase Retro Synthesis
**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 9 — Layer H correctness gate
**Phases:** A10.5
**Inputs:** cross-phase-retro-part-9-claude.md, cross-phase-retro-part-9-codex.md

## Combined verdict

PROCEED-WITH-FIXES with a hard build-path block. Both reviewers independently identified that commit `8147404` (gate behind `test-server`) was applied to `tools/layer-h-correctness/Cargo.toml` + `lib.rs` but NOT to `scripts/layer-h-{smoke,nightly}.sh`, leaving both wrappers running `cargo build --release --workspace` with no feature flag — the binary is silently skipped on a clean tree (Cargo respects `required-features = ["test-server"]`) and the post-build existence check fails. Codex additionally surfaces a structural defect — `bench-e2e::workload::run_rtt_workload` drains and discards events from `engine.events()` before Layer H's `observe_batch` snapshots the queue, so the FSM/event oracle is mechanically starved — that Claude did not catch. Both reviewers also independently agree the row-14 corruption disjunction omits `tcp.rx_bad_csum` (the only counter that fires for TCP-payload corruption under offload-OFF software cksum verify); this fix is one line. Recommend fixing the script feature-flag, the event-drain ordering, and the disjunction omission before A11; defer the rest.

## BLOCK A11 (must-fix before next phase)

- **B-1 — Layer H wrapper scripts don't pass `--features test-server`; binary is silently skipped on clean tree.** Both reviewers flagged.
  - Claude AD-1 (P0): `scripts/layer-h-smoke.sh:94`, `scripts/layer-h-nightly.sh:101` — `cargo build --release --workspace` with no feature flag; `tools/layer-h-correctness/Cargo.toml:22` carries `required-features = ["test-server"]`; `Cargo.toml:36` sets `default = []`; `lib.rs:17` is `#![cfg(feature = "test-server")]`. Post-build existence check at `scripts/layer-h-smoke.sh:233-238` / `scripts/layer-h-nightly.sh:240-245` fails with `missing after build`.
  - Codex Verdict BUG + AD BUG + DD BUG: `tools/layer-h-correctness/Cargo.toml:16,22`; `scripts/layer-h-smoke.sh:93,229`; `scripts/layer-h-nightly.sh:100,236`. Adds the dirty-`target/` failure mode: a stale binary may be deployed instead of cleanly missing.
  - Severity: BUG (Codex) / P0 (Claude). Both agree.

- **B-2 — Event/FSM oracle is starved; `bench-e2e::run_rtt_workload` drains the engine event queue before Layer H's `observe_batch` snapshots it.** Codex-only.
  - Codex Verdict BUG + CPI BUG + OG BUG + DD SMELL: `tools/bench-e2e/src/workload.rs:185,187,204,205` (`drain_and_accumulate_readable` pops every event, drops non-readable kinds in catch-all); `tools/layer-h-correctness/src/workload.rs:127,139` (workload runs first, then `observe_batch`); `tools/layer-h-correctness/src/observation.rs:660,668` (the only path that pushes events into `EventRing`). Result: `StateChange`, `TcpRetrans`, `TcpLossDetected`, transient `Established → non-Established` transitions can be silently consumed before the oracle ever sees them, weakening the "FSM remains Established throughout assertion window" invariant the gate is built around.
  - Severity: BUG (Codex). Promoted to BLOCK because the gate's nominal contract (event replay) is mechanically not enforced; orchestrator-script breakage already forces a re-cut, and this fix lands in the same window.

- **B-3 — Row 14 corruption disjunction omits `tcp.rx_bad_csum`; spuriously false-fails under offload-OFF for TCP-payload corruption.** Both reviewers flagged.
  - Claude TP-4 (BUG-equivalent): `tools/layer-h-correctness/src/scenarios.rs:235-238` defines disjunction as `[eth.rx_drop_cksum_bad, ip.rx_csum_bad]`; software IP-cksum at `crates/dpdk-net-core/src/l3_ip.rs:104-114` covers IP HEADER only; TCP-cksum software verify at `crates/dpdk-net-core/src/engine.rs:4068` bumps `tcp.rx_bad_csum`, NOT `ip.rx_csum_bad`. Most corrupted bytes from `corrupt 0.01%` land in TCP payload (header is 20 B vs hundreds of payload B per segment). One-line fix: add `tcp.rx_bad_csum` to the group.
  - Codex Verdict BUG + CPI BUG: `tools/layer-h-correctness/src/scenarios.rs:235,236`; `crates/dpdk-net-core/src/tcp_input.rs:111,133` → `TcpParseError::Csum`; `crates/dpdk-net-core/src/engine.rs:4060,4063,4068`. Same fix.
  - Note: This becomes operationally observable once an offload-OFF run is added; today the offload-OFF path is never built/run by any script (Claude TP-3). Still BLOCK because the matrix self-describes as "passes under either build profile" — that's a load-bearing claim the implementation does not honor.
  - Severity: BUG (Codex) / TP-4 BUG (Claude). Both agree on counter and fix.

## STAGE-2 FOLLOWUP (real concern, deferred)

- **S2-1 — `scapy-fuzz-runner` carries the same `dpdk-net-core` non-optional + `test-inject` feature pattern that `8147404` fixed for layer-h-correctness.** Claude CPI-1; Codex did not flag (out of Part 9 scope). Already filed as Part 6 S2-1; re-cited because Part 9 explicitly asked.
  - `tools/scapy-fuzz-runner/Cargo.toml:8`. `test-inject` does NOT reroute `tx_frame` so the gateway-ARP symptom doesn't repro, but divergent `inject_rx_frame` at `crates/dpdk-net-core/src/engine.rs:6266` still gets pulled into every workspace consumer.

- **S2-2 — No CI / metadata gate on workspace feature unification across runner crates.** Claude CPI-2.
  - Both T11 (`f6280ab`) and T20 (`8147404`) regressions made it through master because workspace feature resolution isn't validated. A 1-line `cargo metadata` check asserting `dpdk-net-core`'s resolved feature set is exactly `default` would catch this class. Promoted at Part 6; re-flagged because Part 9's B-1 has the same root cause-class (no script-side validation).

- **S2-3 — Stale `DPDK_NET_FAULT_INJECTOR` env var can contaminate pure-netem Layer H runs.** Codex Hidden-coupling LIKELY-BUG.
  - `tools/layer-h-correctness/src/main.rs:185,187` only SETS the env var when scenarios contain an FI spec; never CLEARS it for pure-netem/smoke. `test-server` feature always pulls in `dpdk-net-core/fault-injector` (`Cargo.toml:39,45`). Engine reads `FaultConfig::from_env()` once at construction (`crates/dpdk-net-core/src/engine.rs:1553,1555`); `from_env` treats any nonempty inherited env var as active config (`crates/dpdk-net-core/src/fault_injector.rs:132,140`). Shell-exported value silently activates FI on rows that say `fault_injector: None`. Report header (`main.rs:403,417,419`) still shows `fi_spec = None` — report truth diverges from runtime truth.
  - Defer to Stage 2: low-likelihood in CI-driven runs but a forensic landmine for ad-hoc local debugging. Fix is `std::env::remove_var("DPDK_NET_FAULT_INJECTOR")` at the top of every pure-netem path or, preferably, unconditionally before the conditional set.

- **S2-4 — A10.5 covers only netem-stress correctness; max-throughput correctness gap.** Claude TP-1.
  - 30 s × 17 scenarios × 1 connection × 128 B never exercises 100k+ conn/s / 1+ Gbps stalls + retrans storms. The bugs T21/T22/T23 surfaced are exactly what A10.5 will never catch. Recommend an explicit non-coverage statement in spec §1/§2 OR a parallel A10.5b at max-throughput intensities.

- **S2-5 — RX-mempool-avail side-check is always-pass at smoke intensity.** Claude TP-2.
  - `tcp.rx_mempool_avail` sampled once/sec (`engine.rs:2515-2535`); smoke single-conn 128 B / 50 ms netem ≈ 20 pps; default RX mempool in thousands. Floor never approached. Spec §4 / §5.2 frames this as a meaningful PR #9 leak-detect gate; under smoke it's a no-op. Either document as nightly-only, raise the smoke floor for proof-of-read, or replace with `tcp.mbuf_refcnt_drop_unexpected delta == 0` (intensity-independent — fires on Drop with refcount > 8, a logic error not a level threshold).

- **S2-6 — Disjunction is offload-on-only in practice; offload-OFF arm of row 14 is never built/run.** Claude TP-3.
  - `dpdk-net-core` defaults include `hw-offload-rx-cksum` (`crates/dpdk-net-core/Cargo.toml:20`); orchestrator scripts pass nothing to `cargo build`. Spec §4 row 14 sells the assertion as "passes under either build profile"; only one is ever built. Add an offload-off invocation matrix entry to nightly OR drop the dual-profile claim from spec.

- **S2-7 — `EventKind::Other` fallthrough loses event timestamps.** Claude TD-1 + OG-3.
  - `tools/layer-h-correctness/src/observation.rs:240-249` emits `emitted_ts_ns: 0` for unhandled `InternalEvent` variants (`Readable`, `Writable`, `ApiTimer`). Forensically near-useless in failure bundles. One-line addition in `crates/dpdk-net-core/src/tcp_events.rs` to surface a `emitted_ts_ns` accessor; A10.5's "no engine-side changes" promise punts this.

- **S2-8 — Layer-h consumes `InternalEvent` directly, extending the Part 4 coupling concern.** Claude CA-2.
  - `observation.rs:16,121,167` matches on `InternalEvent::{Connected, StateChange, Closed, Error, TcpRetrans, TcpLossDetected}`. Two consumers (bench-ab-runner + layer-h) now exist; future engine-side reshape gets more expensive. Track a public re-export / type alias.

- **S2-9 — Bring-up boilerplate duplicated ~150 lines across `bench-nightly.sh` and both layer-h wrappers.** Claude DD-5; already noted in `scripts/layer-h-nightly.sh:21-22`. Fix-once-fix-twice maintenance hazard.

- **S2-10 — `--duration-override` arithmetic edge: `Instant + huge Duration` can panic.** Codex SMELL.
  - `tools/layer-h-correctness/src/main.rs:104,209`; `tools/layer-h-correctness/src/workload.rs:121,122`. Normal 30 s runs fine; large CLI value panics on instant overflow instead of returning an arg error. Trivial input-validation fix.

## DISPUTED (reviewer disagreement)

None. All shared findings have aligned severities (BUG ↔ BUG / P0); no classification mismatches.

## AGREED FYI (both reviewers flagged but not blocking)

- **AF-1 — RX-leak side-checks (`rx_mempool_avail`, `mbuf_refcnt_drop_unexpected`) are diagnostic counters, not a complete live leak detector.**
  - Claude TP-2 / OG-1: only `mbuf_refcnt_drop_unexpected delta == 0` is intensity-independent under smoke.
  - Codex Observability FYI: `tcp.rx_mempool_avail` sampled at most once/sec in `poll_once` (`crates/dpdk-net-core/src/engine.rs:2502,2534`); read by Layer H as floor (`tools/layer-h-correctness/src/observation.rs:679,683`). `tcp.mbuf_refcnt_drop_unexpected` bumped from `MbufHandle::Drop` (`crates/dpdk-net-core/src/mempool.rs:281,294,305`).
  - Both classify as informational/sampled-diagnostic. (Claude TP-2 is also surfaced as S2-5 because it has an actionable smoke-intensity remediation.)

- **AF-2 — Memory-ordering / ARM portability is clean.**
  - Claude § "Memory-ordering / ARM-portability concerns": all atomic accesses use `Ordering::Relaxed` for level reads; no `asm!` / `core::arch` / `target_arch`; `EventRing` is single-threaded `VecDeque`.
  - Codex FYI x2: same observation, with engine-side counter increments also relaxed (`crates/dpdk-net-core/src/counters.rs:804,810`). No acquire/release synchronization on counter-protected data.

- **AF-3 — Layer-h does not allocate or own mbufs; only `unsafe` is `rte_get_tsc_hz` + `rte_eal_cleanup`.**
  - Claude CA-1: `tools/layer-h-correctness/src/main.rs:194,366`. Both calls go through `dpdk-net-sys`, the public binding crate.
  - Codex FFI FYI x2: same conclusion; mbuf lifetime coverage is indirect through engine counters and `MbufHandle::Drop`.

- **AF-4 — Hardcoded `dev ens6` for peer netem cleanup despite dynamic `IFACE` discovery elsewhere.**
  - Claude FYI-7: `scripts/layer-h-smoke.sh:299` and `scripts/layer-h-nightly.sh:327` hardcode `dev ens6` while passing `--peer-iface ens6` to the binary.
  - Codex AD SMELL: `scripts/layer-h-smoke.sh:268,274,298,311`; `scripts/layer-h-nightly.sh:275,281,326,341` — peer prep selects `IFACE` dynamically, then cleanup + binary invocation hard-code `ens6`. Mechanically fragile if ENI naming changes on the AMI.

- **AF-5 — Reorder-50% base-delay fix is correctly applied at all sites.**
  - Claude FYI-4: verified at `tools/bench-stress/src/scenarios.rs:93`, `tools/bench-stress/src/netem.rs:227`, `scripts/bench-nightly.sh:545`, plus spec example.
  - Codex FYI: Layer H uses `delay 5ms reorder 50% gap 3` (`tools/layer-h-correctness/src/scenarios.rs:208,211`); bench-stress at `tools/bench-stress/src/scenarios.rs:87,93`; `scripts/bench-nightly.sh:542,545`.

## INDEPENDENT-CLAUDE-ONLY (HIGH/MEDIUM/LOW plausibility)

- **CL-HIGH-1 — AD-2 stale comment: `tools/layer-h-correctness/src/lib.rs:7` claims `Engine::state_of` is test-server-only, but `crates/dpdk-net-core/src/engine.rs:6659` carries no `#[cfg]` and is unconditionally public. `engine.rs:2451` "test-server-gated `state_of`" comment is also stale.** Plausibility HIGH — direct file read + grep verification. Misleads future maintainers about what they can/can't unbundle. Severity: documentation drift, not a correctness bug.

- **CL-HIGH-2 — AD-3 + DD-3 + AD-2 family of staleness around the `test-server` umbrella feature.** `tools/layer-h-correctness/Cargo.toml:42-48` `test-server = ["dep:bench-e2e", "dep:bench-stress", ...]`; neither `bench-e2e` nor `bench-stress` has its own `test-server` feature (verified). The `dep:` directives only enable optional deps; naming is misleading. Plausibility HIGH. Future foot-gun.

- **CL-MEDIUM-1 — TP-5 `rx_rst == 0` on row 14 is anchored to `tcp.rx_rst` but the smoke peer (`echo-server`) doesn't send RST on cksum errors — assertion passes regardless of whether corruption ever happens.** Plausibility MEDIUM — peer-behavior dependent. Defensive-coding gate that doesn't actually exercise its target failure mode.

- **CL-MEDIUM-2 — OG-2 `obs.events_dropped` per-batch delta side-check is a fire alarm without a backing observable under smoke.** Bounded event queue 1024 capacity (`crates/dpdk-net-core/src/tcp_events.rs:119`); single-conn rate < 100/s; queue never overflows. End-of-scenario `obs.events_dropped == 0` covers the same ground. Per-batch + per-scenario form is redundant under smoke. (Note: Codex independently flagged duplicate-assertion as a SMELL at S2 — see CX-MEDIUM-1.) Plausibility MEDIUM.

- **CL-MEDIUM-3 — TD-2 `#[allow(clippy::useless_conversion)]` at `observation.rs:258` papers over a Stage-2 newtype migration intent.** Forward-looking promise; if Stage 2 doesn't migrate `ConnHandle`, becomes accidental tech debt. Plausibility MEDIUM (depends on Stage-2 plan execution).

- **CL-LOW-1 — TD-3 two helpers with `#[allow(clippy::too_many_arguments)]` (`report.rs:203` 9 args, `workload.rs:54` 8 args).** Organic API growth; refactor to small `Args` struct. Marginal readability win. Plausibility LOW (acceptable for tooling crate).

- **CL-LOW-2 — FYI-6 17-row matrix partitioned by `partition_by_fi_spec` (`scenarios.rs:298-309`); orchestrator nightly hardcodes 4 invocations at `scripts/layer-h-nightly.sh:308-313`.** Both encodings agree today; if a 4th composed scenario added, both must update. Plausibility LOW.

- **CL-LOW-3 — FYI-1/2/3 pinning tests (`tests/scenario_parse.rs:22-39`, `observation.rs:23-24` constants, `workload.rs:243-249` warmup/observation iters).** Forward-deliberation pattern, not findings. Plausibility N/A.

## INDEPENDENT-CODEX-ONLY (HIGH/MEDIUM/LOW plausibility)

- **CX-HIGH-1 — `obs.events_dropped` is asserted twice for every scenario; matrix rows include it (e.g. `tools/layer-h-correctness/src/scenarios.rs:57,60`) AND `counters_snapshot.rs:36,37` injects it into global side-checks.** `tools/layer-h-correctness/src/workload.rs:166,176` evaluates scenario expectations then global side-checks, so a single nonzero delta produces duplicate `CounterRelation` failures. Doesn't change pass/fail; makes failure bundles noisier. Plausibility HIGH — direct file inspection. (Adjacent to Claude OG-2 on different angle.)

- **CX-HIGH-2 — TP wrapper-build coverage gap: integration tests use `CARGO_BIN_EXE_layer-h-correctness` (only available with feature-enabled invocation), wrappers do `cargo build --release --workspace`.** `tools/layer-h-correctness/tests/external_netem_skips_apply.rs:8,9`; `scripts/layer-h-{smoke,nightly}.sh:93/100`. Missing case is a script-level test or dry-run that proves the exact release artifact is produced post-`8147404`. Plausibility HIGH. (Companion to B-1.)

- **CX-HIGH-3 — Row-14 static test pins only the current incomplete disjunction (`tools/layer-h-correctness/tests/scenario_parse.rs:119,128`); doesn't exercise the software TCP-cksum path that bumps `tcp.rx_bad_csum`.** Lets the offload-off false-fail (B-3) survive static matrix tests. Plausibility HIGH. (Companion to B-3.)

- **CX-MEDIUM-1 — Report header records `fault_injector` feature compile state (`main.rs:403,417`) but only the selected matrix FI spec (`main.rs:419`); under stale-env contamination (S2-3), header shows `fi_spec = None` while engine actually used inherited FI env.** Plausibility MEDIUM (couples report truth to external shell state). Companion to S2-3.

## Counts

Total: 28; BLOCK-A11: 3; STAGE-2: 10; DISPUTED: 0; AGREED-FYI: 5; CLAUDE-ONLY: 7 (2 HIGH, 3 MEDIUM, 2+ LOW collapsed); CODEX-ONLY: 4 (3 HIGH, 1 MEDIUM)
