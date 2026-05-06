# Code-Quality Review — T11 / T12 / T13 Pressure Suites

**Reviewer date:** 2026-05-06
**Worktree:** `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review/`
**Files under review:**
- `crates/dpdk-net-core/tests/pressure_reassembly_saturation.rs` (T11)
- `crates/dpdk-net-core/tests/pressure_sack_blocks.rs` (T12)
- `crates/dpdk-net-core/tests/pressure_option_churn.rs` (T13)

---

## 1. Blocking issues

**None identified.** All three suites are structurally correct and will pass under the documented invariants. The arithmetic for window/seq, the cap-overflow math, the SACK encode/decode limits, and the FSM transitions all check out against `tcp_input.rs`, `tcp_options.rs`, and `engine.rs`. The teardown safety net is `CovHarness::Drop -> test_clear_pinned_rx_mbufs` (`tests/common/mod.rs:489`), which clears `recv.bytes`, `delivered_segments`, and `recv.reorder` for every conn before `Engine::drop` runs — so even without explicit `close_conn` calls the suites do not UAF on mempool teardown.

---

## 2. Non-blocking issues

### T11 — `pressure_reassembly_saturation.rs`

**T11-N1: Misleading comment about close-vs-assert ordering.**
`pressure_reassembly_saturation.rs:201-203`

```rust
// Close the connection to release the reorder-queue mbufs before the
// harness teardown so the pool-drift assertion remains valid.
let _ = h.eng.close_conn(conn);
```

The comment claims the close is needed for the pool-drift assertion, but the pool-drift assertion has already executed at lines 190-199. The 3 OOO mbufs + 1 in-order mbuf are pinned at assertion time. The test only passes because `Range(-32, 32)` tolerance is far wider than 4 mbufs AND because the level-counter sample is TSC-gated to once per second (so `rx_mempool_avail` typically reads identically pre/post on a fast smoke test). Suggested rewrite:

```rust
// Close the connection so harness teardown sees a quiesced FSM.
// (Pinned reorder-queue mbufs are released by CovHarness::Drop ->
// test_clear_pinned_rx_mbufs regardless; the pool-drift assertion
// above tolerates ±32 mbufs of drift.)
```

**T11-N2: `recv_buf_drops` counted in *bytes*, not segments.**
The cap-overflow frame carries 64 bytes; the comment at line 168 ("at least one cap-drop") is consistent with `Relation::Gt(0)`, but a careful reader expecting "1 drop" might be confused. Worth tightening to e.g. `Relation::Ge(64)` so the assertion documents the byte semantic explicitly.

**T11-N3: No `flow_table().active_conns() == 0` assertion.**
T4/T5 verify the connection table is fully drained. T11 closes the conn only after `bucket.finish_ok` would otherwise have run — and the close is not pumped to completion (no FIN/ACK exchange driven). The conn likely sits in FIN_WAIT_1 at test end. Not a correctness failure (CovHarness::Drop handles it), but a minor consistency gap with peers.

**T11-N4: Hard-coded source/dest tuple values duplicated.**
The OOO frames at lines 113-124 hard-code `40_000` (peer port), `5555` (our port). T12 hoists these into `PEER_PORT` / `OUR_PORT` constants. Consistency upgrade.

### T12 — `pressure_sack_blocks.rs`

**T12-N1: Missing `h.our_iss.set(...)` and `h.peer_seq.set(...)` after manual handshake.**
`pressure_sack_blocks.rs:113-127` parses the SYN-ACK locally and stores the result in a stack `our_iss`. The `CovHarness` fields stay at 0. This is benign for THIS test (no helpers are called that read those fields), but it's a footgun: any future maintainer who appends `h.inject_peer_data(...)` after the manual handshake would silently produce a malformed segment with `our_iss=0`. Recommend:

```rust
let (our_iss, _ack) = dpdk_net_core::test_server::test_packet::parse_syn_ack(syn_ack)
    .expect("parse SYN-ACK");
h.our_iss.set(our_iss);
h.peer_seq.set(PEER_ISS.wrapping_add(1));
```

This costs nothing and makes the harness invariant `our_iss/peer_seq always match the live conn` hold uniformly.

**T12-N2: `parse_syn_ack` called via fully-qualified path instead of the `common::parse_syn_ack` re-export.**
`pressure_sack_blocks.rs:113` writes `dpdk_net_core::test_server::test_packet::parse_syn_ack(...)` while `tests/common/mod.rs:317-320` re-exports `common::parse_syn_ack` for exactly this use case. Style consistency with prior suites.

**T12-N3: Path B ACK has `ack = our_iss + 1` but engine has `snd_una = our_iss + 1` and `snd_nxt = our_iss + 1`.**
The injected ACK exactly equals snd_una (no new data, no in-flight). Per `tcp_input` this is processed as a "duplicate ACK at snd_una" — which is fine for the SACK-decode hermeticity claim — but means the SACK update doesn't actually walk a non-empty retransmit queue. So Path B exercises the *option decoder* but not the SACK-on-retx-queue update. The doc-comment at lines 162-174 should clarify this: the test name is "hermeticity" so the limited scope is intentional, but the description currently reads as if the SACK-update path is exercised.

**T12-N4: No active conn cleanup before `bucket.finish_ok`.**
Same as T11-N3. After Path B, the conn stays in ESTABLISHED. `close_conn` at line 222 fires FIN but isn't driven to completion. CovHarness::Drop saves teardown safety. Minor.

### T13 — `pressure_option_churn.rs`

**T13-N1: `send_bytes` error case spins for 30s without diagnostics.**
`pressure_option_churn.rs:206-209`

```rust
match engine.send_bytes(h, &[0x42]) {
    Ok(n) if n >= 1 => break,
    _ => {}
}
```

Both `Ok(0)` (backpressure) and `Err(_)` (e.g., conn died) fall to `_`. If the conn fails for a real reason, the loop spins 30s before panicking with a generic "send timeout" message. Better diagnostics:

```rust
match engine.send_bytes(h, &[0x42]) {
    Ok(n) if n >= 1 => break,
    Ok(0) => {} // backpressure
    Err(e) => panic!("cycle {cycle}: send_bytes returned Err: {e:?}"),
    _ => {}
}
```

This matches T4 (`pressure_conn_churn.rs:236-243`)'s discrimination.

**T13-N2: Settle window timing — borderline but adequate.**
`tcp_msl_ms = 10` → TIME_WAIT = 20ms. Settle = 500ms = 25× TIME_WAIT. After 256 cycles with sequential opens, only the LAST cycle's TIME_WAIT (or LAST_ACK reaper) is in flight at settle entry. 500ms is fine — but worth noting the suite would still pass with `tcp_msl_ms = 50` (settle = 5× TW) if a future change wanted longer TIME_WAIT for SACK loss-recovery testing in adjacent suites.

**T13-N3: `flow_table().active_conns() == 0` check is correct but timing-dependent.**
The poll-and-drain settle loop at lines 263-268 runs for exactly 500ms. If TIME_WAIT reaper happens to need one more `poll_once` after the 500ms boundary, the check at line 296 fails. Mitigation: the settle pump runs `poll_once` every 2ms = 250 iterations × `poll_once`, and `reap_time_wait` runs inside `poll_once`. Adequate, but a comment on line 263 explaining "≈250 poll_once invocations covers all TIME_WAIT deadlines from the last 256 cycles" would help future readers.

**T13-N4: Minor — `set_nonblocking(false)` is the default for a fresh `TcpListener`.**
`pressure_option_churn.rs:140` is a no-op on Linux. T5 makes the same explicit set; harmless redundancy.

---

## 3. Confirmed correct patterns

### T11 — `pressure_reassembly_saturation.rs`

- **T11-C1**: `recv_buffer_bytes = 4096` with `SEG_BYTES = 1024` ratio is correct. After 1 in-order (1024) + 3 OOO (3072), `free_space_total = 4096 - 1024 - 3072 = 0`. The 4th OOO frame hits the `total_cap == 0` branch in `tcp_input.rs:1411-1412`, dropping the entire 64-byte payload and bumping `recv_buf_drops` by 64. Math verified.

- **T11-C2**: `h.our_iss.get().wrapping_add(1)` ack value is correct. After `do_passive_open`, the engine's snd_una/snd_nxt are at `our_iss + 1` (post-SYN-ACK). The peer must ack `our_iss + 1`. Verified via `tests/common/mod.rs:761`.

- **T11-C3**: Window math for OOO frames. With `recv_buffer_bytes = 4096`, `compute_ws_shift_for(4096) = 0` (since 65535 ≥ 4096), so `rcv_wnd = 4096`. OOO-3 seq offset = 2049, payload = 1024, frame ends at 3073. `3073 < 4096` → in-window. The cap-overflow frame at offset 3073 with 64 bytes ends at 3137 < 4096 → in-window. Both pass `seq_in_window` checks before reaching the cap-drop branch.

- **T11-C4**: `drain_tx_frames()` after each `inject_rx_frame` + `poll_once` matches the T5/T7 pattern. `drain_events(64, |_, _| {})` after each iteration prevents stuck-event leakage into the snapshot. Both correct.

- **T11-C5**: TSC-gated mempool-avail sampling means `Range(-32, 32)` is a defensive widening; the actual delta on a fast smoke test is typically 0 (same TSC bucket pre/post). The tolerance handles slow-CI cases where sampling may fire mid-test.

### T12 — `pressure_sack_blocks.rs`

- **T12-C1**: SYN with `sack_permitted = true` correctly negotiates SACK during handshake; engine records `conn.sack_permitted = true` and emits SACK blocks in subsequent ACKs.

- **T12-C2**: Path A OOO-frame seq calculation creates 8 disjoint gaps. With OOO_SEG=256 and 2×OOO_SEG stride, ranges are `[256,512), [768,1024), [1280,1536), … [3840,4096)` — all distinct, each ≥ 1 byte from prior, each ≤ `recv_buffer_bytes = 256 KiB`. Engine emits ACKs with `last_sack_trigger` populated, capped at `MAX_SACK_BLOCKS_EMIT = 3` per ACK.

- **T12-C3**: Path B SACK encoding fits the 40-byte option budget. With `tcp_timestamps = false`, the option block is just `NOP+NOP+SACK_kind+len+4*8 = 36 bytes`, well within the 40-byte cap. `push_sack_block_decode` (`tcp_options.rs:94`) caps at 4 blocks — 4 SackBlocks accepted.

- **T12-C4**: `recv_buffer_bytes` defaulting to 256 KiB is appropriate — the 8 OOO frames consume ~2 KiB, well below the cap. No buf_full_drop expected and none asserted.

- **T12-C5**: After test, mbufs from OOO frames in Path A are still in the reorder queue. `close_conn` at line 222 drives a FIN attempt; harness `Drop -> test_clear_pinned_rx_mbufs` (`engine.rs:7508`) clears `c.recv.reorder` BEFORE `Engine::drop` runs, so `ReorderQueue::Drop` (`tcp_reassembly.rs:425`) decrements the per-segment refcounts against still-live mempools. Confirmed safe.

### T13 — `pressure_option_churn.rs`

- **T13-C1**: TAP interface `resdtap40` and IP block `10.99.40.x` are distinct from peer suites (T4 uses `resdtap31`/`10.99.31.x`, T5 uses `resdtap32`/`10.99.32.x`). Clean per-T13 namespace.

- **T13-C2**: The `Ok(n) if n >= 1` discriminator handles backpressure correctly. T13's send-loop is bounded by a 30s deadline.

- **T13-C3**: `flow_table().active_conns() == 0` after settle is the right structural check. Matches T4's pattern.

- **T13-C4**: 256-cycle bound is appropriate for the `pressure-test` cargo-feature gate (CI-only, sudo-required). EngineConfig `max_connections = 32` provides slot headroom for TIME_WAIT overlap during the sequential cycles.

- **T13-C5**: `TcpListener::bind((PEER_IP_STR, PEER_PORT))` uses correct kernel-side semantics — engine connects from `OUR_IP` to `PEER_IP:PEER_PORT`, kernel listens on `PEER_IP:PEER_PORT`. Direction verified.

- **T13-C6**: Full option set negotiated per-cycle: `tcp_timestamps = true` + `tcp_sack = true` + WSCALE (auto from `recv_buffer_bytes` default 256KiB) + MSS. Each handshake exercises all four option encoders/decoders.

---

## 4. Summary

All three suites are accept-quality. The only "blocking-class" concerns asked about in the prompt resolve in favor of the suite design:

- **T11 Q1 (recv_buffer math):** correct; `free_space_total = 0` after 1 in-order + 3 OOO frames.
- **T11 Q2 (close before pool-drift):** comment is misleading — close happens AFTER assert. But the assertion still passes due to Range tolerance + TSC-gated sampling. Minor doc fix.
- **T11 Q3 (drain_events sufficient):** yes; matches T5/T7 pattern.
- **T11 Q4 (ack value):** correct.
- **T12 Q1 (`our_iss.set` after manual handshake):** missing but harmless for THIS test; future-proofing fix recommended.
- **T12 Q2 (snd_una math):** correct; engine snd_una = our_iss + 1 after handshake.
- **T12 Q3 (close releases reorder mbufs):** safe via CovHarness::Drop -> test_clear_pinned_rx_mbufs path; close_conn at line 222 is a defensive no-op for the teardown safety case.
- **T12 Q4 (CovHarness::drop fields):** the `our_iss`/`peer_seq` fields are not read by `Drop`, only by injection helpers; the missing `set` calls are a robustness concern, not a correctness one.
- **T13 Q1 (listen call):** N/A; T13 doesn't call `engine.listen()` — it does active connects only. `TcpListener::bind` is the kernel side.
- **T13 Q2 (send_bytes error handling):** suboptimal but timeout-bounded; fix recommended for diagnostics.
- **T13 Q3 (settle window):** 500ms = 25× TIME_WAIT, adequate.
- **T13 Q4 (active_conns == 0):** correct invariant for verifying full close.

**Recommended priority order for follow-up fixes** (none gate the merge):
1. T11-N1 (rewrite misleading close-comment).
2. T12-N1 (set `h.our_iss` / `h.peer_seq` after manual handshake — future-proofing).
3. T13-N1 (discriminate Err vs Ok(0) for clearer panic on transport failure).
4. T11-N2, T12-N2 (style consistency — assertion strength, parse_syn_ack call site).
