# Part 6 Cross-Phase Retro Review (Codex)
**Reviewer:** codex:codex-rescue
**Reviewed at:** 2026-05-05
**Part:** 6 — Test infrastructure (loopback + packetdrill shim + tcpreq + obs gate + coverage expansion)
**Phases:** A7, A8, A8.5

## Verdict
BUG: `tools/packetdrill-shim-runner/src/invoker.rs:18` documents `wall_timeout` as the hard per-script bound, but `tools/packetdrill-shim-runner/src/invoker.rs:25` accepts the argument and `tools/packetdrill-shim-runner/src/invoker.rs:30` discards it with `let _ = wall_timeout;`. The process is then run with blocking `Command::output()` at `tools/packetdrill-shim-runner/src/invoker.rs:44`, and the returned outcome hard-codes `timed_out: false` at `tools/packetdrill-shim-runner/src/invoker.rs:49`. That violates the A7/A8 corpus gates that explicitly treat `out.timed_out` as a no-crash failure condition at `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:73` and `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:134`.

No confirmed mechanical defect was found in tcpreq modular sequence arithmetic, PAWS edge tests, atomic ordering in scoped test-server code, RefCell lock ordering in `crates/dpdk-net-core/src/test_server.rs`, or scoped mbuf lifetime cleanup. Several requested focus areas are not actually present in the named scoped file: `crates/dpdk-net-core/src/test_server.rs:20` defines only `ListenSlot`, and `crates/dpdk-net-core/src/test_server.rs:49` starts packet construction/parsing helpers rather than the loopback engine, port allocator, atomics, RefCells, mempools, or timer wheel.

Summary for parent: I found one confirmed mechanical bug in the packetdrill shim runner: wall timeouts are part of the API and corpus assertions, but the runner never enforces them and always reports `timed_out == false`. I did not find confirmed sequence-arithmetic, atomic-ordering, RefCell-locking, mbuf-refcount, or A8 obs-gate counter-placement bugs in the scoped Rust code at HEAD; the remaining notes are SMELL/FYI around hidden external packetdrill behavior, one-shot coverage counters, and helper preconditions.

## Architectural drift
SMELL: packetdrill shim timing is no longer reviewable from the Rust runner alone. The Rust invoker only creates a subprocess at `tools/packetdrill-shim-runner/src/invoker.rs:39`, passes the script path at `tools/packetdrill-shim-runner/src/invoker.rs:42`, and collects stdout/stderr at `tools/packetdrill-shim-runner/src/invoker.rs:44`; there is no Rust-side packetdrill event-time ns-to-engine-tick conversion to inspect. The closest scoped virtual-time test sets the harness time directly with `set_now_ns(5_500_000)` at `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:75` and calls `pump_timers(now_ns())` at `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:80`, so focus item 1 is covered only as a black-box behavior check.

FYI: the external packetdrill binary is built as a Cargo build-script side effect, not as normal Rust source. `tools/packetdrill-shim-runner/build.rs:22` runs `build.sh`, and `tools/packetdrill-shim-runner/build.rs:25` asserts success. This is not a runtime bug by itself, but it means C shim behavior, time conversion, and shim/engine shared-state assumptions are hidden behind generated artifacts rather than visible in the reviewed Rust files.

FYI: requested port-allocation arithmetic was not found in the scoped `test_server.rs`. The file contains `ListenSlot` fields at `crates/dpdk-net-core/src/test_server.rs:21` through `crates/dpdk-net-core/src/test_server.rs:38` and fixed packet helper constants beginning at `crates/dpdk-net-core/src/test_server.rs:50`, but no allocator, ephemeral-port arithmetic, or port reservation table.

## Cross-phase invariant violations
BUG: the packetdrill no-crash invariant is weakened by the ignored timeout path. `tools/packetdrill-shim-runner/src/invoker.rs:18` says the default timeout should be enough because virtual time advances instantly, and `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:76` classifies a timeout as a corpus failure. Because `tools/packetdrill-shim-runner/src/invoker.rs:30` ignores the timeout and `tools/packetdrill-shim-runner/src/invoker.rs:49` always returns `timed_out: false`, a wedged script does not produce the failure shape expected by A7/A8 corpus tests; it can instead hang the runner.

FYI: no tcpreq probe sequence-number invariant violation was confirmed. The probes consistently use wrapping arithmetic for peer sequence advancement, for example `tools/tcpreq-runner/src/probes/mss.rs:90`, `tools/tcpreq-runner/src/probes/options.rs:107`, `tools/tcpreq-runner/src/probes/reserved.rs:123`, `tools/tcpreq-runner/src/probes/urgent.rs:76`, and `tools/tcpreq-runner/src/probes/rst_ack.rs:194`. The targeted sequence helper tests also exclude the RFC 793 antipode where ordering is intentionally undefined at `crates/dpdk-net-core/tests/proptest_tcp_seq.rs:31` and then check modular transitivity only outside that ambiguous distance at `crates/dpdk-net-core/tests/proptest_tcp_seq.rs:69`.

FYI: no PAWS edge violation was confirmed. The PAWS regression property explicitly generates timestamp deltas around the wrap boundary at `crates/dpdk-net-core/tests/proptest_paws.rs:132`, rejects old timestamps including large backward deltas at `crates/dpdk-net-core/tests/proptest_paws.rs:160`, and treats the `2^31` backward edge as rejected at `crates/dpdk-net-core/tests/proptest_paws.rs:164`.

## Tech debt accumulated
SMELL: several A8/A8.5 counter-coverage cases use the one-shot counter hook as the coverage mechanism rather than driving the real protocol path that owns the increment. The helper is named and documented as a direct counter bump at `crates/dpdk-net-core/tests/common/mod.rs:579`, it snapshots before/after state at `crates/dpdk-net-core/tests/common/mod.rs:584`, and it calls `engine.bump_counter_for_test(...)` at `crates/dpdk-net-core/tests/common/mod.rs:592`. Examples include `crates/dpdk-net-core/tests/counter-coverage.rs:1208`, `crates/dpdk-net-core/tests/counter-coverage.rs:1222`, `crates/dpdk-net-core/tests/counter-coverage.rs:1234`, and `crates/dpdk-net-core/tests/counter-coverage.rs:1618`. This is not an obs-gate bug, but it is test-pyramid debt because those assertions prove the telemetry plumbing exists, not that the production path increments at the intended point.

SMELL: `parse_tcp_seq_ack` is a public test helper with stricter caller preconditions than its sibling parser. `parse_syn_ack` checks the Ethernet/IP/TCP minimum lengths at `crates/dpdk-net-core/src/test_server.rs:201`, `crates/dpdk-net-core/src/test_server.rs:207`, and `crates/dpdk-net-core/src/test_server.rs:213`, but `parse_tcp_seq_ack` indexes `frame[14]` and TCP bytes directly at `crates/dpdk-net-core/src/test_server.rs:226` through `crates/dpdk-net-core/src/test_server.rs:231`. Current scoped callers feed frames drained from the test harness, so I am classifying this as a helper-contract smell rather than a confirmed runtime bug.

FYI: fault-injector chain UAF checks depend on the allocator/sanitizer environment for deterministic detection. The test file says the failure becomes deterministic when run under sanitizer or debug allocator checks at `crates/dpdk-net-core/tests/fault_injector_chain_uaf_smoke.rs:21`, then exercises chain drop behavior at `crates/dpdk-net-core/tests/fault_injector_chain_uaf_smoke.rs:74` and `crates/dpdk-net-core/tests/fault_injector_chain_uaf_smoke.rs:118`. That is acceptable as a smoke test, but it is not a hard proof that every injected-mbuf error path is leak-free in non-sanitized CI.

## Test-pyramid concerns
BUG: the packetdrill corpus tests are capable of asserting script failure and nonzero exit status, but not timeout behavior, because the lower-level runner never reports it. `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:67` calls `run_script_with_timeout`, `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:73` branches on `out.timed_out`, and `tools/packetdrill-shim-runner/src/invoker.rs:49` makes that branch unreachable. This leaves the top-level corpus suite vulnerable to wedging on exactly the class of failures its API says it handles.

FYI: packetdrill corpus execution appears serial in the Rust tests. The corpus loops call `run_script_with_timeout` per script at `tools/packetdrill-shim-runner/tests/corpus_ligurio.rs:67`, `tools/packetdrill-shim-runner/tests/corpus_google.rs:62`, and `tools/packetdrill-shim-runner/tests/corpus_shivansh.rs:54`. I did not find scoped Rust evidence of concurrent script execution or Rust-side shared state between multiple shim subprocesses; any concurrency concern would be a hypothesis about the external binary, so I am not marking a lock-ordering bug.

FYI: the timer-wheel coverage visible in scoped files is fire/reap oriented, not disconnect-mid-run cancel oriented. The shim virtual-time test advances to retransmission time at `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:75` and pumps timers at `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:80`; counter coverage also pumps timers directly at `crates/dpdk-net-core/tests/counter-coverage.rs:1538`. I did not find a scoped A7/A8/A8.5 test that disconnects a test client mid-run and asserts timer-cancel discipline, so focus item 8 remains a coverage gap rather than confirmed defective behavior.

## Observability gaps
FYI: the A8 obs smoke gate is stronger than the one-shot coverage cases. `crates/dpdk-net-core/tests/obs_smoke.rs:72` builds the expected counter set for the scripted path, `crates/dpdk-net-core/tests/obs_smoke.rs:160` starts the exact expected-counter assertion helper, and `crates/dpdk-net-core/tests/obs_smoke.rs:165` performs the exact `assert_eq!`. The fail-loud unexpected-counter walk starts at `crates/dpdk-net-core/tests/obs_smoke.rs:288` and compares every declared counter at `crates/dpdk-net-core/tests/obs_smoke.rs:295`. I did not find evidence that an A8 obs-gate assertion depends on a counter that is bumped by an uncovered path inside the same test.

SMELL: by contrast, the broader counter coverage can pass even if a real data path never exercises the increment site, because `bump_counter_one_shot` directly manipulates the counter under test. The direct hook is visible at `crates/dpdk-net-core/tests/common/mod.rs:592`, and coverage examples such as `crates/dpdk-net-core/tests/counter-coverage.rs:1208` and `crates/dpdk-net-core/tests/counter-coverage.rs:1618` depend on it. This is useful for metric-registration coverage, but it should not be read as proof of protocol-path counter placement.

FYI: `inject_rx_chain_smoke` documents that one stage of the counter plan was later covered elsewhere. The comment at `crates/dpdk-net-core/tests/inject_rx_chain_smoke.rs:11` names `eth.rx_pkts`, and `crates/dpdk-net-core/tests/inject_rx_chain_smoke.rs:18` says that stage was satisfied by a separate frame-injection smoke test. I did not classify this as an A8 obs-gate violation because it is explicit test-plan accounting, not a hidden counter bump under an exact obs assertion.

## Memory-ordering / ARM-portability concerns
FYI: no atomics or explicit memory orderings were found in `crates/dpdk-net-core/src/test_server.rs`; the scoped file begins with data-only `ListenSlot` definitions at `crates/dpdk-net-core/src/test_server.rs:20` and packet helper functions at `crates/dpdk-net-core/src/test_server.rs:49`. Therefore the requested loopback-fast-path atomic review cannot be performed in that file.

FYI: the tcpreq harness serializes shared engine state through a process-wide mutex. The mutex is declared at `tools/tcpreq-runner/src/lib.rs:169`, acquired at `tools/tcpreq-runner/src/lib.rs:245`, and held in `TcpreqHarness` as `_guard` at `tools/tcpreq-runner/src/lib.rs:185`. Cleanup also clears pinned mbufs before engine drop at `tools/tcpreq-runner/src/lib.rs:222`. I did not find a scoped memory-ordering bug in tcpreq shared state.

FYI: coverage harness state follows the same pattern. `crates/dpdk-net-core/tests/common/mod.rs:440` declares `CovHarness`, `crates/dpdk-net-core/tests/common/mod.rs:455` stores the mutex guard, `crates/dpdk-net-core/tests/common/mod.rs:505` acquires the shared harness lock, and `crates/dpdk-net-core/tests/common/mod.rs:471` starts `Drop` cleanup that clears pinned mbufs before dropping the engine. This is a coarse-grained serialization scheme, not an atomics-dependent one.

FYI: relaxed atomics in scoped tests are used as counters or stop flags without publishing dependent data. For example, tcpreq urgent/RST-ACK probes load counters with `Ordering::Relaxed` at `tools/tcpreq-runner/src/probes/urgent.rs:104` and `tools/tcpreq-runner/src/probes/rst_ack.rs:65`, and benchmark stop flags store/load `Relaxed` at `crates/dpdk-net-core/tests/bench_alloc_hotpath.rs:142`, `crates/dpdk-net-core/tests/bench_alloc_hotpath.rs:417`, `crates/dpdk-net-core/tests/multiseg_retrans_tap.rs:96`, and `crates/dpdk-net-core/tests/multiseg_retrans_tap.rs:296`. I did not find a correctness dependency that would require Acquire/Release on ARM in these scoped uses.

## C-ABI / FFI
FYI: no `unsafe`, `extern`, `CStr`, or raw pointer manipulation was found in `tools/packetdrill-shim-runner` or `tools/tcpreq-runner` source during the scoped search. The tcpreq probes mutate Rust byte buffers, for example `tools/tcpreq-runner/src/probes/options.rs:407` constructs a spliced frame by copying slices, and `tools/tcpreq-runner/src/probes/checksum.rs:52` edits the TCP checksum bytes directly. The C/DPDK boundary is below these crates, not in their probe implementations.

FYI: mempool/mbuf cleanup for test-injected buffers is represented in the Rust harnesses I inspected. `tools/tcpreq-runner/src/lib.rs:222` starts harness drop cleanup, `tools/tcpreq-runner/src/lib.rs:228` clears `pinned_ext_mbufs` before the engine is dropped, and `crates/dpdk-net-core/tests/common/mod.rs:477` does the same for the coverage harness. The multi-segment drift test also asserts that draining retransmission frames does not change pool availability at `crates/dpdk-net-core/tests/multi_seg_chain_pool_drift.rs:391` and then verifies pool recovery after close at `crates/dpdk-net-core/tests/multi_seg_chain_pool_drift.rs:405`.

FYI: I did not find an unsafe-invariant bug in the scoped tool crates. The main FFI/cross-language risk remains the external packetdrill shim build noted above, because `tools/packetdrill-shim-runner/build.rs:22` delegates to a shell build and the Rust invoker at `tools/packetdrill-shim-runner/src/invoker.rs:39` treats the result as an opaque binary.

## Hidden coupling
SMELL: several test helpers assume fixed Ethernet + IPv4 + TCP frame layout. `tools/tcpreq-runner/src/lib.rs:34` defines `ETH_HDR_LEN`, `IPV4_HDR_LEN`, and `TCP_HDR_LEN` constants; `tools/tcpreq-runner/src/probes/options.rs:46` derives the TCP data offset from those constants; and `crates/dpdk-net-core/src/test_server.rs:226` assumes the IPv4 header starts at byte 14 when parsing sequence/ack values. That coupling is normal for the current generated frames, but it is not self-defending against malformed or non-Ethernet frames.

FYI: requested RefCell borrow-chain lock ordering was not found in `crates/dpdk-net-core/src/test_server.rs`. The scoped file has no `RefCell::borrow_mut` chain and no shared runtime state; it is limited to passive-listen metadata at `crates/dpdk-net-core/src/test_server.rs:20` and deterministic packet builders beginning at `crates/dpdk-net-core/src/test_server.rs:82`. I also did not find packetdrill runner shared mutable Rust state across concurrently executing scripts; `tools/packetdrill-shim-runner/src/invoker.rs:25` accepts one binary and one script per call.

FYI: tcpreq probe preconditions are tightly coupled to the deterministic harness ports and peer identity. The constants are declared at `tools/tcpreq-runner/src/lib.rs:161` through `tools/tcpreq-runner/src/lib.rs:167`, and individual probes restate expected values, for example `tools/tcpreq-runner/src/probes/mss.rs:18` through `tools/tcpreq-runner/src/probes/mss.rs:27`. I did not find arithmetic breakage from this coupling, but it is a mechanical assumption shared across all probes.

## Documentation drift
BUG: `tools/packetdrill-shim-runner/src/invoker.rs:18` through `tools/packetdrill-shim-runner/src/invoker.rs:20` document a hard wall timeout, but implementation ignores it at `tools/packetdrill-shim-runner/src/invoker.rs:30`. This is documentation drift with runtime effect, not just stale prose.

FYI: comments in the counter coverage file include line-number references that appear inherently fragile across phases. For example, `crates/dpdk-net-core/tests/counter-coverage.rs:921` documents an expected increment site by source line rather than symbol or behavior. I did not classify line-comment drift as a defect because the executable assertions are counter-name based, but the comments should not be treated as durable review evidence.

FYI: the top comment in `test_server.rs` still describes the initial passive-listen shim contract as rejecting additional SYNs with RST+ACK at `crates/dpdk-net-core/src/test_server.rs:7`. Current scoped tests include later duplicate-SYN behavior elsewhere in the test tree, so this header reads like phase-local historical context rather than the complete Stage 1 passive-open behavior. I did not mark it as a bug because it is a comment, but it can mislead future cross-phase reviewers.

## FYI / informational
Focus item 1, arithmetic edges: tcpreq sequence arithmetic uses `wrapping_add` in the reviewed probes, with examples at `tools/tcpreq-runner/src/probes/mss.rs:90`, `tools/tcpreq-runner/src/probes/options.rs:107`, `tools/tcpreq-runner/src/probes/urgent.rs:76`, and `tools/tcpreq-runner/src/probes/rst_ack.rs:194`. Packetdrill ns-to-engine-tick conversion was not found in scoped Rust; only direct virtual-time calls are visible at `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:75` and `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:80`. Port-allocation arithmetic was not found in scoped `test_server.rs`, whose helper constants begin at `crates/dpdk-net-core/src/test_server.rs:50`.

Focus item 2, atomic / memory ordering: no atomics were found in `crates/dpdk-net-core/src/test_server.rs:20` through the packet helper region at `crates/dpdk-net-core/src/test_server.rs:232`. Scoped harness state is serialized by mutex guards at `tools/tcpreq-runner/src/lib.rs:169` and `crates/dpdk-net-core/tests/common/mod.rs:505`.

Focus item 3, lock ordering: no `RefCell::borrow_mut` chains were found in `crates/dpdk-net-core/src/test_server.rs`; the file is data/helper only at `crates/dpdk-net-core/src/test_server.rs:20` and `crates/dpdk-net-core/src/test_server.rs:49`. Packetdrill runner execution is one subprocess per invoker call at `tools/packetdrill-shim-runner/src/invoker.rs:39` through `tools/packetdrill-shim-runner/src/invoker.rs:44`.

Focus item 4, mempool / mbuf leak edges: scoped harness drops clear pinned injected buffers before dropping the engine at `tools/tcpreq-runner/src/lib.rs:222`, `tools/tcpreq-runner/src/lib.rs:228`, `crates/dpdk-net-core/tests/common/mod.rs:471`, and `crates/dpdk-net-core/tests/common/mod.rs:477`. I did not find a confirmed leak in the scoped test-only injection paths.

Focus item 5, unsafe invariants: no unsafe or direct FFI was found in the scoped tool crates; the reviewed code around frame edits is safe Rust at `tools/tcpreq-runner/src/probes/options.rs:407` and `tools/tcpreq-runner/src/probes/checksum.rs:52`.

Focus item 6, error-path correctness: the confirmed error-path issue is packetdrill timeout handling in `tools/packetdrill-shim-runner/src/invoker.rs:25` through `tools/packetdrill-shim-runner/src/invoker.rs:50`. Harness drop cleanup for partially initialized pinned buffers is visible at `tools/tcpreq-runner/src/lib.rs:222` and `crates/dpdk-net-core/tests/common/mod.rs:471`.

Focus item 7, TCP-spec edges: sequence and PAWS property tests directly cover wrap/edge behavior at `crates/dpdk-net-core/tests/proptest_tcp_seq.rs:31`, `crates/dpdk-net-core/tests/proptest_tcp_seq.rs:69`, `crates/dpdk-net-core/tests/proptest_paws.rs:132`, and `crates/dpdk-net-core/tests/proptest_paws.rs:164`. No modular comparison bug was confirmed in tcpreq probe assertions.

Focus item 8, timer wheel: scoped timer evidence is timer firing/pump coverage at `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:75`, `tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs:80`, and `crates/dpdk-net-core/tests/counter-coverage.rs:1538`. I did not find scoped disconnect-mid-run cancel coverage.

Focus item 9, counter increment placement: A8 obs gate exact assertions are at `crates/dpdk-net-core/tests/obs_smoke.rs:160`, `crates/dpdk-net-core/tests/obs_smoke.rs:165`, `crates/dpdk-net-core/tests/obs_smoke.rs:288`, and `crates/dpdk-net-core/tests/obs_smoke.rs:295`, and I did not find an uncovered path bumping a counter assumed by those exact assertions. Broader counter registration tests using `bump_counter_one_shot` at `crates/dpdk-net-core/tests/common/mod.rs:592` remain coverage debt rather than obs-gate evidence.

## Verification trace
Static review was performed at HEAD `9c5f1155cf0573dc8e44fd9d4f89828ace119718` (`phase-a10-5-complete-30-g9c5f115`).

Commands and file reads performed:

- `git log --oneline phase-a-hw-plus-complete..phase-a7-complete`
- `git log --oneline phase-a7-complete..phase-a8-complete`
- `git log --oneline phase-a8-complete..a8.5-test-coverage-complete`
- `git rev-parse HEAD`
- `git describe --tags --always --dirty`
- `git status --short`
- `rg --files tools/packetdrill-shim-runner tools/tcpreq-runner crates/dpdk-net-core/src/test_server.rs crates/dpdk-net-core/tests`
- `rg -n "unsafe|Atomic|Ordering|borrow_mut|RefCell|mempool|mbuf|free|try_clone|refcnt|timer|Timer|shutdown|seq|SEQ|PAWS|tick|counter|inc_|lookup_counter|inject|FaultInjector|CStr|extern|dpdk_net" tools/packetdrill-shim-runner tools/tcpreq-runner crates/dpdk-net-core/src/test_server.rs crates/dpdk-net-core/tests`
- `rg -n "IllegalLength|illegal length|OptionSupport|UnknownOption|tcpreq|MUST-7|MUST-5|MUST-15" .`
- `rg -n "fetch_add|Ordering::|Atomic" crates/dpdk-net-core/src crates/dpdk-net-core/tests tools/tcpreq-runner tools/packetdrill-shim-runner`
- `nl -ba crates/dpdk-net-core/src/test_server.rs`
- `nl -ba tools/packetdrill-shim-runner/src/invoker.rs`
- `nl -ba tools/packetdrill-shim-runner/src/classifier.rs`
- `nl -ba tools/packetdrill-shim-runner/src/main.rs`
- `nl -ba tools/packetdrill-shim-runner/src/counts.rs`
- `nl -ba tools/packetdrill-shim-runner/src/bin/dry-run.rs`
- `nl -ba tools/packetdrill-shim-runner/build.rs`
- `nl -ba tools/packetdrill-shim-runner/tests/shim_virt_time_rto.rs`
- `nl -ba tools/packetdrill-shim-runner/tests/corpus_ligurio.rs`
- `nl -ba tools/packetdrill-shim-runner/tests/corpus_google.rs`
- `nl -ba tools/packetdrill-shim-runner/tests/corpus_shivansh.rs`
- `nl -ba tools/tcpreq-runner/src/lib.rs`
- `nl -ba tools/tcpreq-runner/src/probes/mss.rs`
- `nl -ba tools/tcpreq-runner/src/probes/options.rs`
- `nl -ba tools/tcpreq-runner/src/probes/reserved.rs`
- `nl -ba tools/tcpreq-runner/src/probes/urgent.rs`
- `nl -ba tools/tcpreq-runner/src/probes/checksum.rs`
- `nl -ba tools/tcpreq-runner/src/probes/rst_ack.rs`
- `nl -ba tools/tcpreq-runner/src/probes/mod.rs`
- `nl -ba tools/tcpreq-runner/tests/probe_mss.rs`
- `nl -ba tools/tcpreq-runner/tests/probe_options.rs`
- `nl -ba crates/dpdk-net-core/tests/common/mod.rs`
- `nl -ba crates/dpdk-net-core/tests/counter-coverage.rs`
- `nl -ba crates/dpdk-net-core/tests/obs_smoke.rs`
- `nl -ba crates/dpdk-net-core/tests/inject_rx_chain_smoke.rs`
- `nl -ba crates/dpdk-net-core/tests/inject_rx_frame_smoke.rs`
- `nl -ba crates/dpdk-net-core/tests/fault_injector_smoke.rs`
- `nl -ba crates/dpdk-net-core/tests/fault_injector_chain_uaf_smoke.rs`
- `nl -ba crates/dpdk-net-core/tests/multi_seg_chain_pool_drift.rs`
- `nl -ba crates/dpdk-net-core/tests/bench_alloc_hotpath.rs`
- `nl -ba crates/dpdk-net-core/tests/multiseg_retrans_tap.rs`
- `nl -ba crates/dpdk-net-core/tests/proptest_tcp_seq.rs`
- `nl -ba crates/dpdk-net-core/tests/proptest_paws.rs`

No runtime tests were executed; this was a static mechanical-defect review of the scoped files at HEAD.
