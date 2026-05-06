# Part 2 Cross-Phase Retro Synthesis

**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 2 — TCP FSM core, options, PAWS, reassembly, SACK
**Phases:** A3, A4
**Inputs:** cross-phase-retro-part-2-claude.md, cross-phase-retro-part-2-codex.md

## Combined verdict

**NEEDS-FIX** — Codex identified two BUGs in TCP sequence-length / TIME_WAIT window arithmetic and a third BUG (close-path PAWS bypass) that overlaps a Claude-flagged invariant gap; mechanical correctness errors outweigh Claude's "MINOR-ISSUES" architectural-drift framing.

## BLOCK A11 (must-fix before next phase)

### B1. BUG — Matched-flow RST ACK arithmetic omits SYN/FIN sequence length
- **Source:** Codex §Cross-phase invariant violations
- **Cite:** `crates/dpdk-net-core/src/engine.rs:4822` (ACK = `incoming.seq + payload.len()`); `engine.rs:4832` emits `RST|ACK`; reachable from `tcp_input.rs:493-499` (SYN_RECEIVED dup-SYN error → `TxAction::Rst`).
- **Why block:** `send_rst_unmatched` correctly accounts for SYN/FIN control-bit sequence consumption; the matched-flow helper does not. Any RST emitted in response to a SYN-bearing or FIN-bearing matched segment ACKs one byte too little. Mechanical wire-protocol defect on a path the test-server passive-open hits.

### B2. BUG — TIME_WAIT refresh triggered for every tuple-matching segment, not just in-window segments
- **Source:** Codex §Cross-phase invariant violations + Codex §Hidden coupling (LIKELY-BUG paired)
- **Cite:** `crates/dpdk-net-core/src/tcp_input.rs:1498-1504` (early `TxAction::Ack` return for `TimeWait` *before* the close-path window check at `:1507-1527`); `crates/dpdk-net-core/src/engine.rs:4476-4484` (refreshes `time_wait_deadline_ns` whenever `state == TimeWait && outcome.tx == TxAction::Ack`).
- **Why block:** Old duplicates, stale-seq packets, and malformed control packets that match the 4-tuple keep extending TIME_WAIT past 2×MSL. A3 RFC review F-2 explicitly scoped refresh to "any in-window segment"; HEAD violates that contract. The coupling between `TxAction::Ack` and "segment acceptable" is broken by the early return.

### B3. BUG — Close-path states bypass A4 PAWS, missing-TS check, and SACK decode
- **Source:** Codex §Cross-phase invariant violations (BUG: PAWS/missing-TS) + Claude §Cross-phase invariant violations (`handle_close_path` Outcome population gap) + Claude §Observability gaps (close-state coverage gap on `rx_urgent_dropped`/`rx_zero_window`/`rx_dup_ack`/`rx_paws_rejected`/`rx_sack_blocks`/`rx_dsack`)
- **Cite:** `crates/dpdk-net-core/src/tcp_input.rs:1485-1577` (close-path handler — no option parse, no `ts_recent` check, no SACK decode, no urgent/zero-window/dup-ack Outcome bits); compare `tcp_input.rs:790-856` (established PAWS) and `:876-894` (established SACK decode).
- **Why block:** Codex classifies as BUG (semantic — stale TSval can advance `snd_una`, consume FIN, refresh TIME_WAIT in close states; pairs with B2 as a TIME_WAIT amplification path). Claude classifies the parallel observability/Outcome gaps as SMELL but the underlying control-flow miss is the same defect. States affected: FIN_WAIT_1, FIN_WAIT_2, CLOSING, LAST_ACK, CLOSE_WAIT, TIME_WAIT.

## STAGE-2 FOLLOWUP (real concern, deferred)

### S1. SMELL — A4 received-SACK state mutated before ACK field validated
- **Source:** Codex §Tech debt accumulated + Codex §Observability gaps
- **Cite:** `tcp_input.rs:876-892` (SACK insert + `mark_sacked`); `tcp_input.rs:896-1005` (ACK validation runs after); `tcp_input.rs:1005-1014` (future-ACK challenge); `engine.rs:894-910` (counters).
- **Why deferred:** SACK is advisory; conservative retransmit code may absorb the poison. But future-ACK packets can still bump `tcp.rx_sack_blocks` before `rx_bad_ack`, distorting observability. Move SACK side effects after the `SND.UNA < SEG.ACK <= SND.NXT` gate.

### S2. SMELL — `handle_inbound_syn_listen` discards `parsed_opts.ws_clamped`
- **Source:** Claude §Architectural drift
- **Cite:** `engine.rs:4106-4107` (`parse_options(...).unwrap_or_default()` ignores clamp); `engine.rs:6774-6837` (`cfg(test-server)` passive-open). Compare `tcp_input.rs:667-673` (active-open threads `ws_clamped` through `Outcome`).
- **Why deferred:** Test-server-only; passive-open path silently clamps `WS=15` peer with no operator signal. Backfill the A4 `tcp.rx_ws_shift_clamped` contract on the A7 passive path.

### S3. SMELL — `TcpConn::new_passive` records `ws_shift_out = 0` while SYN-ACK advertises non-zero scale
- **Source:** Claude §Architectural drift
- **Cite:** `tcp_conn.rs:497-544` (`new_passive` leaves `ws_shift_out = 0`); `engine.rs:2385-2421` (`emit_syn_with_flags` advertises `Some(compute_ws_shift_for(recv_buffer_bytes))`); compare `engine.rs:5252` (active-open seeds correctly).
- **Why deferred:** Test-server-only contract gap; once `recv_buffer_bytes > 65535` the peer over-estimates send rate by `2^advertised_ws`. Real listen path (Stage 2) will trip on this if not fixed.

### S4. SMELL — `tcp.rx_syn_ack` incremented on a SYN-only segment
- **Source:** Claude §Cross-phase invariant violations + Claude §Observability gaps
- **Cite:** `engine.rs:4105` (unconditional bump in test-server SYN-only branch). Compare `engine.rs:4128` (production path correctly gates on `SYN && ACK`).
- **Why deferred:** Counter-name semantic violation; rename to `rx_syn` or remove the bogus increment. Counter-coverage tests pass by accident on test-server runs.

### S5. SMELL — `Outcome` populate-side has no compile-time guard
- **Source:** Claude §Cross-phase invariant violations
- **Cite:** `engine.rs:868-927` (`apply_tcp_input_counters` silent no-op when handler doesn't populate).
- **Why deferred:** Underlies B3. Builder-pattern / `Required` marker per Outcome field would surface the gap at typecheck time. Stage-2 refactor candidate.

### S6. SMELL — `engine.rs` god-object growth (2104 → 8141 LOC, 142 methods, ~50 fields)
- **Source:** Claude §Architectural drift
- **Cite:** `engine.rs:706-849` (phase-tagged field markers); `engine.rs:4003-4664` (660-line `tcp_input` body); `engine.rs:6628` (second `impl Engine` block).
- **Why deferred:** Coherent today; future review burden grows monotonically. Extract `tcp_dispatch.rs`, `engine_lifecycle.rs`, `tx_path.rs`.

### S7. SMELL — `tcp_input::handle_established` is 770 lines mixing 9293/7323/2018/2883/6298/8985
- **Source:** Claude §Architectural drift
- **Cite:** `tcp_input.rs:719-~1480`.
- **Why deferred:** Split into `validate_seg`, `process_ack`, `process_options`, `process_data`. Reader can no longer hold all relevant RFC references in head.

### S8. SMELL — `SendRetrans::entries` exposed `pub`; `tcp_input` mutates `entries[i].lost = true` directly
- **Source:** Claude §Cross-phase invariant violations + Claude §Hidden coupling
- **Cite:** `tcp_retrans.rs:46` (`pub entries`); `tcp_input.rs:1092` (direct mutation). Compare `tcp_input.rs:891 mark_sacked(*block)` (encapsulated).
- **Why deferred:** Asymmetric encapsulation; A5 grew this inline as RACK landed. Add `mark_lost(idx)` helper.

### S9. SMELL — Test pyramid gaps for B1 / B2 / B3
- **Source:** Codex §Test-pyramid concerns + Claude §Test-pyramid concerns
- **Cite:** No matched-RST length test feeding SYN/FIN-bearing segment to `emit_rst`; no TIME_WAIT stale-seq filtering test; no close-state PAWS test (Codex). `tests/counter-coverage.rs:1273-1281` and `:1498-1506` only cover ESTABLISHED (Claude).
- **Why deferred:** Add tests alongside the B1/B2/B3 fixes.

### S10. SMELL — `proptest_paws.rs` tests a local rule-wrapper, not the production gate
- **Source:** Claude §Test-pyramid concerns
- **Cite:** `proptest_paws.rs:72-76` (`paws_accept = !seq_lt(ts_val, ts_recent)`); docstring `:1-58` acknowledges the gap.
- **Why deferred:** Honest limitation; weakens the proptest as a regression guard for any future PAWS gate refactor.

### S11. SMELL — Four `#[allow(clippy::too_many_arguments)]` sites
- **Source:** Claude §Tech debt accumulated
- **Cite:** `tcp_conn.rs:369` (`new_client`), `:498` (`new_passive`); `engine.rs:291` (`build_ack_outcome`); `tcp_input.rs:1580`.
- **Why deferred:** Argues for a `TcpConnConfig` carrier struct.

## DISPUTED (reviewer disagreement)

### D1. Close-path PAWS / Outcome-bit gap — BUG vs SMELL
- **Codex:** BUG (semantic — stale TSval advances `snd_una`/consumes FIN/refreshes TIME_WAIT in close states).
- **Claude:** SMELL (frames as Outcome-population invariant + observability gap).
- **Resolution:** Escalated as B3 above (BUG severity preserved per "do not soften BUG to SMELL"). Listed here for traceability only.

## AGREED FYI (both reviewers flagged but not blocking)

### F1. FYI — `bench_alloc_audit.rs:6` doc-comment uses stale crate name `resd-net-core`
- **Source:** Claude §Documentation drift
- **Note:** Codex did not specifically cite this line but Claude flagged it; one-line fix; agreed informational.

### F2. FYI — Repo-rename drift in `docs/superpowers/specs/2026-04-20-stage1-phase-a6-5-hot-path-alloc-elimination-design.md`
- **Source:** Claude §Documentation drift
- **Cite:** Lines 137, 146, 236, 246, 289, 293-294, 362, 378-379 reference `crates/resd-net-core/...`; rename happened at `eb01e79`.

### F3. FYI — `clock.rs:79` TODO (CLOCK_MONOTONIC_RAW) and `clock.rs:32-39` x86_64 compile_error
- **Source:** Claude §Tech debt accumulated + Claude §Memory-ordering / ARM-portability concerns; Codex §Memory-ordering noted no new defect.
- **Note:** Restated from Part 1 retro; impact on PAWS / TS-option on aarch64 ports.

### F4. FYI — A4 TS/SACK earlier closed fixes still present at HEAD
- **Source:** Codex §FYI (prior A4 wscale + four-block SACK decode); Claude §FYI (SipHash upgrade, `build_connect_syn_opts` DRY, `build_ack_outcome` matrix, `ReorderQueue` zero-copy refit, `SackScoreboard` consumed cleanly by A5).
- **Note:** Both reviewers confirm previously-closed findings have not regressed.

### F5. FYI — PR #9-style mbuf leak pattern absent at HEAD
- **Source:** Codex §FYI
- **Cite:** `tcp_reassembly.rs:293-313` (`drop_segment_mbuf_ref` uses `shim_rte_pktmbuf_free_seg`); `:425-434` (Drop walks stored segments).

## INDEPENDENT-CLAUDE-ONLY (only Claude flagged; rate plausibility HIGH/MEDIUM/LOW)

### C1. SMELL — `tcp_conn.rs` pub-field count 28 → 68 (4.2× LOC growth) — **HIGH plausibility**
- **Cite:** `tcp_conn.rs` (substruct `pub` on `snd_retrans.entries`, etc.); 40 net-new fields are A5+.
- Why HIGH: trivially measurable; fits Stage-2 refactor target (`TcpConnConfig`).

### C2. SMELL — `tcp.tx_window_update` is narrow vs RFC 1122 §4.2.2.17 — **MEDIUM plausibility**
- **Cite:** `engine.rs:4790-4791` (only fires on `last_advertised_wnd == 0 && new_window > 0` edge).
- Why MEDIUM: documented A4 design choice ("count receiver re-open"); flagged for traceability only.

### C3. SMELL — `tcp.rx_mempool_avail` / `tx_data_mempool_avail` / `mbuf_refcnt_drop_unexpected` not on C ABI — **MEDIUM plausibility**
- **Cite:** `counters.rs:284,290,304` (declared) vs `api.rs:381-456` (not mirrored); compile-time `size_of` assertion at `api.rs:518` enforces total size.
- Why MEDIUM: tail-padding mechanism is brittle; future field bumps will trip silently. Need explicit field-list discipline.

### C4. SMELL — `tcp_options.rs::TcpOpts.sack_blocks` doc-comment incomplete — **LOW plausibility**
- **Cite:** `tcp_options.rs:57-62` says "always include Timestamps" but encode is conditional on `conn.ts_enabled`.
- Why LOW: doc-text fix; no behavior change.

### C5. SMELL — `Outcome` struct has all fields `pub` and conflates FSM-populated vs engine-synthesized — **MEDIUM plausibility**
- **Cite:** `tcp_input.rs:188-345` (defn); `engine.rs:4031-4060` (synthetic Outcome on parse error).
- Why MEDIUM: pairs with S5; documenting the dual role would help.

### C6. FYI — `flow_table.rs:172` Stage-2 TODO with unverified `bucket_hash` plumbing — **LOW plausibility**
- **Cite:** `bucket_hash: u32` parameter ignored in `lookup_by_hash`.
- Why LOW: `#[cfg(test)]` assertion suggested; TODO is deliberate.

### C7. FYI — `crate::clock::now_ns` read by 25+ sites — **LOW plausibility (informational)**
- **Cite:** Free function, not Engine method; thread-local virtual clock under `feature = "test-server"` (`clock.rs:54`).
- Why LOW: documented design choice.

### C8. FYI — `TcpConn::four_tuple()` read by 30+ engine call sites — **LOW plausibility (informational)**
- **Cite:** Acceptable because FourTuple is value-typed; flagged for Stage-2 peer-tuple-addressing impact.

### C9. FYI — `AtomicU64` layout comment at `api.rs:502-504` should bound to "x86_64 and aarch64" — **LOW plausibility**
- Why LOW: Part-1 territory; doc tighten only.

### C10. FYI — `tcp_input.rs:937-938` and `tcp_conn.rs:662` "wrapping_add on cache-resident state" comments are A6 RTT-fast-path drift — **LOW plausibility**
- Why LOW: comments correctly describe today's code.

## INDEPENDENT-CODEX-ONLY (only Codex flagged; rate plausibility HIGH/MEDIUM/LOW)

(All distinct Codex findings either landed in BLOCK-A11 above (B1, B2, B3) or in STAGE-2 (S1) or AGREED-FYI (F4, F5). The remaining Codex sections — architectural drift, memory-ordering, C-ABI, documentation drift — explicitly disclaim "no new finding" relative to Part 1 / per-phase reviews.)

### X1. FYI — Phase A3/A4 spec docs live under `docs/superpowers/plans/`, not `docs/superpowers/specs/` — **HIGH plausibility (verified)**
- **Source:** Codex §Documentation drift
- **Cite:** `docs/superpowers/plans/2026-04-18-stage1-phase-a3-tcp-basic.md` and `…-phase-a4-options-paws-reassembly-sack.md` exist; the `specs/` siblings do not.
- Why HIGH: easily checked; review-prompt path lookup hazard.

## Counts
Total: 30; BLOCK-A11: 3; STAGE-2: 11; DISPUTED: 1; AGREED-FYI: 5; CLAUDE-ONLY: 10; CODEX-ONLY: 1
