# Part 5 Cross-Phase Retro Synthesis
**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 5 — ENA HW offloads
**Phases:** A-HW, A-HW+
**Inputs:** cross-phase-retro-part-5-claude.md, cross-phase-retro-part-5-codex.md

## Combined verdict

A-HW shipped a clean compile-gate / runtime-latch / spec-banner architecture; A-HW+ added tasteful ENA bring-up checks (WC mapping, LLQ overflow guard, xstats scrape, devarg builder) that are correctly slow-path-only. Both reviewers independently confirmed the **NIC-reported IPv4 checksum BAD double-bump** of `ip.rx_csum_bad` originating in A-HW Task 8 (`e2aae95`), located at `l3_ip.rs:213-220` plus `engine.rs:3928-3931`. This is the same finding flagged in `cross-phase-retro-part-1-codex.md:19,33,39,133` and is correctly attributed here in Part 5. The L4 path is single-bump (correct); the asymmetry is IP-only. Beyond the BUG, the rest of the surface is architectural drift, doc drift, and test-pyramid coverage gaps that are appropriately Stage-2 follow-ups.

## BLOCK A11 (must-fix before next phase)

- **B-1 — `ip.rx_csum_bad` double-bump on NIC-reported IPv4 checksum BAD.** [BUG]
  - Sites: `crates/dpdk-net-core/src/l3_ip.rs:213-220` (offload-aware wrapper bumps `eth.rx_drop_cksum_bad` AND `ip.rx_csum_bad`, returns `Err(L3Drop::CsumBad)`) → `crates/dpdk-net-core/src/engine.rs:3928-3931` (caller's `L3Drop::CsumBad` arm bumps `ip.rx_csum_bad` again).
  - Cited by: Claude (CPI-1), Codex (Verdict-BUG, CPI-1, OG-1).
  - Cross-reference: `cross-phase-retro-part-1-codex.md:19,33,39,133` flagged this from Part 1; both reviewers agree the *origin* is A-HW Task 8 commit `e2aae95`. The doc-comment at `engine.rs:3890` documents the *intended* single-bump semantics. L4 is correct (`engine.rs:4034-4048` returns early on NIC-BAD, single-bump).
  - Fix shape (Codex): move the IP counter bump to exactly one layer, OR return a distinct NIC-BAD drop kind that the caller does not count.
  - Test gap (both reviewers): no integration test drives a NIC-BAD frame; counter-coverage uses synthetic `bump_counter_one_shot` harnesses.

## STAGE-2 FOLLOWUP (real concern, deferred)

- **S-1 — Test-server `port_id == u16::MAX` bypass silently disables every offload latch.** [Claude AD-2 / DD-2]
  - `crates/dpdk-net-core/src/engine.rs:1303-1311` zeros `tx_cksum_offload_active`, `rx_cksum_offload_active`, `rss_hash_offload_active` whenever `port_id == u16::MAX`. A-HW spec §3.3 / §6.1 / §7.1 / §8.2 describe the latches as compile-feature AND `dev_info` only — the test-server short-circuit is undocumented. Result: test-server unit tests with `hw-offload-tx-cksum` exercise the *software-fallback* path, masking divergence from production. Fix: document or refactor to a single `port_id_is_synthetic()` helper.

- **S-2 — A-HW+ "M1" overflow constants duplicated, not reused.** [Claude AD-3]
  - `(WORST_CASE_HEADER, LLQ_DEFAULT_HEADER_LIMIT, LLQ_OVERFLOW_MARGIN) = (94, 96, 6)` is hand-written at `crates/dpdk-net-core/src/engine.rs:1678-1680` AND `crates/dpdk-net-core/tests/knob-coverage.rs:580-582`. A future ceiling change (256B is on the ENA roadmap) could be applied to one site only. Move to `crate::dpdk_consts` or `pub(crate)`.

- **S-3 — `bench-offload-ab` does not exercise the runtime latch.** [Claude CPI-3]
  - `tools/bench-offload-ab/src/matrix.rs:66-115` rebuilds with cargo features (compile-time gate only); the runtime `*_offload_active=false` fallback branches in `tcp_output.rs::tx_offload_finalize` and `l3_ip.rs::ip_decode_offload_aware` are never measured on real ENA. `ahw_smoke.rs` Task 16 covers correctness only. Stage 2: add a runtime-latch perf scenario.

- **S-4 — No integration test drives a NIC-BAD frame; coverage tests are synthetic-bump harnesses.** [Claude CPI-2 / TP-2, Codex TP-1 / TP-2]
  - `crates/dpdk-net-core/tests/counter-coverage.rs:391-450` and `377-388` use `h.bump_counter_one_shot("eth.foo")` then `assert_counter_gt_zero`. The closest real-path test (`l3_ip.rs:343-353` `bad_csum_dropped_when_verifying`) exercises only the *software*-verify path. This is exactly why B-1 stayed undetected. Add a focused unit around `ip_decode_offload_aware` with `RTE_MBUF_F_RX_IP_CKSUM_BAD` + caller-level handling.

- **S-5 — `ahw_smoke.rs` Task 16 (SW-fallback) is the only behavioral test for the runtime latch.** [Claude TP-1]
  - net_tap PMD expected values are pinned by comment only (no version assertion); no offload-on behavioral test on net_tap; A8.5 tests don't gate on offload state. Stage-2 cargo feature-matrix CI already runs but is build-only, not behavioral.

- **S-6 — A-HW+ phase review "tail-append" claim is now historically true but not currently true.** [Claude AD-1 / DD-4]
  - `phase-a-hw-plus-mtcp-compare.md:96` asserts `ena_large_llq_hdr` + `ena_miss_txc_to_sec` are appended at the end of `EngineConfig` / `dpdk_net_engine_config_t`. True at A-HW+ ship (commit `d25e3a6`); A6.6-7 T10 (`080cbf3`) appended `rx_mempool_size` after. ABI is stable (CA-2); the review wording is just stale. Update doc.

- **S-7 — A-HW+ spec file is missing from `docs/superpowers/specs/`.** [Claude verification-trace gap]
  - Only the implementation plan (`docs/superpowers/plans/2026-04-20-stage1-phase-a-hw-plus-ena-obs-knobs.md`) and phase reviews exist. No `docs/superpowers/specs/2026-04-20-stage1-phase-a-hw-plus-*.md`. Notable doc gap.

- **S-8 — RX timestamp dynflag mask construction not range-checked.** [Codex Tech-debt-2 / CA-1]
  - `crates/dpdk-net-core/src/engine.rs:1396-1404` uses `Some(1u64 << flag_bit)` after only `flag_bit >= 0`. ENA steady state returns `None` so unobserved today; a buggy shim/PMD with `flag_bit >= 64` could panic in debug. Use `checked_shl` or explicit `< 64` guard.

- **S-9 — Half-initialized PMD on bring-up errors after `rte_eth_dev_start`.** [Codex HC-1 / HC-2]
  - `Engine::new` runs `rte_eth_dev_configure` → queue setup → `rte_eth_dev_start`, then LLQ verify, RSS RETA, dynfield lookup, MAC lookup, xstat resolve at `engine.rs:1313-1440`. Errors after start (LLQ verify `1362-1367`, RSS RETA `1371-1375`, MAC lookup `1427-1433`) skip `Engine::drop`'s `rte_eth_dev_stop`/`close` cleanup at `engine.rs:6605-6609`. Add a port-start guard so cleanup is mechanical, not relying on post-start steps being infallible.

- **S-10 — `tx_offload_rewrite_cksums` `pseudo_len` is `wrapping_add` with only debug-assert bound.** [Codex Tech-debt-1]
  - `crates/dpdk-net-core/src/tcp_output.rs:270` and `223-234`. Current callers (`build_segment_inner` `115-118`) reject overflow; helper is `pub(crate)` and accepts arbitrary `tcp_hdr_len`/`payload_for_csum_len`. Release-build oversized callers would silently truncate. Add non-debug bound check.

- **S-11 — `Engine.driver_name` captured but never read; `#[allow(dead_code)]` masks it.** [Claude TD-1]
  - `engine.rs:814`. Either expose `pub fn driver_name(&self) -> &[u8]` for diagnostics tools, or drop the field and the 32-byte buffer.

- **S-12 — Stale comments at `engine.rs:778`, counter-coverage line refs.** [Claude TD-2 / DD-3, Codex DD-2]
  - `engine.rs:778` says "the `#[allow(dead_code)]` attributes go away as tasks 7/9/12 wire these latches" — those attrs are gone; remaining `allow`s are on `applied_*_offloads`. Counter-coverage refs (e.g. `engine.rs ~1011` for RX timestamp at `tests/counter-coverage.rs:368-374`) are stale; live site is `engine.rs:1404-1408`. Refresh.

- **S-13 — `and_offload_with_miss_counter` `#[allow(dead_code)]` justification is stale.** [Claude TD-3]
  - `engine.rs:1081`. Comment cites "the test module always exercises it" — there are no tests for this function. The real reason is `--no-default-features` compiles the AND branch away. Update comment.

- **S-14 — `dpdk_net_recommended_ena_devargs` lacks platform doc note.** [Claude TD-4]
  - Function returns devarg string regardless of platform; ENA only exists on AWS Linux x86_64/ARM64. Doc-comment doesn't note "no-op on non-ENA hosts". Minor.

- **S-15 — `xstat_map` private; no resolution-status accessor for forensics.** [Claude OG-1]
  - On a partial-resolution PMD, all-zero output of `scrape_xstats` is indistinguishable from "no events" vs "PMD didn't expose this name". `XstatMap.ids: Vec<Option<u64>>` already encodes the distinction internally; expose via `pub fn xstat_resolution(&self) -> &[bool]` or FFI getter.

- **S-16 — RX timestamp dynfield steady-state not asserted in counter snapshot.** [Claude OG-2]
  - `eth.offload_missing_rx_timestamp == 1` on ENA is healthy (spec §10.5), but operators reading the snapshot have no in-process signal. Add `dpdk_net_event_t.flags & DPDK_NET_EVENT_FLAG_HW_TS_VALID` (Stage 2 per spec §15).

- **S-17 — `EthCounters._pad: [AtomicU64; 2]` near boundary; document next-bump trigger.** [Claude OG-4]
  - Stage-2 multi-queue widening crosses the next 16-B boundary. Const-assert at `crates/dpdk-net/src/api.rs:423` is one-direction equality; future shrink would still pass. Document next-bump threshold at `counters.rs:107-110`; add a back-reference comment at `_pad` declaration.

- **S-18 — Spec §6.3 "UDP analog" unresolved; current behavior diverges from spec.** [Claude DD-1]
  - A-HW spec §6.3 said: "omitted if no UDP TX path." HEAD `engine.rs:1754-1760` DOES include `RTE_ETH_TX_OFFLOAD_UDP_CKSUM` in `requested_tx_offloads`. Harmless (no UDP segments emitted); spec or code should converge.

- **S-19 — `Cargo.toml` RSS feature comment promises "consume it when present"; live `lookup_by_hash` ignores it.** [Codex DD-1]
  - `crates/dpdk-net-core/Cargo.toml:48-50` vs `crates/dpdk-net-core/src/flow_table.rs:157-176` (HashMap-backed; `bucket_hash` ignored, key is full tuple). Mechanically safe; benefit deferred. Update comment.

- **S-20 — `dpdk_net_recommended_ena_devargs` lacks typed error enum.** [Claude CA-3]
  - Hand-rolled `-EINVAL`/`-ERANGE`/`-ENOSPC` returned via `i32`. Add typed `dpdk_net_devargs_err_t` for C++ callers. Stage-2 polish.

- **S-21 — Real-ENA wire-drive correctness test deferred from A-HW Task 18 to A10 bench-e2e (latency only).** [Claude TP-3]
  - `ahw_smoke_ena_hw.rs` requires CAP_NET_ADMIN + live ENA VF; A10 measures latency, not on-wire bytes from offload path vs software path. Minor gap.

## DISPUTED (reviewer disagreement)

- **D-1 — Architectural drift in HW-feature module isolation.** Codex (SMELL, AD-2) flags that A-HW behavior is NOT isolated in an `offloads.rs` module; TX cksum is in `tcp_output.rs`, RX is split between `l3_ip.rs` and `engine.rs`, RSS in `flow_table.rs`, latches in `engine.rs`. Codex says this is a reviewability smell. Claude does not flag this; Claude's AD-1..AD-4 instead frame the dispersion as legible (per-feature hot-path branches "well-localised" in the verdict). Triage: STAGE-2 FOLLOWUP. Codex's reviewability concern is real but does not change runtime behavior; isolation refactor is Stage-2 cleanup.

## AGREED FYI (both reviewers flagged but not blocking)

- **F-1 — Per-engine `*_offload_active` latches are plain `bool`, not atomics. CORRECT given single-threaded construction.** [Claude MA-5, Codex FYI Memory-ordering-1]
  - Writes in `Engine::new` only; reads on owning lcore; `Engine` is `!Sync`. No Acquire/Release publication concern. ARM port: `bool` is 1-byte naturally aligned everywhere.

- **F-2 — Offload counters use `Relaxed` atomics. CORRECT for observability-only counters.** [Implicit in Claude MA-5, Codex FYI Memory-ordering-2]
  - `engine.rs:1089-1099`, `l3_ip.rs:214-218`, `engine.rs:4038-4047`, `ena_xstats.rs:73-93`. No correctness dependency on synchronizing order.

- **F-3 — L4 NIC-BAD path is single-bump (CORRECT); asymmetry vs IP path is the bug surface for B-1.** [Claude CPI-1 implicit, Codex CPI-2 FYI]
  - `engine.rs:4034-4048` increments `eth.rx_drop_cksum_bad` + `tcp.rx_bad_csum` then returns; software path increments only `tcp.rx_bad_csum` in `TcpParseError::Csum` arm at `engine.rs:4060-4073`.

- **F-4 — Counter-coverage tests for `eth.rx_drop_cksum_bad` are synthetic bump harnesses, not real-path hits.** [Claude CPI-2, Codex TP-1]
  - `tests/counter-coverage.rs:377-388` documents this explicitly; explains why B-1 stayed undetected.

## INDEPENDENT-CLAUDE-ONLY (only Claude flagged)

- **CO-1 (HIGH) — `xstat_map` unconditionally allocated; `wc_verify` x86_64-gated; `llq_verify` feature-gated. Heterogeneous gating shape is illegible.** [AD-4]
- **CO-2 (HIGH) — `wc_verify` does on-bring-up filesystem read of `/sys/kernel/debug/x86/pat_memtype_list`.** [HC-2] Unmocked filesystem read on engine_create critical path; slow-path-only but worth flagging.
- **CO-3 (MEDIUM) — `llq_verify` uses process-global `OnceLock<LlqVerdict>`.** [HC-3] Multi-engine multi-port future Stage-2 will need per-port verdicts; non-trivial refactor.
- **CO-4 (MEDIUM) — `ena_xstats::resolve_xstat_ids` runs unconditionally on non-ENA PMDs.** [HC-4] Two failed FFI calls per `engine_create`; documented; correct posture.
- **CO-5 (MEDIUM) — `mbuf.hash.rss` shim accessor lacks endianness convention comment.** [MA-4] DPDK normalizes to host-endian; one-line comment recommended.
- **CO-6 (LOW) — `Engine::scrape_xstats(&self)` is `pub` not `pub(crate)`; cooperative cadence enforcement only.** [I-2] Documented at `engine.rs:2030-2033`; recommendation only.
- **CO-7 (LOW) — `dpdk_net_recommended_ena_devargs` validates `miss_txc_to_sec > 60` but not `large_llq_hdr ∈ {0,1}`.** [I-3] Caller passing `large_llq_hdr=7` gets `large_llq_hdr=1`; documented.
- **CO-8 (LOW) — Six `hw-*` features + `hw-offloads-all` meta naming convention is consistent.** [I-4] Informational.
- **CO-9 (LOW) — `_pad` shrunk from `[_;9]` to `[_;2]`; const-assert at `crates/dpdk-net/src/api.rs:423` is the authoritative ABI mirror.** [I-5] Recommend a back-reference comment at the `_pad` site.
- **CO-10 (LOW) — A-HW spec §10 RX-timestamp `tcp_events.rs:164` comment was already obsolete by Task 11.** [I-1] Spec wording aged out; not a code bug.

## INDEPENDENT-CODEX-ONLY (only Codex flagged)

- **CX-1 (MEDIUM) — Scoped filenames in the prompt no longer map 1:1 to HEAD.** [Codex AD-1] `eal.rs`, `offloads.rs`, `l2_eth.rs` absent; live names are `l2`, `l3_ip`, `llq_verify`, `wc_verify`, `ena_xstats`. Documentation/orientation note for future reviewers.
- **CX-2 (MEDIUM) — Feature-off knob test for RX cksum offload is good but does not exercise default feature-on NIC-BAD path.** [Codex TP-3] `tests/knob-coverage.rs:755-790` calls `ip_decode_offload_aware` with `ol_flags=u64::MAX, rx_cksum_offload_active=true` and expects software verification — good offload-off coverage; on-path coverage gap restated.
- **CX-3 (LOW) — ENA xstat scrape error path preserves cumulative counters; only zeros allowance snapshot.** [Codex OG-2] `apply_on_error` at `ena_xstats.rs:96-128`; no defect found.
- **CX-4 (LOW) — Opaque engine `Box::into_raw` / `Box::from_raw` matched.** [Codex CA-2] `crates/dpdk-net/src/lib.rs:51-56` and `272-279`. No layout mismatch.
- **CX-5 (LOW) — `dpdk_net_scrape_xstats` immutable engine ref; mutable raw access gated under `test-server`.** [Codex CA-3] `crates/dpdk-net/src/lib.rs:65-76, 661-668`. Confirmed correct.
- **CX-6 (LOW) — RefCell borrow ordering around TX ring drain explicitly scoped.** [Codex HC-3] `engine.rs:5474-5495, 6191-6208`. No nested `borrow_mut` panic.
- **CX-7 (LOW) — Internet checksum streaming fold + carry is correct.** [Codex FYI-1] `l3_ip.rs:37-68, 266-276, 387-437`. No defect.
- **CX-8 (LOW) — TX software cksum + offload pseudo-header setup are internally consistent for current callers.** [Codex FYI-2] `tcp_output.rs:115-118, 164-177, 259-278, 320-340`.
- **CX-9 (LOW) — RSS hash truncation/cast not a current correctness issue (HashMap-backed lookup ignores `bucket_hash`).** [Codex FYI-3] `flow_table.rs:43-61, 157-176`.
- **CX-10 (LOW) — `MBUF_FAST_FREE` requested only as a PMD offload bit; no Rust-side free-path bypass.** [Codex FYI-4] `engine.rs:1720-1731`; `MbufHandle::Drop` at `mempool.rs:261-312`.
- **CX-11 (LOW) — No new A-HW timer-wheel entry for RX timestamp epoch wrap.** [Codex FYI-5] `engine.rs:1865-1900, 3776-3784`. RX timestamp threaded through RX dispatch as optional per-mbuf value; timer-wheel unrelated.

## Counts

Total: 47; BLOCK-A11: 1; STAGE-2: 21; DISPUTED: 1; AGREED-FYI: 4; CLAUDE-ONLY: 10; CODEX-ONLY: 11
