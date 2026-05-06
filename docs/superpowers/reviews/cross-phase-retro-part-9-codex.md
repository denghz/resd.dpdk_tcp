# Part 9 Cross-Phase Retro Review (Codex)
**Reviewer:** codex:codex-rescue
**Reviewed at:** 2026-05-05
**Part:** 9 — Layer H correctness gate
**Phases:** A10.5

## Verdict
BUG: Hold Layer H as a correctness gate until the wrapper/build and observation-path defects are fixed. The post-merge feature gate makes the binary require `test-server` (`tools/layer-h-correctness/Cargo.toml:16`, `tools/layer-h-correctness/Cargo.toml:22`), but both shipped wrappers still run only `cargo build --release --workspace` (`scripts/layer-h-smoke.sh:93`, `scripts/layer-h-nightly.sh:100`) and then expect `target/release/layer-h-correctness` to exist (`scripts/layer-h-smoke.sh:229`, `scripts/layer-h-nightly.sh:236`). On a clean tree this skips the binary; on a dirty/stale `target/`, it risks deploying an old binary.

BUG: The event/FSM part of the gate is not observing the event stream it claims to replay. Layer H runs `bench_e2e::workload::run_rtt_workload` for each batch (`tools/layer-h-correctness/src/workload.rs:127`), and that helper drains `engine.events()` while dropping non-readable events in its catch-all arm (`tools/bench-e2e/src/workload.rs:185`, `tools/bench-e2e/src/workload.rs:204`). Layer H then calls `observe_batch` afterward (`tools/layer-h-correctness/src/workload.rs:139`), where the FSM oracle expects to drain and record events (`tools/layer-h-correctness/src/observation.rs:660`, `tools/layer-h-correctness/src/observation.rs:668`). The result is a correctness gate that can miss `StateChange`, `TcpRetrans`, and other forensic events already consumed by the workload helper.

BUG: The corruption row is not correct under the offload-off software TCP-checksum path. The row requires either `eth.rx_drop_cksum_bad` or `ip.rx_csum_bad` to advance (`tools/layer-h-correctness/src/scenarios.rs:235`), but when RX checksum offload is off or inactive the IP decoder falls back to software verification (`crates/dpdk-net-core/src/l3_ip.rs:194`, `crates/dpdk-net-core/src/l3_ip.rs:221`) and TCP checksum failures are reported as `TcpParseError::Csum` (`crates/dpdk-net-core/src/tcp_input.rs:111`, `crates/dpdk-net-core/src/tcp_input.rs:133`), which increments `tcp.rx_bad_csum` (`crates/dpdk-net-core/src/engine.rs:4060`, `crates/dpdk-net-core/src/engine.rs:4068`). That counter is absent from the disjunction, so row 14 can false-fail when corruption lands in TCP bytes with offload disabled.

## Architectural drift
BUG: The gateway-ARP regression fix correctly made the Layer H crate non-default-feature gated (`tools/layer-h-correctness/src/lib.rs:17`, `tools/layer-h-correctness/Cargo.toml:39`), but the operational scripts were not updated to build the gated artifact. The crate documents the required standalone command (`tools/layer-h-correctness/Cargo.toml:34`, `tools/layer-h-correctness/Cargo.toml:35`), while the smoke and nightly wrappers still perform a workspace build only (`scripts/layer-h-smoke.sh:93`, `scripts/layer-h-nightly.sh:100`). This is drift between the post-8147404 build contract and the A10.5 runner contract.

SMELL: The scripts discover the peer data interface dynamically, then ignore that value and hard-code `ens6` for netem. The peer prep block selects `IFACE` from non-management links and configures that device (`scripts/layer-h-smoke.sh:268`, `scripts/layer-h-smoke.sh:274`, `scripts/layer-h-nightly.sh:275`, `scripts/layer-h-nightly.sh:281`), but cleanup and the binary invocation hard-code `ens6` (`scripts/layer-h-smoke.sh:298`, `scripts/layer-h-smoke.sh:311`, `scripts/layer-h-nightly.sh:326`, `scripts/layer-h-nightly.sh:341`). This is mechanically fragile if ENI naming changes on the AMI.

## Cross-phase invariant violations
BUG: The Layer H invariant "FSM remains Established throughout the assertion window" is weakened by the event-drain ordering. `observe_batch` checks `state_of` first (`tools/layer-h-correctness/src/observation.rs:645`, `tools/layer-h-correctness/src/observation.rs:646`) and then searches drained events for `Established -> non-Established` transitions (`tools/layer-h-correctness/src/observation.rs:661`, `tools/layer-h-correctness/src/observation.rs:663`). The batch workload drains and drops unrelated events before that replay (`tools/bench-e2e/src/workload.rs:185`, `tools/bench-e2e/src/workload.rs:204`), so a transient illegal transition can be absent from the failure bundle and oracle even though the gate was designed around event replay.

BUG: The row-14 offload-neutral corruption assertion violates its own offload-neutral intent. Offload-on L4 BAD can increment `eth.rx_drop_cksum_bad` (`crates/dpdk-net-core/src/engine.rs:4031`, `crates/dpdk-net-core/src/engine.rs:4041`), and IP BAD can increment `ip.rx_csum_bad` (`crates/dpdk-net-core/src/l3_ip.rs:213`, `crates/dpdk-net-core/src/l3_ip.rs:218`), but software TCP BAD increments `tcp.rx_bad_csum` (`crates/dpdk-net-core/src/engine.rs:4063`, `crates/dpdk-net-core/src/engine.rs:4068`). The Layer H matrix only permits the first two names (`tools/layer-h-correctness/src/scenarios.rs:235`, `tools/layer-h-correctness/src/scenarios.rs:236`).

## Tech debt accumulated
SMELL: `obs.events_dropped` is asserted twice for every Layer H scenario. The matrix rows include it directly, e.g. the first row (`tools/layer-h-correctness/src/scenarios.rs:57`, `tools/layer-h-correctness/src/scenarios.rs:60`), while the global side-check list also injects it (`tools/layer-h-correctness/src/counters_snapshot.rs:36`, `tools/layer-h-correctness/src/counters_snapshot.rs:37`). `run_one_scenario` evaluates scenario expectations and then global side-checks (`tools/layer-h-correctness/src/workload.rs:166`, `tools/layer-h-correctness/src/workload.rs:176`), so a single nonzero delta can produce duplicate `CounterRelation` failures. It does not change pass/fail, but it makes failure bundles noisier than the mechanical signal.

SMELL: `--duration-override` is converted directly with `Duration::from_secs` (`tools/layer-h-correctness/src/main.rs:104`, `tools/layer-h-correctness/src/main.rs:209`) and then added to `Instant::now()` with `+` (`tools/layer-h-correctness/src/workload.rs:121`, `tools/layer-h-correctness/src/workload.rs:122`). Normal 30-second runs are fine, but a huge CLI value can panic on instant overflow instead of returning a normal argument error. This is an arithmetic edge in the runner control plane, not in counter delta math.

## Test-pyramid concerns
BUG: The tests validate the gated binary when Cargo explicitly builds it, but they do not cover the wrapper build path that operators actually run. The integration test resolves `CARGO_BIN_EXE_layer-h-correctness` (`tools/layer-h-correctness/tests/external_netem_skips_apply.rs:8`, `tools/layer-h-correctness/tests/external_netem_skips_apply.rs:9`), which is only available in the feature-enabled test invocation; the wrappers instead run `cargo build --release --workspace` (`scripts/layer-h-smoke.sh:93`, `scripts/layer-h-nightly.sh:100`) and then check for a release binary after the fact (`scripts/layer-h-smoke.sh:233`, `scripts/layer-h-nightly.sh:240`). The missing case is a script-level test or dry-run that proves the exact release artifact is built after 8147404.

SMELL: The row-14 test pins only the current, incomplete disjunction. It asserts the corruption group contains `eth.rx_drop_cksum_bad` and `ip.rx_csum_bad` (`tools/layer-h-correctness/tests/scenario_parse.rs:119`, `tools/layer-h-correctness/tests/scenario_parse.rs:128`) but does not exercise the software TCP checksum path that increments `tcp.rx_bad_csum` (`crates/dpdk-net-core/src/engine.rs:4068`). That lets the offload-off false-fail described above survive the static matrix tests.

## Observability gaps
BUG: Failure bundles can be empty or misleading for event-driven failures because the batch workload drains the same queue before `observe_batch` snapshots it. `drain_and_accumulate_readable` pops every event (`tools/bench-e2e/src/workload.rs:185`, `tools/bench-e2e/src/workload.rs:187`) and drops non-matching/non-readable event kinds (`tools/bench-e2e/src/workload.rs:204`, `tools/bench-e2e/src/workload.rs:205`), while `observe_batch` is the only path that pushes events into the Layer H `EventRing` (`tools/layer-h-correctness/src/observation.rs:660`, `tools/layer-h-correctness/src/observation.rs:668`). Counter assertions may still fail, but the forensic event window is mechanically starved.

FYI: The PR #9 RX leak side-checks are diagnostic counters, not direct mbuf ownership proofs inside Layer H. `tcp.rx_mempool_avail` is a most-recent sample of `rte_mempool_avail_count(rx_mp)` (`crates/dpdk-net-core/src/counters.rs:278`, `crates/dpdk-net-core/src/counters.rs:284`) refreshed at most once per second in `poll_once` (`crates/dpdk-net-core/src/engine.rs:2502`, `crates/dpdk-net-core/src/engine.rs:2534`) and read by Layer H as a floor check (`tools/layer-h-correctness/src/observation.rs:679`, `tools/layer-h-correctness/src/observation.rs:683`). `tcp.mbuf_refcnt_drop_unexpected` is bumped from `MbufHandle::Drop` when pre-refcount is zero or post-decrement remains above threshold (`crates/dpdk-net-core/src/mempool.rs:281`, `crates/dpdk-net-core/src/mempool.rs:294`, `crates/dpdk-net-core/src/mempool.rs:305`). They can fire, but they are sampled/diagnostic signals rather than a complete live leak detector.

## Memory-ordering / ARM-portability concerns
FYI: No memory-ordering bug was observed in Layer H counter reads. Snapshot counters are independent `AtomicU64` telemetry loaded with `Ordering::Relaxed` (`tools/layer-h-correctness/src/counters_snapshot.rs:43`, `tools/layer-h-correctness/src/counters_snapshot.rs:46`), and live observation loads `rx_mempool_avail` plus `obs.events_dropped` with relaxed ordering (`tools/layer-h-correctness/src/observation.rs:681`, `tools/layer-h-correctness/src/observation.rs:691`). Those values are used as monotonic/level diagnostics, not as synchronization guards for shared data.

FYI: The engine increment sites reviewed also use relaxed atomic increments for telemetry counters (`crates/dpdk-net-core/src/counters.rs:804`, `crates/dpdk-net-core/src/counters.rs:810`). I did not find a Layer H path that depends on acquire/release visibility across data protected by those counters.

## C-ABI / FFI
FYI: Layer H does not allocate or construct its own mbufs in the reviewed crate. It opens a connection via the bench-e2e helper (`tools/layer-h-correctness/src/workload.rs:70`, `tools/layer-h-correctness/src/workload.rs:87`) and drives request/response batches through the engine (`tools/layer-h-correctness/src/workload.rs:104`, `tools/layer-h-correctness/src/workload.rs:127`). Mbuf lifetime coverage is therefore indirect through engine counters and `MbufHandle::Drop`, not a new Layer H ownership surface.

FYI: The only `unsafe` in the Layer H crate is in process/DPDK setup and cleanup: `rte_get_tsc_hz()` (`tools/layer-h-correctness/src/main.rs:194`) and `rte_eal_cleanup()` (`tools/layer-h-correctness/src/main.rs:365`, `tools/layer-h-correctness/src/main.rs:366`). I did not find unsafe code in the assertion, snapshot, scenario, report, or observation library modules.

## Hidden coupling
LIKELY-BUG: A stale parent-process `DPDK_NET_FAULT_INJECTOR` can contaminate pure-netem Layer H runs. Layer H only sets the variable when the selected scenarios contain an FI spec (`tools/layer-h-correctness/src/main.rs:185`, `tools/layer-h-correctness/src/main.rs:187`); it never clears it for pure-netem or smoke selections. The `test-server` feature always enables `dpdk-net-core/fault-injector` (`tools/layer-h-correctness/Cargo.toml:39`, `tools/layer-h-correctness/Cargo.toml:45`), and engine construction reads `FaultConfig::from_env()` once (`crates/dpdk-net-core/src/engine.rs:1553`, `crates/dpdk-net-core/src/engine.rs:1555`). Since `from_env` treats any nonempty inherited env var as active config (`crates/dpdk-net-core/src/fault_injector.rs:132`, `crates/dpdk-net-core/src/fault_injector.rs:140`), a shell-exported value can silently add FI to rows whose matrix says `fault_injector: None`.

SMELL: The report header exposes whether the binary was compiled with the fault-injector feature (`tools/layer-h-correctness/src/main.rs:403`, `tools/layer-h-correctness/src/main.rs:417`) and separately records only the selected matrix FI spec (`tools/layer-h-correctness/src/main.rs:419`). In the stale-env case above, the header can show `fi_spec = None` while the engine actually used an inherited FI env var. That couples report truth to external shell state not reflected in the selected matrix.

## Documentation drift
BUG: The crate-level post-gateway-ARP documentation says workspace builds without `test-server` skip Layer H and gives the standalone build command (`tools/layer-h-correctness/src/lib.rs:3`, `tools/layer-h-correctness/src/lib.rs:8`), but the operational scripts still document and execute workspace-only builds (`scripts/layer-h-smoke.sh:90`, `scripts/layer-h-smoke.sh:94`, `scripts/layer-h-nightly.sh:98`, `scripts/layer-h-nightly.sh:101`). This is not just prose drift; it is the same mechanical break as the Verdict build finding.

SMELL: The `EventRing` comments say the assertion window drains events into the failure bundle (`tools/layer-h-correctness/src/observation.rs:4`, `tools/layer-h-correctness/src/observation.rs:10`), but the actual batch workload drains the engine event queue before Layer H's observer (`tools/bench-e2e/src/workload.rs:185`, `tools/layer-h-correctness/src/workload.rs:139`). The documentation describes the intended design rather than the current control flow.

## FYI / informational
FYI: Counter delta arithmetic in Layer H is conservative. Snapshots store `u64` values (`tools/layer-h-correctness/src/counters_snapshot.rs:39`, `tools/layer-h-correctness/src/counters_snapshot.rs:41`), deltas are computed as `i128` post-minus-pre (`tools/layer-h-correctness/src/counters_snapshot.rs:71`, `tools/layer-h-correctness/src/counters_snapshot.rs:78`), and `<=N` checks compare through `u128` after rejecting negative deltas (`tools/layer-h-correctness/src/assertions.rs:61`, `tools/layer-h-correctness/src/assertions.rs:66`). I did not find wrapping/saturating counter-delta math in the assertion engine.

FYI: The disjunctive evaluator does not saturate or wrap deltas; it reuses the same `snapshot_delta` result type and stores `i128` observed deltas (`tools/layer-h-correctness/src/assertions.rs:173`, `tools/layer-h-correctness/src/assertions.rs:183`). It does, however, treat missing group members as zero (`tools/layer-h-correctness/src/assertions.rs:176`, `tools/layer-h-correctness/src/assertions.rs:182`); pre-flight and `select_counter_names` should prevent that in normal operation (`tools/layer-h-correctness/src/main.rs:321`, `tools/layer-h-correctness/src/main.rs:344`, `tools/layer-h-correctness/src/workload.rs:28`, `tools/layer-h-correctness/src/workload.rs:30`).

FYI: The netem reorder base-delay fix is present in both Layer H and bench-stress. Layer H uses `delay 5ms reorder 50% gap 3` (`tools/layer-h-correctness/src/scenarios.rs:208`, `tools/layer-h-correctness/src/scenarios.rs:211`), bench-stress uses the same base-delay requirement (`tools/bench-stress/src/scenarios.rs:87`, `tools/bench-stress/src/scenarios.rs:93`), and `scripts/bench-nightly.sh` mirrors that spec for operator-side netem (`scripts/bench-nightly.sh:542`, `scripts/bench-nightly.sh:545`).

FYI: The bench-stress p999 ratio arithmetic is straightforward floating-point division after guarding non-positive baselines (`tools/bench-stress/src/main.rs:333`, `tools/bench-stress/src/main.rs:342`). I did not find loss/jitter percentage arithmetic in Layer H; its netem matrix uses literal `tc netem` specs (`tools/layer-h-correctness/src/scenarios.rs:127`, `tools/layer-h-correctness/src/scenarios.rs:167`, `tools/layer-h-correctness/src/scenarios.rs:227`).

## Verification trace
- `git status --short`
- `git log --oneline phase-a10-deferred-fixed..phase-a10-5-complete`
- `git log --oneline phase-a10-5-complete..HEAD`
- `git show --stat --name-only --oneline ce6fc24`
- `git show --stat --name-only --oneline 9d65e37`
- `git show --stat --name-only --oneline 8147404`
- `rg --files tools/layer-h-correctness scripts docs/superpowers | sort`
- `rg -n "unsafe|Atomic|Ordering|RefCell|borrow_mut|wrapping|saturating|overflow|score|loss|jitter|timer|mbuf|mempool|corrupt|checksum|counter|assert|gateway|ARP|arp|offload|netem" tools/layer-h-correctness scripts/layer-h-smoke.sh scripts/layer-h-nightly.sh scripts/bench-nightly.sh scripts/bench-nightly.md scripts -g '!*cross-phase-retro*'`
- `nl -ba tools/layer-h-correctness/src/assertions.rs`
- `nl -ba tools/layer-h-correctness/src/counters_snapshot.rs`
- `nl -ba tools/layer-h-correctness/src/observation.rs`
- `nl -ba tools/layer-h-correctness/src/scenarios.rs`
- `nl -ba tools/layer-h-correctness/src/workload.rs`
- `nl -ba tools/layer-h-correctness/src/main.rs`
- `nl -ba tools/layer-h-correctness/src/report.rs`
- `nl -ba tools/layer-h-correctness/src/lib.rs && nl -ba tools/layer-h-correctness/Cargo.toml`
- `rg -n "rx_mempool_avail|mbuf_refcnt_drop_unexpected|events_dropped|rx_drop_cksum_bad|ip\\.rx_csum_bad|tx_retrans|tx_rto|tx_tlp|rx_dup_ack|fault_injector\\.(drops|dups|reorders)|fault_injector" crates/dpdk-net-core tools/bench-stress tools/bench-e2e -g '*.rs'`
- `rg -n "RefCell|borrow_mut|unsafe|Ordering|Atomic" tools/layer-h-correctness crates/dpdk-net-core/src -g '*.rs'`
- `rg -n "score|scor|rate|percent|pct|loss|jitter|delay|reorder|duplicate|corrupt|ceil|floor|round|saturating|wrapping" tools/layer-h-correctness tools/bench-stress/src scripts/bench-nightly.sh scripts/layer-h-smoke.sh scripts/layer-h-nightly.sh -g '*.rs' -g '*.sh'`
- `nl -ba crates/dpdk-net-core/src/counters.rs | sed -n '270,340p'`
- `nl -ba crates/dpdk-net-core/src/counters.rs | sed -n '430,740p'`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '880,925p;2498,2540p;3000,3035p;3198,3215p;3360,3376p;3428,3443p;3678,3705p;3884,3935p;4018,4050p;6224,6238p;6654,6678p;6874,6895p'`
- `nl -ba crates/dpdk-net-core/src/mempool.rs | sed -n '260,315p'`
- `nl -ba scripts/layer-h-smoke.sh`
- `nl -ba scripts/layer-h-nightly.sh`
- `nl -ba scripts/bench-nightly.sh | sed -n '520,590p'`
- `nl -ba tools/bench-stress/src/netem.rs && nl -ba tools/bench-stress/src/scenarios.rs`
- `cargo metadata --no-deps --format-version 1`
- `find target/release -maxdepth 1 -type f -name 'layer-h-correctness' -ls 2>/dev/null || true`
- `rg -n "layer-h-correctness|required-features|test-server|cargo build --release --workspace|cargo build -p layer-h-correctness" Cargo.toml tools/layer-h-correctness scripts/layer-h-smoke.sh scripts/layer-h-nightly.sh scripts/bench-nightly.sh`
- `nl -ba crates/dpdk-net-core/src/fault_injector.rs | sed -n '1,120p;180,350p'`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '1548,1562p;3718,3732p'`
- `rg -n "DPDK_NET_FAULT_INJECTOR|FaultConfig::from_env|fault-injector" tools/bench-stress/src tools/layer-h-correctness/src crates/dpdk-net-core/src/fault_injector.rs crates/dpdk-net-core/src/engine.rs scripts/layer-h-nightly.sh scripts/layer-h-smoke.sh`
- `nl -ba tools/bench-stress/src/main.rs | sed -n '186,204p;426,450p'`
- `nl -ba tools/layer-h-correctness/tests/scenario_parse.rs`
- `nl -ba tools/layer-h-correctness/tests/external_netem_skips_apply.rs`
- `rg -n "pub fn run_rtt_workload|fn run_rtt_workload|pump|timer|poll_once|Instant|rdtsc|sleep" tools/bench-e2e/src -g '*.rs'`
- `nl -ba tools/bench-e2e/src/workload.rs | sed -n '1,220p'`
- `rg -n "rx_bad_csum|CsumBad|tcp checksum|checksum" crates/dpdk-net-core/src/tcp_input.rs crates/dpdk-net-core/src/engine.rs crates/dpdk-net-core/src/l3_ip.rs -g '*.rs'`
- `nl -ba crates/dpdk-net-core/src/tcp_input.rs | sed -n '80,150p;380,460p;1450,1490p'`
- `nl -ba crates/dpdk-net-core/src/engine.rs | sed -n '4048,4098p'`
- `nl -ba crates/dpdk-net-core/src/l3_ip.rs | sed -n '188,224p'`
- `nl -ba tools/bench-e2e/src/workload.rs | sed -n '220,330p'`
- `nl -ba crates/dpdk-net-core/src/tcp_events.rs | sed -n '140,180p'`
- `cargo test -p layer-h-correctness --features test-server --no-run`
- `cargo test -p layer-h-correctness --features test-server`
