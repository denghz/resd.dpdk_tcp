# Part 1 Cross-Phase Retro Review (Claude)

**Reviewer:** general-purpose subagent (opus 4.7) — covering for superpowers:code-reviewer
**Reviewed at:** 2026-05-05
**Part:** 1 — Crate skeleton, EAL bring-up, L2/L3 (PMD wrapper, ARP, ICMP)
**Phases:** A1, A2

## Verdict

**NEEDS-FIX**

Two ABI-stability defects (silently-ignored public `dpdk_net_engine_config_t` fields) plus one panic-across-FFI hazard would each block sign-off on their own. Several cross-phase couplings have accreted around the A1/A2 surface without doc updates. Test pyramid for the A1/A2 surface itself is healthy; the issues live above the L2/L3 modules in the engine + public ABI shim.

## Architectural drift

- **Spec scope description for Part 1 is wrong / drifted.** The cross-phase brief lists `crates/dpdk-net-core/src/{eal,l2_eth,l3_ip,arp,icmp,flow_table}.rs`. The actual crate has neither `eal.rs` nor `l2_eth.rs` — `eal_init` lives at `crates/dpdk-net-core/src/engine.rs:932` (a free function plus a `static EAL_INIT: Mutex<bool>` at `:930`), and the L2 module is `l2.rs` (no `_eth` suffix). The brief reflects an earlier decomposition that never landed; the spec / plan / brief should be rewritten to match. Either move `eal_init` to its own module to match the brief or update the brief; landing-as-is hides the EAL bring-up logic inside the 8141-line `engine.rs`.

- **`maybe_emit_gratuitous_arp` is *only* called inside `Engine::poll_once` — never via `tcp_timer_wheel`** despite spec §8 promising "gratuitous-ARP refresh timer every N seconds" and the A2 plan promising "switching to it [the real timer wheel] is a ~3-line change in `poll_once`." A6 shipped the real timer wheel (file `tcp_timer_wheel.rs` exists; `TimerKind::ApiPublic`, etc.) yet A2's naïve `last_garp_ns` poll-loop check was never migrated. Sites: `engine.rs:6238-6255` (the helper), `engine.rs:2591` and `engine.rs:2659` (called twice from `poll_once` — once on idle and once on busy paths). Functionally correct but the documented post-A2 cleanup never happened. (Phase-a2 RFC review's `AD-3` records this as expected at A2 sign-off; the cross-phase concern is that A6 didn't close it.)

- **Gateway-MAC discovery silently grew an ARP-REQUEST probe path that the A2 plan and spec §8 do not mention.** `Engine::maybe_probe_gateway_mac` (`engine.rs:6512-6538`) emits a unicast ARP REQUEST every 1s when `gateway_mac == [0;6]`. A2's accepted-divergence record (mTCP review `AD-2`, RFC review `AD-2`) says "gateway MAC is resolved once at `Engine::new` via `/proc/net/arp` and never refreshed" — the prose is now stale. The probe runs from both the idle and busy poll paths (`engine.rs:2592`, `engine.rs:2660`). Spec §8 should be updated to note "or actively ARP-probed when the configured `gateway_mac` is zero." Otherwise the spec→code contract no longer holds, and a future reviewer who treats spec §8 as ground truth will mistake the probe for a regression.

- **`our_mac` is exposed as `pub fn our_mac() -> [u8;6]` (`engine.rs:2049`) but is the only Engine accessor returning copy-out internal L2 identity.** It's fine, but the symmetric `gateway_mac()` and `gateway_ip()` accessors return through different mechanisms (`Cell` vs `cfg` field, `engine.rs:2090-2102`) — three siblings, three patterns. Stage 1's audit gate (knob-coverage) only checks `EngineConfig` field drift, not Engine accessors. Accumulates on every phase that adds an Engine accessor.

## Cross-phase invariant violations

- **EAL init can panic across the C ABI boundary** — `engine::eal_init` does `CString::new(*s).unwrap()` (`engine.rs:959`) and `EAL_INIT.lock().unwrap()` (`engine.rs:933`). The first panics if any caller-passed argv string contains an interior NUL; the second panics on poison. Spec §3 says "the Rust implementation must not panic across the `extern "C"` boundary" and the project relies on `panic = "abort"` (release) to convert library panics to process abort, but the call chain is: C caller → `dpdk_net_eal_init` (`crates/dpdk-net/src/lib.rs:118`) → `engine::eal_init`. A test/dev build with `panic = "unwind"` UB-unwinds across the FFI boundary. The paired comment at `engine.rs:2632-2640` explicitly notes this hazard for the RX dispatch path and silently skips null mbufs to avoid it; the EAL init path was never given the same treatment. Convert both unwraps to error returns.

- **`clock.rs` hard-`compile_error!`s on non-x86_64** (`clock.rs:39`) — but `feedback_arm_roadmap.md` says "ARM on roadmap; don't bake x86_64-only atomic/layout/memory-ordering assumptions into ABI or FFI." `rdtsc` is naturally x86-only, but the way it's wired (`compile_error!` at the inline-fn site rather than a target-gated module pair providing both `rdtsc()` and an `aarch64`-equivalent timer read) means the entire crate fails to compile on aarch64, blocking every other module from building. A6.7 added `tools/bench-micro` reachable via the `bench-internals` feature on the harness side; that surface is now also locked to x86_64 because of this single file. Either gate the crate's `Cargo.toml` `[target.'cfg(target_arch = "x86_64")']` or implement the aarch64 path (`cntvct_el0` read) — the former is honest about scope, the latter unblocks Graviton work.

- **A2's `EthCounters` field name `rx_drop_miss_mac` (`counters.rs:10`) was preserved verbatim** even though A6.7's audit policy doesn't enforce A1-era field-name spellings. Public ABI mirror at `dpdk_net_eth_counters_t` (`api.rs:314`) carries the same name. This is fine today; flagged because the per-phase reviews' spec text says "MissMac" while the counter says "miss_mac" — two casing conventions for one concept that survived A1→A2→A6→A8.5.

## Tech debt accumulated

- **`crates/dpdk-net-core/src/clock.rs:79` — TODO(spec §7.5)** — spec mandates `CLOCK_MONOTONIC_RAW`; `Instant::now()` uses `CLOCK_MONOTONIC`. Inline rationale (worst-case 25 µs over a 50 ms window vs 2% test tolerance) is sound. **Recommendation: defer** (it's documented, bounded, and the bench-baseline numbers already look fine; flagged here only because no later phase has revisited it).

- **`crates/dpdk-net-core/src/flow_table.rs:172` — TODO (Stage 2)** — bucket-hash forward-compat for the flat-bucket table. The function `lookup_by_hash(.., bucket_hash)` accepts the hash and immediately discards it (`let _ = bucket_hash`). The plumb-but-discard pattern is intentional (per spec §6.5 / A-HW) so RX path callers are forward-compatible. The `#[allow(unused_variables)]` on the param at `:170` is the right gate; **recommendation: keep as-is, but add an inline test asserting the function returns identical results for two different `bucket_hash` values on the same tuple** — currently the unused-variable contract is enforceable only by code inspection.

- **`Engine::tx_data_frame` is `#[allow(dead_code)]`** at `engine.rs:2287`. A5 task 10 inlined the alloc+append+refcnt+tx_burst sequence into `send_bytes` to capture the mbuf for `snd_retrans`, leaving `tx_data_frame` as a vestigial helper "for future data-frame control paths that don't need in-flight tracking." No call site has materialised in 5+ phases. **Recommendation: delete.** The helper is non-trivial (60+ lines), maintenance liability, and the rationale text would still apply if the use case materialises later — re-deriving from `tx_frame` / `send_bytes` would take an hour.

- **`PortConfigOutcome::applied_rx_offloads` and `applied_tx_offloads` carry `#[allow(dead_code)]`** (`engine.rs:1112-1117`). The fields are computed but never read off the struct — every consumer reads via the `*_offload_active` boolean latches alongside. The dead-code allow is silently shipping a "we computed this for log clarity" intent that no log emit actually consumes (the bring-up banner reads `applied_rx_offloads` / `applied_tx_offloads` via the local variables before the struct construction; the struct copy is unused). **Recommendation: drop the two fields and the allow.**

- **`rx_drop_nomem_prev` accessor `pub(crate) fn rx_drop_nomem_prev(&self) -> u64` is `#[allow(dead_code)]`** (`engine.rs:2685-2688`). Comment says "lifts once Task 21's driver harness exercises the accessor directly." T21 shipped (per recent commits 1563932 / 72a2214). **Recommendation: verify whether T21 ended up using this accessor; if not, drop or wire it.**

- **Two stale `#[allow(unused_variables)]` on `ip_decode_offload_aware`** parameters `ol_flags`, `rx_cksum_offload_active`, `counters` at `l3_ip.rs:201-203`. Inside the `#[cfg(feature = "hw-offload-rx-cksum")]` arm at `:206` the variables are consumed; inside the `#[cfg(not(...))]` arm at `:225-228` they are explicitly bound to `_` to silence warnings. The function signature has top-level `#[allow]` annotations even though the body has the explicit `_` bindings — belt-and-suspenders that obscures whether the params are actually used. **Recommendation: remove the function-signature allows; the explicit `let _ = ` bindings are sufficient.**

## Test-pyramid concerns

- **`l2_l3_tap.rs` integration test (`crates/dpdk-net-core/tests/l2_l3_tap.rs`) is gated behind `DPDK_NET_TEST_TAP=1`** and never runs in default CI. The unit tests at `l2.rs:57-118`, `l3_ip.rs:233-438`, `icmp.rs:95-203`, `arp.rs:362-756` cover decode-path and builder-path identity but **none of them exercise the `Engine::handle_arp` / `Engine::handle_ipv4` / `Engine::rx_frame` dispatcher wiring**. The A2 mTCP review correctly observed wiring is integration-test-only. Today the only end-to-end test is the env-gated TAP harness; if a future refactor breaks the dispatcher (say, swapping the `match l2.ethertype` order or the counter-bump site), nothing fails until someone explicitly runs `DPDK_NET_TEST_TAP=1 cargo test`. **Recommendation: add a unit test using `#[cfg(feature = "test-inject")]` at engine level (the inject path already exists) that injects a single ARP REQUEST and asserts `eth.rx_arp` / `eth.tx_arp` deltas.** The inject path was added in A9 — A2's wiring tests pre-date it and nobody backfilled.

- **`build_arp_reply` / `build_gratuitous_arp` / `build_arp_request` have explicit Ethernet-min pad regression tests** (`arp.rs:418-442`, `arp.rs:720-748`) — good. But none of them cross-check that the **engine's `tx_frame` consumer accepts the buffer length they produce**. `tx_frame` rejects `bytes.len() > u16::MAX` (`engine.rs:2124`) and the mempool data room is 256 bytes (`engine.rs:1213`), so `ARP_FRAME_LEN = 60` is safe. But there's no compile-time or runtime cross-check; a future ARP variant that emits 4096 bytes would silently fail through `tx_drop_nomem`. **Recommendation: add a const_assert in arp.rs that `ARP_FRAME_LEN <= 256`.**

- **Counter-coverage matrix has a hole around `ip.rx_drop_unsupported_proto`.** The `l3_ip.rs:121-123` decoder rejects every protocol other than TCP / ICMP. The crafted-frame TAP test sends UDP at line ~250 and asserts the counter delta. If `DPDK_NET_TEST_TAP=1` isn't set in CI, the only coverage of this counter is the zero-init test (`counters.rs:872`). The default-build test sweep doesn't bump it. **Recommendation: add a Rust-side unit test or an inject-path scenario that walks the UDP/IGMP/SCTP rejection branch.**

- **`parse_proc_arp_line` test coverage has a quote-edge gap** (`arp.rs:474-487`): the test data uses a fixed-width column layout that doesn't match real `/proc/net/arp` output. A real `/proc/net/arp` line uses a tab-or-space separator that varies by kernel version; the test would still pass because `split_whitespace()` is permissive, but a malformed entry with embedded `\t` in the MAC field would not be caught. **Recommendation: add a test with the actual byte-for-byte format from a current Linux 6.x kernel.** Low priority — the parser is bound-checked.

## Observability gaps

- **`ip.rx_drop_short` is double-bumped on `BadTotalLen`** (`engine.rs:3924-3927`): both the `Short` case (`:3913`) and the `BadTotalLen` case map to `ip.rx_drop_short`. The L3 decoder returns *distinct* enum variants (`L3Drop::Short` for header < 20 bytes, `L3Drop::BadTotalLen` for `total_len > pkt.len()`); the engine collapses them to one counter. This is a documented decision (`bad_total_len_dropped` test asserts `ip.rx_drop_short` increment, see commit `d8dad1f`), but **it loses observability fidelity**: an operator seeing `ip.rx_drop_short` climbing cannot tell "framing truncation upstream" vs "malformed total_len field." **Recommendation: add an `ip.rx_drop_bad_total_len` counter (slow-path, fits the existing `_pad: [u64; 4]`) and split the bump.** The decoder enum variant already distinguishes them, so the cost is one counter field.

- **`OtherDropped | Malformed` ICMP results collapse to a no-op** (`engine.rs:3986`). A2 mTCP review's `AD-8` documented this and explicitly noted "deferred to Stage 2 hardening or an A3+ counter-refinement pass." A3-A10 shipped without picking it up. **Recommendation: split into `ip.rx_icmp_other_dropped` + `ip.rx_icmp_malformed` (slow-path) — fits the same `_pad` budget as above.**

- **`eth.rx_pkts` is bumped *per-burst* via `add(&self.counters.eth.rx_pkts, n as u64)` at `engine.rs:2610`, but the `inject_rx_frame` path bumps it *per-mbuf* via `inc(&self.counters.eth.rx_pkts)` at `engine.rs:6362`.** Both are correct in their context, but the inject path's comment explicitly notes this counter-bump dance was added late ("A8.5 T10 follow-up regression") to fix counter-coverage drift. The two paths' counter-bump shapes diverged silently; a future reviewer auditing per-burst-batched semantics will need to reconstruct why these two sites differ. **Recommendation: extract a `bump_rx_accepted(n)` helper so both paths fold through one site.**

- **`gateway_mac()` accessor returns the *learned* mac via `Cell::get`** (`engine.rs:2090`) — this read is non-atomic and the writer at `set_gateway_mac` (`engine.rs:2097`) likewise uses `Cell::set`. The Engine is single-lcore (per spec §3) so this is fine on x86_64 today, but a future refactor that exposes `Engine` across lcores would silently introduce a data race. The other `Cell` fields (`rx_mempool_avail_last_sample_tsc`, `last_gw_arp_req_ns`, `last_ephemeral_port`) inherit the same single-lcore assumption. **Recommendation: document the single-lcore-write invariant on `Engine` itself, not just inside the per-lcore TCP design notes.** Critical now because A6.7 added `engine_no_eal_harness` (multi-thread bench surface) — the harness side avoids the issue by not touching these cells, but the contract is implicit.

## Memory-ordering / ARM-portability concerns

- **`clock.rs:39` `compile_error!("dpdk-net-core currently only supports x86_64");`** — covered above under invariant violations, also a portability blocker.

- **`siphash_4tuple` (`flow_table.rs:43-61`) uses `std::collections::hash_map::RandomState` — process-random, runtime-stable, but the function casts the 64-bit hash to `u32` via `h.finish() as u32`** which discards the high 32 bits. On x86_64 this is correct because every `u32` value is a valid bucket index given a `Vec<…>`-shaped table. On a future flat-bucket implementation that expects the *full* 64-bit hash for cache-line distribution, this truncation would silently halve the entropy. The `lookup_by_hash` path already promises forward-compat with NIC RSS hashes (32-bit Toeplitz). **Recommendation: document that the bucket selector is a `u32` contract by design; OR widen now to `u64` to match RSS internally.** (The `nic_rss_hash` is `u32` per DPDK ABI, so widening would require fold-down at the seam — keeping `u32` is fine if documented.)

- **Counter writes are uniformly `Ordering::Relaxed`** (spec §9.1 says this is intentional). Slow-path counters are correct. Hot-path counters under `obs-poll-saturation` and `obs-byte-counters` are also correct because of the single-writer-lcore invariant. No drift here; flagged for completeness — the spec text justifies the choice and the code matches.

## C-ABI / FFI

- **`dpdk_net_engine_config_t.tcp_min_rto_ms`, `tcp_timestamps`, `tcp_sack`, `tcp_ecn` are exposed in the public C ABI but never read by the implementation.** Severity: high — a C++ caller setting any of them gets silently ignored, and the spec §4 contract claims they are normative.
  - `tcp_min_rto_ms`: declared at `crates/dpdk-net/src/api.rs:34`, mirrored in `include/dpdk_net.h:92`. NEVER read by `dpdk_net_engine_create` (`lib.rs:141-269`). The `_us` triplet (`tcp_min_rto_us`/`tcp_initial_rto_us`/`tcp_max_rto_us`) supersedes it per the comment at `api.rs:35-38`, but the field stays allocated on the struct and the comment doesn't say "deprecated, ignored — use the `_us` field instead." A caller seeing both `tcp_min_rto_ms` and `tcp_min_rto_us` in the header has no signal which one wins.
  - `tcp_timestamps`, `tcp_sack`, `tcp_ecn`: declared at `api.rs:28-30`, mirrored in `include/dpdk_net.h:86-88`. NEVER read by `dpdk_net_engine_create` (no consumption site exists). The actual SYN options are hard-coded at `engine.rs:243-257` (`build_connect_syn_opts`) which always sends MSS+WS+SACK-permitted+TS regardless of the config flags. `EngineConfig` (the core-side struct, `engine.rs:376`) does not even have these fields. Setting `tcp_timestamps=false` and `tcp_sack=false` does not turn the options off. Spec §4 line 91-93 ("`bool tcp_timestamps; /* RFC 7323; default true */ bool tcp_sack; /* RFC 2018; default true */ bool tcp_ecn; /* RFC 3168; default false */`") is currently a lie at the C ABI level.
  - **Recommendation:** either (a) delete the four fields from `dpdk_net_engine_config_t` (ABI break — but Stage 1 is pre-1.0), or (b) wire them through to `EngineConfig` and gate the option-builder on them. (b) is the spec-honest choice. (a) requires an ABI version bump in `dpdk_net.h`.

- **No `tcp_initial_rto_ms` field on the C ABI** even though spec §4 line 98 lists it. The `_us`-suffixed `tcp_initial_rto_us` is the actual field. A2-era callers reading the spec would write code expecting the `_ms`-suffixed field, get a compile error, then discover `_us` exists. The comment at `api.rs:35-38` partially explains but is buried; the spec text needs an update.

- **`dpdk_net_eal_init` is the public wrapper at `lib.rs:118-138`** — converts every error to `-libc::EAGAIN`. The core `engine::eal_init` returns `Error::EalInit(errno)` carrying the actual DPDK errno; that detail is dropped at the FFI seam. A C++ caller debugging hugepage exhaustion vs argv parse error can't tell from the return value. **Recommendation: pass the negative errno through directly** (DPDK already returns `-rte_errno` semantics). Low priority — out-of-band stderr captures the real cause.

- **`crates/dpdk-net/cbindgen.toml` `[export] include = [...]` whitelist** (`cbindgen.toml:53-74`) explicitly force-emits 14 type names. This is a maintenance hazard: a future type with a forgotten entry silently drops out of the header. The drift-check script in commit `c069421` should also assert "every `#[repr(C)]` type in `dpdk-net/src/api.rs` appears in the header." Today the script only diffs the header against the regenerated bytes — a missing-from-include type would never surface because both runs would omit it identically. **Recommendation: add a positive coverage assertion.**

## Hidden coupling

- **`engine.rs` reads `arp::ARP_FRAME_LEN` via raw `[u8; arp::ARP_FRAME_LEN]` stack arrays** at `engine.rs:3871`, `engine.rs:6248`, `engine.rs:6525`. The constant is `pub const ARP_FRAME_LEN: usize = 14 + 28 + 18 = 60` (`arp.rs:20`). If a future ARP module needs to grow the frame (jumbo Ethernet doesn't apply, but a new htype might), every engine call site would have to track. Today it's three sites and a `[u8; ...]` literal. **Recommendation: have arp.rs export a `ScratchBuf` type or use a small `SmallVec` on the engine side**; flagged as latent rather than active.

- **`engine.rs` directly calls `arp::resolve_from_proc_arp` and `arp::read_default_gateway_ip`** in pre-`Engine::new` code paths (the public crate's `dpdk_net_resolve_gateway_mac` shim at `dpdk-net/src/lib.rs`). This is fine — the helpers are explicitly `pub` per A2's public API addition — but the `arp.rs` module-doc says "static-gateway mode. We don't run a dynamic resolver on the data path" while three lines below the data-path engine *also* runs `Engine::handle_arp` → `set_gateway_mac` (since A2 mTCP review's `AD-1` accepted divergence). The module doc was written before the dynamic mac learning landed. **Recommendation: rewrite the module doc to match what the file actually does** — static-gateway *seed* + dynamic refresh from inbound ARP.

- **`engine.rs:1146` `Engine::new` is 420+ lines** straddling clock init, mempool sizing, port-offload negotiation, RSS reta programming, RX-timestamp dynfield lookup, MAC read, xstat resolution, and final struct construction. None of this fits in a single function's mental model. The phase-by-phase accumulation pattern (A2 added L2/L3 fields, A-HW added offload latches, A-HW+ added ENA xstats, A6 added histogram edges, A10 added mempool size knobs) is the cause; no consolidation pass ever ran. **Recommendation: split into `Engine::new` (top-level) → `Engine::bring_up_port`, `Engine::resolve_offloads`, `Engine::populate_runtime_state` helpers.** Today's `configure_port_offloads` is the only one factored out. Out of scope for a "fix what's broken" pass; flagged as accumulated complexity.

- **`THREAD_COUNTERS_PTR` thread-local in `mempool.rs:21-24`** — set/cleared by `Engine::new` and `Engine::drop`. This couples the mempool layer to engine lifecycle through global thread-local state instead of an explicit handle. The leak-detection rationale (`mempool.rs:11-20` doc-comment) is valid; the implementation is a phase-by-phase patch (A10 Stage A added it). The pattern works because Stage 1 is one engine per lcore — but the `EngineNoEalHarness` (`engine.rs:99`) shares no such counter binding, so the harness's mbuf paths silently lose the diagnostic. **Recommendation: have the harness either set the pointer (matching production) or document the diagnostic gap.**

## Documentation drift

- **`crates/dpdk-net-core/src/arp.rs` module doc** (lines 1-13) says "static-gateway mode. We don't run a dynamic resolver on the data path" — but `classify_arp` at `:127-139` (added later) is a partial dynamic resolver (REPLY-from-gateway path + gratuitous-from-gateway path both update the cell). Doc is out of date.

- **Phase-a2 mTCP review's `AD-1`** (`reviews/phase-a2-mtcp-compare.md:42-45`) says "We do not learn gateway MAC from inbound ARP replies" — but the current `classify_arp` *does* learn from inbound ARP replies. The cross-phase concern: the review was correct *at A2 sign-off*, but the same finding has no entry in any later mTCP review noting "A6 fixed AD-1 from A2 review." If a future maintainer reads the A2 review as ground truth, they will be misled. **Recommendation: add a "Status: superseded by [phase-aN review entry] / commit SHA" line to A2's `AD-1`.** Same applies to A2's `AD-2`.

- **Spec §6.4 `AD-A8.5-tx-wscale-position`** (lines 469-470) says we emit options in `<MSS, NOP+WSCALE, SACKP, TS, [NOPs+SACK-blocks]>` order. `build_connect_syn_opts` at `engine.rs:243-257` constructs `TcpOpts { mss, wscale, sack_permitted: true, timestamps: Some(...) }` and the actual byte-order ends up in `tcp_options.rs`. The spec text and the construction site are far enough apart that a tracing maintainer would have to walk through `tcp_options::TcpOpts::serialize` to verify. Low priority — the regression test `tools/tcpreq-runner` corpus pin asserts the byte order — but flagged as drift-hostile.

- **A1 plan (`docs/superpowers/plans/2026-04-17-stage1-phase-a1-skeleton.md`) at line 906-939** documents the EAL bring-up sequence with code snippets that match the current `Engine::new` shape (configure → rx_queue_setup → tx_queue_setup → start). Good. But the plan's text mentions `eal_init` as part of A1; the cross-phase brief mentions `eal.rs` — neither corresponds to the current physical layout (`eal_init` is in `engine.rs`). **Recommendation: drop the cross-phase brief's `eal.rs` reference or migrate `eal_init` to its own module.**

## FYI / informational

- **`Engine::dispatch_one_rx_mbuf` was extracted from `poll_once`** at A9 (visible at `engine.rs:2640-2643` invocation site). The extraction is clean and the helper is shared between RX-burst dispatch and `inject_rx_frame`. Good factoring; just documenting that the original A1 single-function poll loop is now multi-helper.

- **`siphash24.rs` (per-process random hasher)** is fine as-is. The `RandomState` per-process seed makes hash values runtime-unique but stable within a run. Tests at `flow_table.rs:248-356` correctly test stability-within-run.

- **`Engine` struct holds 30+ fields** (see lines 696-800 area). Many are `RefCell`/`Cell`/`OnceCell`. Single-lcore design makes this safe, but reviewers should be told upfront — the engine-doc lacks a "this is per-lcore-only; never share across lcores" warning.

- **The `bench_alloc_audit.rs` module is `#[cfg(feature = "bench-alloc-audit")]`** at `lib.rs:5-6`. This is sound; mentioned because the cross-phase brief lists "bench-alloc-audit" as a steady-state hot-path zero-allocation gate but doesn't note that the module is feature-gated and absent from default-build coverage.

- **`eal_init` returns `Ok` if already initialized** (`engine.rs:933-936`). The mutex guard is correct. But the second-and-later callers cannot tell whether their EAL args were honored or silently ignored. A C++ caller that initializes once at thread A and once at thread B with different args gets thread A's args silently. Stage 1 is one-engine-per-lcore so this is fine; calling out for awareness.

- **No A1 phase reviews exist** (`docs/superpowers/reviews/` has no phase-a1-* files). Spec §10.13/§10.14 explicitly exempts A1 ("phase A1 is exempt because it ships no algorithmic code"). Cross-phase concerns about A1 (EAL bring-up, mempool sizing formulas, clock calibration drift) accordingly never surfaced through the per-phase review gate. The A1 surface is therefore the *least*-reviewed surface in the project despite carrying core invariants like `EAL_INIT: Mutex<bool>`, `THREAD_COUNTERS_PTR`, `clock::init()`. The cross-phase pass should treat any A1 finding with extra weight precisely because no per-phase review caught it.

## Verification trace

Files read (all paths under `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review/`):
- `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md` (§§1-9.1.1)
- `docs/superpowers/plans/2026-04-17-stage1-phase-a1-skeleton.md` (excerpts via grep — line refs only)
- `docs/superpowers/plans/2026-04-17-stage1-phase-a2-l2-l3.md` (excerpts via grep + first 200 lines)
- `docs/superpowers/plans/stage1-phase-roadmap.md` (first 200 lines incl. A1/A2 boundaries)
- `docs/superpowers/reviews/phase-a2-mtcp-compare.md` (full)
- `docs/superpowers/reviews/phase-a2-rfc-compliance.md` (full)
- `crates/dpdk-net-sys/{Cargo.toml, src/lib.rs, build.rs, wrapper.h}` (full)
- `crates/dpdk-net-core/src/{lib.rs, l2.rs, l3_ip.rs, icmp.rs, arp.rs, clock.rs, flow_table.rs, mempool.rs}` (full or near-full)
- `crates/dpdk-net-core/src/engine.rs` (line ranges 1-200, 920-1568, 1820-1900, 2055-2150, 2540-2710, 3815-4060, 6230-6540 + several greps)
- `crates/dpdk-net-core/src/counters.rs` (greps + targeted reads)
- `crates/dpdk-net/src/{Cargo.toml, build.rs, lib.rs, api.rs}` (relevant sections)
- `crates/dpdk-net/cbindgen.toml`
- `include/dpdk_net.h` (lines 70-189)
- `crates/dpdk-net-core/tests/l2_l3_tap.rs` (first 200 lines)

Greps run (selective):
- `eal_init`, `rte_eal_init`, `rte_eth_dev_configure`, `rte_eth_dev_start` across `crates/`
- `tcp_min_rto_ms`, `tcp_initial_rto_ms`, `tcp_min_rto_us`, `tcp_timestamps`, `tcp_sack`, `tcp_ecn` across `crates/` and `include/`
- `TODO|FIXME|XXX|todo!()|unimplemented!()|unreachable!()` across all A1/A2 source files
- `#\[allow|#\[cfg` across A1/A2 modules
- `Ordering::Relaxed|SeqCst|Acquire|Release` in `counters.rs`
- `unwrap()|expect(|panic!` across `engine.rs`, `clock.rs`, `mempool.rs`
- A2 counter increments (`eth.rx_arp`, `eth.tx_arp`, `ip.rx_drop_*`, etc.) in `engine.rs`
- `gateway_mac`, `last_gw_arp_req_ns`, `maybe_emit_gratuitous_arp`, `maybe_probe_gateway_mac` in `engine.rs`

Cross-references:
- `git log --oneline phase-a1-complete | head -50` (28 A1 commits)
- `git log --oneline phase-a1-complete..phase-a2-complete` (24 A2 commits)
- `git tag` (all 18 phase tags listed)

## Working notes

- The five high-impact findings, ranked by severity:
  1. **C-ABI dead fields** (`tcp_min_rto_ms`, `tcp_timestamps`, `tcp_sack`, `tcp_ecn`) — silent caller-intent loss, spec §4 lie.
  2. **Panic-across-FFI in `eal_init`** — UB under unwind builds.
  3. **`compile_error!` x86_64 lock** — blocks every other module from building on aarch64 / Graviton; conflicts with `feedback_arm_roadmap.md`.
  4. **`maybe_emit_gratuitous_arp` never moved to timer wheel** — A2's "~3-line A6 change" never landed; spec §8 prose is now ahead of code.
  5. **`ip.rx_drop_short` double-bumped on `BadTotalLen`** — observability fidelity loss; documented in test but not in counter.

- Everything else is documentation drift, accumulated complexity, or test-coverage gaps that would not block sign-off on their own. The two test-pyramid concerns (TAP-gated integration, missing default-build dispatcher coverage) collectively mean the A2 wiring is *under-tested* in CI — a refactor breaking it would not surface until the env-gated harness ran. With A9's `test-inject` available, this is now cheap to fix.

- A1 has no per-phase reviews. Every A1 invariant (EAL init mutex, clock calibration, per-lcore thread-locals) carries proportionally more cross-phase risk because it never went through the §10.13/§10.14 gate. The C-ABI dead-field finding is downstream of A1's `dpdk_net_engine_config_t` definition — an A1 review would have flagged it then.
