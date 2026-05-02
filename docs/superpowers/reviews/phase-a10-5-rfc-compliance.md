# Phase A10.5 RFC Compliance Review

**Phase:** A10.5 — Layer H correctness under WAN-condition fault injection
**Reviewed at:** 2026-05-01
**Reviewer:** rfc-compliance-reviewer subagent (opus 4.7)
**Phase-scoped diff:** `git diff master..HEAD` on branch phase-a10.5
**Phase plan:** docs/superpowers/plans/2026-05-01-stage1-phase-a10-5-layer-h-correctness.md
**Spec refs claimed:** §10.8, §10.10, §6.1 FSM legality
**RFC focus areas:** RFC 9293 §3.10, RFC 6298, RFC 8985, RFC 5681, RFC 6675, RFC 5961

## Summary

A10.5 is **a correctness assertion harness**, not engine wire behavior — 17 netem-matrix scenario rows, an FSM-legality oracle replaying the engine's `InternalEvent` stream, three side-checks (mbuf-leak, RX-mempool floor, events-dropped), and a per-scenario verdict surface. Per spec §2 "Out of scope": no new counters, no new events, no engine modifications. Therefore the only RFC-relevant claims are about the **assertion table's correctness expectations** vs. RFC clauses, not RFC behavior of the stack itself.

For RFC purposes A10.5 reduces to four questions:

1. **Are the FSM-oracle's "no Established → ≠Established under adversity" assertions consistent with RFC 9293 §3.10.7?** Yes — `transition_conn` only emits `StateChange` for legitimate `from != to` transitions (`engine.rs:4348`), and the only paths from ESTABLISHED out of ESTABLISHED reachable under netem-only adversity are: peer FIN (CLOSE_WAIT), peer RST (CLOSED), keepalive timeout (CLOSED), or self-initiated close (FIN_WAIT_1). The matrix runs against an echo-server that never half-closes mid-workload, the runner does not initiate close until the assertion window has ended (workload.rs:182 `engine.close_conn(conn)` after deadline), and the trading-latency preset disables keepalive (spec §6.4 line 446). The remaining residual paths are RST-induced (RFC 9293 §3.10.7 line 3602–3606, legal RST → CLOSED) — and a spurious peer-RST under netem loss/dup/reorder is itself a correctness defect that the oracle correctly catches.

2. **Is the row-10 `tx_rto > 0` expectation consistent with RFC 6298?** Yes — under correlated-burst loss (`loss 1% 25%`) a back-to-back lost segment plus its retransmission ladder will eventually exhaust the RACK reordering window and fall through to RTO. The trading-latency preset's `minRTO = 5 ms` (spec §6.4 line 447, AD-row in §6.4) is a deliberately documented deviation — RFC 6298 §2.4 SHOULD-rounds RTO to 1 s, and our 5 ms floor is preserved. The assertion does **not** require RFC 6298 §2.4 floor behavior; it just requires that **some** RTO fires under correlated burst loss, which is consistent with both the standard and the preset.

3. **Are rows 11–13 (`tx_retrans == 0` under duplication/reorder) consistent with RFC 8985?** Mostly yes — the matrix banks on RACK absorbing duplicates and small reorders before any 3-dup-ACK fast-retransmit fires. Our spec §6.3 says "3-dup-ACK fast retrans is disabled (counter visibility only via `rx_dup_ack`)" (line 433) — RACK is the **sole** loss-detection path. Under that constraint, plain duplicates produce dup-ACKs but no retransmits; reorder gap=3 with `delay 5ms` produces dup-ACKs but stays inside RACK's `min_RTT/4` reorder window (RFC 8985 §6.2 line 754). One concern surfaced under FYI-3: row 13's `delay 5ms` is on the boundary of plausible RACK reo_wnd values when `min_RTT` is very small — non-blocking but documented.

4. **Is the matrix missing a check for RFC 5961 challenge-ACK firing under adversity?** No — challenge-ACK is the engine's response to *out-of-window* RST/SYN segments under attack (RFC 5961 §3.2 line 380–409). netem produces in-window dup/reorder/loss but not out-of-window injections; FaultInjector also operates on RX mbufs without seq-rewriting. The phase plan's §2 explicitly defers RFC 5961 attack-pattern testing as "orthogonal to the netem matrix" — confirmed correct. RFC 5961 challenge-ACK behavior is not assertable from this matrix because nothing in the matrix triggers it.

No MUST or SHOULD violations. One non-blocking FYI on the row 13 reorder boundary case (already documented in spec §11). All §6.4 deviations the matrix interacts with (Nagle off, delayed-ACK off, minRTO=5ms, maxRTO=1s, CC off-by-default, RACK 3-dup-ACK disabled, A5.5 TLP rows) are pre-existing; A10.5 introduces zero new deviations.

## Verdict

**PROCEED**

## Scope

### Files reviewed (phase-scoped — `tools/layer-h-correctness/` only)

- `/home/ubuntu/resd.dpdk_tcp-a10.5/tools/layer-h-correctness/src/scenarios.rs` — the 17-row MATRIX (lines 49–278).
- `/home/ubuntu/resd.dpdk_tcp-a10.5/tools/layer-h-correctness/src/observation.rs` — FSM oracle (`fsm_replay_batch` line 568, `observe_batch` line 637), `EventRing`, `Verdict`, `FailureReason`.
- `/home/ubuntu/resd.dpdk_tcp-a10.5/tools/layer-h-correctness/src/assertions.rs` — `Relation` enum, `evaluate_counter_expectations`, `evaluate_disjunctive`, `evaluate_global_side_checks`.
- `/home/ubuntu/resd.dpdk_tcp-a10.5/tools/layer-h-correctness/src/workload.rs` — `run_one_scenario` lifecycle (lines 53–198) + `select_counter_names`.
- `/home/ubuntu/resd.dpdk_tcp-a10.5/tools/layer-h-correctness/src/counters_snapshot.rs` — `lookup_counter` wrapper, `MIN_RX_MEMPOOL_AVAIL = 32` constant, `SIDE_CHECK_COUNTERS`.
- `/home/ubuntu/resd.dpdk_tcp-a10.5/tools/layer-h-correctness/src/main.rs` — CLI entry (lines 1–50 sampled; CLI shape only, no RFC surface).
- Engine files cross-referenced for the oracle's invariants:
  - `/home/ubuntu/resd.dpdk_tcp-a10.5/crates/dpdk-net-core/src/engine.rs:4341–4370` — `transition_conn` filter (`from == to` returns early; only legitimate transitions emit `StateChange`).
  - `/home/ubuntu/resd.dpdk_tcp-a10.5/crates/dpdk-net-core/src/engine.rs:3356–3366` — `drain_events` callback API.
  - `/home/ubuntu/resd.dpdk_tcp-a10.5/crates/dpdk-net-core/src/counters.rs:184–185` — `tcp.rx_dup_ack` field.
  - `/home/ubuntu/resd.dpdk_tcp-a10.5/crates/dpdk-net-core/src/counters.rs:137–139` — `tcp.tx_retrans`, `tcp.tx_rto`, `tcp.tx_tlp` fields.
  - `/home/ubuntu/resd.dpdk_tcp-a10.5/crates/dpdk-net-core/src/counters.rs:284` — `tcp.rx_mempool_avail` (AtomicU32, intentionally absent from `lookup_counter`).
  - `/home/ubuntu/resd.dpdk_tcp-a10.5/crates/dpdk-net-core/src/counters.rs:290` — `tcp.mbuf_refcnt_drop_unexpected`.

### RFC sections walked

- `docs/rfcs/rfc9293.txt:3500–3700` — §3.10.7 SEGMENT ARRIVES (sequence acceptability + RST/SYN handling in synchronized states; ESTABLISHED + RST → CLOSED at line 3594–3606).
- `docs/rfcs/rfc6298.txt:100–280` — §2.1–2.5 (RTO computation), §3 (Karn's), §5 (manage RTO timer + 5.5 doubling).
- `docs/rfcs/rfc8985.txt:300–800` — §3.3 reordering resilience design, §6.2 RACK upon receiving an ACK (reorder window starts at `min_RTT/4` per line 754).
- `docs/rfcs/rfc5681.txt:178–185` — §2 dup-ACK definition.
- `docs/rfcs/rfc5681.txt:419–500` — §3.2 fast-retransmit/recovery 3-dup-ACK trigger.
- `docs/rfcs/rfc5961.txt:350–500` — §3 blind RST attack mitigation + challenge-ACK semantics, §4 SYN attack mitigation.

### Spec §6.3 rows verified (against MATRIX assertions)

- **RFC 9293 row** (spec line 424): "client FSM complete; no LISTEN/accept" — A10.5's FSM oracle asserts `state == Established` throughout the assertion window and flags any `StateChange { from: Established, to: ≠ Established }`. Consistent with the spec's eleven-state FSM. The runner's lifecycle (workload.rs:182) only initiates close *after* the deadline, so the assertion window is guaranteed to be ESTABLISHED-only on the runner side.
- **RFC 6298 row** (spec line 429): "minRTO=5ms, maxRTO=1s, both tunable" — row 10 (`loss_correlated_burst_1pct`) asserts `tcp.tx_rto > 0`. Trading-latency preset's 5 ms minRTO is a documented deviation in §6.4; the assertion is consistent with RFC 6298 §5.5 (RTO doubling) given the 5 ms floor.
- **RFC 8985 row** (spec line 433): "RACK-TLP primary; 3-dup-ACK disabled (counter visibility only via `rx_dup_ack`)" — rows 11/12/13 assert `rx_dup_ack > 0` AND `tx_retrans == 0`. Consistent: RACK absorbs reorder/dup, dup-ACK counter still bumps for forensics, no fast-retransmit fires.
- **RFC 5681 row** (spec line 428): "off-by-default; Reno via `cc_mode`; `dup_ack` counter strict per §2 in A5" — `tcp.rx_dup_ack` (counters.rs:185) increments per RFC 5681 §2 dup-ACK definition (verified `rx_dup_ack` counter exists; A5 review confirmed strict definition). Matrix's `>0` assertions are consistent.
- **RFC 5961 row** (spec line 436): "challenge-ACK on out-of-window seqs" — the matrix does not assert challenge-ACK behavior. Per phase plan §2 ("matrix doesn't explicitly assert challenge-ACK behavior — adversity could plausibly trigger it"), netem doesn't generate out-of-window injections; orthogonal to this phase. Confirmed correct scope.

### Spec §6.4 deviations the matrix touches (all pre-existing; A10.5 adds none)

- Trading-latency defaults: Nagle off (line 445), TCP keepalive off (line 446), minRTO=5ms (line 447), maxRTO=1s (line 448), CC off-by-default (line 449), TFO disabled (line 450).
- A5.5 TLP rows: `AD-A5-5-srtt-from-syn` (line 456), `AD-A5-5-rack-mark-losses-on-rto` (line 457), `AD-A5-5-tlp-arm-on-send` (line 458), `AD-A5-5-tlp-pto-floor-zero` (line 459), `AD-A5-5-tlp-multiplier-below-2x` (line 460), `AD-A5-5-tlp-skip-flight-size-gate` (line 461), `AD-A5-5-tlp-multi-probe` (line 462), `AD-A5-5-tlp-skip-rtt-sample-gate` (line 463).
- A6 close path: `AD-A6-force-tw-skip` (line 464). Not on the matrix's hot path — close happens after the assertion window, and the runner uses default-flag close (`engine.close_conn(conn)` at workload.rs:182).
- A8 URG drop: `AD-A8-urg-dropped` (line 465). Not exercised by netem/FI adversity.

## Must-fix (MUST/SHALL violations)

_(none — 0 open)_

## Missing-SHOULD

_(none — 0 open)_

## Accepted-deviation

A10.5 introduces **no new accepted deviations**. Every §6.4 row that an A10.5 assertion is adjacent to is pre-existing and was previously cited in A5/A5.5/A6 reviews. The matrix's correctness expectations are consistent with these deviations:

- **AD-A10-5-rto-floor-5ms-trading-latency** (pre-existing, cited for traceability)
  - RFC clause: `docs/rfcs/rfc6298.txt:157-159` — "(2.4) Whenever RTO is computed, if it is less than 1 second, then the RTO SHOULD be rounded up to 1 second."
  - Spec §6.4 line: `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md:447` — "minRTO | RFC 6298 RECOMMENDS 1s | **5ms** (tunable)".
  - Our code behavior: trading-latency preset sets `tcp_min_rto_us = 5_000` (5 ms). Row 10 (`loss_correlated_burst_1pct`) asserts `tcp.tx_rto > 0` under `loss 1% 25%` netem; with a 5 ms floor and RFC 6298 §5.5 doubling, RTO firings during a 30 s assertion window are plausible. The assertion is consistent: it only requires *some* RTO fire (`>0`), not a specific cadence. The 5 ms deviation is preserved through the matrix.

- **AD-A10-5-rack-3-dup-ack-disabled** (pre-existing, cited for traceability)
  - RFC clause: `docs/rfcs/rfc5681.txt:438-444` — "The TCP sender SHOULD use the 'fast retransmit' algorithm... arrival of 3 duplicate ACKs... as an indication that a segment has been lost."
  - Spec §6.3 line: `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md:433` — "A5 implements RACK-TLP as the primary loss-detection path; 3-dup-ACK fast retrans is disabled (counter visibility only via `rx_dup_ack`)."
  - Our code behavior: RACK (RFC 8985) is the only loss-detection path; the dup-ACK counter still increments per RFC 5681 §2 strict definition for visibility, but no fast-retransmit fires from it. Rows 11/12 (`dup_05pct`, `dup_2pct`) assert `rx_dup_ack > 0` AND `tx_retrans == 0` — both consistent with this configuration. RACK absorbs duplicates without retransmitting (RFC 8985 §3.3 reorder window logic).

- **AD-A10-5-rack-reo-wnd-min-rtt-quarter** (pre-existing, cited for traceability)
  - RFC clause: `docs/rfcs/rfc8985.txt:753-754` — "When the reordering window is not set to 0, it starts with a conservative RACK.reo_wnd of RACK.min_RTT/4."
  - Spec §6.3 line: `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md:433` (A5.5 RACK row) + A5.5 review's `AD-A5-5-*` cluster.
  - Our code behavior: RACK initial reo_wnd = `min_RTT/4` per RFC 8985 §6.2. Row 13 (`reorder_depth_3`, `delay 5ms reorder 50% gap 3`) asserts `tx_retrans == 0`. With `min_RTT ≈ measured latency` (likely a few hundred µs on bench-pair hardware) plus the netem `delay 5ms`, `min_RTT/4` is roughly 1–2 ms; the netem reorder gap moves packets by `gap × inter-packet time`, often well within reo_wnd. Boundary case documented in spec §11 ("Reorder gap=3 boundary case"); FYI-3 below.

- **AD-A10-5-keepalive-off** (pre-existing, cited for traceability)
  - RFC clause: `docs/rfcs/rfc9293.txt` §3.8.4 (TCP Keep-Alives) — implementation MAY include keepalive; not MUST/SHOULD.
  - Spec §6.4 line: `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md:446` — "TCP keepalive | optional | **off** | exchanges close idle; application heartbeats are preferred".
  - Our code behavior: keepalive disabled → no spurious keepalive-timeout-driven `Established → Closed` transition during the 30 s scenario window. Without this deviation, a long quiescent test could transition the FSM out of ESTABLISHED via a keepalive expiry; with it off, the only paths out of ESTABLISHED during the window are peer-driven (FIN/RST). The oracle is therefore tight in default builds.

- **AD-A10-5-cc-off-by-default** (pre-existing, cited for traceability)
  - RFC clause: `docs/rfcs/rfc5681.txt:209` — "The slow start and congestion avoidance algorithms MUST be used by a TCP sender to control the amount of outstanding data being injected into the network".
  - Spec §6.4 line: `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md:449` — "Congestion control | RFC 5681 MUST | **off-by-default**".
  - Our code behavior: trading-latency preset runs with `cc_mode = 0` (off). The matrix's `tx_retrans <= N` upper-bound assertions would behave differently under Reno (slow-start collapse on loss reduces effective send rate, which reduces total retransmits) — but the matrix is intentionally calibrated against the no-CC path. This deviation does not break any A10.5 assertion; it simply defines the regime the bounds were chosen against.

## FYI

- **I-1** — The FSM-oracle's "no Established → ≠Established" assertion is correct under the runner's discipline. Per `tools/layer-h-correctness/src/workload.rs:182` the runner only calls `engine.close_conn(conn)` *after* the per-scenario deadline expires, so the runner-initiated FIN_WAIT_1 transition never lands in the assertion window. The peer (echo-server) does not initiate close mid-workload. RFC 9293 §3.10.7 ESTABLISHED + peer-FIN → CLOSE_WAIT (line 3603–3606) is therefore not a normal-path event during the window; if it fires it's a peer misbehavior the oracle should flag, which is what the matrix does. The `from == to` filter at `engine.rs:4348` correctly suppresses self-transition spam (Established → Established is never emitted as a `StateChange`), so the oracle can use `from: Established, to: ≠ Established` as a tight predicate without false positives.

- **I-2** — RFC 9293 §3.10.7 ESTABLISHED + RST → CLOSED is a legal transition (line 3602–3606: "any outstanding RECEIVEs and SEND should receive 'reset' responses... Enter the CLOSED state, delete the TCB, and return"). Under netem-only adversity (loss/dup/reorder/corrupt) the peer should not generate a spurious RST — netem doesn't synthesize TCP control segments, only forwards/drops/dups them. If a RST arrives during the assertion window it's either (a) the peer's retransmit-budget exhausted on its side, or (b) a malformed segment our parser rejected and replied to. Either is a real correctness signal the oracle should catch — and the matrix correctly catches it via `from: Established, to: Closed` flagged as illegal. Consistent with RFC 5961 §3.2's "RST with sequence exactly RCV.NXT MUST reset" path (line 383–385) being legitimate but unwanted under netem.

- **I-3** — Row 13 (`reorder_depth_3`, `delay 5ms reorder 50% gap 3`) `tx_retrans == 0` boundary case. RFC 8985 §6.2 reorder window starts at `min_RTT/4` (line 754). On a low-RTT bench-pair link, `min_RTT` may be ~500 µs (DPDK userspace + bench-pair hardware), so `min_RTT/4 ≈ 125 µs`. The netem `delay 5ms` adds 5 ms uniform delay, which becomes the new dominant latency and hence the new `min_RTT` after the engine's first RTT sample. RACK then computes `reo_wnd = 5_000 / 4 = 1.25 ms`. netem `reorder gap 3` delays every 4th packet; the inter-packet time at typical RTT-loop cadence is sub-ms. So the reorder typically lands within the reo_wnd (no spurious lost-mark), and `tx_retrans` stays at 0. **However**, if `min_RTT` updates lazily (RACK uses windowed-min per RFC 8985 §6.2 line 642), the early window can be pre-`delay`-update and reo_wnd is smaller — narrow risk that the first reorder triggers a retransmit before RACK has caught up. Spec §11 documents this risk explicitly. Non-blocking; if the assertion flakes on a real bench-pair, the bundle's `event_window` will show the timing.

- **I-4** — RFC 5961 challenge-ACK is correctly out of scope for this phase. RFC 5961 §3.2 line 380–409 specifies challenge-ACK firing on `RST { sequence ∈ window, sequence ≠ RCV.NXT }`, and §4 line 466–502 on `SYN ∈ synchronized state`. netem produces `dup` (resends an unmodified segment — sequence is exactly the prior `RCV.NXT - PrevPayload`, not in-window-but-not-RCV.NXT) and `reorder` (delays a packet but doesn't rewrite seq). Neither produces the out-of-window-RST or in-state-SYN that triggers challenge-ACK. FaultInjector (`fault_injector.{drops,dups,reorders}`) operates on whole mbufs without seq rewriting (per A9 design). Therefore the matrix has no scenario where challenge-ACK semantics would fire, and the absence of a challenge-ACK assertion is correct. RFC 5961 attack-pattern testing belongs to a different harness (TCP-Fuzz / tcpreq); spec §10.8 explicitly defers it.

- **I-5** — Row 14 (`corruption_001pct`, `corrupt 0.01%`) disjunctive assertion `[eth.rx_drop_cksum_bad, ip.rx_csum_bad] >0` correctly handles both checksum-validation paths. RFC 9293 §3.1 mandates pseudo-header TCP checksum (covers all bytes). Our default build with `hw-offload-rx-cksum` on lets the NIC drop bad-cksum mbufs and bumps `eth.rx_drop_cksum_bad`; the rfc-compliance build path (offload off) drops at SW path and bumps `ip.rx_csum_bad`. Disjunctive OR satisfies the row under either profile without runtime introspection — a pre-existing offload-aware pattern documented in spec §6.4 (no new deviation).

- **I-6** — The matrix's upper bounds (`<=10000`, `<=50000`, `<=200000`) are deliberately generous to catch *catastrophic* retransmit ladders, not enforce a tight RFC-aligned budget. Per spec §11 ("Counter-bound generosity vs catch rate") the bounds are positioned to catch failures that manifest as millions/billions of retransmits, not protocol-timing slips. RFC 6298 §5 specifies retransmit timer behavior but not a per-window quota; our bounds do not violate any RFC clause. They simply choose where to draw the operational ceiling.

- **I-7** — `tcp.mbuf_refcnt_drop_unexpected == 0` and `tcp.rx_mempool_avail >= 32` global side-checks (`counters_snapshot.rs:868`, `MIN_RX_MEMPOOL_AVAIL = 32`) are PR #9 leak-detect signals, not RFC-driven. RFC 9293 §3.10 says nothing about RX-mempool exhaustion handling — this is a memory-management invariant of our DPDK userspace stack. The matrix's enforcement of these as global side-checks every batch is operationally correct and orthogonal to RFC behavior; included here for completeness and to confirm no RFC overlap.

- **I-8** — `obs.events_dropped` per-batch and end-of-scenario `==0` checks. `obs.events_dropped` increments when the engine's bounded event queue (4096 default per spec line 327) drops the oldest event. RFC 9293 §3.10 is silent on the API event-delivery layer; this is an internal observability invariant. The matrix's `==0` enforcement is operationally correct: a non-zero delta means the FSM oracle could miss a `StateChange`, which would invalidate the oracle's negative guarantee. Defensive design choice; not RFC-driven.

- **I-9** — Per spec §10.14 ("don't flag clauses scoped to later phases"), I confirmed PMTU-blackholing (RFC 8899 PLPMTUD) is correctly deferred to Stage 2. The phase plan's "Out of scope" line is authoritative; A10.5's matrix has zero PLPMTUD scenarios.

## Verification trace

1. Read phase plan `docs/superpowers/plans/2026-05-01-stage1-phase-a10-5-layer-h-correctness.md` to identify the matrix structure (17 rows, 14 base + 3 composed) and the in-scope spec sections (§10.8, §10.10, §6.1).

2. Read phase spec `docs/superpowers/specs/2026-05-01-stage1-phase-a10-5-layer-h-correctness-design.md` §1–§6 to confirm: assertion engine asserts against existing observability surface only; no new counters, events, or engine modifications; FSM oracle implementation in `observation.rs`.

3. Cross-checked against parent stage1 design `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md` §6.3 RFC compliance matrix (lines 415–438) and §6.4 deviation tables (lines 439–470). Confirmed every counter referenced in the matrix exists in `crates/dpdk-net-core/src/counters.rs` (`tcp.tx_retrans`, `tcp.tx_rto`, `tcp.tx_tlp`, `tcp.rx_dup_ack`, `tcp.mbuf_refcnt_drop_unexpected`, `tcp.rx_mempool_avail`, `obs.events_dropped`, `eth.rx_drop_cksum_bad`, `ip.rx_csum_bad`, `fault_injector.{drops,dups,reorders}`).

4. RFC walks (using vendored `docs/rfcs/`):
   - **RFC 9293 §3.10.7 SEGMENT ARRIVES** (lines 3500–3700) — verified ESTABLISHED state's RST/SYN/data acceptance rules; confirmed RST → CLOSED is the only legitimate exit-from-ESTABLISHED reachable via segment processing under netem-only adversity.
   - **RFC 6298 §2.4, §5.5** — verified RTO floor (1 s SHOULD per §2.4 line 157) and RTO doubling (§5.5 line 261); confirmed the 5 ms preset deviation is documented in §6.4 line 447.
   - **RFC 8985 §6.2** — verified RACK reo_wnd starts at `min_RTT/4` (line 754) and adapts via DSACK; confirmed row 13's boundary-case rationale.
   - **RFC 5681 §2** — verified dup-ACK definition (lines 178–185); confirmed `tcp.rx_dup_ack` increments per the strict §2 definition (A5 review's prior verification).
   - **RFC 5961 §3, §4** — verified challenge-ACK is RST-out-of-window or SYN-in-state-driven; confirmed netem/FI cannot produce these triggers.

5. Engine cross-checks:
   - `crates/dpdk-net-core/src/engine.rs:4348` — confirmed `from == to` self-transition filter at `transition_conn` (so the oracle never sees Established → Established events).
   - `crates/dpdk-net-core/src/engine.rs:3356–3366` — confirmed `drain_events` callback API signature matches the oracle's usage.
   - `crates/dpdk-net-core/src/counters.rs:577` — confirmed `KNOWN_COUNTER_COUNT = 118` and the matrix's named counters all appear in `ALL_COUNTER_NAMES` (lines 500–552).

6. Walked the 17 matrix rows:
   - Rows 1–6 (delay-only): tight `tx_retrans` bounds (`==0` for jitterless, `<=10`/`<=20` for jittered). RFC 6298 §5 timer behavior should not fire spurious retransmits under bounded delay; consistent.
   - Rows 7–9 (loss): `tx_retrans > 0 && tx_retrans <= K` for K ∈ {10000, 50000, 200000}. RFC 6298 §5.5 RTO + RFC 8985 §6.2 RACK both legitimate triggers; consistent.
   - Row 10 (correlated burst): adds `tx_rto > 0 && tx_tlp > 0`. RFC 6298 §5.5 RTO + RFC 8985 §7.2 TLP both expected to fire under correlated bursts; consistent.
   - Rows 11–13 (dup/reorder): `rx_dup_ack > 0 && tx_retrans == 0`. Per RFC 5681 §2 strict dup-ACK definition + spec §6.3 line 433 (3-dup-ACK disabled); consistent. Row 13 boundary-case noted (FYI-3).
   - Row 14 (corruption): disjunctive `[eth.rx_drop_cksum_bad, ip.rx_csum_bad] >0`. RFC 9293 §3.1 pseudo-header checksum; consistent (FYI-5).
   - Rows 15–17 (composed): combine netem with FaultInjector; assert `fault_injector.* > 0` AND TCP-level retransmit/dup-ACK counters. No new RFC surface beyond rows 7–8 + rows 11–13.

7. Confirmed all three side-checks (mbuf-leak, RX-mempool floor, events-dropped) are operational signals, not RFC-driven (FYI-7, FYI-8). Confirmed RFC 5961 attack-pattern testing is correctly out of scope (FYI-4).

8. Confirmed the lifecycle (`workload.rs:55–198`): warmup → snapshot_pre → drain handshake events → batched workload-and-observe loop → snapshot_post → evaluate → close. Close happens *after* the assertion window, so the runner's FIN never lands in the oracle's "ESTABLISHED-only" predicate.

9. No new accepted deviations introduced. All §6.4 entries the matrix is adjacent to are pre-existing and were verified in A5/A5.5/A6 reviews; cross-referenced for traceability.

10. Verdict: PROCEED. Zero Must-fix and zero Missing-SHOULD. All Accepted-deviations cite concrete spec §6.4 lines. The phase-a10-5-complete tag is unblocked from the RFC-compliance side.
