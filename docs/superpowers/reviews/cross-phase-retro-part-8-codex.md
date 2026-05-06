# Part 8 Cross-Phase Retro Review (Codex)
**Reviewer:** codex:codex-rescue
**Reviewed at:** 2026-05-05
**Part:** 8 — AWS infra + benchmark harness + DPDK 24.x + perf cherry-picks (largest part)
**Phases:** A10 (incl. PR #9 deferred-fixes + T17-T23 post-phase tickets)

## Verdict

Ship posture: **do not treat the post-A10 max-throughput comparator as fully trustworthy yet**. The PR #9 RX mbuf cliff fix is preserved in the main owning-drop paths, and the obvious `rte_mbuf_refcnt_update(-1)` reintroduction was not found in `MbufHandle::Drop` or `ReorderQueue::drop_segment_mbuf_ref`. However, the T17/T22 follow-ups left several benchmark-mechanical defects that can either hide the very stalls A10 is trying to diagnose or make an implemented comparator unreachable from the normal CLI.

1. **BUG — `dpdk_net_conn_stats` still reports the dead `snd.pending` queue after T17 moved the real send-buffer pressure to `snd_nxt - snd_una`.** `crates/dpdk-net-core/src/tcp_conn.rs:725` computes `pending` from `self.snd.pending.len()`, and `crates/dpdk-net/src/lib.rs:772` copies that into the public `send_buf_bytes_pending` ABI field. The actual send cap in `Engine::send_bytes` is now `send_buffer_bytes.saturating_sub(in_flight)`, with `in_flight = seq_start.wrapping_sub(snd_una)` at `crates/dpdk-net-core/src/engine.rs:5339` and `:5341`. T21's own investigation documents that `snd.pending` is production-stale (`docs/superpowers/reports/t21-stall-investigation.md:48`) and the benchmark diag had to stop surfacing it in one stall path (`tools/bench-vs-mtcp/src/dpdk_burst.rs:277`). This is not only an in-bench message issue: the C ABI contract still advertises per-order send-buffer forensics, but under real TX pressure it returns `0` pending and full free space.

2. **BUG — the implemented T22 mTCP driver is unreachable from the normal `bench-vs-mtcp --stacks mtcp` dispatcher.** The main dispatcher still calls functions named `run_burst_grid_mtcp_stub` and `run_maxtp_grid_mtcp_stub` at `tools/bench-vs-mtcp/src/main.rs:344` and `:399`. Those functions invoke the wrapper, but if the now-implemented C driver returns `Ok`, they immediately `bail!` instead of emitting rows (`tools/bench-vs-mtcp/src/main.rs:724` and `:738` for burst; `:1274` and `:1279` for maxtp). The C driver is no longer a stub (`tools/bench-vs-mtcp/peer/mtcp-driver.c:22` says implemented; maxtp output is emitted at `:943`), so this stale Rust-side stub wiring turns a successful mTCP comparator into a hard error.

3. **LIKELY-BUG — mTCP subprocess timeout is accepted but ignored.** `tools/bench-vs-mtcp/src/mtcp.rs:145` and `:275` carry timeout fields, but `invoke_driver` discards the value at `tools/bench-vs-mtcp/src/mtcp.rs:389` and waits with `child.wait_with_output()` at `:396`. With the T22 driver now doing real DPDK/mTCP work, a process hung in EAL init, connect, `mtcp_epoll_wait`, or a path not covered by the driver's internal `OP_DEADLINE_SECS` can wedge the parent benchmark indefinitely. This is especially dangerous in nightly orchestration, where stderr may never be collected and subsequent stacks will not run.

4. **LIKELY-BUG — mTCP maxtp includes post-window drain bytes in the numerator but not in the duration.** The measurement window ends at `tools/bench-vs-mtcp/peer/mtcp-driver.c:878`. Then a residual echo drain continues adding to `bytes_echoed` for up to 50 ms at `:880` and `:896`, while `duration_s` remains `measure_end - measure_start` at `:917` and goodput uses the inflated `bytes_echoed` at `:927`. The existing T22 report notes a related residual-drain bias as non-blocking, but this is a measurement correctness issue: on low-throughput or high-latency cells, a 50 ms post-window numerator can move the reported goodput without extending the denominator.

5. **SMELL / fragile-safe — surviving `refcnt_update(-1)` sites are rollbacks, not owning drops, but the invariant is implicit.** The PR #9 class was fixed in `MbufHandle::Drop` via `rte_pktmbuf_free_seg` at `crates/dpdk-net-core/src/mempool.rs:273` and in OOO segment drop via `crates/dpdk-net-core/src/tcp_reassembly.rs:298`. Remaining negative updates in reviewed paths are rollback-only: pre-dispatch RX bump rollback at `crates/dpdk-net-core/src/engine.rs:4185` and `:4232`, chained-link OOO insert rollback at `crates/dpdk-net-core/src/tcp_input.rs:1393`, and retransmit chain-fail rollback at `crates/dpdk-net-core/src/engine.rs:6163`. They are mechanically defensible because another reference must remain alive, but the code does not mark those sites with the same explicit "surviving owner" contract. A future edit that moves the original free earlier could silently recreate the cliff class.

## Architectural drift

**SMELL — benchmark tools keep bypassing the C ABI and depending on engine internals.** A10 made this worse rather than better. `bench-ab-runner` imports `InternalEvent` directly at `tools/bench-ab-runner/src/workload.rs:47` and drains `engine.events()` through a `RefMut` at `:356`. `bench-vs-mtcp` reads engine diagnostic state through `Engine::diag_conn_stats` and `diag_input_drops` (`crates/dpdk-net-core/src/engine.rs:2452`, `:2468`) to drive stall forensics. `bench-micro` gained a public `EngineNoEalHarness` export under `bench-internals` at `crates/dpdk-net-core/src/lib.rs:62`. The direction is now "benchmark harness as privileged friend of engine", not "benchmark harness as user of stable product API".

**SMELL — diagnostic accessors are growing ad hoc.** T17 added `tx_data_mempool_size`/mempool diagnostics, T21 added `InputDropsSnapshot`, and max-throughput benches call those directly. That is understandable during a rescue, but it hard-codes which five input-drop counters matter (`crates/dpdk-net-core/src/engine.rs:2479`) and leaves C ABI consumers without equivalent stall attribution.

**SMELL — mTCP comparator has a version and process boundary that is only locally documented.** The C driver explains DPDK 20.11 sidecar constraints (`tools/bench-vs-mtcp/peer/mtcp-driver.c:13`), while the rest of the A10 perf work is on DPDK 23.11 with 24.x adoption deferred. This is a legitimate design choice, but cross-stack benchmark readers need a single visible note that mTCP numbers include a DPDK-20.11 sidecar and subprocess boundary.

## Cross-phase invariant violations

**BUG — public ConnStats send-buffer invariant is broken post-T17.** A5.5 introduced `dpdk_net_conn_stats` for "bytes in send path plus RTT" forensics. T17's production TX path now accepts directly into `snd_retrans` mbuf refs and gates by in-flight bytes, while `ConnStats` stayed pinned to the old `snd.pending` queue. See the first verdict finding. This violates the cross-phase invariant that A5.5 introspection describes real send-path state.

**FYI — TCP sequence arithmetic in the T17 maxtp accumulator is mostly correct for the intended window.** `SndUnaAccumulator::accumulate` uses per-conn `wrapping_sub` at `tools/bench-vs-mtcp/src/dpdk_maxtp.rs:477`, then widens to `u64`. This fixes the single pre/post wrap loss in a 60 s 100 Gbps window. The remaining assumption is that each sample interval sees less than one full 32-bit wrap per connection; the comment at `:435` acknowledges the 1 s cadence. If a future 400 Gbps single-flow setup is tested, the sample interval must shrink or switch to a wider ACK source.

**FYI — modular TCP arithmetic in the core send cap is preserved.** `Engine::send_bytes` uses `seq_start.wrapping_sub(snd_una)` for in-flight (`crates/dpdk-net-core/src/engine.rs:5339`) and advances `cur_seq` with `wrapping_add` (`:5578`). I did not find a T17 change that replaced modular comparison with linear `u64` comparisons in this path.

## Tech debt accumulated

**SMELL — stale bench-nightly iteration workaround outlives the RX cliff fix.** `scripts/bench-nightly.sh:495` says the default was lowered below a deterministic iteration ~7051 cliff and still sets `BENCH_ITERATIONS=5000` at `:503`. The PR #9 `MbufHandle::Drop` fix landed later, and long-run fix reports reference 100k-iteration validation. If 5k is the desired fast nightly default, the comment should say that; currently it tells operators the old cliff is still unresolved.

**SMELL — Rust-only `tx_data_mempool_size` knob.** `EngineConfig` documents the formula and manual override at `crates/dpdk-net-core/src/engine.rs:409`, but the C ABI path pins it to zero/formula at `crates/dpdk-net/src/lib.rs:214`. That is a reasonable ABI-conservation choice, but it is not visible to C/C++ callers who may hit the same T17 pool-sizing issue under unusual `max_connections * send_buffer_bytes` products.

**SMELL — mTCP wrapper docs still describe a stub.** `tools/bench-vs-mtcp/src/mtcp.rs:26` through `:37` says the client driver is a stub, while the C file says implemented. This documentation mismatch likely contributed to the stale `*_stub` dispatcher functions remaining after T22.

## Test-pyramid concerns

**LIKELY-BUG coverage gap — there is no unit/integration test that exercises the mtcp `Ok` path through `main.rs`.** The wrapper parser tests accept successful JSON (`tools/bench-vs-mtcp/src/mtcp.rs:786`), but the dispatcher's `Ok` branch still bails. A tiny fake-driver integration test could have caught this without live DPDK: point `--mtcp-driver-binary` at a script that prints valid JSON and exits 0, then assert CSV rows are emitted.

**SMELL — max-throughput bugs are still found by full AWS benches first.** T17's pool sizing, handle cleanup, and K=1MiB stall fixes are all benchmark-surfaced. There are targeted mempool leak regressions now, but I did not find a fast test for maxtp bucket churn with C>1, stale handle teardown, or K=1MiB repeated burst acceptance. Those are the actual shapes that failed.

**SMELL — no failing test protects the ConnStats send-buffer meaning.** Existing stats tests check shape and simple saturation, not that real production `send_bytes` pressure makes `send_buf_bytes_pending` nonzero or `send_buf_bytes_free` shrink. The stale field survived because the diagnostics learned to compute `in_flight` locally rather than fixing the shared ABI projection.

## Observability gaps

**BUG-adjacent — T17's new stall signals are sampled or stderr-only.** `tcp.tx_data_mempool_avail` is sampled once per second in `poll_once` (`crates/dpdk-net-core/src/engine.rs:2517` through `:2534`), and force-close diagnostics print a snapshot to stderr (`:3469`). `close_persistent_connections` soft-fails by `eprintln!` only at `tools/bench-vs-mtcp/src/dpdk_maxtp.rs:241`. A scheduled benchmark can produce invalid cleanup state without a structured CSV marker unless a later bucket fails.

**SMELL — mTCP JSON carries fields the wrapper ignores.** The C driver emits `bytes_sent_total` for burst and maxtp (`tools/bench-vs-mtcp/peer/mtcp-driver.c:647`, `:947`), but the Rust parser reads only samples or `(goodput_bps, pps)` (`tools/bench-vs-mtcp/src/mtcp.rs:425`, `:456`). That means sanity fields can drift, including the T22-noted maxtp "total" semantics, without the Rust harness noticing.

**SMELL — input-drop diagnostics are engine-wide, not per connection.** `diag_input_drops` loads engine-wide counters (`crates/dpdk-net-core/src/engine.rs:2479`). In the C>1 maxtp case, a wedged bucket logs the same aggregate counters beside every conn (`tools/bench-vs-mtcp/src/dpdk_maxtp.rs:384`). This is useful triage but can misattribute drops when multiple conns are active.

## Memory-ordering / ARM-portability concerns

**FYI — reviewed Relaxed atomics were counters/snapshots, not synchronization flags in the engine.** T21's `diag_input_drops` uses `Ordering::Relaxed` loads for counters (`crates/dpdk-net-core/src/engine.rs:2479`), and T17's mempool avail sampler stores with Relaxed (`:2525`, `:2533`). These are telemetry snapshots; they do not publish memory protected by a flag. I did not find an A10/T17-T23 `Relaxed` flag that another thread uses for ordering in the production engine.

**SMELL — benchmark-side `AtomicBool` stop flags use Relaxed.** Example: Linux maxtp's helper thread loops on `done_t.load(Ordering::Relaxed)` and the owner stores true at `tools/bench-vs-mtcp/src/linux_maxtp.rs:381` and `:422`. For a simple "eventually stop" flag this is acceptable on Rust's atomic model, but if later used to publish buffers or statistics, this must become Release/Acquire or use channels.

**SMELL — current design still assumes one engine lcore plus external snapshot readers.** The `RefCell` API surface (`engine.events()`, `engine.flow_table()`) is not thread-safe by construction. Relaxed counters are fine as long as there is no cross-thread causality requirement; Stage 2 multi-lcore work will need a fresh memory-ordering review rather than inheriting A10's choices.

## C-ABI / FFI

**BUG — C ABI `dpdk_net_conn_stats` exposes stale send-buffer fields.** `crates/dpdk-net/src/api.rs:268` defines the ABI struct with `send_buf_bytes_pending/free`, and `crates/dpdk-net/src/lib.rs:772` fills them from the stale core projection. This is the highest-impact C-ABI issue found because it can mislead per-order forensics in production consumers, not only benchmark code.

**SMELL — no C ABI for TX data mempool sizing.** C callers cannot override `tx_data_mempool_size`; `dpdk_net_engine_create` forces the formula sentinel (`crates/dpdk-net/src/lib.rs:214`). If the formula is meant to be sufficient for all C callers, that should be documented in the header/guide. If not, this needs an ABI extension plan.

**SMELL — mTCP C driver has trusted-CLI arithmetic.** `samples_bps` is allocated as `sizeof(*samples_bps) * a->bursts` at `tools/bench-vs-mtcp/peer/mtcp-driver.c:556` after parsing a raw `u64`. The Rust wrapper's normal `bursts` value protects the live path, but direct driver invocation can overflow allocation size. This is low risk for bench automation, but it is still a mechanical C edge.

## Hidden coupling

**BUG — T22 requires coordinated edits in C driver, Rust wrapper, and dispatcher, but only the C driver was made real.** The C file now emits valid JSON; the wrapper can parse that JSON; the main dispatcher still treats successful output as impossible. The hidden contract spans `peer/mtcp-driver.c`, `src/mtcp.rs`, and `src/main.rs`, and no integration test covers all three.

**SMELL — `Engine::flow_table()` returns `RefMut<FlowTable>` for benchmarks.** The maxtp runner snapshots internals by borrowing the flow table directly (`tools/bench-vs-mtcp/src/dpdk_maxtp.rs:606`, `:621`). This is a brittle coupling to single-threaded internals and makes lock/borrow ordering a benchmark responsibility.

**FYI — no new T17-T23 `RefCell::borrow_mut` chain panic found in the inspected hot fixes.** The risky code generally drops borrows before re-entering other engine methods, for example `send_bytes` drops the TX ring borrow before `drain_tx_pending_data` (`crates/dpdk-net-core/src/engine.rs:5474` through `:5493`), and `reap_time_wait` pops handles out of scratch before transition/removal (`:3620` through `:3628`).

## Documentation drift

**SMELL — bench-nightly still documents the old 7051 cliff.** See `scripts/bench-nightly.sh:495` through `:503`. This is now actively misleading because A10 deferred-fixes and PR #9 were about removing that cliff.

**SMELL — `mbuf_data_slice` comment says "Stage A2, only segment" while multi-seg RX exists elsewhere.** `crates/dpdk-net-core/src/lib.rs:65` through `:77` returns only head data. L3 rejects packets whose IPv4 total length exceeds that first slice (`crates/dpdk-net-core/src/l3_ip.rs:85`). This is an inherited Part 4 issue, but still relevant because A10 bench/stress work can hide jumbo/scatter behavior behind normal MTU tests.

**SMELL — mTCP wrapper says stub while C driver says implemented.** `tools/bench-vs-mtcp/src/mtcp.rs:26` conflicts with `tools/bench-vs-mtcp/peer/mtcp-driver.c:22`, and the dispatcher behavior matches the stale wrapper docs, not the implemented C driver.

## FYI / informational

**FYI — PR #9 cliff fix appears preserved.** `MbufHandle::Drop` uses `shim_rte_pktmbuf_free_seg` at `crates/dpdk-net-core/src/mempool.rs:281`, and `ReorderQueue::drop_segment_mbuf_ref` uses the same pool-aware free at `crates/dpdk-net-core/src/tcp_reassembly.rs:312`. I did not find a later owning-drop path changed back to `rte_mbuf_refcnt_update(-1)`.

**FYI — T17 TX data pool sizing formula is internally coherent.** The formula is documented at `crates/dpdk-net-core/src/engine.rs:409` and implemented at `:1222` through `:1250`. `bench-vs-mtcp` pins `tx_data_mempool_size: 32_768` for its actual grid bound at `tools/bench-vs-mtcp/src/main.rs:1716` through `:1733`, avoiding the enormous 512-connection formula default.

**FYI — DPDK 24.x was investigated but not promoted in the reviewed range.** The relevant commits document a 23.11 ship recommendation and 24.11 deferrals; I did not treat "DPDK 24.x adopt" as completed runtime behavior in this review.

## Verification trace

- `git tag --list 'phase-a*'` confirmed `phase-a9-complete`, `phase-a10-complete`, `phase-a10-deferred-fixed`, and `phase-a10-5-complete`.
- `git log --oneline phase-a9-complete..phase-a10-complete`, `phase-a10-complete..phase-a10-deferred-fixed`, and `phase-a10-deferred-fixed..HEAD` used to identify A10 main, PR #9 deferred fixes, and T17-T23/post-phase commits.
- `rg -n "refcnt_update|rte_pktmbuf_free_seg|MbufHandle|drop_segment_mbuf_ref"` over `crates/dpdk-net-core tools scripts` used for the mbuf/refcount audit.
- `rg -n "Ordering::Relaxed|AtomicBool|AtomicU|fetch_|store|load"` used for memory-ordering review; only counter/snapshot uses were found in the inspected T17-T23 production paths.
- Directly inspected T17 commit `8b25f8f` for TX mempool sizing, `tx_data_mempool_avail`, and maxtp cleanup.
- Directly inspected T21 commit `e2dddf1` for `InputDropsSnapshot` and stall diagnostics.
- Directly inspected T22 commit `72a2214` plus post-gate `71cf77e` for C mTCP driver implementation and known review findings.
- Reviewed current HEAD line references in `engine.rs`, `tcp_conn.rs`, `mempool.rs`, `tcp_reassembly.rs`, `dpdk_maxtp.rs`, `mtcp.rs`, `main.rs`, and `mtcp-driver.c`; findings cite HEAD line numbers.
