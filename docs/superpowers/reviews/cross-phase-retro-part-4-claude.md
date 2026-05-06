# Part 4 Cross-Phase Retro Review (Claude)
**Reviewer:** general-purpose subagent (opus 4.7) — covering for superpowers:code-reviewer
**Reviewed at:** 2026-05-05
**Part:** 4 — Public API + zero-copy + FFI safety
**Phases:** A6, A6.5, A6.6, A6.7

## Verdict

The Part-4 surface is materially in good shape — every Stage-1 ABI symbol is
emitted, every counter group has a compile-time `size_of`/`align_of` mirror
assertion, the `MbufHandle::Drop` path is the correct
`rte_pktmbuf_free_seg` (post-PR-#9 fix), and every safety-critical
hot-path allocation has been retired. The main architectural drift
sits in the **C-ABI documentation/spec layer**: the `rx_mempool_size`
formula doc-comment in `crates/dpdk-net/src/api.rs` (and therefore the
generated `include/dpdk_net.h`) still claims `2 * max_connections * …`
even though A10 deferred-fix raised the implementation to `4 * …`,
`tcp_min_rto_ms` is a vestigial ABI field never read by the engine,
and the standalone `tests/ffi-test/` integration test maintains a
hand-rolled `Cfg` mirror that has fallen out of sync with
`dpdk_net_engine_config_t` (missing the A6.6-7 `rx_mempool_size`
field, so passing it to `dpdk_net_engine_create` is unsound when
`DPDK_NET_TEST_TAP=1`). The biggest cross-phase invariant violation
is that A10's `bench-ab-runner` and A10.5's `layer-h-correctness`
crates consume `dpdk_net_core::tcp_events::InternalEvent` directly
instead of going through `dpdk_net_event_t` + `dpdk_net_poll`, so the
performance / correctness numbers attributed to "the public stack"
are systematically biased low on FFI translation cost.

## Architectural drift

- **`tcp_min_rto_ms` is a vestigial C-ABI field.**
  `crates/dpdk-net/src/api.rs:34` declares
  `pub tcp_min_rto_ms: u32` and the generated header
  (`include/dpdk_net.h:92`) carries `uint32_t tcp_min_rto_ms`. The
  engine creator (`crates/dpdk-net/src/lib.rs:1186, :1231` in test
  fixtures) sets it to `0` but `dpdk_net_engine_create` (lib.rs:140-269)
  never reads the field — only the `_us` cousins are plumbed. The
  comment at api.rs:35-36 even says "A5 Task 21: RTO config in µs.
  `tcp_initial_rto_ms` was removed". Both `_ms` variants should have
  gone; only `tcp_initial_rto_ms` did. cpp-consumer still sets
  `cfg.tcp_min_rto_ms = 20` (`examples/cpp-consumer/main.cpp:28`),
  which is silently ignored.

- **`dpdk_net_poll` accepts `_timeout_ns` (with leading underscore) on
  the C ABI.** Original spec
  (`docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md:212`) prescribes
  `timeout_ns`. Implementation in `crates/dpdk-net/src/lib.rs:494-577`
  takes `_timeout_ns` (underscore-prefixed Rust idiom for "intentionally
  unused"); cbindgen passes the underscore through to the C signature
  (`include/dpdk_net.h:559`). This makes the C-side parameter look
  internal-private and is mildly user-hostile. Either implement the
  poll-with-timeout semantics the spec promised, or rename the
  parameter and document explicitly that the value is currently
  ignored.

- **`Engine::pump_tx_drain` (`crates/dpdk-net-core/src/engine.rs:7097`)
  is `pub` but functionally inert under default features.** It calls
  `crate::test_tx_intercept::is_empty()`, whose backing thread-local
  is only populated by `push_tx_frame` calls that live inside
  `#[cfg(feature = "test-server")]` blocks. In production builds the
  function is guaranteed to return `false`. The function lacks a
  `cfg(feature = "test-server")` gate, so it pollutes the public
  `Engine` API and is reachable from any `dpdk_net_core` consumer.

## Cross-phase invariant violations

- **A10's `bench-ab-runner` consumes `InternalEvent` directly,
  bypassing `dpdk_net_event_t` + `build_event_from_internal`.**
  `tools/bench-ab-runner/src/workload.rs:44-47` imports
  `dpdk_net_core::engine::Engine`, `flow_table::ConnHandle`, and
  `tcp_events::InternalEvent`; `workload.rs:214` and similar match
  on `InternalEvent::Connected` / `Readable` / `Error` / `Closed`
  directly. This means the bench numbers do **not** include the
  per-event translation cost
  (`crates/dpdk-net/src/lib.rs:494-577 `dpdk_net_poll` →
  `lib.rs:349-491 build_event_from_internal` →
  `lib.rs:522-567` per-conn `flow_table.get` lookup for the readable
  scatter-gather payload). For a Stage-1 stack whose primary
  consumption mode is FFI from C++, this systematically understates
  end-user latency. A6's careful payload-translation contract is
  not what's being measured.

- **A10.5's `layer-h-correctness` crate also bypasses the public
  ABI.** `tools/layer-h-correctness/src/observation.rs:16, :167-170,
  :337-338, :570-586, :662` consume `InternalEvent` directly and
  call `engine.poll_once()` / `engine.drain_events(...)` via the
  Rust-direct API. The "Layer H" correctness verdict therefore
  pertains to the core-crate API, not the C-ABI surface that ships
  to consumers. The `dpdk_net_core::engine::Engine::events()`
  accessor at `crates/dpdk-net-core/src/engine.rs:2483` is `pub`
  and returns `RefMut<EventQueue>`, which makes this bypass
  trivially available.

- **`Engine::pump_tx_drain` and `Engine::pump_timers`
  (`engine.rs:7097, :7111`) are exposed at top-level `pub`** even
  though they exist purely to serve A7's `pump_until_quiescent` /
  `pump_until_quiescent_raw` test-FFI helpers
  (`crates/dpdk-net/src/lib.rs:88-111`, both `cfg(feature =
  "test-server")`). These should at minimum be `pub(crate)` with a
  `cfg(any(test, feature = "test-server"))` gate, otherwise external
  Rust consumers can drive engine quiescence on every poll without the
  intended pump discipline.

## Tech debt accumulated

- **`#![allow(non_camel_case_types, non_snake_case,
  clippy::missing_safety_doc)]` at `crates/dpdk-net/src/lib.rs:1`.**
  The `clippy::missing_safety_doc` allow is broad-stroke and means
  many `unsafe extern "C" fn` declarations that should carry a
  `# Safety` rustdoc don't —
  `dpdk_net_engine_destroy` (lib.rs:272), `dpdk_net_poll` (lib.rs:494),
  `dpdk_net_now_ns` (lib.rs:590), `dpdk_net_counters` (lib.rs:595),
  `dpdk_net_flush` (lib.rs:584), `dpdk_net_engine_create` (lib.rs:140),
  `dpdk_net_close` (lib.rs:1037), `dpdk_net_send` (lib.rs:988),
  `dpdk_net_shutdown` (lib.rs:1075), `dpdk_net_timer_add` (lib.rs:1098),
  `dpdk_net_timer_cancel` (lib.rs:1121), `dpdk_net_conn_stats`
  (lib.rs:739), `dpdk_net_conn_rtt_histogram` (lib.rs:801) — none of
  these have `# Safety` sections. Several A6/A6.6-7 additions
  (`dpdk_net_engine_add_local_ip`, `dpdk_net_recommended_ena_devargs`,
  `dpdk_net_rx_mempool_size`, `dpdk_net_resolve_gateway_mac`) DO have
  `# Safety` sections, so the audit-of-record left a partial trail.
  A6.7 should have either added the missing docs or scoped the allow
  more narrowly.

- **Unrelated tail-test allocations are not wrapped in scratch-reuse**
  even though the bench-alloc-audit framework exists.
  `crates/dpdk-net-core/src/engine.rs:2159, :2252, :2321, :2763` all
  call `Vec::with_capacity(...)` per intercepted TX frame inside
  `#[cfg(feature = "test-server")]` blocks. Not a hot-path defect
  (test-only path), but they all allocate per call — and the same
  module already pre-allocates `tx_frame_scratch: RefCell<Vec<u8>>`
  (engine.rs:727) for the production path. Reusing that scratch from
  the test-server intercept paths would unify the allocation discipline
  across the two builds.

- **PR #9 deferred-fix items are tracked closed** in
  `docs/superpowers/reports/README.md:90-103` (rx_mempool_avail
  + mbuf_refcnt_drop_unexpected counters, `MbufHandle::Drop`
  fixed to `rte_pktmbuf_free_seg`, `rx_mempool_size` per-conn term
  raised 2× → 4×, regression tests landed). The closing tag
  `phase-a10-deferred-fixed` is in place. **No outstanding tech debt
  on PR #9 specifically.**

## Test-pyramid concerns

- **`tests/ffi-test/tests/ffi_smoke.rs:65-141` maintains a hand-rolled
  `Cfg` mirror that is stale.** The defining `dpdk_net_engine_config_t`
  (`crates/dpdk-net/src/api.rs:19-77`) added
  `rx_mempool_size: u32` at the tail in A6.6-7 Task 10, but the
  ffi-test `Cfg` (lines 65-106) ends at `ena_miss_txc_to_sec: u8` and
  has no `rx_mempool_size` field. When `ffi_eal_init_and_engine_lifecycle`
  runs (gated by `DPDK_NET_TEST_TAP=1`), it builds `cfg: Cfg` on the
  stack and casts to `*const dpdk_net_engine_config_t`, so
  `dpdk_net_engine_create` reads the 4 bytes past the `Cfg` end as
  `rx_mempool_size` — undefined-behaviour territory. Worse, the test's
  build comment claims it tests the "exact same path as a C consumer"
  while in fact it does NOT include `dpdk_net.h` (cf.
  `examples/cpp-consumer/main.cpp:1` which DOES). The cpp-consumer
  C++ build is the real C-ABI exercise; the Rust ffi-test crate is
  rubber-stamping a stale shim and adds little value.

- **A6.7 panic-firewall test
  (`crates/dpdk-net/tests/panic_firewall.rs`) is good** — spawns a
  child process, calls the test-only FFI panic entry, asserts SIGABRT.
  Real exercise.

- **A6.6-7 "no-alloc-on-hot-path" audit (Task 20)** appears to live
  outside of this scope's file set. The script approach is sound but
  the gate is implicit; a flake here only surfaces in CI logs, not via
  the standard `cargo test` matrix.

## Observability gaps

- **No counter for `InternalEvent::Writable` emit.** Spec
  (`docs/superpowers/specs/2026-04-19-stage1-phase-a6-public-api-completeness-design.md:433`)
  classifies WRITABLE / TIMER as observability-only events without a
  counter pair, so this is intentional and matches the design. **Not a
  defect.**

- **`mbuf_refcnt_drop_unexpected`, `rx_mempool_avail`, and
  `tx_data_mempool_avail` are intentionally NOT mirrored on the C
  ABI.** Documented at `crates/dpdk-net-core/src/counters.rs:271-277`
  and `:300-304` ("Forensic-only … not mirrored on the C ABI side").
  These three counters fit inside the `dpdk_net_tcp_counters_t`
  cacheline tail-padding so the existing `size_of` mirror assertion at
  `crates/dpdk-net/src/api.rs:511, :518` still passes. **Subtle
  C-ABI footgun:** if a future Stage-1 phase adds a named
  `dpdk_net_tcp_counters_t` field, it would shadow the unmapped
  region the core uses for these forensics, and the size assertion
  would fail. The `_pad` strategy used on
  `dpdk_net_eth_counters_t._pad: [u64; 2]` (api.rs:361) /
  `dpdk_net_ip_counters_t._pad: [u64; 4]` (api.rs:378) is not applied
  to `dpdk_net_tcp_counters_t`, which is precisely where the
  Rust-only forensics counters live. Should be made explicit with a
  `dpdk_net_tcp_counters_t._reserved_for_rust_only_forensics: [u64; 3]`
  marker so the C-ABI side documents the layout reservation.

## Memory-ordering / ARM-portability concerns

- **All atomic ops on counters use `Ordering::Relaxed`** — correct
  for slow-path stats counters (no synchronization role) and matches
  the C-ABI atomic-load helper contract documented at
  `include/dpdk_net_counters_load.h`. No memory-ordering defect found.

- **`MbufHandle::Drop` reads-then-decs without a fence**
  (`crates/dpdk-net-core/src/mempool.rs:281-285`). Documented
  invariant: the engine serializes mbuf operations on one lcore (no
  other thread mutates the refcount concurrently). The pre/post
  computation explicitly avoids re-reading after the dec so a
  recycled-slot UAF is avoided. **Sound on x86_64 and ARM64-Graviton
  given the single-thread invariant.** No defect.

- **Cache-line size = 64 baked in via `#[repr(C, align(64))]`**
  (`api.rs:310, :363, :380, :457, :477; counters.rs:6, :113, :131,
  :307, :779`). Graviton 2/3 cachelines are 64 B, so this works for the
  ARM-on-roadmap target, but Apple Silicon (M1 = 128 B cachelines) is
  not currently a target and would break the rationale at counters.rs:345
  (`assert!(align_of::<EthCounters>() == 64);`). Acceptable for the
  current roadmap. Worth flagging for a future "support Apple-Silicon
  hosts for dev builds" line item if it ever lands.

- **`EventQueue::with_cap` clamps the underlying `VecDeque` capacity
  at `cap.min(DEFAULT_SOFT_CAP) == 4096`** while storing the unclamped
  `cap` in `soft_cap` (`crates/dpdk-net-core/src/tcp_events.rs:147-150`).
  If a caller configures `event_queue_soft_cap > 4096`, the `VecDeque`
  will grow on demand — that's a malloc on the events-emit path, which
  is hot in observability-heavy workloads. Either honor the user-supplied
  cap at construction (matching the soft_cap field) or document the
  4096 ceiling.

## C-ABI / FFI

- **`dpdk_net_engine_config_t.rx_mempool_size` doc-comment is stale
  on the public API surface.** `crates/dpdk-net/src/api.rs:67-76` and
  the generated `include/dpdk_net.h:128-139` document the formula as
  `2 * max_connections * ceil(recv_buffer_bytes / mbuf_data_room) +
  4096`. The actual implementation
  (`crates/dpdk-net-core/src/engine.rs:1191-1196`) uses
  `4u32.saturating_mul(cfg.max_connections)…` — bumped 2 → 4 in
  commit `010b57b` (A10 deferred-fix Stage B "Defense in depth").
  The lib.rs FFI getter doc (`crates/dpdk-net/src/lib.rs:604-624`)
  was updated to the correct formula but never propagated back to the
  C-ABI struct doc-comment. Result: cbindgen generates a header that
  lies to C consumers about RX mempool sizing — important because
  applications size hugepage allocations based on this estimate.

- **21 `pub extern "C" fn` symbols in `crates/dpdk-net/src/lib.rs`,
  21 emitted prototypes in `include/dpdk_net.h`** (verified by parsing
  both). No symbol drift between Rust source and the cbindgen output.
  No C++ name-mangling issues; the `cpp_compat = true` cbindgen
  config and the wrapping `extern "C" {}` block at header line 506
  hold up.

- **`dpdk_net_iovec_t` is duplicated** between
  `crates/dpdk-net/src/api.rs:147-152` (FFI side) and
  `crates/dpdk-net-core/src/iovec.rs:17-21` (core side) per the
  documented constraint: cbindgen `parse_deps = false` precludes
  re-exporting a `pub type` alias. The compile-time
  `size_of`/`align_of` assertion in api.rs:156-160 keeps the two
  byte-identical. Sound, but worth flagging as a documented exception
  to the "no duplication" rule.

- **`dpdk_net_close` accepts `flags: u32`**
  (`crates/dpdk-net/src/lib.rs:1036-1053`) and the only defined flag is
  `DPDK_NET_CLOSE_FORCE_TW_SKIP = 1 << 0`. Undefined bits are silently
  ignored. RFC-compatibility-wise this matches `shutdown(2)` behavior.
  No defect.

## Hidden coupling

- **`pub fn events() -> RefMut<EventQueue>`
  (`crates/dpdk-net-core/src/engine.rs:2483`) leaks the internal
  `VecDeque<InternalEvent>` to any Rust-direct consumer.** That's
  what enables the bench-ab-runner / layer-h-correctness bypass
  documented in "Cross-phase invariant violations". The accessor was
  added in A5.5 era and never narrowed. The right shape for a
  "give me events" API would be the closure-based `drain_events(max,
  sink)` already on the engine — `pub fn events()` should be
  `pub(crate)` or removed.

- **`crates/dpdk-net/src/lib.rs:509-575 `dpdk_net_poll`** dereferences
  `c.readable_scratch_iovecs` per Readable event via
  `engine.flow_table().get(*conn)`. The call goes through
  `RefMut<FlowTable>` (`engine.rs:2436`), which means each event in a
  burst takes a `flow_table.borrow_mut()` and immediately drops it.
  Not unsafe (single-threaded), but gives the borrow checker work and
  briefly takes a `RefMut` on a hot-path sink callback. The drain loop
  at `lib.rs:509` could borrow once and reuse the `RefMut` across all
  events in the burst (`drain_events` callback already gets `&Engine`).

- **`OpaqueEngine` newtype + raw-pointer transmutes**
  (`crates/dpdk-net/src/lib.rs:52-76`) are correct but the
  `dpdk_net_rx_mempool_size` getter at lib.rs:632-645 inlines its own
  `&*(p as *const OpaqueEngine)` instead of going through
  `engine_from_raw`. Two paths to the same effect; minor inconsistency
  worth folding into a unified helper.

## Documentation drift

- **`crates/dpdk-net/src/api.rs:35-36` claims "tcp_initial_rto_ms was
  removed"** but `tcp_min_rto_ms` is still in the struct
  (api.rs:34, header line 92, lib.rs:1186 / :1231). The comment
  documents only half the migration.

- **`crates/dpdk-net/src/api.rs:67-76 / include/dpdk_net.h:128-139
  `rx_mempool_size` formula doc** is stale (2× per-conn — see C-ABI
  / FFI section).

- **`crates/dpdk-net-core/src/counters.rs:287-289` says
  "no production path holds more than 32 handles to one mbuf
  concurrently"** but the actual threshold is `MBUF_DROP_UNEXPECTED_THRESHOLD = 8`
  (`mempool.rs:26-27`, lowered in commit `921e7a5` "lower
  MBUF_DROP_UNEXPECTED_THRESHOLD 32 -> 8"). The counter doc-comment
  refers to the pre-A10 threshold, the constant has moved.

- **`crates/dpdk-net/src/test_ffi.rs:10-13` says "every entry except
  `set_time_ns` and `accept_next` runs `pump_until_quiescent`"**.
  Verified at the call sites — the discipline is upheld. No drift.

- **`crates/dpdk-net/src/lib.rs:494-499 `dpdk_net_poll`** declares the
  `_timeout_ns` arg but the rustdoc on the function is empty. Spec
  `2026-04-17-dpdk-tcp-design.md:212` declared `timeout_ns` as the
  arg name; the rename to `_timeout_ns` (Rust idiom) inadvertently
  documented the C-side promise away.

## FYI / informational

- **`#[no_mangle] pub unsafe extern "C" fn` consistency.** Every
  Stage-1 ABI symbol (verified across `crates/dpdk-net/src/lib.rs`)
  uses the same `#[no_mangle] pub unsafe extern "C" fn` shape. cbindgen
  emits the right C declaration. No defect.

- **`drain_events` callback signature**
  (`crates/dpdk-net-core/src/engine.rs:3681`) takes
  `FnMut(&InternalEvent, &Engine)`. The `&Engine` second parameter is
  the seam that lets `dpdk_net_poll` resolve the per-conn
  `readable_scratch_iovecs` for the Readable variant
  (`crates/dpdk-net/src/lib.rs:509-575`). Clean design.

- **A6 + A6.6-7 implementation plans** (`docs/superpowers/specs/...`)
  are detailed and traced via per-task commit subjects. Walking back
  through `git log --oneline phase-a5-5-complete..phase-a6-complete`
  and the corresponding A6.5 / A6.6-7 ranges, every task has its own
  commit + cbindgen artifact + test. **Process discipline is
  exemplary.**

- **`dpdk_net_test.h`** (the test-server-feature-gated FFI surface)
  has its own integration test at
  `crates/dpdk-net/tests/test_header_excluded.rs` proving no
  test-only symbol leaks into the production header. Solid guard.

- **`#[cfg(feature = "obs-none")]` paths** in lib.rs and engine.rs
  short-circuit slow-path counters and `EventQueue::push`. Clean
  conditional-compile usage; no stale gates noticed.

## Verification trace

Commands run during this review:

- `git tag | grep "phase-a[5678]"` — phases A5..A8 tags present;
  A6, A6.5, A6.6-7 ranges populated.
- `git log --oneline phase-a5-5-complete..phase-a6-complete` — 23
  A6 task commits + brainstorm + reviews. Drift from tag → HEAD checked
  via inspection at HEAD (no untagged regressions of A6 surfaces).
- `git log --oneline phase-a6-complete..phase-a6-5-complete` — 13
  A6.5 task commits + spec + plan; bench-alloc-audit added.
- `git log --oneline phase-a6-5-complete..phase-a6-6-7-complete` —
  22 A6.6-7 fused-phase commits + spec + plan.
- `git log --oneline phase-a6-6-7-complete..HEAD` — A7 / A8 / A8.5 /
  A9 / A10 / A10.5 / phase-a-hw / phase-a-hw-plus follow-on work;
  multiple `MbufHandle` / `rx_mempool_size` related commits scanned
  for drift.
- File reads:
  - `crates/dpdk-net/src/lib.rs` (1863 lines) — 21 extern "C" symbols.
  - `crates/dpdk-net/src/api.rs` (542 lines) — 5 layout assertions.
  - `crates/dpdk-net/src/test_ffi.rs` (lines 1-330) — verify pump
    discipline.
  - `crates/dpdk-net/src/test_only.rs` — panic-test entry.
  - `crates/dpdk-net/cbindgen.toml` — production header config;
    test exclusion list verified.
  - `crates/dpdk-net/tests/{api_shutdown.rs, panic_firewall.rs,
    test_header_excluded.rs}` — integration tests.
  - `tests/ffi-test/tests/ffi_smoke.rs` — hand-rolled `Cfg` shim found
    stale (missing `rx_mempool_size`).
  - `examples/cpp-consumer/main.cpp` (header lines) — uses
    `#include "dpdk_net.h"` directly; sets the vestigial
    `tcp_min_rto_ms`.
  - `crates/dpdk-net-core/src/{counters.rs, engine.rs, mempool.rs,
    iovec.rs, tcp_events.rs}` — alloc, atomic, refcount sites.
  - `include/dpdk_net.h` (805 lines) — public C ABI; symbol parity
    verified.
  - `include/dpdk_net_counters_load.h` — atomic-load helper.
  - `docs/superpowers/specs/2026-04-29-a10-deferred-fixes-design.md`
    — PR #9 deferred-fix scope.
  - `docs/superpowers/reports/README.md:90-103` — closing-commit
    table for A10 deferred-fixes (closed).
- Searches:
  - `grep "Vec::new\|Vec::with_capacity\|Box::new" engine.rs` — every
    surviving allocation in scope is at engine-construct or inside
    `#[cfg(feature = "test-server")]`. No hot-path Vec/Box.
  - `grep "TODO\|FIXME\|XXX\|unimplemented!\|unreachable!"` in
    `crates/dpdk-net/src/`, `crates/dpdk-net-core/src/{iovec,counters}.rs`
    — zero hits. Clean.
  - `grep "MbufHandle\|try_clone\|drop_segment_mbuf_ref\|rte_pktmbuf_free"`
    in `engine.rs, tcp_reassembly.rs, tcp_conn.rs, mempool.rs` —
    every Drop site goes through `rte_pktmbuf_free_seg` (post-PR-#9 fix).
  - `grep "use dpdk_net_core" tools/ tests/` — flagged
    `bench-ab-runner`, `bench-stress`, `bench-vs-linux`, `tcpreq-runner`,
    `packetdrill-shim-runner`, `layer-h-correctness` consume core APIs
    directly. The first and last are the cross-phase invariant
    violations called out above.
  - `grep "Ordering::"` — all `Relaxed`. No memory-ordering issue.
  - `grep "align(64)\|align_of"` — 64-byte cachelines hardcoded
    (Graviton-compatible).
- Cross-checked already-shipped reviews to avoid duplication:
  `phase-a6-{mtcp-compare,rfc-compliance}.md`,
  `phase-a6-5-{mtcp-compare,rfc-compliance}.md`,
  `phase-a6-6-7-{mtcp-compare,rfc-compliance}.md`,
  `cross-phase-retro-part-{1,2,3}-{claude,codex,synthesis}.md`. None
  of the findings above appear in those documents.
