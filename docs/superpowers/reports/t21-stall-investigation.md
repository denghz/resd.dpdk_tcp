# T21 — Engine TX-path cwnd-stuck @ C=1 large-W investigation (2026-05-05)

**Source:** general-purpose subagent (opus 4.7), task `af6a487d5ae6c0932`. Read-only code investigation, no code changes.

## Bug recap

`bench-vs-mtcp` maxtp grid at C=1 shows non-monotonic stalls:

| C | W (bytes) | Goodput (Mbps) |
|---|-----------|----------------|
| 1 | 64        | 248            |
| 1 | 256       | 789            |
| 1 | 1024      | 2,416          |
| 1 | 4096      | **0**          |
| 1 | 16384     | 4,152          |
| 1 | 65536     | **0**          |

Burst grid: warmup burst 1 stalls at exactly **4096/65,536 bytes accepted in 180s** (4 KiB ≈ 3 MSS ≈ ~IW10's worth).

C=4 and C=16 succeed at every W. The stall is single-conn-specific and W-dependent in a non-monotonic way.

## Most-likely root cause (prioritised)

**H1 — snd_wnd-stuck after first burst is most likely.** The 4096-byte specific value is suspicious: that's exactly `snd_wnd` if the peer's SYN-ACK advertised an unscaled window of 4096. Per RFC 7323, SYN/SYN-ACK windows are never WS-shifted; the engine stores the raw 16-bit value at `tcp_input.rs:629`:

```rust
conn.snd_wnd = seg.window as u32;
```

If after 4096 bytes accepted the peer's pure-no-data ACKs arrive but `seg.ack == snd_una` (delayed ACK or piggyback before any data is ACKed), the **window-update gate is inside the `if seg.ack > snd_una` branch (line 901)**. Pure-window-update ACKs that don't advance ack hit the `else` branch (line 1015) which does NOT update snd_wnd. RFC 9293 says window updates may arrive on segments that don't advance ack — this is a conformance gap.

For our bench, every Linux ACK after we send data covers ≥1 byte, so this gap shouldn't be fatal under healthy conditions. But under specific timing (peer batches an ACK), it could become the latch.

**H4 (state leakage) less likely** since W=16384 (between two failures) succeeds — the alternating pattern doesn't fit deterministic per-bucket leakage.

## Falsifiable predictions (decode T21 diag in `66afff7`)

The diag added at the three bench-arm bail sites prints `snd_una snd_nxt snd_wnd send_buf_pending send_buf_free srtt_us rto_us`. Decoder:

| Symptom | Likely root cause |
|---|---|
| `snd_wnd == 4096` (or other small constant), `srtt_us == 0` | **H1**: snd_wnd never updated. Peer ACKs never absorbed (or only ack-advance ACKs are absorbed and they aren't arriving). |
| `snd_wnd >= 64K`, `snd_nxt - snd_una == 4096`, `srtt_us == 0` | **H2**: send-side stuck. ACKs arriving but engine not advancing snd_una. Look at `tcp_input.rs::handle_established` ACK-advance logic. |
| `snd_una != initial_iss + 1` (carryover), correlated with bucket parity | **H4**: state leakage. Look at `close_persistent_connections` reaping vs flow-table slot reuse. |

## Defect found in T21 diag itself

**Important caveat from agent:** the diag's `send_buf_bytes_pending` field comes from `tcp_conn.rs:725-731`:
```rust
let pending = self.snd.pending.len() as u32;
```
But `snd.pending` is only used in TLP/probe code (Stage 2 follow-up — see `engine.rs:3121` comment). The production `send_bytes` path does NOT push to `snd.pending`. So `send_buf_pending` is **always 0** and `send_buf_free` is always = full send buffer size in the diag. **Misleading.**

**Fix:** the diag should emit `in_flight = snd_nxt - snd_una` (actual unacked bytes) and `room_in_peer_wnd = snd_wnd - in_flight` (the value `send_bytes` actually uses to clamp acceptance). These ARE the signals the operator wants to see. Applied as a follow-up commit.

## Shortest path to fix / further diagnosis

**Highest-value next step (per agent):** add slow-path counters at every input-drop site in `tcp_input.rs::handle_established` — at minimum:
- `bad_seq` (~line 783)
- `bad_option` (~line 800/816)
- `paws_rejected` (~line 840)
- `bad_ack` (~line 1010)
- `urgent_dropped` (~line 738)

Re-run with `acked_bytes_in_window == 0` cells dumping these. The non-zero counter on the failing bucket reveals exactly which input-validation rejection drops the peer's ACK. This is concrete fix-driving signal.

**Lower-effort sanity check:** `tcpdump -i <iface>` the failing C=1 W=4096 run for 5s. Asymmetry on wire (peer sending ACKs, engine not absorbing) confirms input-drop hypothesis. No peer ACKs at all flips suspect to send-side / handshake / arp.

## Smoking-gun bug?

**Not from code reading alone.** The W-specific deterministic stall (W=4096 fails, W=16384 succeeds, W=65536 fails) under intervening C>1 success at the same W is hard to explain by send-side bug. Suggests **input-side**: a sequence-space property at those W values triggers a quirk in inbound ACK processing.

The agent didn't find a clear single-cause defect. Counters at drop sites + tcpdump are the ways to actually prove H1 vs H2 vs H4.

## Status

- T21 hypothesis ranking: **H1 (snd_wnd-stuck) > H2 (snd_una-stuck) > H4 (state leakage)**.
- Diag from `66afff7` is misleading on `send_buf_pending` — fix in this iteration.
- Drop-site counters: separate follow-up (~30 LOC across 5 sites).
- Real diagnostic data needs another bench-pair run. Cost ~$1-2.
