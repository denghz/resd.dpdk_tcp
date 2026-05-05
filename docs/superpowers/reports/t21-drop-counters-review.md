# T21 drop-site-counter wiring — implementation + reviews (2026-05-05)

## Implementation

Surface the 5 existing TCP-input drop-site counters in the bench-vs-mtcp
stall-bail diagnostic message so the next bench-pair run gives hard
attribution data on which `handle_established` validation rejected
the peer's ACK during the C=1 large-W stall.

Subagent-produced (general-purpose, opus 4.7, task `ad962108`).

### Files changed

- `crates/dpdk-net-core/src/engine.rs` (+38): `InputDropsSnapshot` Copy POD struct (5×u64) + `pub fn diag_input_drops(&self) -> InputDropsSnapshot` accessor.
- `tools/bench-vs-mtcp/src/dpdk_burst.rs` (+32/-3): warmup / first-segment / drain-mid-flight bail messages append `| input_drops: bad_seq=N bad_option=N paws_rejected=N bad_ack=N urgent_dropped=N`.
- `tools/bench-vs-mtcp/src/dpdk_maxtp.rs` (+27/-3): wedged-bucket stderr log appends the same shape.

### Counters wired (no new counters added)

All 5 counters already existed at `crates/dpdk-net-core/src/counters.rs:163-189` and were already incremented per-segment by `apply_tcp_input_counters` (`engine.rs:868-927`) based on `Outcome` flags set in `tcp_input.rs::handle_established`:

| Drop site | Line | Flag | Counter |
|---|---|---|---|
| URG drop | 736 | `urgent_dropped` | `rx_urgent_dropped` |
| out-of-window | 782 | `bad_seq` | `rx_bad_seq` |
| option parse fail | 798 | `bad_option` | `rx_bad_option` |
| TS missing on TS-conn | 814 | `bad_option` | `rx_bad_option` |
| PAWS fail | 838 | `paws_rejected` | `rx_paws_rejected` |
| ACK ahead of snd_nxt | 1008 | `bad_ack` | `rx_bad_ack` |

### Drop sites NOT counted (justified)

- `tcp_input.rs:761` (no-ACK seg in ESTABLISHED) — RFC-malformed traffic, separate concern from peer-ACK loss.
- `tcp_input.rs:1428` (seq < rcv_nxt) — soft drop, not a rejection.
- `handle_close_path:1522` — already uses `bad_seq` flag, wired transitively.
- `handle_syn_received` / `handle_syn_sent` — pre-ESTABLISHED, out of T21 scope.

## Three-stage review

### Spec-compliance review (subagent `a3a600b1`, general-purpose opus 4.7)

**Verdict: PASS / ship-as-is.**

- All 5 counters covered. No 6th drop site exists in `handle_established` (grep-confirmed via `awk 'NR>=719 && NR<=1483 && /return Outcome/'`).
- Diag emission consistent across all 4 sites. `anyhow::bail!` produces single-line output (grep-friendly).
- Slow-path discipline: `Ordering::Relaxed` on all loads + increments (ARM-safe).
- Build + 424 unit tests pass.

### Code-quality review (subagent `a45525810`, superpowers:code-reviewer opus 4.7)

**Verdict: PASS-WITH-CAVEATS / ship-as-is.** No correctness defects. One non-blocking cohesion suggestion:

- The 5-field format-tail (`bad_seq={} bad_option={} paws_rejected={} bad_ack={} urgent_dropped={}`) is duplicated verbatim at 4 emission sites. Optional refactor: `impl Display for InputDropsSnapshot` to collapse to `... | input_drops: {drops}`. Eliminates drift risk if a 6th counter lands.
- Filed as follow-up; not a blocker.

### Codex review (subagent `ad6a7f55`, codex:codex-rescue opus 4.7)

**Verdict: NEW-FINDING (non-critical).**

One uncovered drop path: `tcp_input.rs:759` — `if (seg.flags & TCP_ACK) == 0 { return Outcome { rx_zero_window, ..Outcome::base() }; }`. Returns `Outcome::base()` with NO flags set. Not critical for the C=1 healthy-peer ACK stall hypothesis because peer window-update / ACK segments must carry TCP_ACK by definition. If this site fires it indicates malformed peer traffic — separate concern from T21's investigation scope. Spec-compliance reviewer also accepted this site as out-of-scope.

Codex confirmed:
- All 6 ACK-carrying drop sites covered (URG @ 733-740, bad_seq @ 779-787, bad_option @ 790-804 + 808-820, paws_rejected @ 837-843, bad_ack @ 1005-1014). Engine maps flags to atomics at `engine.rs:873-916`.
- Diag emission format `input_drops: bad_seq={} ...` parseable across all 4 sites; Rust `u64` Display is locale-independent decimal.
- Snapshot loads are 5 separate relaxed atomics (`engine.rs:2476-2480`) — fine for stall attribution but not a proof of "no new drops between snapshots".

Codex couldn't run cargo locally (sandbox read-only) but spec + code-quality both confirmed build clean and 424 tests pass — sufficient cross-verification.

### Refined hypothesis-vs-counter prediction table (codex)

| Hypothesis | bad_seq | bad_option | paws_rejected | bad_ack | urgent_dropped | Notes |
|---|---:|---:|---:|---:|---:|---|
| **H1**: snd_wnd-stuck | likely >0 (window update rejected by seq-window gate) | possible >0 (TS/options invalid) | possible >0 (TS regression) | usually 0 | 0 | Window-update is gated by ACK-advance + window rules at `tcp_input.rs:970-982`; rejection before that surfaces as bad_seq/bad_option/PAWS. |
| **H2**: snd_una-stuck | possible >0 | possible >0 | possible >0 | likely >0 (ACK ahead of `snd_nxt`) | 0 | New-ACK advance only accepted at `tcp_input.rs:901-907`; future-ACK rejections hit bad_ack at `1005-1014`. |
| **H4**: state-leakage | possible >0 | usually 0 unless parser corrupted | possible >0 | possible >0 | usually 0 | Corrupt `rcv_nxt`, `snd_nxt`, or `ts_recent` points at bad_seq, bad_ack, or PAWS respectively. **Multiple counters rising together strengthens H4.** |

If ALL counters stay 0 during the stall: **H1 wire-level** — peer ACKs aren't reaching the engine at all (gateway / RSS queue routing / ARP staleness). That's the most-likely-root-cause-isn't-engine signal; check `tcpdump` on the failing run.

## Build verification

```
cargo check -p dpdk-net-core   # clean (pre-existing warnings only)
cargo check -p bench-vs-mtcp   # clean
cargo test -p dpdk-net-core --lib --test-threads=1   # 424 pass / 1 ignored / 0 fail
```

## What this gets us next bench-pair

When a C=1 W={4096,65536} bucket stalls, the diag-bail message will say:

```
warmup burst stalled with 4096/65536 bytes accepted (no forward progress in 180s) | diag: snd_una=N snd_nxt=N+4096 in_flight=4096 snd_wnd=K room_in_peer_wnd=L srtt_us=M rto_us=N | input_drops: bad_seq=A bad_option=B paws_rejected=C bad_ack=D urgent_dropped=E
```

Predicted decoder (per the T21 investigation `t21-stall-investigation.md`):

| Hypothesis | Expected diag pattern |
|---|---|
| **H1** snd_wnd never advances | `snd_wnd` small constant + `srtt_us=0` + ALL `input_drops` = 0 (peer ACKs simply not arriving — wire-level issue) |
| **H1'** snd_wnd advances once but engine drops subsequent ACKs | `snd_wnd` small + `srtt_us=0` + `bad_seq` or `paws_rejected` or `bad_ack` non-zero |
| **H2** snd_una stuck despite ACKs | `snd_wnd >= 64K` + `bad_ack` non-zero (engine receives ACKs but rejects them as "ahead of snd_nxt") |
| **H4** state leakage | `snd_una != initial_iss + 1` carryover, drops-counters mostly 0 |

Codex post-gate review will refine this prediction table once it lands.
