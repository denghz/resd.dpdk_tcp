# Part 2 Cross-Phase Retro Review (Claude)

**Reviewer:** general-purpose subagent (opus 4.7) — covering for superpowers:code-reviewer
**Reviewed at:** 2026-05-05
**Part:** 2 — TCP FSM core, options, PAWS, reassembly, SACK
**Phases:** A3, A4

## Verdict

**MINOR-ISSUES**

The TCP FSM core, options codec, reassembly, and SACK scoreboard hold up well across A5 / A6 / A8 churn. The sequence-comparator helpers in `tcp_seq.rs` are used everywhere they need to be — every raw seq arithmetic site uses `wrapping_add` / `wrapping_sub` and feeds the helper, never raw signed `<` / `>`. The major drifts are (a) the cross-phase backfill counters wired in A4 are scoped only to `handle_established` and silently miss the same conditions in `handle_close_path`, (b) `handle_inbound_syn_listen` discards `parsed_opts.ws_clamped` so passive-open never bumps `tcp.rx_ws_shift_clamped`, (c) `rx_syn_ack` is incremented on a SYN-only segment in the test-server path (counter-name semantic violation), and (d) `engine.rs` has grown 7×, `tcp_input.rs` 3.8×, and `tcp_conn.rs` 6.4× since A4 — the modules are still coherent but the engine has accreted enough phase-tagged sections that any future reviewer will need to skim line-comment headers to navigate it. None block ship; all are accumulated cross-phase debt.

## Architectural drift

- **`engine.rs` grew from 2104 lines at `phase-a4-complete` to 8141 at HEAD (3.9× over A5 / A5.5 / A6 / A6.5 / A6.6-7 / A7 / A8 / A8.5 / A9 / A10 / A10.5).** `pub struct Engine` now carries ~50 fields, decorated with phase markers ("Phase A3 additions", "Phase A5 additions", "A6 (spec §3.X)", "A6.6-7 Task 10", "A10 Stage A", "A8 T17", "A9 Task 6") that prove the file has become a god-object accreting per-phase responsibility (`engine.rs:706-849`). One side-effect: there are 142 method definitions on `impl Engine` (or `impl Drop for Engine` / second `impl Engine` block at `engine.rs:6628`). The original A3 design called for a thin engine over per-module state. The A4 plan didn't predict this and never had a reviewer flag it; A5+ kept stacking. Stage-2 candidate: extract `tcp_dispatch.rs` (the `tcp_input` body at `engine.rs:4003-4664` is 660 lines), `engine_lifecycle.rs` (engine new/drop/eal_init), `tx_path.rs` (the `flush_tx_pending_data` family).

- **`tcp_input.rs` grew from 1763 → 3355 (1.9×) post-A4** with A5/A6 RACK + RTT-sample + DSACK paths inlined into `handle_established` (now 770 lines from line 719 to ~1480). The A4 review reports the function as "RFC-clean for A4 scope"; it's now a multi-RFC composite (9293 § 3.10.7.4 + 7323 §3-§5 + 2018 §3-§4 + 2883 §4 + 6298 §3 Karn's + 8985 §6.1). A reader can no longer keep the relevant RFC references in their head. Stage-2 candidate: split `handle_established` into `validate_seg`, `process_ack`, `process_options`, `process_data`.

- **`tcp_conn.rs` grew from 323 → 1364 (4.2×).** `pub struct TcpConn` had 28 `pub` fields at A4-complete; today it has 68 (`tcp_conn.rs`, plus `pub` on substructs like `snd_retrans.entries`). All 40 net-new fields are A5+ additions (TLP probe state, RACK state, retrans deque, RTT histogram, listen-slot bookkeeping). Container is fine; the comment block `tcp_conn.rs:240-258` documenting `last_advertised_wnd` + `last_sack_trigger` is the only A4 backfill — every other added field is A5+, so the A4-era reviewer wouldn't have flagged scope creep, but a Stage-2 reviewer would.

- **`Engine::handle_inbound_syn_listen` (engine.rs:6774-6837, `cfg(test-server)`-gated passive-open path) silently discards `parsed_opts.ws_clamped`.** The active-open path in `handle_syn_sent` (`tcp_input.rs:667-673`) checks `parsed_opts.ws_clamped` and threads it through `Outcome::ws_shift_clamped` so the engine can `inc(tcp.rx_ws_shift_clamped)` and emit a one-shot stderr log. The passive-open path calls `parse_options(...).unwrap_or_default()` at `engine.rs:4106-4107` and then ignores the parser's clamp signal. A peer that SYN-bombs a `test-server` listener with `WS=15` would silently get clamped without any operator-visible signal. A4 didn't anticipate `test-server` (A7+ feature); the cross-phase concern is that the new path in A7 didn't backfill the A4 contract.

- **`TcpConn::new_passive` (`tcp_conn.rs:497-544`, `cfg(test-server)`) leaves `ws_shift_out = 0`** but `Engine::emit_syn_with_flags` (`engine.rs:2385-2421`) emits the SYN-ACK with `wscale: Some(compute_ws_shift_for(recv_buffer_bytes))` — i.e. a non-zero scale is advertised on the wire while the conn struct records `ws_shift_out = 0`. Subsequent post-handshake ACKs from the passive-side conn will right-shift `free_space` by `ws_shift_out = 0` (no scaling), but the peer right-shifts our advertised window by the WS shift we put in the SYN-ACK. Result: the peer's send rate is over-estimated by `2^ws_shift_out_we_advertised` once `recv_buffer_bytes > 65535`. Test-server-only, but a contract gap a real listen path will trip on. Active-open path correctly seeds `c.ws_shift_out = ws_out` at `engine.rs:5252`.

## Cross-phase invariant violations

- **`handle_close_path` (`tcp_input.rs:1485-1577`) does not set `Outcome::urgent_dropped`, `Outcome::rx_zero_window`, `Outcome::dup_ack`, or apply PAWS / SACK-decode.** A4's cross-phase counter backfill plan was "every data-bearing inbound segment fires the relevant rx_* outcome bit". The implementation only realized this in `handle_established` (`tcp_input.rs:735-746` for URG+zero-window; `:810-856` for PAWS; `:876-894` for SACK decode). `handle_close_path` is reached for FIN_WAIT_1 / FIN_WAIT_2 / CLOSING / LAST_ACK / CLOSE_WAIT / TIME_WAIT — all of which can carry data segments (CLOSE_WAIT is read-only-from-our-side; FIN_WAIT_2 sees inbound data until peer FIN). A peer URG segment to a CLOSE_WAIT conn never bumps `tcp.rx_urgent_dropped`; a zero-window advertisement on an inbound FIN never bumps `tcp.rx_zero_window`; a stale-TSval segment to a half-closed conn never bumps `tcp.rx_paws_rejected`. Counter-coverage tests (`tests/counter-coverage.rs:1273-1281` for `rx_zero_window`, `:1498-1506` for `rx_urgent_dropped`) only exercise ESTABLISHED, so the gap is invisible.

- **`engine.rs:4105` increments `tcp.rx_syn_ack` on a SYN-only segment** ("peer SYN observed" comment). The counter name promises "RX SYN+ACK" (handshake-completion observation, mirrored on the C ABI as `rx_syn_ack`); the production-build line `engine.rs:4128` correctly gates on `(parsed.flags & TCP_SYN) != 0 && (parsed.flags & TCP_ACK) != 0`. The test-server path at line 4105 is unconditional once the SYN-only branch is taken. Either rename the counter to `rx_syn` (and add a separate `rx_syn_ack`) or remove the line 4105 increment and rely on line 4128 for the SYN-only listen-side observation via a different counter. As-is, the counter's invariant ("rx_syn_ack >= rx_data" — handshake precedes data) holds by accident on test-server runs because passive-listen sees a SYN-only first, but the cross-phase audit would fail any consumer that maps the counter name to its RFC 9293 segment shape.

- **Direct `pub` on `SendRetrans::entries` (`tcp_retrans.rs:46`) lets `tcp_input::handle_established` mutate `conn.snd_retrans.entries[i].lost = true` (`tcp_input.rs:1092`).** A4 didn't ship `tcp_retrans` (that's A5), so this isn't an A4 invariant per se. The cross-phase concern is that A5 chose to expose `entries` as `pub` rather than provide a `mark_lost(idx)` method, and A6 didn't tighten it. The result: any A3/A4 reviewer auditing whether tcp_input "respects the retrans encapsulation" would have to rebuild the contract from scratch, because no module-level doc-comment describes which fields tcp_input may mutate vs which `SendRetrans` reserves to itself. Compare `mark_sacked(*block)` on line 891 (encapsulated method) — the same module is ambidextrous about whether internal vs external mutation is allowed.

- **`apply_tcp_input_counters` (`engine.rs:868-927`) only fires for `Outcome` fields that `handle_close_path` populates.** Since the close-path Outcome leaves `urgent_dropped = false` / `rx_zero_window = false` / `dup_ack = false` / `paws_rejected = false`, the counter dispatch is a silent no-op for close-state-bearing segments. The plumbing is right-by-construction *if and only if* every per-state handler populates every relevant Outcome bit. There's no compile-time guard on the populate-side. Stage-2 candidate: `Outcome` could carry a `Required` marker per field saying "every dispatcher must explicitly set this true/false" via a builder pattern, surfacing the gap at typecheck time.

## Tech debt accumulated

- **Two TODOs in scope** — both deliberately deferred:
  - `flow_table.rs:172` — `// TODO (Stage 2): when we swap by_tuple for a flat bucket array, use bucket_hash`. Stale-condition check: the `bucket_hash: u32` parameter is plumbed through `lookup_by_hash` but ignored; if Stage-2 lands and the bucket-hash plumbing was already wrong, no caller would catch it. Tag a `#[cfg(test)]` assertion that the hash matches the by-tuple lookup result for now.
  - `clock.rs:79` — `// TODO(spec §7.5): spec mandates CLOCK_MONOTONIC_RAW; Rust's Instant::now() uses CLOCK_MONOTONIC`. Calibration uses CLOCK_MONOTONIC which absorbs NTP slew up to ~500 ppm — accepted with reasoning at `clock.rs:80-84`. Still an open spec deviation; documented but not in any `accepted-divergences.md`.

- **No `unimplemented!()` / `unreachable!()` in scope are stale.** `engine.rs:3858` and `engine.rs:3990` (l2_decode / ip_decode unreachable arms) are tight to the upstream filter and provably reach the unreachable only on contract violation — ARM-portable too because they're not arch-conditional.

- **`#[allow(clippy::too_many_arguments)]`** on `tcp_conn.rs:369` (`new_client`), `tcp_conn.rs:498` (`new_passive`), `engine.rs:291` (`build_ack_outcome`), `tcp_input.rs:1580` (untracked). All four are pure helper signatures that grew naturally; none are hiding a bug, but the proliferation of "too many args" is a smell that argues for a `TcpConnConfig` carrier struct (which would also re-enable Default-based extension).

- **`#[cfg(feature = "obs-none")]` gates** (`tcp_input.rs:940`, `tcp_input.rs:955`, `tcp_conn.rs:667-669`, `tcp_conn.rs:1315-1317`, `tcp_events.rs:6,162,166,208,237`) are part of the A10.5 obs-none gate that compiles event emission away. None are stale; all gate either the event push or the timestamp capture. No silently-shipped deferred fix.

- **`bench_alloc_audit.rs:6` says "every downstream consumer of resd-net-core"** — stale crate name (renamed to `dpdk-net-core` at commit `eb01e79`). One-line doc-comment fix.

## Test-pyramid concerns

- **`proptest_paws.rs` re-implements the PAWS rule in a local wrapper `paws_accept(ts_recent, ts_val) = !seq_lt(ts_val, ts_recent)`** (`proptest_paws.rs:72-76`) and tests that wrapper, not the production gate. The file's docstring (`:1-58`) acknowledges this — "if the PAWS gate is ever refactored to diverge from this 1-line rule, the local wrapper must be updated in lockstep" — and points at two tap tests at `tcp_input.rs:2495` and `:2522` as the end-to-end anchor. The proptest gives RFC 7323 §5.3 R3 properties (P1-P7) but only against the wrapper; a production refactor that, say, stops checking PAWS for FIN segments would still pass the proptest. Honest limitation, but it weakens the proptest's value as a regression guard for the actual gate.

- **Counter-coverage tests scope only ESTABLISHED.** `tests/counter-coverage.rs::cover_tcp_rx_zero_window` (`:1273-1281`) and `cover_tcp_rx_urgent_dropped` (`:1498-1506`) drive the harness through `do_passive_open` then inject one segment in ESTABLISHED. No coverage test exercises the same conditions in close states, so the `handle_close_path` Outcome-population gap above is invisible to the test pyramid.

- **`tcp_options_paws_reassembly_sack_tap.rs` (the A4 integration test)** is 238 lines covering option negotiation smoke vs a kernel listener. Good end-to-end. Pairs nicely with the per-module proptests (`proptest_tcp_options.rs` 90 LOC, `proptest_tcp_reassembly.rs` 349 LOC, `proptest_tcp_sack.rs` 135 LOC, `proptest_tcp_seq.rs` 48 LOC). Pyramid is healthy at the unit + property level.

- **Unit tests in `tcp_input.rs`** assert on `Outcome` fields directly (`:2495 paws_drops_segment_with_stale_tsval_and_emits_challenge_ack` checks `out.paws_rejected == true` etc.) — that's appropriate because Outcome is the documented FSM-handler contract. Not a "asserting on internal state" violation; it IS the boundary.

## Observability gaps

- **A4 declared but partially-wired counters (close-state gap, see "Cross-phase invariant violations" above):**
  - `tcp.rx_urgent_dropped` — wired only in `handle_established` (`tcp_input.rs:735-740`). Gap: 6 close states + SYN_RECEIVED.
  - `tcp.rx_zero_window` — wired only in `handle_established` (`:746` reading `seg.window == 0`). Gap: same.
  - `tcp.rx_dup_ack` — wired only in `handle_established` (`:1018-1034`). Gap: same.
  - `tcp.rx_paws_rejected` — wired only in `handle_established` (`:837-844`). Gap: same. (A4 RFC review I-3 acknowledges PAWS scope is established + close states; the close-state gap was missed.)
  - `tcp.rx_bad_seq` — wired in `handle_established` (`:783`), `handle_close_path` (`:1524`), `handle_syn_received` (`:509`). This one IS complete across states — proves the others CAN be done; just weren't.
  - `tcp.rx_sack_blocks` / `tcp.rx_dsack` / `tcp.tx_tlp_spurious` — gated through `handle_established`'s SACK decode loop (`tcp_input.rs:876-894`). Inbound ACKs in close states bypass the decode entirely; SACK on a FIN_WAIT_2 inbound ACK is rare but not zero on lossy networks.

- **`tcp.rx_syn_ack` semantic violation** at `engine.rs:4105` (see Cross-phase invariant violations).

- **`tcp.tx_window_update` is narrow** by spec — only fires on the `last_advertised_wnd == 0 && new_window > 0` edge (`engine.rs:4790-4791`). RFC 1122 §4.2.2.17 mentions window-update segments more broadly (any pure ACK changing advertised window). This is an explicit A4 design choice ("we count the moment the receiver re-opens after stalling out") and documented; flag for traceability so a future reviewer doesn't expect RFC 1122-shape semantics.

- **`tcp.rx_mempool_avail` / `tcp.tx_data_mempool_avail` / `tcp.mbuf_refcnt_drop_unexpected`** are declared on `TcpCounters` (`counters.rs:284,290,304`) but NOT mirrored on the C ABI `dpdk_net_tcp_counters_t` (`api.rs:381-456`). The Rust comment at `counters.rs:271-277` claims these fit in the C struct's tail-padding under `#[repr(C, align(64))]`. The compile-time `size_of` assertion at `api.rs:518` enforces the size match. Flagged because the comment-claimed mechanism ("fit in tail-padding") is brittle: any future field bump will trip the size assertion silently in release builds (it's a `const _: ()` block). C consumers that expected `state_trans` to be the last layout-stable matrix will be wrong without warning.

## Memory-ordering / ARM-portability concerns

- **All counter atomics use `Ordering::Relaxed`** (`counters.rs:805,810`). Single-lcore engine; correct for stats counters. ARM-safe.

- **`clock.rs:32-39` hard-fails at compile on non-x86_64** (`compile_error!("dpdk-net-core currently only supports x86_64")`). Already flagged in Part 1 retro; restated here because it's the timestamp source for PAWS (`tcp_input.rs:829 now_ns`) and TS option (`tcp_input.rs:912 now_us`). Any aarch64 port must replace this with `cntvct_el0` reads or move to `clock_gettime(CLOCK_MONOTONIC_RAW)`.

- **`AtomicU64` layout assumption** at `crates/dpdk-net/src/api.rs:502-504` — the comment "AtomicU64 has the same layout as u64 on targets we support" is true for x86_64 and aarch64 (both 8-byte align, lock-free). On 32-bit ARM it might pull in libatomic; project scope explicitly rules out 32-bit so this is fine, but the comment should say "x86_64 and aarch64" to bound the contract. (Part-1 finding territory — flagging here only to confirm the A4 counter struct didn't introduce new portability hazards.)

- **`dpdk_net_core` x86_64 lock-in** doesn't touch `tcp_seq.rs` (pure u32 modular arithmetic), `tcp_options.rs` (be_bytes encoding), `tcp_reassembly.rs` (pure SmallVec), or `tcp_sack.rs` (pure SmallVec). The A3/A4 cores ARE ARM-clean — the lock-in is at the engine + clock layer.

## C-ABI / FFI

- **A4 added 22 new fields to `dpdk_net_tcp_counters_t`** (`api.rs:411-455`) — order matches the Rust core struct (`counters.rs:131-305`), enforced by the compile-time `size_of::<dpdk_net_tcp_counters_t>() == size_of::<CoreTcp>()` at `api.rs:518`. No deprecation drift between A3 and A4; the A3 fields (`recv_buf_drops` etc.) carry their A3 docstrings forward unchanged.

- **`state_trans: [[u64; 11]; 11]`** is at the same offset in both Rust and C structs — 121-cell matrix introduced in A3, preserved unchanged through A4. No ABI break.

- **`TcpOpts.sack_blocks: [SackBlock; MAX_SACK_BLOCKS_DECODE]`** is `pub` on the Rust side (`tcp_options.rs:64-75`) but is NOT exposed to C (no public FFI for option bundles). When A6 added the public `dpdk_net_connect` API it didn't reach into option-bundle internals, so no FFI surface is at risk.

## Hidden coupling

- **`tcp_input::handle_established` mutates `conn.snd_retrans.entries[i].lost = true`** directly via `pub` field access (`tcp_input.rs:1092`, `tcp_retrans.rs:46`). No `mark_lost(idx)` helper. Compare `conn.snd_retrans.mark_sacked(*block)` at `tcp_input.rs:891` (encapsulated method). The asymmetry is a tell: lost-marking grew up inline in tcp_input as part of A5 RACK, never refactored into the SendRetrans interface.

- **`Outcome` (`tcp_input.rs:188-345`) is the FSM↔engine boundary type but is `pub` with all fields `pub`,** enabling external construction from `engine.rs:4031-4060` of synthetic Outcomes (e.g. on parse error). One-way coupling is fine; the smell is that no doc comment on Outcome distinguishes "fields populated by FSM handlers" vs "fields synthesized by engine for parse failures" — they share a struct without a discriminator.

- **`TcpConn::four_tuple()` (`tcp_conn.rs`)** is read by 30+ engine call sites. Acceptable because FourTuple is value-typed. No coupling concern — flagged so a Stage-2 refactor that introduces a peer-tuple-based addressing mode (e.g. flow-label vs 5-tuple) knows the impact surface.

- **`crate::clock::now_ns` is read by 25+ sites** across `tcp_input.rs`, `engine.rs`, `tcp_conn.rs`, `iss.rs`. It's a free function (no Engine reference), so call sites are easy to enumerate. Fine for production; means tests need `set_virt_ns(...)` to drive the clock and can never have two engines on different clocks — the `feature = "test-server"` virtual clock is thread-local (`clock.rs:54`), so cross-thread engine tests serialize on the clock. Documented design choice, not a bug.

## Documentation drift

- **`docs/superpowers/specs/2026-04-20-stage1-phase-a6-5-hot-path-alloc-elimination-design.md`** still references `crates/resd-net-core/...` and `resd_net_core::bench_alloc_audit::CountingAllocator` at lines 137, 146, 236, 246, 289, 293-294, 362, 378-379. The repo-wide rename happened at commit `eb01e79`; `git show eb01e79 --stat | grep specs` confirms it touched some specs (the dpdk-tcp-design.md, a5/a5.5/a-hw/a6 specs) but missed the A6.5 spec. Out-of-A4-scope finding but flagged because the A4 cross-phase brief lives in the same docs/superpowers/specs/ tree.

- **`bench_alloc_audit.rs:6`** — comment says "downstream consumer of resd-net-core". Same rename miss.

- **`tcp_input.rs:937-938`** — comment refers to "wrapping_add on cache-resident state" (also at `tcp_conn.rs:662`). These are A6 RTT-sample fast-path comments, not A4. Doc-comment context drift is minor; the comments correctly describe today's code.

- **`tcp_options.rs::TcpOpts.sack_blocks` doc-comment (`tcp_options.rs:57-62`)** says "encode path (`push_sack_block`) still caps at MAX_SACK_BLOCKS_EMIT (3) since our outbound ACKs always include Timestamps." The "always include Timestamps" claim is conditional (`conn.ts_enabled`); on a peer that didn't negotiate TS, a 4-block emit budget could fit. The cap-at-3 is the intentional design choice but the rationale stated in the doc is incomplete — should read "since our outbound ACKs include Timestamps when negotiated, and the worst-case 12-byte TS overhead leaves room for only 3 blocks in the 40-byte option budget."

## FYI / informational

- **SipHash upgrade is silent A3-era win.** A3 commit `5e4617c` ("siphash via default hasher") was a `std::hash::DefaultHasher` placeholder; today's `iss.rs:64` uses a hand-rolled SipHash-2-4 (`siphash24.rs`) keyed on a 16-byte process secret. This matches RFC 6528 + spec §6.5 and supersedes the A3 placeholder. Worth noting because a reader seeing "siphash via default hasher" in the A3 commit log might worry; the upgrade landed (commit not in scope but the production code is correct).

- **`build_connect_syn_opts` (`engine.rs:243-257`) is the single source of SYN-option-bundle construction** for both active connect (`engine.rs:5384-5389` via the connect path) and passive SYN-ACK retransmit (`engine.rs:2401`). Good DRY. Original A3 had this inlined; A4 extracted the helper. Cross-phase-coherent.

- **`build_ack_outcome` (`engine.rs:292-373`) similarly centralizes the A4 ACK-window+options matrix** so post-handshake ACK construction has one site. Counter dispatch (`tcp.tx_zero_window`, `tcp.tx_sack_blocks`) is driven by Outcome flags — keeping the helper pure for unit tests. Good architectural decision.

- **`ReorderQueue` zero-copy refactor (A6.5 Task 4)** is documented at `tcp_reassembly.rs:1-15` and `:32-40` (the `OooSegment.Clone` ownership contract). The A4 design did not have mbuf-ref bookkeeping; A6.5 retrofitted it without changing the A4 tap-test surface. Cross-phase clean.

- **`SackScoreboard` (A4) is consumed by A5 RACK/TLP via `is_sacked`/`prune_below`** (`tcp_input.rs:1080-1081`, `tcp_input.rs:891 mark_sacked`). The A4 module exposes a small encapsulated interface (`tcp_sack.rs:19-138`); A5 didn't need to reach into the `[SackBlock; 4]` array. Architectural-coherent.

## Verification trace

Inspection only — no compile or test runs. Trace:

- `git log --oneline phase-a2-complete..phase-a3-complete` (32 A3 commits) and `phase-a3-complete..phase-a4-complete` (33 A4 commits) reviewed.
- Crate rename commit `eb01e79` confirmed; phase-a3/a4 tags point to `crates/resd-net-core/` paths (verified via `git ls-tree`).
- Engine.rs LOC growth measured: 1152 → 2104 → 8141 (A3 → A4 → HEAD).
- tcp_input.rs LOC growth: 885 → 1763 → 3355.
- tcp_conn.rs LOC growth: 213 → 323 → 1364; pub-field count 28 → 68 (HEAD).
- Counter-write sites for A4-declared counters enumerated via `grep -rn "tcp\.<counter>\." src/`; every counter has at least one write site (no declared-but-unwritten counters).
- `wrapping_(add|sub)` outside `tcp_seq.rs` reviewed (~50 sites) — all compatible with the wrap-safe contract; no raw signed `<`/`>` on seq variables found outside the helper.
- C-ABI mirror at `crates/dpdk-net/src/api.rs:381-456` field-by-field compared to `crates/dpdk-net-core/src/counters.rs:131-305` — same order, compile-time `size_of` assertion at `api.rs:518` confirms layout match.
- Tag tree at `phase-a3-complete` and `phase-a4-complete` queried (`git ls-tree -r ...`) to anchor scope.
