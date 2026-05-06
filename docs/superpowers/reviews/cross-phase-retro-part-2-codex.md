# Part 2 Cross-Phase Retro Review (Codex)
**Reviewer:** codex:codex-rescue
**Reviewed at:** 2026-05-05
**Part:** 2 — TCP FSM core, options, PAWS, reassembly, SACK
**Phases:** A3, A4

## Verdict

NEEDS-FIX.

I found four mechanical defects in the A3/A4 TCP path at HEAD that were not already covered by the per-phase reviews or Part 1. The strongest two are TCP sequence-length arithmetic errors around RST/TIME_WAIT handling. The other two are ordering/scope defects in A4 option/SACK processing: SACK blocks are applied before the ACK field is validated, and close-path states bypass the A4 PAWS/options gate entirely.

## Architectural drift

- **FYI — No new architecture finding from this mechanical pass.** The codebase has substantial HEAD drift after `phase-a4-complete` in the same TCP files, including zero-copy RX reassembly, RACK/TLP, timer wheel, test-server passive open, and hardware-offload paths. I treated that as review context rather than a standalone architecture finding, and only listed mechanical defects that still touch the A3/A4 TCP surfaces.

## Cross-phase invariant violations

- **BUG — Matched-flow RST ACK arithmetic omits SYN/FIN sequence length.** `send_rst_unmatched` correctly adds `payload_len + SYN + FIN`, and the A3 mTCP review already called that out as correct for unmatched flows. The matched-flow helper still computes the ACK as only `incoming.seq + incoming.payload.len()` at `crates/dpdk-net-core/src/engine.rs:4822`, then emits `RST|ACK` at `crates/dpdk-net-core/src/engine.rs:4832`. That is mechanically wrong for any `TxAction::Rst` generated in response to a SYN-bearing or FIN-bearing matched segment: the control bit consumes one sequence number, so the reset acknowledges one byte too little. One reachable HEAD path is the test-server SYN_RECEIVED duplicate-SYN error branch: `handle_syn_received` returns `TxAction::Rst` for a SYN with `seg.seq != conn.irs` at `crates/dpdk-net-core/src/tcp_input.rs:493-499`, and the engine routes it through this helper. The new mechanical angle is not the skip-listed SYN_SENT `RstForSynSentBadAck` fix, and not the already-covered unmatched-flow RST helper; it is the matched-flow RST helper's stale length formula.

- **BUG — TIME_WAIT refresh is triggered for every tuple-matching segment, not just in-window segments.** `handle_close_path` returns `TxAction::Ack` immediately for `TcpState::TimeWait` at `crates/dpdk-net-core/src/tcp_input.rs:1498-1504`, before the close-path ACK check and window check at `crates/dpdk-net-core/src/tcp_input.rs:1507-1527`. The engine then refreshes `time_wait_deadline_ns` whenever the connection is still `TimeWait` and `outcome.tx == TxAction::Ack` at `crates/dpdk-net-core/src/engine.rs:4476-4484`. The comment says "in-window", and the A3 RFC review F-2 fix was explicitly scoped to "any in-window segment"; at HEAD, an old duplicate outside `[rcv_nxt, rcv_nxt + rcv_wnd)`, a pure segment with a stale seq, or even a malformed control packet that parses and matches the tuple can keep extending TIME_WAIT. This is a timer-ordering and sequence-window defect, not a disagreement with the accepted 2xMSL policy.

- **BUG — Close-path states bypass A4 PAWS and missing-TS validation.** A4 added option parsing and PAWS in `handle_established`, including rejection of missing timestamps on a TS-enabled connection and stale `TSval` checks before ACK/data processing at `crates/dpdk-net-core/src/tcp_input.rs:790-856`. The close-path handler for `FinWait1`, `FinWait2`, `Closing`, `LastAck`, `CloseWait`, and `TimeWait` starts at `crates/dpdk-net-core/src/tcp_input.rs:1485` and performs RST handling, TIME_WAIT ACK, ACK-bit check, window check, ACK advance, and FIN processing through `crates/dpdk-net-core/src/tcp_input.rs:1535-1538` without parsing `seg.options` or checking `conn.ts_enabled`/`conn.ts_recent`. A stale timestamp or a missing TS option that would be dropped in ESTABLISHED can therefore advance `snd_una`, consume FIN, or refresh TIME_WAIT once the FSM enters close states. This is not covered by the A4 RFC review's established-state PAWS closure; it is a cross-state coverage hole in the same negotiated-option invariant.

## Tech debt accumulated

- **SMELL — A4 received-SACK state is mutated before the ACK field is known-valid.** In `handle_established`, peer SACK blocks are classified, inserted into `conn.sack_scoreboard`, and pushed into `conn.snd_retrans.mark_sacked()` at `crates/dpdk-net-core/src/tcp_input.rs:876-892`. Only after that does ACK validation run at `crates/dpdk-net-core/src/tcp_input.rs:896-1005`, including the future-ACK challenge branch at `crates/dpdk-net-core/src/tcp_input.rs:1005-1014`. A segment with `SEG.ACK > SND.NXT` can therefore poison advisory SACK state before being rejected as `rx_bad_ack`. I classify this as SMELL rather than BUG because SACK is advisory and later retransmission code may still be conservative, but mechanically the side effect belongs after the `SND.UNA < SEG.ACK <= SND.NXT` gate or behind an explicit "SACK allowed on duplicate ACK only" predicate.

## Test-pyramid concerns

- **SMELL — Existing tests pin the fixed SYN_SENT and SACK-block-count cases, but not the matched-RST length helper.** The per-phase reviews mention `syn_sent_plain_ack_wrong_seq_sends_rst` for `RstForSynSentBadAck`, and A4 added tests for four decoded SACK blocks. I did not find a default-build unit test that feeds a matched SYN/FIN-bearing segment into `emit_rst` and asserts the ACK includes the control-bit sequence length. That is why the unmatched-flow helper and matched-flow helper can have different arithmetic.

- **SMELL — TIME_WAIT tests appear to cover refresh existence, not unacceptable-segment filtering.** The A3 RFC review closed "refresh on retransmitted FIN", but the HEAD code refreshes based only on `TxAction::Ack`. A small pure `handle_close_path` test plus an engine-level test for stale seq in TIME_WAIT would pin the intended "in-window only" contract.

- **SMELL — PAWS coverage is established-state focused.** A4 tests and reviews cite established-state TS/PAWS behavior. I found no close-state test that starts with `ts_enabled=true`, sends a FIN_WAIT/LAST_ACK/TIME_WAIT segment without TS or with stale TS, and asserts `rx_bad_option`/`rx_paws_rejected` rather than ACK advance or timer refresh.

## Observability gaps

- **SMELL — Future-ACK packets can increment SACK/DSACK observability before `rx_bad_ack`.** Because SACK decode side effects precede ACK validation, `sack_blocks_decoded` is set from the parsed option count at `crates/dpdk-net-core/src/tcp_input.rs:893` and is preserved into the future-ACK `Outcome` at `crates/dpdk-net-core/src/tcp_input.rs:1008-1013`. `apply_tcp_input_counters` then adds `tcp.rx_sack_blocks` before or alongside the `tcp.rx_bad_ack` bump at `crates/dpdk-net-core/src/engine.rs:894-910`. That can make malformed/future ACKs look like useful SACK signal in counters.

- **SMELL — Close-state PAWS drops are unobservable because the check is absent.** `apply_tcp_input_counters` has the right counters for `rx_paws_rejected`, `ts_recent_expired`, and `rx_bad_option` at `crates/dpdk-net-core/src/engine.rs:873-883`, but close-path states never set those `Outcome` fields. Operators will see ACK/FIN/close behavior rather than the option defect that caused it.

## Memory-ordering / ARM-portability concerns

- **FYI — No new Atomic ordering defect in A3/A4 TCP code.** The scoped atomic operations I reviewed are counter increments/loads using `Ordering::Relaxed`, matching the project counter contract and Part 1's conclusion. I did not find a Relaxed load that was being used as a synchronization edge for non-atomic TCP connection state in the A3/A4 path.

## C-ABI / FFI

- **FYI — No new A3/A4 C-ABI finding beyond the skip-listed Part 1 synthesis.** The Part 1 synthesis already blocks on dead C ABI TCP option fields (`tcp_timestamps`, `tcp_sack`, `tcp_ecn`) and related config drift. I did not re-list those here.

## Hidden coupling

- **LIKELY-BUG — TIME_WAIT refresh correctness is coupled to the coarse `TxAction::Ack` outcome.** The engine cannot tell why `handle_close_path` chose ACK; it just tests `conn.state == TimeWait && outcome.tx == TxAction::Ack` at `crates/dpdk-net-core/src/engine.rs:4481`. That was sufficient when the handler only ACKed acceptable TIME_WAIT retransmits, but HEAD's early TIME_WAIT return at `crates/dpdk-net-core/src/tcp_input.rs:1500-1504` makes the timer-refresh code depend on a side effect that no longer encodes "segment acceptable". Static review confirms the coupling; the exact runtime impact depends on receiving stale tuple-matching traffic near the 2xMSL boundary.

## Documentation drift

- **FYI — The task prompt listed the A3/A4 phase specs under `docs/superpowers/specs/`, but this worktree stores those two documents under `docs/superpowers/plans/`.** `docs/superpowers/specs/2026-04-18-stage1-phase-a3-tcp-basic.md` and `docs/superpowers/specs/2026-04-18-stage1-phase-a4-options-paws-reassembly-sack.md` do not exist at HEAD; the matching files are `docs/superpowers/plans/2026-04-18-stage1-phase-a3-tcp-basic.md` and `docs/superpowers/plans/2026-04-18-stage1-phase-a4-options-paws-reassembly-sack.md`. This did not block the review because the main design spec and phase plans were still readable.

## FYI / informational

- **FYI — The prior A4 fixes for window scale and four-block SACK decode remain present at HEAD.** The decoder has separate `MAX_SACK_BLOCKS_EMIT = 3` and `MAX_SACK_BLOCKS_DECODE = 4` at `crates/dpdk-net-core/src/tcp_options.rs:37-48`, and the parser's SACK branch rejects invalid block counts while decoding up to four blocks at `crates/dpdk-net-core/src/tcp_options.rs:357-384`. I did not re-open the already-closed A4 E-2/F-8 findings.

- **FYI — The PR #9-style mbuf leak pattern did not reappear in the A4 reassembly queue at HEAD.** `ReorderQueue::drop_segment_mbuf_ref` now uses `shim_rte_pktmbuf_free_seg` at `crates/dpdk-net-core/src/tcp_reassembly.rs:293-313`, and `ReorderQueue::Drop` walks stored segments at `crates/dpdk-net-core/src/tcp_reassembly.rs:425-434`. I did not find a new A3/A4 reassembly path that allocates or ref-bumps an mbuf and then loses the ref on an early return.

## Verification trace

Commands run from `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review`:

- `git tag --list 'phase-a[234]-complete'`
- `git log --oneline phase-a2-complete..phase-a3-complete`
- `git log --oneline phase-a3-complete..phase-a4-complete`
- `git show --stat --oneline --find-renames phase-a2-complete..phase-a3-complete -- crates/resd-net-core/src crates/resd-net-core/tests crates/dpdk-net-core/src crates/dpdk-net-core/tests`
- `git show --stat --oneline --find-renames phase-a3-complete..phase-a4-complete -- crates/resd-net-core/src crates/resd-net-core/tests crates/dpdk-net-core/src crates/dpdk-net-core/tests`
- `git show --patch --find-renames phase-a2-complete..phase-a3-complete -- crates/resd-net-core/src crates/resd-net-core/tests crates/dpdk-net-core/src crates/dpdk-net-core/tests`
- `git show --patch --find-renames phase-a3-complete..phase-a4-complete -- crates/resd-net-core/src crates/resd-net-core/tests crates/dpdk-net-core/src crates/dpdk-net-core/tests`
- `git diff --stat phase-a4-complete..HEAD -- crates/dpdk-net-core/src/tcp_input.rs crates/dpdk-net-core/src/engine.rs crates/dpdk-net-core/src/tcp_options.rs crates/dpdk-net-core/src/tcp_reassembly.rs crates/dpdk-net-core/src/tcp_sack.rs crates/dpdk-net-core/src/tcp_conn.rs crates/dpdk-net-core/src/tcp_output.rs crates/dpdk-net-core/tests`
- `git log --oneline phase-a4-complete..HEAD -- crates/dpdk-net-core/src/tcp_input.rs crates/dpdk-net-core/src/engine.rs crates/dpdk-net-core/src/tcp_options.rs crates/dpdk-net-core/src/tcp_reassembly.rs crates/dpdk-net-core/src/tcp_sack.rs crates/dpdk-net-core/src/tcp_conn.rs crates/dpdk-net-core/src/tcp_output.rs`
- `git log --oneline -S 'sack_scoreboard.insert' -- crates/resd-net-core/src/tcp_input.rs crates/dpdk-net-core/src/tcp_input.rs`
- `git log --oneline -S 'fn emit_rst' -- crates/resd-net-core/src/engine.rs crates/dpdk-net-core/src/engine.rs`
- `git log --oneline -S 'if conn.state == TcpState::TimeWait' -- crates/resd-net-core/src/tcp_input.rs crates/dpdk-net-core/src/tcp_input.rs crates/resd-net-core/src/engine.rs crates/dpdk-net-core/src/engine.rs`
- `rg --files docs/superpowers/reviews crates/dpdk-net-core/src crates/dpdk-net-core/tests`
- `rg -n '\b(seq|ack|snd_una|snd_nxt|rcv_nxt|left|right|ts_val|ts_recent|window|wnd)\b.*(<|>|<=|>=|==)|(<|>|<=|>=|==).*\b(seq|ack|snd_una|snd_nxt|rcv_nxt|left|right|ts_val|ts_recent|window|wnd)\b' crates/dpdk-net-core/src/tcp_*.rs crates/dpdk-net-core/src/engine.rs`
- `rg -n 'Atomic|fetch_add|fetch_sub|load\(|store\(|compare_exchange|Ordering::|unsafe|rte_pktmbuf_alloc|rte_pktmbuf_free|timer_wheel\.(add|cancel)|RefCell|borrow_mut|\?;|if let Err|match .*Err|Box::from_raw|Box::into_raw|MaybeUninit|transmute' crates/dpdk-net-core/src/tcp_*.rs crates/dpdk-net-core/src/engine.rs crates/dpdk-net-core/tests`
- `rg -n 'fn handle_syn_sent|fn handle_established|fn handle_close_path|fn parse_sack|fn process|fn advance|fn insert\(|fn drain|fn prune|fn send_bytes|fn close_conn|fn emit_ack|fn build_ack_outcome|fn tcp_input|timer_wheel\.(add|cancel)|borrow_mut\(\)|rte_pktmbuf_alloc|rte_pktmbuf_free|MbufHandle::from_raw|shim_rte_mbuf_refcnt_update|wrapping_sub|wrapping_add|seq_lt|seq_le|seq_gt|seq_ge|in_window|fetch_add' crates/dpdk-net-core/src/tcp_input.rs crates/dpdk-net-core/src/engine.rs crates/dpdk-net-core/src/tcp_options.rs crates/dpdk-net-core/src/tcp_reassembly.rs crates/dpdk-net-core/src/tcp_sack.rs crates/dpdk-net-core/src/tcp_conn.rs crates/dpdk-net-core/src/tcp_output.rs`
- `rg -n 'future ACK|bad_ack|bad ACK|SACK|scoreboard|TIME_WAIT|TimeWait|emit_rst|RST|PAWS|close path|FIN_WAIT|LastAck|SYN-ACK.*option|bad option' docs/superpowers/reviews/phase-a3-mtcp-compare.md docs/superpowers/reviews/phase-a3-rfc-compliance.md docs/superpowers/reviews/phase-a4-mtcp-compare.md docs/superpowers/reviews/phase-a4-rfc-compliance.md docs/superpowers/reviews/cross-phase-retro-part-1-codex.md docs/superpowers/reviews/cross-phase-retro-part-1-synthesis.md`
- `sed -n '1,240p' docs/superpowers/reviews/phase-a3-mtcp-compare.md`
- `sed -n '1,260p' docs/superpowers/reviews/phase-a3-rfc-compliance.md`
- `sed -n '1,280p' docs/superpowers/reviews/phase-a4-mtcp-compare.md`
- `sed -n '1,320p' docs/superpowers/reviews/phase-a4-rfc-compliance.md`
- `sed -n '1,320p' docs/superpowers/reviews/cross-phase-retro-part-1-codex.md`
- `sed -n '1,360p' docs/superpowers/reviews/cross-phase-retro-part-1-synthesis.md`
- `sed -n '330,430p' docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md`
- `rg --files docs/superpowers | rg '2026-04-18-stage1-phase-a[34]|phase-a3|phase-a4|options-paws|tcp-basic'`
- `nl -ba crates/dpdk-net-core/src/tcp_seq.rs crates/dpdk-net-core/src/tcp_conn.rs crates/dpdk-net-core/src/tcp_options.rs crates/dpdk-net-core/src/tcp_reassembly.rs crates/dpdk-net-core/src/tcp_sack.rs | sed -n '1,1200p'`
- `nl -ba crates/dpdk-net-core/src/tcp_input.rs | sed -n '1,1700p'`
- `nl -ba crates/dpdk-net-core/src/tcp_reassembly.rs | sed -n '1,460p'`
- `nl -ba crates/dpdk-net-core/src/tcp_sack.rs | sed -n '1,180p'`
- `nl -ba crates/dpdk-net-core/src/tcp_input.rs | sed -n '400,1120p'`
- `nl -ba crates/dpdk-net-core/src/tcp_input.rs | sed -n '1110,1565p'`
- `nl -ba crates/dpdk-net-core/src/tcp_input.rs | sed -n '858,1016p;1485,1528p'`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '4000,4680p'`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '4680,5680p'`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '5680,6225p'`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '4814,4845p;4476,4487p'`
- `nl -ba crates/dpdk-net-core/src/tcp_options.rs | sed -n '1,330p'`
- `nl -ba crates/dpdk-net-core/src/tcp_options.rs | sed -n '320,430p'`
- `nl -ba crates/dpdk-net-core/src/tcp_output.rs | sed -n '1,320p'`
- `nl -ba crates/dpdk-net-core/src/tcp_retrans.rs | sed -n '1,175p'`
- `rg -n 'fn apply_tcp_input_counters|rx_bad_option|rx_reassembly_queued|rx_zero_window|rx_dup_ack|rx_sack_blocks' crates/dpdk-net-core/src/engine.rs crates/dpdk-net-core/src/counters.rs`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '840,930p'`

I also re-read the cited HEAD lines immediately before writing each finding.
