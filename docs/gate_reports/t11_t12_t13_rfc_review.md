# T11/T12/T13 — RFC Compliance Gate Review

- **Reviewer:** rfc-compliance-reviewer subagent (opus 4.7)
- **Date:** 2026-05-06
- **Subject:** Three new pressure-test suites (A11.3 Lanes A/B/C)
- **RFCs in scope:** 9293 (TCP), 2018 (SACK), 7323 (TCP Extensions), 1122 (Host Requirements)
- **Worktree:** `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review/`
- **Spec deviations referenced:** §6.4 (Nagle off, delayed-ACK off, advertise free_space accept at full capacity, MSL configurable, RFC 7323 negotiation rules)

## Files reviewed

- `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review/crates/dpdk-net-core/tests/pressure_reassembly_saturation.rs` (T11 — Lane A)
- `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review/crates/dpdk-net-core/tests/pressure_sack_blocks.rs` (T12 — Lane B)
- `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review/crates/dpdk-net-core/tests/pressure_option_churn.rs` (T13 — Lane C)

Engine code cross-referenced (assertion correctness depends on these):

- `crates/dpdk-net-core/src/tcp_seq.rs:30` — `in_window(start, seq, len)` half-open `[start, start+len)`.
- `crates/dpdk-net-core/src/tcp_input.rs:771–778` — RFC 9293 §3.10.7.4 acceptability test (both-edges form).
- `crates/dpdk-net-core/src/tcp_input.rs:1302–1426` — reassembly cap-overflow path (silent drop, `buf_full_drop` accumulation, no RST emission).
- `crates/dpdk-net-core/src/tcp_input.rs:648` — `conn.sack_enabled = parsed_opts.sack_permitted` (peer-SYN-gated, RFC 2018 §4 MUST).
- `crates/dpdk-net-core/src/tcp_input.rs:876–894` — SACK decode gated on `conn.sack_enabled`.
- `crates/dpdk-net-core/src/tcp_options.rs:357–386` — SACK option decode (block_bytes multiple-of-8, ≤ 4 blocks, RFC 2018 §3 layout).
- `crates/dpdk-net-core/src/engine.rs:316–410` — `build_ack_outcome` (RFC 2018 §4 trigger-block-first, sack-emit gated on `sack_enabled`).
- `crates/dpdk-net-core/src/engine.rs:5939–6020` — `close_conn` (ESTABLISHED → FIN_WAIT_1; CLOSE_WAIT → LAST_ACK).
- `crates/dpdk-net-core/src/tcp_conn.rs:380` — `rcv_wnd` set once at conn creation (deviation §6.4 row "Receive-window shrinkage vs. buffer occupancy": advertise free_space, accept at full capacity).

## Verdict

**PASS** — no MUST violations and no Missing-SHOULD findings. All assertions in T11/T12/T13 are RFC-consistent, and the scenarios exercise the in-scope RFC clauses correctly. Three accepted deviations are touched (already covered by spec §6.4); two FYI items recorded.

---

## Findings

### Must-fix (MUST/SHALL violations) — none

No findings.

### Missing SHOULD (not in §6.4 allowlist) — none

No findings.

### Confirmed correct behaviors

#### C-1 — T11 OOO segment window-acceptability check is RFC 9293 §3.10.7.4 row-4 compliant

- **RFC clause:** `docs/rfcs/rfc9293.txt:3505–3511` — Table 6 row 4 (SEG.LEN > 0, RCV.WND > 0): `RCV.NXT =< SEG.SEQ < RCV.NXT+RCV.WND  or  RCV.NXT =< SEG.SEQ+SEG.LEN-1 < RCV.NXT+RCV.WND`.
- **Test math (T11 lines 110–158):** with `rcv_wnd = RECV_BUF = 4096` and `rcv_nxt` after the in-order fill, the three OOO frames have seq offsets `1, 1025, 2049` (each 1024 bytes long). The overflow frame at offset `3073` carries 64 bytes → last byte at offset `3136`. Both `3073 < 4096` and `3136 < 4096` pass.
- **Engine behavior (`tcp_input.rs:771–778`):** uses the AND-form (both edges in window) — stricter than RFC's OR but a supersetting drop policy; all four T11 frames pass either form. The 3073 < 4096 claim in T11's comment is correct.
- **Note on the AND-vs-OR strictness:** the engine's check is documented at `tcp_input.rs:770` as "stricter than mTCP's (both edges)". This is a pre-existing project deviation that is **out of scope** for these three suites — none of them inject a segment that straddles `rcv_nxt + rcv_wnd`, so the AND-vs-OR distinction never affects the test verdicts.

#### C-2 — T11 cap-overflow drop without RST is RFC 9293 §3.10.7.4 SHLD-31 + RFC 1122 §4.2.2.20 compliant

- **RFC clauses:** 
  - `docs/rfcs/rfc9293.txt:3545–3547` — "Segments with higher beginning sequence numbers SHOULD be held for later processing (SHLD-31)." 
  - `docs/rfcs/rfc1122.txt:5458–5468` — "a TCP SHOULD be capable of queueing out-of-order TCP segments." Resource-exhaustion fallback is implementation-defined; RFC does not mandate RST.
- **Engine behavior (`tcp_input.rs:1411–1426`):** when `total_cap == 0`, the engine accumulates `buf_full_drop` (counted in `tcp.recv_buf_drops`) and returns `tx: TxAction::None` — silent drop, no RST. Mbuf pre-bump is rolled back to keep refcounts clean.
- **T11 assertion (line 186):** `tcp.tx_rst == 0` is the correct invariant for resource-exhaustion drops.

#### C-3 — T11 `rx_reassembly_hole_filled == 0` is consistent with the workload

- **RFC clause:** `docs/rfcs/rfc9293.txt:3479–3489` — out-of-order segments are queued; in-order arrival drains the gap.
- **Workload (T11 phase 3, line 96–129):** OOO frames are injected at `rcv_nxt + 1`, `rcv_nxt + 1025`, `rcv_nxt + 2049` — leaving a 1-byte hole at exactly `rcv_nxt`. No segment is ever injected at offset 0 (i.e. seq = `rcv_nxt`), so the gap is never closed and the contiguous-drain path (`tcp_input.rs:1278–1290`) is never triggered.
- **Engine semantic (`tcp_conn.rs:140–149`):** `free_space_total = cap − in_order_bytes − reorder_bytes`. The reorder queue retains its bytes until the gap closes; T11's `Eq(0)` assertion correctly captures the "no drain ever occurred" invariant.

#### C-4 — T12 SACK negotiation handshake is RFC 2018 §2 + §4 compliant

- **RFC clauses:**
  - `docs/rfcs/rfc2018.txt:106–110` — "an enabling option, 'SACK-permitted', which may be sent in a SYN... [It] MUST NOT be sent on non-SYN segments."
  - `docs/rfcs/rfc2018.txt:233–239` — "If the data receiver has received a SACK-Permitted option on the SYN... it MAY elect to generate SACK options... If the data receiver has not received a SACK-Permitted option for a given connection, it MUST NOT send SACK options on that connection."
- **T12 handshake (lines 92–127):** EngineConfig has `tcp_sack = true`; the manually-built SYN sets `opts.sack_permitted = true` (line 99). Engine sets `conn.sack_enabled = parsed_opts.sack_permitted` (`tcp_input.rs:648`) at SYN reception, and the SYN-ACK response echoes `sack_permitted = self.cfg.tcp_sack` via `build_connect_syn_opts` (`engine.rs:2544–2550`). Both directions of the SACK-permitted exchange happen on SYN/SYN-ACK only.
- **Engine emission gate:** SACK blocks are emitted in `build_ack_outcome` only when `sack_enabled && !reorder_segments.is_empty()` (`engine.rs:346`). RFC 2018 §4 MUST is honored.

#### C-5 — T12 Path-B SACK option byte layout is RFC 2018 §3 compliant

- **RFC clause:** `docs/rfcs/rfc2018.txt:152–166` — SACK option carries `Kind=5 | Length | <left edge | right edge>×N`, where each block has `right > left` and "Each block represents received bytes of data that are contiguous and isolated".
- **T12 Path B (lines 175–198):** for each round, `left = base + j*512`; `right = left + 256`. Always `right > left` (256 bytes) and consecutive blocks are separated by 256-byte gaps (non-overlapping, isolated).
- **Decode-path validation (`tcp_options.rs:358–365`):** rejects malformed SACK with `BadSackBlockCount` if `block_bytes` is zero, not a multiple of 8, or > 4 blocks. T12's encoder uses `push_sack_block_decode` which caps at 4 (line 94–98 of `tcp_options.rs`); each block serializes to 8 bytes; total option = 2 (hdr) + 32 (4 blocks) = 34 bytes — fits in TCP options space without TS. Decoder accepts → no `tcp.rx_bad_option` bumps. T12's `Eq(0)` assertion holds.
- **Note on RFC 2018 §4 first-block rule:** RFC 2018 §4 line 254–260 says the **emit**-side first SACK block "MUST specify the contiguous block of data containing the segment which triggered this ACK". Path B injects synthetic SACKs from a hand-rolled "peer" — this MUST applies to the emitter. The test peer's compliance with that MUST is not the engine's concern; the engine's job is to **decode** the blocks robustly, which it does. T12's hermeticity concern is correctly scoped.

#### C-6 — T12 `tcp_timestamps = false` does not violate RFC 7323 §3.2

- **RFC clause:** `docs/rfcs/rfc7323.txt:705–707` — "If a TSopt is received on a connection where TSopt was not negotiated in the initial three-way handshake, the TSopt MUST be ignored and the packet processed normally."
- **T12 setup:** EngineConfig.`tcp_timestamps = false`. Synthetic peer's SYN does not carry TS (`build_tcp_frame` with `TcpOpts::default()` plus only `mss` and `sack_permitted` set). Engine sets `conn.ts_enabled = false` (`tcp_input.rs:656–658`). All subsequent ACKs in Path B carry no TS, satisfying the negotiated state.
- **PAWS:** not triggered, since TS is not negotiated. RFC 7323 §3.2 path correctly inactive.

#### C-7 — T13 `tcp_msl_ms = 10` (TIME_WAIT = 20 ms) is RFC 9293 §3.4.2 compliant

- **RFC clause:** `docs/rfcs/rfc9293.txt:1107–1109` — "For this specification the MSL is taken to be 2 minutes. This is an engineering choice, and may be changed if experience indicates it is desirable to do so."
- **Test config:** `tcp_msl_ms = 10` → TIME_WAIT 2×MSL = 20 ms. Within RFC's "engineering choice" latitude. The test peer is the local Linux kernel acting as echo server; the cycles run sequentially with `max_connections = 32` providing 32-slot TIME_WAIT headroom for the 256 cycles.
- **Quiet-time concept (RFC 9293 §3.4.3):** does not apply — engine maintains ISS monotonicity via the `(ticks_since_boot_at_4µs) + SipHash(...)` construction (spec §6.5 line 477), so consecutive 4-tuple reuses within MSL get monotonically advancing ISS values. RFC 6528 §3 is satisfied independently.

#### C-8 — T13 active-close after Readable is RFC 9293 §3.6 Case 2 compliant

- **RFC clause:** `docs/rfcs/rfc9293.txt:1570–1579` — "If an unsolicited FIN arrives from the network, the receiving TCP endpoint can ACK it and tell the user that the connection is closing. The user will respond with a CLOSE..."
- **Workload (T13 lines 200–256):** kernel echo peer reads 1 byte, writes 1 byte, drops the socket → kernel emits FIN. Engine sees the data byte (Readable event fires), then the FIN (engine moves to CLOSE_WAIT). When the test calls `engine.close_conn(h)`, the connection is in CLOSE_WAIT → LAST_ACK transition (`engine.rs:5964`), emitting our FIN. After the kernel's final ACK, engine reaches CLOSED and emits the Closed event.
- **FSM transitions (`engine.rs:5962–5966`):** `Established → FinWait1` (Case 1: peer-FIN-not-yet-arrived race) or `CloseWait → LastAck` (Case 2: peer-FIN-already-arrived). Both are RFC 9293 §3.6 compliant.
- **Race-tolerance:** the test waits for Readable then close. Either ordering of {data, FIN} delivery from the kernel produces a correct close trajectory; the engine handles both.

#### C-9 — T13 `tcp.conn_open == tcp.conn_close` parity invariant is RFC-consistent

- **RFC clause:** `docs/rfcs/rfc9293.txt:1654` — TIME_WAIT terminates after 2×MSL (MUST-13). After expiry, a connection's bookkeeping is released (it counts as `conn_close`).
- **Engine behavior:** the `tcp.conn_open` counter increments at active-open ESTABLISHED transitions; `tcp.conn_close` increments at every CLOSED-state arrival (after TIME_WAIT expiry on the active-close side or directly on RST/LAST_ACK paths). 256 cycles × strict open/close parity is the right invariant for hermeticity.
- **Settle window (T13 lines 263–268):** 500 ms post-cycle drain is > 25× the configured TIME_WAIT (20 ms), giving every active-close TCB time to reach CLOSED before snapshot. The `flow_table.active_conns() == 0` post-condition (line 296–300) is independently checked — strong consistency oracle.

---

## Accepted deviations (covered by spec §6.4)

#### AD-1 — Receive window not shrunk as buffer fills (advertise free_space, accept at full capacity)

- **RFC clause:** `docs/rfcs/rfc9293.txt:937–941` — RCV.WND in the acceptability test is the "currently open receive window". The implicit reading is that as buffer fills, RCV.WND should shrink.
- **Spec §6.4 line 444:** "Receive-window shrinkage vs. buffer occupancy ... advertise free_space; accept at full capacity ... we keep the ingress seq-window check at initial capacity (`recv_buffer_bytes`, default 256 KiB) so we accept everything the peer sends, and expose the drop condition via `tcp.recv_buf_drops`."
- **Engine code:** `tcp_conn.rs:380` initializes `rcv_wnd = recv_buf_bytes.min(u16::MAX as u32)` once at connection creation; never reduced. The advertised window in outgoing ACKs (`engine.rs:326–331`) reflects real `free_space`, while the ingress check uses the larger `rcv_wnd`.
- **T11 dependency:** T11's "3073 < 4096 (rcv_wnd)" math relies on this deviation. With strict-RFC behavior, after 1024 bytes were delivered in-order plus 3072 bytes queued in reorder, RCV.WND would be ~0 and the overflow segment would fail the acceptability test (rejected as out-of-window before reaching cap-drop). The test correctly exercises the cap-drop path because of this deviation.

#### AD-2 — Delayed-ACK off by default (per-segment ACK)

- **RFC clause:** `docs/rfcs/rfc9293.txt:3485–3489` — MUST-58/-59: "the processing of received segments MUST be implemented to aggregate ACK segments whenever possible".
- **Spec §6.4 line 443:** "Delayed ACK ... off + per-segment ACK in A3; burst-scope coalescing in A6 ... 200ms ACK delay is catastrophic for trading."
- **Test relevance:** T11/T12 do not assert on outgoing ACK count beyond `tcp.tx_sack_blocks > 0`; T13 does not bound ACK rate. The trading-latency-default is consistent with the suites' expectations.

#### AD-3 — `AD-A6-force-tw-skip` not in use; default 2×MSL TIME_WAIT preserved

- **RFC clause:** `docs/rfcs/rfc9293.txt:1654` — "remain in the TIME-WAIT state for a time 2xMSL ... (MUST-13)".
- **Spec §6.4 line 464:** TIME_WAIT-skip is per-close opt-in; default behavior is RFC-exact 2×MSL.
- **T13 behavior:** uses `engine.close_conn(h)` (no `FORCE_TW_SKIP` flag). Default 2×MSL with `MSL = 10 ms` is honored. Spec §6.5 line 481 documents 2×MSL as RFC 9293 MUST-13 exact for the default close path.

---

## FYI (informational — no action)

#### I-1 — T12 Path B injects SACK ACKs that don't follow RFC 2018 §4 first-block rule

- The first-block-must-cover-trigger rule (`docs/rfcs/rfc2018.txt:254–260`) is an **emit-side** constraint on the data receiver. T12 simulates a hand-rolled peer-as-data-receiver injecting fabricated SACK ranges; this peer is not bound by RFC 2018 §4 because it's a test fixture, not a deployed receiver.
- The engine's role is to **decode** these blocks and update its scoreboard / retransmit queue. The decode path does not validate first-block-trigger correlation (such a check would require knowledge of the peer's reception order, which the receiver cannot have). Robustness to arbitrary block ordering from a synthetic peer is the right target for a hermeticity test.
- No action required. Real-peer first-block correctness is asserted elsewhere (T12 Path A exercises our own engine's emission, which honors §4 via `last_sack_trigger` machinery in `tcp_input.rs:1330–1332`).

#### I-2 — T13 close timing relies on the kernel's FIN arriving before our `close_conn`

- The test waits on Readable (data byte) then immediately calls `close_conn`. Depending on kernel scheduling, the kernel's FIN may arrive before our `close_conn` (we transition CLOSE_WAIT → LAST_ACK, RFC §3.6 Case 2) or after (we transition ESTABLISHED → FIN_WAIT_1 → FIN_WAIT_2 → TIME_WAIT, RFC §3.6 Case 1). Both trajectories are RFC-correct and end at CLOSED; the test's `Closed` event subscription is path-agnostic.
- The 500 ms settle window plus 30 s cycle timeout absorbs both paths' worst-case duration. No correctness gap.

---

## Per-question answers (from review brief)

### T11 (reassembly saturation)

1. **Window-acceptability for OOO at `seq = rcv_nxt + 3073`, `rcv_wnd = 4096`:** Yes, RFC 9293 §3.10.7.4 row-4 compliant. The 64-byte segment last-byte = `rcv_nxt + 3136 < rcv_nxt + 4096`. Engine's both-edges-in-window check passes. The test's claim "3073 < 4096" matches `in_window(start, seq, len)`'s half-open `[start, start+len)` semantics (`tcp_seq.rs:30`).
2. **Reassembly cap-overflow → silent drop, no RST:** Yes, correctly asserted (`tcp.tx_rst == 0`). RFC 9293 SHLD-31 (`rfc9293.txt:3545–3547`) and RFC 1122 §4.2.2.20 (`rfc1122.txt:5458–5468`) treat OOO queueing as SHOULD; resource-exhaustion fallback is silent drop, not RST.
3. **`rx_reassembly_hole_filled == 0` correctness:** Yes, valid because no in-order frame at `seq = rcv_nxt` is ever injected. The 1-byte gap remains open; the contiguous-drain path is never triggered.

### T12 (SACK hermeticity)

1. **SACK negotiation in handshake:** Yes, RFC 2018 §2 + §4 compliant. SYN carries `sack_permitted = true`; engine echoes via `build_connect_syn_opts` because `tcp_sack = true`; `conn.sack_enabled` set from peer's SYN at `tcp_input.rs:648`. SACK-permitted is sent only on SYN segments (RFC 2018 §2 MUST-NOT for non-SYN).
2. **Path B blocks contiguous, non-overlapping, `right > left`:** Yes, RFC 2018 §3 layout-compliant. `right = left + 256` always; consecutive blocks separated by 256-byte gaps. Decoder accepts (no `rx_bad_option` bumps).
3. **First-SACK-block-most-recent rule:** Not relevant for hermeticity testing — that's an emit-side constraint on the data receiver. The engine's decode path is correctly robust to arbitrary block ordering. Recorded as I-1 above.
4. **`tcp_timestamps = false` and RFC compliance:** No issues. RFC 7323 §3.2 (`rfc7323.txt:705–707`) explicitly handles the not-negotiated case ("MUST be ignored and the packet processed normally"). T12's Path-B ACKs carry no TS, consistent with non-negotiated state.

### T13 (option churn)

1. **256 sequential connect/close cycles over TAP:** No RFC issues. Each cycle is a fresh TCB with full option negotiation (MSS + SACK + WSCALE + Timestamps). ISS monotonicity protects against PAWS-class issues across same-4-tuple reuses (RFC 6528 §3 + spec §6.5 line 477).
2. **`tcp_msl_ms = 10` (TIME_WAIT = 20 ms):** RFC-compliant. RFC 9293 line 1107–1109 explicitly permits MSL as an engineering choice. Spec §6.4's `tcp_msl_ms` is not listed as a deviation — it's a configurable parameter.
3. **Active-close after Readable:** RFC 9293 §3.6 Case 2 compliant. CLOSE_WAIT → LAST_ACK trajectory (or ESTABLISHED → FIN_WAIT_1 if kernel FIN hasn't arrived yet); both end at CLOSED. Engine FSM (`engine.rs:5962–5966`) handles both.

---

## Gate decision

**PASS** — three new pressure-test suites (T11/T12/T13) ship with no RFC compliance issues. All assertions accurately model RFC 9293, RFC 2018, RFC 7323, and RFC 1122 invariants. Three pre-existing accepted deviations (§6.4 rows on advertise-free_space-accept-full-capacity, delayed-ACK off, default 2×MSL TIME_WAIT) are correctly leveraged. Two FYI items recorded for traceability; no action required.
