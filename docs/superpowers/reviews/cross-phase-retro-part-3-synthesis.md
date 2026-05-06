# Part 3 Cross-Phase Retro Synthesis

**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 3 — Loss recovery + observability (RACK/RTO/TLP/retransmit + event log + RTT histogram)
**Phases:** A5, A5.5, A5.6
**Inputs:** cross-phase-retro-part-3-claude.md, cross-phase-retro-part-3-codex.md

## Combined verdict

**CONDITIONAL SHIP — pending review of 4 BUG-class findings raised by Codex that Claude did not surface.**

Claude verdict was SHIP (no blocking architectural defects; gates uniform, timer-cancel discipline preserved, bug_008 RACK migration sound). Codex verdict identified four mechanical BUG-class findings: TLP and RACK both treat a void `retransmit()` primitive as success-signaling (counters/probes record success even on ENOMEM/stale-entry early returns); the documented `event_queue_soft_cap = 4096` default is rejected at the C ABI entry point (zero-init fails with NULL instead of substituting the default); and invalid RTO bound ordering reaches a runtime clamp panic in release builds because validation is `debug_assert!`-only. The histogram spec/code disagreement at 30000 µs is the one finding both reviewers independently surfaced. None of the BUG findings are architectural defects — they are mechanical surface-area gaps in the A5/A5.5 contract — but each is a release-build runtime hazard that should be addressed before A11.

## BLOCK A11 (must-fix before next phase)

### B1 — Invalid RTO bounds reach a release-build clamp panic
- **Severity:** BUG (Codex)
- **Source:** Codex C-ABI / FFI section
- **Sites:** `crates/dpdk-net-core/src/engine.rs:5169-5171`, `crates/dpdk-net-core/src/tcp_rtt.rs:26-28`, `:51-53`
- `EngineConfig` exposes `tcp_min_rto_us`, `tcp_initial_rto_us`, `tcp_max_rto_us` as public C-ABI knobs; `RttEstimator::new` only `debug_assert!`s ordering. Release builds accept `min > max`; first sample reaches `u32::clamp(min, max)` which panics. Validation must be added at `Engine::new` / C ABI creation time alongside the existing histogram-edges validation.
- **Rationale for BLOCK-A11:** Release-build panic on caller-supplied config is a public-ABI hazard; A11 introduces additional config surfaces and would inherit the hole.

### B2 — `event_queue_soft_cap` documented default not applied at C ABI entry
- **Severity:** BUG (Codex)
- **Source:** Codex Observability gaps section
- **Sites:** `crates/dpdk-net/src/lib.rs:149-151`, `include/dpdk_net.h:105-107`, `crates/dpdk-net-core/src/engine.rs:1465-1467`
- `dpdk_net_engine_create` rejects `event_queue_soft_cap < 64` BEFORE default substitution. Header documents "Default 4096; must be >= 64" — a zero-initialized C config that relies on documented defaults fails with NULL. Either substitute `0 → 4096` before the bound check (matches the `0 → tcp_min_rto_us` pattern at `lib.rs:919-921`) or drop the documented default from the header.
- **Rationale for BLOCK-A11:** Public-ABI contract drift directly observable by zero-init C callers; cheap to fix; A11 inherits.

## STAGE-2 FOLLOWUP (real concern, deferred)

### S1 — TLP fire records probe success even when retransmit queued nothing
- **Severity:** BUG (Codex)
- **Source:** Codex Cross-phase invariant violations section
- **Sites:** `crates/dpdk-net-core/src/engine.rs:3206-3208`, `:3221-3225`, `:5879-5881`, `:5933-5935`, `:6221-6223`
- `on_tlp_fire` calls void `retransmit()`, then unconditionally increments `tcp.tx_tlp`, records the probe, consumes the TLP budget, and may emit `TcpRetrans`/`TcpLossDetected`. Under header-mbuf exhaustion, stale-index drift, or chain failure, counter and probe state diverge from on-wire reality. Repair via `RetransmitOutcome` enum or analogous result.
- **Why STAGE-2 not BLOCK:** Forensic counter divergence under ENOMEM is observability accuracy, not a correctness/UAF/leak; current behavior is fail-safe (over-reports probes, no spurious frame). Triage rule prefers STAGE-2 over BLOCK when uncertain.

### S2 — RACK loss accounting same void-retransmit success assumption
- **Severity:** BUG (Codex)
- **Source:** Codex Cross-phase invariant violations section
- **Sites:** `crates/dpdk-net-core/src/engine.rs:4286-4288`, `:4294-4299`
- A5 counter contract says `tcp.tx_rack_loss` means RACK fired AND a retransmit was queued. HEAD increments immediately after void `self.retransmit(...)`; ENOMEM/stale-entry produces `tx_rack_loss` and per-packet `TcpRetrans` events without `tx_retrans` or an actual queued frame. Same fix as S1.
- **Why STAGE-2 not BLOCK:** Same reasoning as S1; sibling defect, single repair (return type on retransmit primitive).

### S3 — `retransmit()` primitive returns `()` despite multiple callers needing success/failure
- **Severity:** SMELL (Codex)
- **Source:** Codex Tech debt accumulated section
- **Sites:** `crates/dpdk-net-core/src/engine.rs:5824-5826`, `:6161-6163`
- Underlying tech-debt that produces S1+S2. Repair shape suggested: `enum RetransmitOutcome { Queued { seq, xmit_count }, NoSuchEntry, NoBackingMbuf, NoMem }`.

### S4 — RTT sampler retains u32-µs scheme; sibling defect to bug_008 not covered by that fix
- **Severity:** FYI (Claude, but with concrete sample-loss window)
- **Source:** Claude Memory-ordering / ARM-portability section
- **Sites:** `crates/dpdk-net-core/src/tcp_input.rs:912, 918, 945`, `crates/dpdk-net-core/src/tcp_rack.rs:159-169`
- bug_008 migrated only the RACK age helper to u64-ns. RTT sampler still truncates `now_ns / 1_000` to `u32` µs and `wrapping_sub`s. On long-lived flow at the ~71-min u32-µs boundary, sampler silently discards legitimate Karn-RTT samples for ~60s once per ~71 min. RTO defaults stand; estimator goes stale not wrong.
- **Why STAGE-2:** Sample loss not value corruption; trading flows are not typically 71-min idle but Stage-2 may have long-lived control flows.

### S5 — `obs.events_queue_high_water` `fetch_max` Relaxed loses monotonicity under multi-engine merge
- **Severity:** FYI / Stage-2 explicit (Claude)
- **Source:** Claude Observability gaps section
- **Site:** `crates/dpdk-net-core/src/tcp_events.rs:175-178`
- Correct for single-lcore RTC; flagged for Stage-2 ARM/multi-engine aggregation.

### S6 — `EventQueue::with_cap` caps initial heap capacity at `DEFAULT_SOFT_CAP=4096` even when caller asks for higher
- **Severity:** SMELL (Claude)
- **Source:** Claude Architectural drift section
- **Site:** `crates/dpdk-net-core/src/tcp_events.rs:148`
- Any soft_cap above 4096 incurs amortized VecDeque growth from 4096 to cap. Inconsistent with Stage-1 audit's "pre-size to high-water mark" pattern (`tcp_conn.rs:420`, `:430`, `tcp_timer_wheel.rs:90`).

### S7 — RACK reaches into `tcp_retrans::RetransEntry` directly (layering inversion)
- **Severity:** SMELL / Stage-2 explicit (Claude)
- **Source:** Claude Hidden coupling section
- **Site:** `crates/dpdk-net-core/src/tcp_rack.rs:6`
- Stage-2 abstraction debt; alternate retrans-queue shapes would force RACK updates first.

### S8 — `tlp_config()` translates `u32::MAX` sentinel in core, not in ABI translator
- **Severity:** SMELL (Claude)
- **Source:** Claude Hidden coupling section
- **Sites:** `crates/dpdk-net-core/src/tcp_conn.rs:551-561`, `crates/dpdk-net/src/lib.rs:919-921`
- Two-stage substitution split across crate boundaries; today single-use-site safe but a Stage-2 site that reads `tlp_pto_min_floor_us` directly would project `u32::MAX` as a 71-min floor.

### S9 — `LossCause` enum lacks `#[repr(u8)]` despite ABI `u8` wire encoding
- **Severity:** FYI (Claude)
- **Source:** Claude FYI section
- **Site:** `crates/dpdk-net-core/src/tcp_events.rs:13-25`
- A future variant insertion (e.g. `Bbr` between `Rack` and `Tlp`) would silently shift the wire encoding. Add `#[repr(u8)]` to pin.

## DISPUTED (reviewer disagreement)

### D1 — Severity of the cbindgen header missing the wraparound + threading contract
- **Claude:** Treats as documentation drift requiring action ("copy spec §3.3's full contract block into the Rust doc-comment"). Argues spec §3.3 line 117-119's "do not read from a different thread than the engine's poll thread" is a real concurrency contract that the public ABI silently drops.
- **Codex:** Did not flag.
- **Triage:** Independent-Claude-only finding; not a reviewer disagreement strictly. Listed here because Claude implies action while Codex implicitly cleared it. Reclassified to CLAUDE-ONLY below for clarity.

### D2 — `Drop for Engine` Step 1 comment misrepresents `snd_retrans` mbuf reclamation
- **Claude:** Documentation drift — comment says "drops every TcpConn, which drops ... snd_retrans ... mbufs back into still-alive mempools" but `RetransEntry.mbuf` is non-owning `crate::mempool::Mbuf` with no `Drop`.
- **Codex:** Explicitly inspected the retransmit rollback / final-release path looking for "PR #9-style final-release mbuf leak" and did NOT find a new defect; "the final-release paths that mattered for PR #9 use `shim_rte_pktmbuf_free_seg`, and the TX retransmit drift regression documents the rollback pair as the intended invariant."
- **Resolution:** Both reviewers agree there is no functional leak/UAF. Claude flags the comment as misleading; Codex implicitly accepts the mempool-teardown reclamation as sufficient. Treat as DISPUTED severity (Claude: documentation drift requiring fix; Codex: not a defect). Recommend resolving by updating the comment per Claude's suggestion.

## AGREED FYI (both reviewers flagged but not blocking)

### A1 — A5.6 histogram spec/code disagreement at 30000 µs (bucket 11 vs 12)
- **Severity:** SMELL (both)
- **Source:** Claude Test-pyramid concerns + FYI; Codex Test-pyramid concerns
- **Sites:** `crates/dpdk-net-core/src/rtt_histogram.rs:28-31, 79-83`, `docs/superpowers/specs/2026-04-19-stage1-phase-a5-6-rtt-histogram-design.md:193-195`
- Runtime ladder is internally consistent (`30000 > 25000` AND `30000 <= 50000` → bucket 12 under inclusive `<=` rule). Spec §7.1 expected output says bucket 11. Unit test resolves by declaring code source-of-truth instead of fixing the doc. Both reviewers agree: fix the spec.

### A2 — A5.6 has no standalone phase tag; histogram code is A6-labeled at HEAD
- **Severity:** FYI (both)
- **Source:** Claude Verification trace; Codex Architectural drift
- **Sites:** `docs/superpowers/specs/2026-04-19-stage1-phase-a5-6-rtt-histogram-design.md:1-3`, `crates/dpdk-net-core/src/engine.rs:23-25`
- Matches task brief (design doc says ABSORBED INTO A6); not a defect.

## INDEPENDENT-CLAUDE-ONLY (only Claude flagged)

### CO1 — cbindgen-emitted header drops wraparound + threading contract for `dpdk_net_tcp_rtt_histogram_t`
- **Severity:** SMELL — **Plausibility: HIGH**
- **Sites:** `crates/dpdk-net/src/api.rs:280-293`, `include/dpdk_net.h:441-449,668-688`, spec §3.3 lines 96-123
- Claude verified the Rust doc-comment is 4 lines vs spec's ~28-line contract. Mechanical and verifiable. Action: copy spec §3.3 contract into the Rust doc-comment so cbindgen propagates it.

### CO2 — No test asserts `tcp_per_packet_events=false` no-op invariant on RACK/TLP/RTO emit blocks
- **Severity:** SMELL — **Plausibility: HIGH**
- **Sites:** `crates/dpdk-net-core/src/engine.rs:4293` (and sibling RTO/TLP blocks)
- Regression that drops the runtime gate would not be caught by `obs_smoke.rs` or any retrans-tap test. Claude proposes a single integration scenario `tcp_per_packet_events_off_emits_zero_per_packet_events_under_loss` with one counter assertion.

### CO3 — `obs.events_dropped` overflow soft-cap path FIFO + counter behavior not directly tested
- **Severity:** SMELL — **Plausibility: HIGH**
- **Site:** `crates/dpdk-net-core/src/tcp_events.rs:168-171, 209-235`
- "push N+1 events, assert q.len() == N AND `obs.events_dropped == 1`" not present. Protects against pop_front/counter-inc reordering under panic.

### CO4 — `tcp_rack.rs:107-117 rack_mark_losses_on_rto` wrapper kept "for tests" only — fork-maintenance hazard
- **Severity:** SMELL — **Plausibility: MEDIUM**
- **Sites:** `crates/dpdk-net-core/src/tcp_rack.rs:107-117`, `crates/dpdk-net-core/src/tcp_retrans.rs:101-141`
- Engine uses only `_into` form. Same dual-API shape in `tcp_retrans::prune_below`. Acceptable today.

### CO5 — `tcp_timer_wheel.rs:6-8` module-level `#![allow(dead_code)]` is now stale
- **Severity:** SMELL — **Plausibility: HIGH**
- **Site:** `crates/dpdk-net-core/src/tcp_timer_wheel.rs:6-8`
- A6 fully consumes every variant; the blanket allow now hides genuine future drift. Mechanical fix.

### CO6 — `rtt_histogram.rs:9 #[repr(C, align(64))]` hardcodes 64-byte cache line; ARM portability
- **Severity:** FYI — **Plausibility: HIGH**
- **Site:** `crates/dpdk-net-core/src/rtt_histogram.rs:9` (and 5 sites in `counters.rs`)
- ARM Neoverse-N1 = 64 B, ThunderX2 / Apple silicon = 128 B. Today single per-conn histogram so no false-sharing today. Stage-2 ARM port concern; consistent with project_arm_roadmap memory.

### CO7 — `Drop for Engine` Step 1 comment overstates `snd_retrans` mbuf reclamation
- **Severity:** SMELL — **Plausibility: HIGH** (functionally fine, comment misleading)
- **Site:** `crates/dpdk-net-core/src/engine.rs:6556-6558`
- See D2 above. No functional defect. Comment fix recommended.

### CO8 — `tcp_timer_wheel.rs:1-8` module doc says "A6 adds public timer API on top" — past-tense drift
- **Severity:** FYI — **Plausibility: HIGH**
- **Site:** `crates/dpdk-net-core/src/tcp_timer_wheel.rs:1-8`
- Trivial documentation freshness.

### CO9 — `tcp_tlp.rs:50-51 Default::default() = a5_compat(5_000)` hard-codes A5 default floor
- **Severity:** SMELL — **Plausibility: MEDIUM**
- **Site:** `crates/dpdk-net-core/src/tcp_tlp.rs:50-51`
- A5.5 made `floor_us` engine-configurable. `Default::default()` now exclusively for tests. Recommendation: gate behind `#[cfg(test)]`.

### CO10 — No `cache_line_size!()` macro; every alignment is a hard `align(64)` literal
- **Severity:** FYI — **Plausibility: HIGH**
- **Sites:** `counters.rs:6, 113, 131, 307, 779`, `rtt_histogram.rs:9`
- Single-source-of-truth refactor target before any aarch64 port. Sibling to CO6.

## INDEPENDENT-CODEX-ONLY (only Codex flagged)

### CX1 — Ignored synthetic-peer TAP scenarios don't force ENOMEM around `on_tlp_fire`/RACK fire
- **Severity:** SMELL — **Plausibility: HIGH**
- **Site:** `crates/dpdk-net-core/tests/tcp_rack_rto_retrans_tap.rs:50-58`
- Direct corollary to S1+S2: void-return accounting bug invisible without injected header-mempool allocation failure. Targeted unit/harness test required to land alongside the `RetransmitOutcome` repair.

### CX2 — `tcp_retrans.rs` `hdrs_len` field doc says "set to 0 after first retransmit" but HEAD intentionally leaves it unchanged
- **Severity:** SMELL — **Plausibility: HIGH**
- **Sites:** `crates/dpdk-net-core/src/tcp_retrans.rs:36-40`, `crates/dpdk-net-core/src/engine.rs:6212-6216`
- Stale field doc in the exact mbuf-shape area reviewers look at for retrans leaks / duplicate-header bugs. Mechanical doc fix.

## Counts
Total: 24; BLOCK-A11: 2; STAGE-2: 9; DISPUTED: 1 (D2; D1 reclassified to CO1); AGREED-FYI: 2; CLAUDE-ONLY: 10; CODEX-ONLY: 2
