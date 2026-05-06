# Part 4 Cross-Phase Retro Synthesis

**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 4 — Public API + zero-copy + FFI safety
**Phases:** A6, A6.5, A6.6, A6.7
**Inputs:** cross-phase-retro-part-4-claude.md, cross-phase-retro-part-4-codex.md

## Combined verdict

The Part-4 ABI surface is mechanically intact (21 `extern "C"` symbols
emitted, layout assertions present, `MbufHandle::Drop` post-PR-#9 fix
preserved, panic-firewall test real), but two reviewers converge on a
must-fix correctness defect cluster. **Codex flags two new BUGs not seen
by Claude:** (1) READABLE event lifetime is broken across queued
`dpdk_net_poll` calls — `poll_once` clears every connection's
`readable_scratch_iovecs` at the top before `drain_events` runs, so any
queued READABLE that survives a previous `max_events` cap reconstructs
its `segs` view against now-cleared scratch (potential UAF / empty view
to C); (2) the chained-mbuf RX path enters L3 with a head-segment-only
slice via `mbuf_data_slice`, so legitimate multi-segment frames whose IP
total length spans the chain are dropped at `l3_ip.rs` before the TCP
chain walker can run. Both reviewers independently flag the
`tests/ffi-test/tests/ffi_smoke.rs` `Cfg` mirror missing the
`rx_mempool_size` tail field as a concrete unsafe FFI precondition
violation (OOB read when `DPDK_NET_TEST_TAP=1`). Beyond that, both agree
on `Engine::events()` leaking `RefMut<EventQueue>`, the
`rx_mempool_size` formula doc-comment drift (2× vs 4×), and the
`Relaxed` ordering / `MbufHandle` Drop being sound under the
single-lcore invariant.

## BLOCK A11 (must-fix before next phase)

- **[BUG][CODEX] READABLE event lifetime invalidated across queued polls.**
  `dpdk_net_poll` calls `e.poll_once()` before checking `events_out` /
  `max_events` (`crates/dpdk-net/src/lib.rs:504-505`); top-of-poll
  clears `delivered_segments` and `readable_scratch_iovecs` for every
  connection (`crates/dpdk-net-core/src/engine.rs:2554, :2570`);
  `drain_events` only drains up to caller's `max`
  (`crates/dpdk-net-core/src/engine.rs:3681-3683`). `InternalEvent::Readable`
  stores only `conn`, `seg_idx_start`, `seg_count`, `total_len`
  (`crates/dpdk-net-core/src/tcp_events.rs:46-52`), so a later drain
  reconstructs `segs` from cleared scratch
  (`crates/dpdk-net/src/lib.rs:530, :543`) and may return a dangling or
  empty view. Violates header promise at `include/dpdk_net.h:164` that
  array + mbuf-backed bases remain valid until next poll. **Tied
  hidden-coupling finding at `crates/dpdk-net/src/lib.rs:509, :530`.**

- **[BUG][CODEX] Chained-mbuf RX hits L3 with head-only slice.**
  `dispatch_one_real_mbuf` calls `mbuf_data_slice(m)` then
  `rx_frame(bytes, ...)` (`crates/dpdk-net-core/src/engine.rs:3756, :3790`);
  `mbuf_data_slice` uses only `shim_rte_pktmbuf_data_len(m)` for the
  current segment (`crates/dpdk-net-core/src/lib.rs:74-76`). IPv4
  parser rejects when `total_len > pkt.len()`
  (`crates/dpdk-net-core/src/l3_ip.rs:85-87`); engine maps to
  `ip.rx_drop_short` (`crates/dpdk-net-core/src/engine.rs:3924`). TCP
  chain walker (`crates/dpdk-net-core/src/tcp_input.rs:1128, :1176`)
  runs only after L2/L3/TCP accept the head, so legitimate multi-seg
  frames are dropped before zero-copy preserves the tail. Test gap:
  the existing zero-copy test direct-constructs a `TcpConn` and calls
  `tcp_input::dispatch` over the head link
  (`crates/dpdk-net-core/tests/rx_zero_copy_multi_seg.rs:12, :159`),
  bypassing the failing L2/L3 path.

- **[BUG][BOTH] FFI smoke `Cfg` mirror missing `rx_mempool_size` tail
  field.** Claude: "ffi-test `Cfg` (lines 65-106) ends at
  `ena_miss_txc_to_sec: u8` and has no `rx_mempool_size` field … reads
  the 4 bytes past the `Cfg` end as `rx_mempool_size` — undefined-
  behaviour territory" (`tests/ffi-test/tests/ffi_smoke.rs:65-141`).
  Codex: same finding at `tests/ffi-test/tests/ffi_smoke.rs:63, :106,
  :143`; `dpdk_net_engine_create` reads the field at
  `crates/dpdk-net/src/lib.rs:213`. Production ABI defines it at
  `crates/dpdk-net/src/api.rs:67` / `include/dpdk_net.h:128`. Concrete
  unsafe FFI precondition violation when `DPDK_NET_TEST_TAP=1`.

## STAGE-2 FOLLOWUP (real concern, deferred)

- **[BUG][CODEX] `inject_rx_chain` test-inject path under-reports
  `eth.rx_pkts`.** Normal polling and `inject_rx_frame` both bump
  `eth.rx_pkts` (`crates/dpdk-net-core/src/engine.rs:2609-2610,
  :6352-6362`); `inject_rx_chain` allocates, links, dispatches the head
  (`crates/dpdk-net-core/src/engine.rs:6389, :6492`) without the
  equivalent increment. Test-only path; correctness counter drift, not
  production hot-path.

- **[CLAUDE] A10 `bench-ab-runner` consumes `InternalEvent` directly.**
  `tools/bench-ab-runner/src/workload.rs:44-47, :214` matches on
  `InternalEvent::Connected/Readable/Error/Closed` directly, bypassing
  `dpdk_net_event_t` + `build_event_from_internal`. Bench numbers
  systematically understate FFI translation cost
  (`crates/dpdk-net/src/lib.rs:494-577, :349-491, :522-567`).

- **[CLAUDE] A10.5 `layer-h-correctness` bypasses public ABI.**
  `tools/layer-h-correctness/src/observation.rs:16, :167-170, :337-338,
  :570-586, :662` consumes `InternalEvent` directly via
  `engine.poll_once()` / `engine.drain_events(...)`. Layer-H verdict
  pertains to core-crate API, not C-ABI surface that ships.

- **[SMELL][BOTH] `Engine::events()` leaks
  `RefMut<EventQueue>`.** Claude (Hidden coupling): `pub fn events()`
  at `crates/dpdk-net-core/src/engine.rs:2483` enables the
  bench/layer-H bypasses; should be `pub(crate)` or removed in favor of
  closure-based `drain_events`. Codex (Architectural drift): same
  finding — exposes mutable internal structure rather than a typed
  event-delivery boundary; matters because READABLE references
  per-connection scratch indirectly
  (`crates/dpdk-net-core/src/tcp_events.rs:46`).

- **[CLAUDE] `Engine::pump_tx_drain` and `Engine::pump_timers` are
  top-level `pub` but test-only-effective.**
  `crates/dpdk-net-core/src/engine.rs:7097, :7111` — `pump_tx_drain`
  reads thread-local populated only under `cfg(feature =
  "test-server")`; both serve A7's `pump_until_quiescent` /
  `_raw` test-FFI helpers (`crates/dpdk-net/src/lib.rs:88-111`). Should
  be `pub(crate)` with `cfg(any(test, feature = "test-server"))` gate.

- **[CLAUDE] `#![allow(clippy::missing_safety_doc)]` mutes 13+
  `unsafe extern "C" fn` Safety sections.** `crates/dpdk-net/src/lib.rs:1`
  is broad-stroke; `dpdk_net_engine_destroy` (lib.rs:272),
  `_poll` (:494), `_now_ns` (:590), `_counters` (:595),
  `_flush` (:584), `_engine_create` (:140), `_close` (:1037),
  `_send` (:988), `_shutdown` (:1075), `_timer_add` (:1098),
  `_timer_cancel` (:1121), `_conn_stats` (:739),
  `_conn_rtt_histogram` (:801) carry no `# Safety`. A6/A6.6-7
  newer additions DO. A6.7 should have either filled the gap or
  scoped the allow.

- **[CLAUDE] `EventQueue::with_cap` clamps `VecDeque` capacity at
  `cap.min(4096)` while storing unclamped `cap` in `soft_cap`.**
  `crates/dpdk-net-core/src/tcp_events.rs:147-150`. If user
  configures `event_queue_soft_cap > 4096`, the `VecDeque` grows on
  demand — malloc on the events-emit path, hot in observability-heavy
  workloads. Honor the user cap or document the 4096 ceiling.

- **[SMELL][CODEX] No ABI size/version guard around
  `dpdk_net_engine_config_t`.** `dpdk_net_engine_create` checks only
  null then treats the pointer as the current full struct
  (`crates/dpdk-net/src/lib.rs:141, :148`). Tail-field additions
  depend on every caller and test rebuilding against the latest header
  (`include/dpdk_net.h:78, :139`). Add a size/version field for
  future safety.

- **[CLAUDE] `dpdk_net_tcp_counters_t` lacks `_reserved_for_rust_only_forensics`
  padding marker.** `mbuf_refcnt_drop_unexpected`, `rx_mempool_avail`,
  `tx_data_mempool_avail` are intentionally not mirrored
  (`crates/dpdk-net-core/src/counters.rs:271-277, :300-304`) but live
  in tail-padding. The `_pad` strategy on
  `dpdk_net_eth_counters_t._pad: [u64; 2]` (api.rs:361) and
  `_ip_counters_t._pad: [u64; 4]` (api.rs:378) is not applied here.
  Future named-field addition would silently shadow forensics region.

- **[SMELL][CODEX] `EventQueue` overflow not unit-tested.** Drop-oldest
  with relaxed counter accounting at
  `crates/dpdk-net-core/src/tcp_events.rs:153, :168`; in-module tests
  cover only FIFO + outstanding length (`:208, :237`). No assertion
  that overflow drops exactly one old event, bumps `obs.events_dropped`,
  and latches high-water.

- **[SMELL][CLAUDE] Test-server intercept `Vec::with_capacity` per
  TX frame.** `crates/dpdk-net-core/src/engine.rs:2159, :2252, :2321,
  :2763` — module already has `tx_frame_scratch: RefCell<Vec<u8>>`
  (engine.rs:727); reuse across builds for allocation discipline.

- **[CLAUDE] `dpdk_net_poll` per-event `RefMut<FlowTable>` borrow
  churn.** `crates/dpdk-net/src/lib.rs:509-575` does
  `engine.flow_table().get(*conn)` per event in the burst (single-
  threaded, not unsafe; gives the borrow checker work). Could borrow
  once and reuse across the burst since `drain_events` callback
  already gets `&Engine`.

- **[CLAUDE] `dpdk_net_rx_mempool_size` getter inlines its own
  `&*(p as *const OpaqueEngine)`.** `crates/dpdk-net/src/lib.rs:632-645`
  bypasses the unified `engine_from_raw` helper (lib.rs:52-76). Minor;
  fold into one helper.

## DISPUTED (reviewer disagreement)

*(none — the reviewers' classifications are disjoint rather than
contradictory; Claude's blocker-grade item is Codex's BUG, and vice
versa for the FFI smoke test which both classify as BUG.)*

## AGREED FYI (both reviewers flagged but not blocking)

- **[FYI][BOTH] PR #9 `MbufHandle::Drop` leak fix intact at HEAD.**
  Claude: "every Drop site goes through `rte_pktmbuf_free_seg`
  (post-PR-#9 fix)"
  (`crates/dpdk-net-core/src/mempool.rs:281-285`,
  `docs/superpowers/reports/README.md:90-103`). Codex: pre-reads
  refcount and releases via `shim_rte_pktmbuf_free_seg`
  (`crates/dpdk-net-core/src/mempool.rs:261, :273`); `Engine::drop`
  clears flow-table and queued mbuf owners before mempools are
  dropped (`crates/dpdk-net-core/src/engine.rs:6541, :6556`).

- **[FYI][BOTH] `rx_mempool_size` formula doc drift on the config
  field (2× → 4×).** Claude (Documentation drift / C-ABI):
  `crates/dpdk-net/src/api.rs:67-76` and `include/dpdk_net.h:128-139`
  document `2 * max_connections * ceil(...) + 4096`; runtime is
  `4 * ...` (`crates/dpdk-net-core/src/engine.rs:1191-1196`, commit
  `010b57b` A10 deferred-fix Stage B); getter doc updated
  (`crates/dpdk-net/src/lib.rs:604-624`) but config-field doc not.
  Codex same: getter `crates/dpdk-net/src/lib.rs:603-609` /
  `include/dpdk_net.h:575-581` correct, struct field
  `crates/dpdk-net/src/api.rs:67-70` / `include/dpdk_net.h:128-132`
  stale.

- **[FYI][BOTH] Counter atomics use `Relaxed`; documented as snapshots
  not synchronization.** Claude: "all `Relaxed`. No memory-ordering
  defect found." Codex: `crates/dpdk-net-core/src/counters.rs:3, :5`
  documented; C ABI advises atomic-load helper especially on ARM
  (`crates/dpdk-net/src/api.rs:295, :299`,
  `include/dpdk_net.h:231, :236`).

- **[FYI][BOTH] `MbufHandle` refcount path is sound under single-lcore
  invariant.** Claude: pre/post computation avoids re-read after dec;
  sound on x86_64 and ARM64-Graviton
  (`crates/dpdk-net-core/src/mempool.rs:281-285`). Codex: ownership
  transferred through DPDK refcount primitives, not Rust atomic
  ordering pair (`crates/dpdk-net-core/src/mempool.rs:250, :254, :261, :283`).

- **[FYI][CODEX] `EventQueue` is `VecDeque<InternalEvent>` behind
  engine `RefCell`; not the old SPSC ring.**
  `crates/dpdk-net-core/src/tcp_events.rs:119, :161, :181`,
  `crates/dpdk-net-core/src/engine.rs:708`. Only atomics in the
  queue path are `Relaxed` observability counters
  (`crates/dpdk-net-core/src/tcp_events.rs:170, :177`). *(Claude did
  not assert the contrary; included here as agreed-context.)*

## INDEPENDENT-CLAUDE-ONLY

### HIGH

- **[HIGH][CLAUDE] `tcp_min_rto_ms` is a vestigial C-ABI field.**
  `crates/dpdk-net/src/api.rs:34` declares it; header at
  `include/dpdk_net.h:92` carries `uint32_t tcp_min_rto_ms`; engine
  creator (lib.rs:1186, :1231 fixtures) sets to `0` but
  `dpdk_net_engine_create` (lib.rs:140-269) never reads it — only
  `_us` cousins are plumbed. Comment at api.rs:35-36 says
  `tcp_initial_rto_ms` was removed, but `tcp_min_rto_ms` is the other
  half that should also have gone. cpp-consumer still sets it
  (`examples/cpp-consumer/main.cpp:28`), silently ignored.

- **[HIGH][CLAUDE] `dpdk_net_poll` accepts `_timeout_ns` (underscore-
  prefixed) on the C ABI.** Spec
  (`docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md:212`)
  prescribes `timeout_ns`; impl at `crates/dpdk-net/src/lib.rs:494-577`
  takes `_timeout_ns`; cbindgen passes underscore through to C
  signature (`include/dpdk_net.h:559`). Either implement
  poll-with-timeout or rename + document the parameter is currently
  ignored. (Codex flags the doc-comment side at `include/dpdk_net.h`
  / lib.rs:514-516 separately as a SMELL — see CODEX-ONLY MEDIUM.)

### MEDIUM

- **[MEDIUM][CLAUDE] `dpdk_net_iovec_t` duplicated** between
  `crates/dpdk-net/src/api.rs:147-152` and
  `crates/dpdk-net-core/src/iovec.rs:17-21`; the compile-time
  `size_of`/`align_of` assertion (api.rs:156-160) keeps them
  byte-identical. Sound but worth flagging as a documented exception
  to "no duplication".

- **[MEDIUM][CLAUDE] Doc-comment threshold drift in
  `mbuf_refcnt_drop_unexpected`.** `crates/dpdk-net-core/src/counters.rs:287-289`
  says "no production path holds more than 32 handles to one mbuf
  concurrently"; actual constant is `MBUF_DROP_UNEXPECTED_THRESHOLD = 8`
  (`mempool.rs:26-27`, lowered in `921e7a5`). Doc refers to pre-A10
  threshold.

### LOW

- **[LOW][CLAUDE] 64-byte cacheline hardcoded via
  `#[repr(C, align(64))]`.** Across api.rs:310, :363, :380, :457, :477
  and counters.rs:6, :113, :131, :307, :779. Graviton 2/3 = 64 B; OK
  for current ARM-on-roadmap target. Apple-Silicon (M1 = 128 B) would
  break but is not a target. Flag for future "support Apple-Silicon
  hosts for dev builds".

- **[LOW][CLAUDE] `dpdk_net_close` `flags: u32` ignores undefined
  bits.** `crates/dpdk-net/src/lib.rs:1036-1053`; only
  `DPDK_NET_CLOSE_FORCE_TW_SKIP = 1 << 0`. Matches `shutdown(2)`. Not
  a defect; informational.

- **[LOW][CLAUDE] A6.6-7 no-alloc-on-hot-path audit (Task 20) outside
  `cargo test` matrix.** Script approach sound, gate is implicit; flake
  surfaces only in CI logs.

## INDEPENDENT-CODEX-ONLY

### HIGH

- **[HIGH][CODEX] Stale READABLE scratch failure has no observable
  ABI error path.** If `flow_table().get(conn)` still returns the
  connection, `dpdk_net_poll` emits READABLE with
  `readable_scratch_iovecs.as_ptr().add(seg_idx_start)`
  (`crates/dpdk-net/src/lib.rs:530, :543`) and does not validate that
  current scratch length still covers `seg_count` (`:549, :551`).
  Ties into the BLOCK-A11 READABLE-lifetime BUG.

### MEDIUM

- **[MEDIUM][CODEX] READABLE translation path relies on comments
  rather than enforced owner type.** ABI comment at
  `crates/dpdk-net/src/lib.rs:510, :514` says scratch isn't mutated
  until app consumed event, but actual control flow can run new
  `poll_once` before queued READABLE drains
  (`crates/dpdk-net/src/lib.rs:504`,
  `crates/dpdk-net-core/src/engine.rs:3681`). Mechanical debt because
  `InternalEvent::Readable` carries indices, not owned/copied iovec
  data (`crates/dpdk-net-core/src/tcp_events.rs:46-52`).

- **[MEDIUM][CODEX] `dpdk_net_poll` READABLE comment overpromises.**
  Comment at `crates/dpdk-net/src/lib.rs:514-516` says top-of-next-
  `poll_once` runs after the app has consumed the event, but the
  function can return without draining when `events_out` is null or
  `max_events == 0` after already calling `poll_once`
  (`crates/dpdk-net/src/lib.rs:504-505`).

### LOW

- **[LOW][CODEX] Multi-seg zero-copy test bypasses real RX parser
  path.** Test direct-constructs `TcpConn` and calls
  `tcp_input::dispatch` over head link
  (`crates/dpdk-net-core/tests/rx_zero_copy_multi_seg.rs:12, :159`),
  while production enters via `dispatch_one_real_mbuf -> rx_frame`
  (`crates/dpdk-net-core/src/engine.rs:3756, :3790`). Leaves L2/L3
  total-length rejection path untested. Tied to BLOCK-A11 chained-mbuf
  BUG.

- **[LOW][CODEX] FFI smoke comment claims byte shim must match
  cbindgen layout but doesn't include the generated header.**
  `tests/ffi-test/tests/ffi_smoke.rs:63` vs
  `include/dpdk_net.h:128`; turns the test into a second schema to
  maintain by hand. Tied to BLOCK-A11 ffi-test BUG.

- **[LOW][CODEX] `drain_events` not RefCell-reentrant; defect is
  lifetime/order coupling, not active nested borrow.** Event-queue
  borrow scoped to `pop()` expression
  (`crates/dpdk-net-core/src/engine.rs:3684`) before sink runs
  (`:3687`). Informational rebut to a possible reentrancy concern.

- **[LOW][CODEX] Timer-wheel cancel matches documented tombstone
  model.** `TimerWheel::cancel` at
  `crates/dpdk-net-core/src/tcp_timer_wheel.rs:202, :207`; FFI docs
  say callers must drain queued TIMER events
  (`include/dpdk_net.h:792, :795`,
  `crates/dpdk-net/src/lib.rs:1122, :1133`).

- **[LOW][CODEX] Header mechanically current for `rx_mempool_size`
  field emission.** `include/dpdk_net.h:128, :139, :575, :605`. Drift
  is in formula text + in-tree mirror only.

## Counts

Total: 33; BLOCK-A11: 3; STAGE-2: 13; DISPUTED: 0; AGREED-FYI: 5;
CLAUDE-ONLY: 7; CODEX-ONLY: 8

## Verification trace

Sources cross-read: the two input files plus referenced line-anchors
(no fresh code reads). Carry-forward: PR-#9 closure status confirmed by
both reviewers; no new PR-#9 regressions. Reviewer methodology:
Claude used commit-range walks + grep for hot-path allocs / atomics /
TODOs / `MbufHandle` Drop sites; Codex did static review only with
`rg`/`wc` over the public-ABI / generated-header / engine RX-dispatch
ranges. Each retained finding cites both source documents where
overlap exists.
