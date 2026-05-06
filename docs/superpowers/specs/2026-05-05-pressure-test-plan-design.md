# Pressure-Test Plan — Design

**Status:** FINAL — codex review incorporated, ready for implementation
**Author:** general-purpose subagent (opus 4.7), reconciled with codex:codex-rescue second opinion
**Created:** 2026-05-05
**Finalized:** 2026-05-05
**Trigger:** user observation that "many bugs surface in max-throughput bench" + cross-phase retro Pattern P9 (test-pyramid middle-layer gap) + Part 8 BLOCK-A11 #7 (missing pressure-correctness layer) + Part 9 S2-4 (Layer H covers netem only)
**Linked implementation plan:** `docs/superpowers/plans/2026-05-05-pressure-test-plan.md`

---

## Background

Stage 1 closed nine phases (A1 → A10.5) and produced a workspace with 425 unit tests, 116 integration tests, 31 fuzz/proptest cases, 29 sanitizer/special-build tests, plus 3 large benchmark drivers (bench-stress, bench-vs-mtcp, layer-h-correctness). On paper this is dense coverage. In practice three latent classes have repeatedly shipped to the AWS bench fleet undetected by `cargo test`:

1. **TX-side resource sizing under sustained throughput.** T17's three bugs (TX data-mempool divisor wrong, conn-handle leak in maxtp, K=1 MiB per-write stall) all manifested only at 60 s × 64 conns × ≥1 KiB write sizes. None were reproducible with the existing 100k-iter `long_soak_stability.rs` because that test runs **one** connection at PAYLOAD=128 B; it stresses leak-balance over time, not per-instant resource pressure across many flows.
2. **Counter-placement double-bumps and omissions.** The NIC-BAD `ip.rx_csum_bad` double-bump (Part 1 BLOCK-A11 #3) survived A-HW → A6.7 because the test pyramid's "real-path NIC-BAD" tier doesn't exist; counter-coverage uses `bump_counter_one_shot` for addressability, not real-path drive.
3. **Class-of-bug invisibility under reorder-storm + ENOMEM intersections.** Layer-H-correctness asserts FSM-stays-Established + counter expectations under netem stress, but at 1-conn × 30 s × 128 B per scenario; this never approaches the rx_mempool floor or the TLP/RACK void-retransmit accounting bug (Part 3 STAGE-2 S1, S2).

The cross-phase retro names the structural cause: **the test pyramid has unit-level (`cargo test`) and benchmark-level (`bench-vs-mtcp` AWS sweep) but no pressure-correctness middle layer.** Per-phase reviewers see "tests added for this phase" and accept synthetic counter-bumps as coverage; they cannot see "this counter is incremented twice along the offload-on path and once along the offload-off path" because that requires reading the *intersection* of multiple phases' code under sustained adversity.

This plan defines that middle layer. The goal is not to replace `cargo test` (fast, deterministic, dev-laptop) or AWS bench (real ENA hardware, end-to-end performance). It is to introduce a third tier: **deterministic pressure-correctness** that catches counter, mempool, and lifetime invariants under workloads that approximate AWS bench shape but run in-process or on a single host so they fit per-PR CI. The driver is correctness assertions, not performance numbers.

---

## Current test surface inventory

| Layer | Suite / Tool | Purpose | Pressure-correctness coverage |
|-------|--------------|---------|------------------------------|
| Unit (default cargo test) | `crates/dpdk-net-core/src/*` mod tests (425) | Module-level addressability + arithmetic | None — single-call paths only |
| Unit (counter coverage) | `tests/counter-coverage.rs`, `tests/knob-coverage.rs` | `bump_counter_one_shot` addressability + ALL_COUNTER_NAMES sync | None — synthetic bumps, no real-path drive |
| Unit (offload knobs) | `tests/ahw_smoke.rs`, `ahw_smoke_ena_hw.rs` | A-HW runtime latch behavior on net_tap / real ENA | None |
| Integration (TAP-gated) | `tests/tcp_basic_tap.rs`, `tcp_options_paws_reassembly_sack_tap.rs`, `tcp_rack_rto_retrans_tap.rs`, `multiseg_retrans_tap.rs`, `inject_rx_chain_smoke.rs` | RFC-feature behavior on net_tap; `DPDK_NET_TEST_TAP=1` gated | Limited — single-conn, short-run, no churn |
| Integration (test-server) | `tests/test_server_*.rs` | passive-open/close FSM trajectories | None — single trajectory each |
| Soak (TAP + sudo) | `tests/long_soak_stability.rs` | 100k-iter single-conn RTT echo, all 5 leak invariants | **Partial — 1 conn, 128 B PAYLOAD, no churn, no offload matrix** |
| Soak (cycle) | `tests/connect_close_cycle.rs`, `engine_restart_cycle_stable.rs` | Conn-open/close churn leak detection | Partial — single-trajectory churn, no concurrent load |
| Soak (netem) | `tests/rx_mempool_no_leak_ooo_netem.rs` | OOO + netem mempool drift | Partial — narrow netem profile, single conn |
| Property | `tests/proptest_paws.rs`, `proptest_rack_xmit_ts.rs`, `proptest_tcp_options.rs`, `proptest_tcp_reassembly.rs`, `proptest_tcp_sack.rs`, `proptest_tcp_seq.rs` | Per-RFC-rule small-input fuzz | None — pure logic, no engine integration |
| Fuzz (libFuzzer) | `crates/dpdk-net-core/fuzz/fuzz_targets/*` (engine_inject, header_parser, tcp_options, tcp_reassembly, tcp_sack, tcp_seq, tcp_state_fsm) | Crash/panic-only; `engine_inject` TAP-gated no-op without env | None — no counter or mempool oracle |
| Adversarial integration | `tools/scapy-fuzz-runner` + `tools/tcpreq-runner` + `tools/packetdrill-shim` | RFC compliance via external test corpora | None — pass/fail only, no sustained pressure |
| Layer H (correctness gate) | `tools/layer-h-correctness` 17 scenarios × 30 s × 1 conn | netem-stress correctness; FSM-stays-Established + counter expectations | **Partial — netem only, single conn, single payload size, smoke=5 rows; explicitly does NOT cover max-throughput per Part 9 S2-4** |
| Bench (stress) | `tools/bench-stress` netem + FaultInjector p999 ratio | Throughput-under-netem with informational ratio | None — limits informational, not enforced (Part 8 S2-14) |
| Bench (max-tp) | `tools/bench-vs-mtcp` maxtp 28 buckets × 60 s warmup | AWS-fleet sustained throughput vs comparators | **THIS IS WHERE BUGS SURFACE.** No counter/mempool/lifetime oracle; only bytes/sec output |
| Bench (burst) | `tools/bench-vs-mtcp` burst grid | First-byte latency comparator | None |
| Bench (RX zero-copy) | `tools/bench-rx-zero-copy` | Microbench delivery cycle | None |
| Bench (offload A/B) | `tools/bench-offload-ab` | Compile-time offload matrix | None — runtime latch never measured (Part 5 S-3) |

**Summary of gaps as classified by purpose:**

- **Addressability:** strong (counter-coverage, knob-coverage)
- **Unit-correctness:** strong on isolated logic; weak when crossing engine boundaries
- **Integration:** uneven — TAP-gated suites cover RFC features but at smoke intensity
- **Property/fuzz:** pure-logic only; no engine-integrated property suite
- **Pressure-correctness:** **MISSING** — no suite drives sustained workload + asserts counter/mempool invariants
- **Benchmark:** strong on numbers, weak on correctness oracles around the numbers

---

## Pressure-test categories needed

Twelve categories grouped into four tiers. Tier-1 must land in A11; Tier-2 should land before Stage-2 multi-thread; Tier-3 is forensic/optional; Tier-4 is infrastructure.

### Tier 1 — Block A11 (high-bug-yield, addresses recent regressions)

1. **Sustained max-throughput correctness.** Drives the maxtp W × C grid (or a reduced subset) but asserts mempool drift, counter monotonicity, and `mbuf_refcnt_drop_unexpected == 0` instead of bytes/sec. Catches T17-class bugs (TX-mempool divisor, conn-handle leak, send-stall) at PR time.
2. **Concurrent-connection churn.** N=64 → 256 conns opening, transferring, closing on staggered schedules. Catches `conn_table_full` accounting, time-wait reaper races, slot-recycle regressions invisible to the single-conn `connect_close_cycle.rs`.
3. **Real-path counter parity matrix (offload on/off).** Replaces synthetic `bump_counter_one_shot` for the offload-relevant counter set (`ip.rx_csum_bad`, `tcp.rx_bad_csum`, `eth.rx_drop_cksum_bad`, `eth.offload_missing_*`). Drives a corruption stream through the engine and asserts each counter is incremented exactly once per drop along both paths.

### Tier 2 — Pre-Stage-2 (enables multi-thread refactor with confidence)

4. **Large-windows-with-loss correctness.** RACK / RTO / TLP under sustained 0.5–3% loss with realistic BDPs. Catches TLP/RACK void-retransmit accounting bugs (Part 3 S1, S2) that current TAP smoke tests cannot exercise.
5. **Slow-receiver back-pressure.** Engine sustains throughput with a peer that drains at 10–50% of line rate. Catches send-buffer accounting drift, `tcp.send_buf_full` semantics, zero-window oscillation.
6. **Mempool-near-exhaustion.** Drive workload that intentionally stresses the rx-mempool to <10% available. Asserts engine remains live, `eth.rx_drop_nomem` rises monotonically, no silent corruption, and recovers when pressure relaxes.
7. **Reassembly-hole-list saturation.** Sustained reorder + holes to push the reassembly queue to its hard cap (32 holes per spec). Asserts new arrivals beyond cap are accounted in `tcp.rx_drop_reassembly_overflow` (or the chosen drop counter), no UAF, no memory growth past steady state.
8. **SACK-block exhaustion.** Drive holes to fill SACK option space. Asserts SACK serialization caps at 4 blocks, no panic on overflow, oldest-block-evicted policy holds.

### Tier 3 — Forensic / nightly-only

9. **Timer-storm.** Many concurrent timers firing in <1 ms (e.g. RTO storm on 256 conns simultaneously). Catches timer-wheel slot growth regressions invisible at single-conn (Part 4 S2-7-class) and the slot-recycle invariants from `long_soak_stability`.
10. **FIN/RST flood.** Connection-tear-down under flood from peer (RST every 100 µs to random ports). Asserts `flow_table` accounting holds, no leak past time-wait reaping.
11. **Recv-buf saturation.** Engine receives at line rate while application drains slowly. Asserts back-pressure surfaces as counter (`tcp.recv_buf_drops`) per spec rule "don't throttle peer; surface pressure" (user feedback `feedback_performance_first_flow_control.md`).
12. **PMTU-blackhole / ICMP-frag-needed.** Peer drops large frames + sends frag-needed ICMP; asserts engine adapts MSS via `ip.pmtud_updates` and resumes flow without RST.

### Tier 4 — Infrastructure (blocks all of the above if missing)

(See "Infrastructure gaps" section.)

---

## Per-suite plan

### Suite 1 — `pressure-max-throughput` (Tier 1, BLOCK-A11)

**Hosting crate:** new — `crates/dpdk-net-core/tests/pressure_max_throughput.rs` (TAP-gated). Drive logic colocated with engine because the counter snapshots must touch private `AtomicU32` level fields and cannot be reached from `tools/`.

**Naming caveat:** This is paired-engine / TAP pressure-correctness. It does NOT exercise ENA descriptor cleanup, hardware offload stamping, RSS hash plumbing, or PMD completion behavior — those bits in `dispatch_one_real_mbuf` (`crates/dpdk-net-core/src/engine.rs:3757-3790`) only fire on a real PMD. ENA-realistic pressure remains in nightly AWS bench-vs-mtcp.

**Workload shape (two-tier matrix, accepted from codex critique #1):**

Per-PR smoke (must hold T17 mempool-divisor + conn-handle-leak signal):
- Single bucket: `N=16 conns, W=16 KiB, duration=10s` (≤ 25 s wall budget incl. EAL/TAP setup).
- Peer: in-host kernel echo on TAP (matches `long_soak_stability.rs` shape).
- Send pacing: tight loop, no `thread::sleep`.

Nightly (must hold the full T17 K=1 MiB per-write stall):
- `N ∈ {1, 16, 64}, W ∈ {64 B, 1 KiB, 16 KiB, 1 MiB}, duration = 60 s` (12 buckets × 70 s incl. warmup ≈ 14 min).
- Plus offload-OFF arm of the same 12 buckets (parallel-host, separate CI runner).
- The `N=64, W=1 MiB, 60 s` bucket is the explicit T17 K=1MiB-stall regression detector.

**AWS-fleet:** unchanged (existing maxtp 28-bucket × 60 s on real ENA).

**Counters / events to assert:**

| Counter | Relation | Rationale |
|---------|----------|-----------|
| `tcp.tx_data_mempool_avail` | `min_avail >= tx_data_mempool_size − (N × ceil(send_buffer_bytes / peer_mss) + 2 × tx_ring_size + retrans_headroom)` during bucket; post-close recovery to within `drift ≤ 32` of baseline | Capacity-formula form (codex #2); `5%` floor was wrong because consumption is per-MSS-segment, not capacity-fraction |
| `tcp.rx_mempool_avail` | post-close recovery to within `drift ≤ 32` of baseline (sampled once/sec) | Slow-path counter sampled at most once/sec (`engine.rs:2502-2534`); "continuously" was misleading |
| `eth.tx_drop_nomem` | `delta == 0` end-of-bucket | **Hard real-time tripwire** — replaces fixed-percentage availability floor as the regression signal |
| `tcp.mbuf_refcnt_drop_unexpected` | `delta == 0` end-of-bucket | `long_soak` final gate, intensity-independent (`mempool.rs:281-307`) |
| `obs.events_dropped` | `delta == 0` end-of-bucket | Soft-cap holds; **must be checked in every pressure suite** because event-queue overflow is a separate failure channel from mbuf refcnt (`tcp_events.rs:153-171`) |
| `eth.tx_drop_full_ring` | `delta / eth.tx_pkts_delta < 0.001` | Catches TX-ring sizing regressions (Part 1 S-17) |
| `tcp.send_buf_full` | informational; logged at end of bucket | T17 K=1 MiB stall surfaces here under nightly `W=1 MiB` bucket |
| `tcp.tx_payload_bytes` | `>= 0.95 × workload-emitted` (requires `obs-byte-counters` feature) | Cross-stack accounting parity (Part 8 S2-15) |
| Diag: `Engine::diag_input_drops` snapshot | `0 across all categories` | Catches T21 input-drop regressions |
| Conn-handle leak | per-bucket: `engine.flow_table().active_conns()` returns to baseline `± 0` post-close + drain | T17 conn-handle leak class |
| Timer-wheel slots | `slots_at_end - slots_at_warmup ≤ 64` | Existing `long_soak` invariant under multi-conn |

Note on level-counter access: `tcp.tx_data_mempool_avail` and `tcp.rx_mempool_avail` are `AtomicU32` level fields and are NOT reachable through the generic `lookup_counter` table (`counters.rs:291-304, 546-551`). The pressure helper module (I5) MUST add a typed `read_level_counter_u32(name)` accessor or read the `AtomicU32` fields directly; the Layer H scenario `Snapshot` path is delta-counter-only and cannot be reused as-is.

**Pass criteria:**
- All counter relations hold per-bucket (no asserts on bytes/sec; this is correctness, not perf).
- Bucket failure dumps full counter snapshot + last 1024 InternalEvents + `EngineConfig` + diag to `target/pressure-test/<bucket>/<timestamp>/` for forensics (I3).

**Cadence:**
- **Per-PR `cargo test`:** the single `N=16, W=16KiB, 10s` bucket only. Invocation: `cargo test -p dpdk-net-core --test 'pressure_*' --features pressure-test` (package-scoped; codex #4).
- **Nightly:** full 12-bucket sweep × {offload-ON, offload-OFF} (≈ 28 min total).
- **AWS-fleet:** unchanged.

**Phase gating:** new phase **A11.1** — first deliverable.

---

### Suite 2 — `pressure-conn-churn` (Tier 1, BLOCK-A11)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_conn_churn.rs`.

**Workload shape:**
- 256 concurrent conns; each runs open → 5×128 B RTT → close on a staggered 200 ms schedule.
- 30 s sustained churn. Per-PR subset: `N=64, 10 s` window.
- The previously stated `≥ 1000 conn open/close / sec` is now a **measured target**, NOT a pass precondition (codex #1: 256 conns × 200 ms stagger ceilings at ≈ 1.28 k/s and TAP/kernel adds variability). The pass test is the counter parity below; the rate is logged.

**Counters / events to assert:**
- `tcp.conn_open` delta == `tcp.conn_close` delta (exact arithmetic, after end-of-test drain settles).
- `tcp.conn_table_full == 0` (sized correctly for workload; existing assertion in `tests/counter-coverage.rs:801-816` for addressability).
- `tcp.conn_time_wait_reaped` rises monotonically, no leak past `tcp_msl_ms × 4` post-drain.
- `tcp.mbuf_refcnt_drop_unexpected == 0`, all four mempool drifts ≤ 32 (matches `long_soak`).
- `obs.events_dropped == 0` (event-queue side check — required for every pressure suite per codex #2).
- `tcp.tx_rst` delta == 0 (no spurious RSTs under clean churn).
- Timer-wheel `slots_len()` post-warmup growth ≤ 64.
- Logged but informational: actual measured `conn-open-per-sec` rate.

**Pass criteria:** counter parity + leak invariants. No throughput threshold; the rate is observed not asserted.

**Cadence:**
- **Per-PR:** N=64, 10 s window (~12 s wall, package-scoped invocation).
- **Nightly:** N=256, 30 s window.

**Phase gating:** A11.1 (parallel with Suite 1).

---

### Suite 3 — `pressure-counter-parity-offload-matrix` (Tier 1, BLOCK-A11)

**Hosting crate:** create `crates/dpdk-net-core/tests/pressure_counter_parity.rs` (keeps `counter-coverage.rs` focused on addressability). Requires `--features pressure-test,test-inject` plus a new `inject_rx_frame_with_ol_flags` accessor (codex NIC-BAD-injection-feasibility item: shim already supports OR-ing flags via `crates/dpdk-net-sys/shim.c:170-188` + `wrapper.h:80-86`, but neither `inject_rx_frame` nor `inject_rx_chain` calls it today — `engine.rs:6295-6367, 6388-6497`).

**Workload shape (4 distinct rows; was conflated):**
- Row A — offload-ON, IP NIC-BAD: synthesize valid Ethernet frame, set `RTE_MBUF_F_RX_IP_CKSUM_BAD` via `inject_rx_frame_with_ol_flags`.
- Row B — offload-ON, L4 NIC-BAD: synthesize valid Ethernet+IP, set `RTE_MBUF_F_RX_L4_CKSUM_BAD`.
- Row C — offload-OFF, software IP-cksum bad: synthesize Ethernet+IP with corrupted IP header checksum (no `ol_flags`).
- Row D — offload-OFF, software TCP-cksum bad: synthesize Ethernet+IP+TCP with corrupted TCP checksum (no `ol_flags`).
- Each row injects 1000 frames into a single ESTABLISHED conn; deterministic; ≤ 5 s wall per row.

**Counters / events to assert (per row, expected after fix):**

| Row | Path | `eth.rx_drop_cksum_bad` | `ip.rx_csum_bad` | `tcp.rx_bad_csum` | Notes |
|-----|------|------------------------|------------------|------------------|-------|
| A | offload-ON, IP NIC-BAD | `+= 1` | `+= 1` | `+= 0` | Single bump per layer, post-fix |
| B | offload-ON, L4 NIC-BAD | `+= 1` | `+= 0` | `+= 1` | TCP path drops in `tcp_input` (`engine.rs:4034-4047`) |
| C | offload-OFF, IP-cksum bad | `+= 0` | `+= 1` | `+= 0` | Pure software validation |
| D | offload-OFF, TCP-cksum bad | `+= 0` | `+= 0` | `+= 1` | Pure software validation |

**Pre-existing bug surfaced (codex #2):** Row A currently shows `ip.rx_csum_bad += 2` because `l3_ip.rs:213-220` bumps it once on `CksumOutcome::Bad` AND `engine.rs:3928-3931` bumps it again on `L3Drop::CsumBad`. The corrected expectation above (`+= 1`) means **Suite 3 Row A will FAIL on the current master HEAD until the double-bump is fixed**. This is intentional: per Pattern P4, "the test that catches the bug must fail on the buggy code, not pass with adjusted expectations". See "Reconciliation notes" item #2 for operator decision on whether to land the fix and Suite 3 in the same A11.1 PR or as two sequential PRs.

The "exactly one site bumps the counter" invariant from Pattern P4 is asserted as **per-frame counter-delta == 1 on each layer** that legitimately classifies the drop, and `== 0` on every other layer.

**Pass criteria:** absolute equality on every counter delta after the IP NIC-BAD double-bump fix is in tree. `obs.events_dropped == 0` and `tcp.mbuf_refcnt_drop_unexpected == 0` checked as side gates.

**Cadence:** per-PR `cargo test`, ≤ 25 s wall total. Invocation: `cargo test -p dpdk-net-core --test pressure_counter_parity --features pressure-test,test-inject`.

**Phase gating:** A11.1 (parallel with Suites 1 + 2 once shared harness ships).

---

### Suite 4 — `pressure-loss-recovery` (Tier 2)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_loss_recovery.rs` (TAP-gated, behind `pressure-test`). NOT extended into `tools/layer-h-correctness`: Part 9 explicitly scopes Layer H as netem-only single-conn smoke (`docs/superpowers/reviews/cross-phase-retro-part-9-synthesis.md:41-45`); pressure suites stay in `dpdk-net-core/tests/` until Stage 2's `dpdk-net-test-support` extraction (Pattern P2) lands and a sibling runner crate becomes safe (codex #3).

**Workload shape:**
- Baseline: N=4 conns × 16 KiB sustained writes × 60 s.
- netem profile sweep: {0.5%, 1%, 3%} loss × {0, 5 ms} delay × {0, gap=3} reorder → 12 buckets.
- **Plus one ENOMEM bucket (codex missing-category):** N=4, 1% loss + reorder gap=3, with `EngineConfig.tx_data_mempool_size_override` set small enough to force occasional retransmit ENOMEM, 60 s. This is where the void-retransmit accounting class (Part 3 S1, S2) actually surfaces.

**Counters / events to assert:**
- `tcp.tx_retrans > 0` (loss recovery fired).
- `tcp.tx_rto + tcp.tx_tlp > 0` (RACK/RTO/TLP path engaged).
- Per-event consistency: every `InternalEvent::TcpRetrans` ⇒ `tcp.tx_retrans` delta ≥ 1 — **the void-retransmit oracle**: under the ENOMEM bucket, an event that fires but is silently dropped because no mbuf could be allocated MUST not bump `tcp.tx_retrans` (Part 3 S1, S2 was previously bumping the counter optimistically before the actual TX completed).
- ENOMEM bucket: `eth.tx_drop_nomem > 0` (we did force the regime); `tcp.tx_retrans` increase still matches the count of *successful* retransmits, not the count of *attempted*.
- `tcp.tx_payload_bytes` matches application-level sent-bytes within 5% (requires `obs-byte-counters`).
- `obs.events_dropped == 0`, `tcp.mbuf_refcnt_drop_unexpected == 0`.
- No `tcp.tx_rst` (engine doesn't give up under recoverable loss).

**Pass criteria:** event-vs-counter parity + recovery counter > 0 + ENOMEM bucket holds the parity invariant under forced allocation failure.

**Cadence:** nightly only (13 buckets × 60 s = ~14 min).

**Phase gating:** A11.2.

---

### Suite 5 — `pressure-slow-receiver` (Tier 2)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_slow_receiver.rs` (TAP-gated; the kernel peer's pacing is the slow-drain mechanism).

**Workload shape:**
- 1 conn, 64 KiB writes in tight loop; peer drains at 10 MB/s (paced by `thread::sleep` in echo loop).
- 30 s window.

**Counters / events to assert:**
- `tcp.send_buf_full > 0` (back-pressure surfaces).
- `tcp.tx_zero_window` and `tcp.rx_zero_window` consistent (peer's zero-window probes seen).
- `tcp.tx_window_update > 0` once peer drains.
- No `tcp.tx_rst`, no `tcp.conn_close` mid-run.
- `tcp.tx_payload_bytes` reflects what peer actually drained (not what app asked to send).

**Pass criteria:** back-pressure visible via counters; no engine misbehavior.

**Cadence:** **nightly** (codex #5: ≤35 s wall budget is unmeasured; promote to per-PR only after CI wall-time data exists).

**Phase gating:** A11.2.

---

### Suite 6 — `pressure-mempool-exhaustion` (Tier 2)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_mempool_exhaustion.rs`. Requires a constructor knob `EngineConfig.rx_mempool_size_override` (small value, e.g. 256 mbufs) to force the exhaustion regime — currently exists as `rx_mempool_size`.

**Workload shape:**
- Small RX mempool (256 mbufs).
- Peer floods at line rate; engine-side application drains slowly (1 read/10 ms).
- 30 s window.

**Counters / events to assert:**
- `eth.rx_drop_nomem` rises monotonically under pressure.
- `tcp.rx_mempool_avail` reports values > 0 always (no negative wrap; no underflow).
- After pressure relaxes (peer pauses 5 s mid-test), `rx_mempool_avail` recovers to within 10% of baseline.
- `tcp.mbuf_refcnt_drop_unexpected == 0` (no UAF / double-free under pressure).
- Engine continues responding — `tcp.rx_pkts > 0` for the post-pause window.

**Pass criteria:** monotone drops + recovery + no UAF.

**Cadence:** **nightly** (codex #5: per-PR cadence requires measured CI wall time first).

**Phase gating:** A11.2.

---

### Suite 7 — `pressure-reassembly-saturation` (Tier 2)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_reassembly_saturation.rs` (uses `inject_rx_frame` directly — no TAP needed).

**Workload shape:**
- Single conn in ESTABLISHED.
- Drive sustained reorder + holes via `inject_rx_frame` to push the reassembly queue against its **byte-cap** (`tcp_reassembly.rs:70-80`). The byte-cap dropped path bumps `tcp.recv_buf_drops` (`engine.rs:885-892, 4586-4590`).
- Deterministic; ≤ 5 s wall.

**Counters / events to assert (corrected per codex #2):**
- `tcp.recv_buf_drops > 0` once byte-cap is exceeded — this is the actual current overflow counter; the previously-named `tcp.rx_drop_reassembly_overflow` does NOT exist in `ALL_COUNTER_NAMES` (`counters.rs:510-514`).
- Test-only queue-depth accessor (added under `pressure-test`): reassembly byte-occupancy never exceeds spec cap.
- `tcp.rx_reassembly_queued + tcp.rx_reassembly_hole_filled` accounting parity (these are real counters at `counters.rs:510-514`).
- `tcp.mbuf_refcnt_drop_unexpected == 0` after conn close.
- `obs.events_dropped == 0`.

**Optional follow-up (operator decision; see Reconciliation notes #4):** if a hole-count cap (vs byte-cap) is desired, add a new `tcp.rx_drop_reassembly_overflow` counter to `TcpCounters` + `ALL_COUNTER_NAMES` + bump-site, then extend Suite 7 to assert it. Until that happens, this suite asserts byte-cap behavior only.

**Pass criteria:** cap holds; counter accounting consistent.

**Cadence:** **nightly** (codex #5: per-PR cadence requires measured CI wall time first).

**Phase gating:** A11.3.

---

### Suite 8 — `pressure-sack-blocks` (Tier 2)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_sack_blocks.rs`.

**Workload shape:**
- Inject 16 distinct holes (4× the SACK serialization cap of 4).
- Deterministic; ≤ 2 s wall.

**Counters / events to assert:**
- Outgoing SACK option carries ≤ 4 blocks (asserted via SACK option parser on synthetic outbound mbuf).
- `tcp.tx_sack_blocks` reflects exact SACK-block emission count.
- Eviction policy: oldest block dropped (asserted via `tcp_input::SACK_TX_BLOCKS_*` test hooks if exposed; otherwise via observed outbound option ordering).
- No panic, no `tcp.rx_bad_option`.
- `obs.events_dropped == 0`.

**Cadence:** **nightly** (codex #5: per-PR cadence requires measured CI wall time first).

**Phase gating:** A11.3.

---

### Suite 9 — `pressure-timer-storm` (Tier 3)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_timer_storm.rs`.

**Workload shape:**
- 256 conns; force RTO storm by netem-disconnecting peer-side mid-test.
- 5 s without response → all conns hit RTO simultaneously.
- 30 s window.

**Counters / events to assert (codex #2: replaced nonexistent `tcp.tx_data_delta`):**
- `tcp.tx_rto > 0` and `tcp.tx_retrans > 0` post-storm.
- Per-conn FSM state returns to `Established` for all 256 conns post-recovery (asserted via test-only `engine.flow_table().states()` accessor under `pressure-test`).
- Post-recovery: `tcp.tx_payload_bytes` strictly increases over the post-recovery window (requires `obs-byte-counters` feature).
- Timer-wheel slots growth post-warmup ≤ 64 (existing invariant under storm).
- `tcp.tx_rst == 0` (engine doesn't give up under recoverable RTO).
- `obs.events_dropped == 0`.

**Cadence:** nightly only.

**Phase gating:** A11.4.

---

### Suite 10 — `pressure-fin-rst-flood` (Tier 3, split per codex missing-categories)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_fin_rst_flood.rs` (uses `inject_rx_frame` for adversarial frame stream).

**Sub-suite 10a — `unmatched-rst-flood` (nightly):**
- Inject 100k unsolicited RSTs at random 4-tuples; ~10 active legit conns running echoes in parallel.
- 10 s wall.
- Asserts: `tcp.rx_unmatched` delta == count of unmatched-RST injections (exact arithmetic, codex pattern P4); `tcp.conn_close == 0` for the active conns (RSTs at unmatched 4-tuples MUST NOT close anything); `obs.events_dropped == 0`; flow-table size returns to baseline.

**Sub-suite 10b — `matched-rst-flood` (nightly):**
- 64 active conns, RST injected at each conn's 4-tuple every 100 ms for 10 s.
- Asserts: `tcp.rx_rst` delta == count of matched-RST injections; `tcp.conn_rst` rises consistent with `tcp.rx_rst`; flow-table baseline recovery post-test; `obs.events_dropped == 0`; CPU/liveness preserved (engine still pumps a control echo conn opened separately).

**Sub-suite 10c — `fin-storm-deterministic-smoke` (per-PR):**
- N=64 active conns, mid-transfer; peer sends FIN to each; engine completes its half-close, app issues close, conns drain.
- 5 s wall.
- Asserts: `tcp.conn_open` delta == `tcp.conn_close` delta exactly; flow-table baseline; no spurious RST; `obs.events_dropped == 0`.
- Codex missing-category: isolates FIN teardown regressions that the bundled RST flood masks.

**Phase gating:** A11.4 for 10a/10b; 10c is per-PR (Tier 1) and rolls into A11.1 alongside Suite 2.

---

### Suite 11 — `pressure-recv-buf-saturation` (Tier 3)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_recv_buf_saturation.rs`.

**Workload shape:**
- Bucket 11a — sustained slow drain: 1 conn, peer floods at line rate, application drains every 10 ms, 30 s window.
- Bucket 11b — app-starvation pause (codex missing-category): 1 conn, peer flow at moderate rate, app pauses both event-drain and read-drain for 5 s, then resumes for 10 s. Asserts the engine does not silently lose events nor mbufs.

**Counters / events to assert:**
- 11a: `tcp.recv_buf_drops > 0` (back-pressure surfaces per `feedback_performance_first_flow_control.md`).
- 11a: engine does NOT throttle peer at link layer (no zero-window held permanently).
- 11a: `tcp.rx_zero_window` rises but `tcp.tx_window_update` follows when app drains.
- 11a/11b: no mbuf leak (`rx_mempool_avail` returns to baseline post-drain).
- 11b: `obs.events_dropped` either remains `== 0` (if soft-cap absorbs the 5 s pause) OR the test logs the overflow count and instead asserts post-resume liveness — operator must decide; document the chosen semantics in the suite header (open question #4 below).
- 11b: post-resume the receive path continues (peer-to-app payload bytes increase post-resume, requires `obs-byte-counters`).
- `tcp.mbuf_refcnt_drop_unexpected == 0`.

**Cadence:** **nightly** (codex #5: per-PR cadence requires measured CI wall time first).

**Phase gating:** A11.4.

---

### Suite 12 — `pressure-pmtu-blackhole` (Tier 3, defers Stage 2 PMTUD work)

**Hosting crate:** `tools/layer-h-correctness` row 18 (replaces existing PMTU-blackhole placeholder per Part 9 Layer H matrix note).

**Workload shape:** as Layer H — peer netem `mtu 600 + reject-large-with-icmp`. Single conn.

**Counters / events to assert:**
- `ip.rx_icmp_frag_needed > 0`.
- `ip.pmtud_updates > 0`.
- Workload completes (engine reduced MSS).
- No `tcp.tx_rst`.

**Cadence:** Layer H nightly.

**Phase gating:** Stage 2 PMTUD task; placeholder in A11 layer-H matrix.

---

### Suite 13 — `pressure-socket-buffer-underrun` (Tier 2, codex missing-category)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_socket_buffer_underrun.rs`.

**Workload shape:**
- 1 conn. Application alternates 1-byte and 64-KiB reads/writes in a tight loop.
- 30 s window, deterministic.

**Counters / events to assert:**
- `tcp.rx_partial_read_splits > 0` (the dedicated counter at `crates/dpdk-net-core/src/counters.rs:264-270` is the oracle).
- No payload loss across the alternating read sizes (CRC-checked on application side).
- No mempool drift on either RX or TX side.
- `obs.events_dropped == 0`, `tcp.mbuf_refcnt_drop_unexpected == 0`.

**Pass criteria:** partial-read counter rises, no payload loss, no leak.

**Cadence:** nightly (until per-PR wall time measured).

**Phase gating:** A11.2.

---

### Suite 14 — `pressure-option-negotiation-churn` (Tier 2, codex missing-category)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_option_churn.rs`. Reuses option-parser proptests as input generators where applicable.

**Workload shape:**
- 256 sequential connection-open/close cycles. Each open negotiates a random combination of {MSS=536/1460/9000, WSCALE=0..14, TS on/off, SACK on/off}.
- Single in-host kernel peer (TAP). 30 s window.

**Counters / events to assert:**
- Negotiated state observed in events matches what the peer offered (sanity: no carryover between cycles).
- No stale `ts_recent` carryover across distinct 4-tuple cycles (asserted via test-only diag accessor).
- No stale SACK state carryover.
- `tcp.rx_bad_option == 0`, `obs.events_dropped == 0`, `tcp.mbuf_refcnt_drop_unexpected == 0`.

**Pass criteria:** option negotiation is hermetic per cycle.

**Cadence:** nightly.

**Phase gating:** A11.3.

---

### Suite 15 — `pressure-listen-accept-exhaustion` (Tier 3, codex missing-category)

**Hosting crate:** `crates/dpdk-net-core/tests/pressure_listen_accept_exhaustion.rs`. Requires `test-server` for listen slots (`crates/dpdk-net-core/src/engine.rs:854-861`).

**Workload shape:**
- 1 listener. Inject 100k SYNs at distinct 4-tuples faster than the application can call `accept`.
- 10 s wall.

**Counters / events to assert:**
- `tcp.conn_table_full > 0` (capacity hit, consistent with addressability test in `tests/counter-coverage.rs:801-816`).
- No listen-slot leak: post-test, listener returns to baseline slot count.
- For accepted flows: clean three-way completion, no spurious RST.
- `obs.events_dropped == 0` (or operator-acknowledged overflow with documented semantics).
- `tcp.mbuf_refcnt_drop_unexpected == 0`.

**Pass criteria:** capacity hit cleanly, no leak, accepted conns proceed.

**Cadence:** nightly.

**Phase gating:** A11.4.

---

## Infrastructure gaps

The suites above cannot land without these unblockers. List ordered by criticality.

### I0 — `cargo metadata` workspace-feature gate (Pattern P1 CI gate) — **HARD PREREQUISITE FOR EVERYTHING ELSE**
This was previously listed as I4 but per codex #6 is reordered to I0: it MUST land before adding any new feature flags. Today's defaults include all hardware offload features (`crates/dpdk-net-core/Cargo.toml:15-24`); test-only features (`test-server`, `test-inject`, `fault-injector`, `obs-none`, `bench-internals`) are default-off but `tools/scapy-fuzz-runner/Cargo.toml:7-8` already demonstrates the leak class — a non-optional dependency on `dpdk-net-core` with a test feature enabled, which `cargo metadata --workspace` then promotes into the production graph.

The gate is a 5–15 line script (`scripts/check-workspace-features.sh`) that:
- Runs `cargo metadata --format-version 1 --workspace --release`.
- Parses the resolved feature set for `dpdk-net-core`.
- Asserts that none of the names `pressure-test`, `test-server`, `test-inject`, `fault-injector`, `obs-none`, `bench-internals` is enabled in the resolution graph for production binaries (the allowlist of crates allowed to enable these = test/bench tools only).
- Exits non-zero on violation; integrates as a CI step that runs on every push to master and every PR.

**Without I0, adding `pressure-test` (I1) recreates the same leak class.** Already named in `cross-phase-retro-summary.md` Theme A item 6 and is a Pattern P1 prerequisite for all subsequent infrastructure work in this plan.

### I1 — Pressure-test feature gate + dedicated `pressure-test` cargo feature (depends on I0)
A single non-default `pressure-test` feature in `crates/dpdk-net-core/Cargo.toml` that enables:
- Test-only `EngineConfig` overrides for `rx_mempool_size`, `tx_data_mempool_size`, `tx_hdr_mempool_size`.
- Test-only diag accessors needed for cross-suite assertions (`flow_table().active_conns()`, `flow_table().states()`, `reassembly_queue_depth()`).
- A typed `read_level_counter_u32(name)` accessor for `tcp.tx_data_mempool_avail`, `tcp.rx_mempool_avail` (the generic `lookup_counter` cannot read these `AtomicU32` fields; codex #2).

Operator decision (open question #3 below): whether `pressure-test` should imply `test-inject` and `test-server` automatically, or remain three orthogonal features. Recommended: orthogonal, with `pressure-test` requiring explicit `--features pressure-test,test-inject` so the I0 metadata gate can distinguish injection vs server-FSM vs assertion plumbing.

After this lands, I0's metadata gate must continue to assert that `pressure-test` is NOT in the resolution graph of any production binary.

### I2 — In-process loopback driver for max-throughput suites
Currently `long_soak_stability.rs` uses TAP+kernel-echo, which caps throughput at ~10–20 Gbps and requires sudo. For per-PR cadence on Suite 1 (`pressure-max-throughput`), a faster loopback option is needed. Two candidates:

- **Option A (preferred):** virtual-clock + paired engine instances (one tx, one rx) talking via `inject_rx_frame` shim. Existing scaffold in `engine_no_eal_harness.rs`.
- **Option B:** `net_null` PMD + custom RX poller that reflects TX frames. Closer to real datapath, but requires new shim code.

Without I2, Tier 1 suites only run nightly (TAP+sudo) and lose the per-PR catch-T17-class signal.

### I3 — Failure-bundle dump on assertion fail
On any pressure-test assertion failure, dump:
- Last 1024 `InternalEvent`s.
- Full `Counters` snapshot (eth + ip + tcp + obs + fault).
- Diag snapshot (`Engine::diag_input_drops`, `flow_table` state, `timer_wheel.slots_len`).
- `EngineConfig` (so reproducer knows the regime).

To `target/pressure-test/<suite>/<bucket>/<timestamp>/` directory. Adapter helper in `crates/dpdk-net-core/tests/common/mod.rs`.

### I4 — (PROMOTED to I0; see above)
The cargo-metadata workspace-feature gate is now I0. This number is intentionally retired so prior cross-references resolve to I0; do not reuse.

### I5 — Counter-delta DSL for assertion clarity
Today's tests inline assertion arithmetic (`(c2 - c1) >= n`); harder to scale across 12 suites. A small helper module:

```rust
// crates/dpdk-net-core/tests/common/pressure.rs
pub struct CounterSnapshot { /* Counters clone */ }
impl CounterSnapshot {
    pub fn delta_since(&self, before: &Self) -> CounterDelta;
}
pub fn assert_delta(d: &CounterDelta, name: &str, rel: Relation);
```

Mirrors the `(counter, relation)` pair pattern from `tools/layer-h-correctness/src/scenarios.rs:50` and `tools/bench-stress/src/scenarios.rs:42` — same DSL across all three layers reduces double-encoding.

### I6 — Long-running soak harness with leak detection (ASAN axis)
Suites 9–11 (Tier 3) benefit from running under ASAN for the FIN/RST flood and timer storm. Promote ASAN to a CI matrix axis per Pattern P11 / Part 7 STAGE-2 recommendation. One green-CI run/day under ASAN.

### I7 — Test-time NIC-BAD frame injector (codex-verified shape)
Suite 3 (counter-parity offload matrix) needs to inject a real NIC-BAD-flagged mbuf through the offload-ON path. Codex verified the supporting plumbing:
- C shim already supports OR-ing arbitrary `ol_flags`: `crates/dpdk-net-sys/shim.c:170-188` + `wrapper.h:80-86`.
- RX BAD flag constants are exposed: `crates/dpdk-net-core/src/dpdk_consts.rs:35-46`.
- The dispatch path consumes `ol_flags` correctly: `dispatch_one_real_mbuf` → `rx_frame` (`engine.rs:3757-3790`); IP/L4 offload classifiers honor the bits (`l3_ip.rs:151-189`, `engine.rs:4034-4047`).
- The gap: existing `inject_rx_frame` (`engine.rs:6295-6367`) and `inject_rx_chain` (`engine.rs:6388-6497`) allocate/copy/dispatch but never call `shim_rte_mbuf_or_ol_flags`. The fallback test-server injector (`engine.rs:6724-6732`) bypasses mbuf flag reads entirely.

Required deliverable: under `#[cfg(feature = "test-inject")]`, add
```rust
pub fn inject_rx_frame_with_ol_flags(&self, frame: &[u8], ol_flags: u64) -> Result<(), InjectErr>
```
that shares the existing allocate/copy body and calls `unsafe { sys::shim_rte_mbuf_or_ol_flags(mbuf.as_ptr(), ol_flags) }` before `dispatch_one_rx_mbuf`. No new C shim is needed. Suite 3 MUST require `test-inject` and MUST NOT use the fallback test-server injector for NIC-BAD rows.

---

## Phase ordering

Sub-phases under the umbrella **A11 (pressure-correctness layer)**, with sequenced lanes per codex #6 (no full-parallel until shared harness exists).

### Phase A11.0 — Hard prerequisite (sequential, blocks all subsequent work)
**Step 1:** I0 lands as its own PR (cargo-metadata workspace-feature gate). CI must be green on master and red on a synthetic feature-leak branch.
**Step 2:** I1 (the `pressure-test` feature itself) lands as a follow-up PR. I0 then asserts `pressure-test` does not leak into production.
**Step 3:** I3 (failure-bundle helper) lands.

These three are strictly sequential because they share the same `Cargo.toml` and `tests/common/` plumbing. Estimated cost: ~1.5 agent-days.

### Phase A11.1 — Tier 1 (Block A11)
After A11.0 ships:
- Lane A: I2 (in-process loopback driver) → Suite 1 (`pressure-max-throughput`).
- Lane B: Suite 2 (`pressure-conn-churn`) + Suite 10c (`fin-storm-deterministic-smoke`).
- Lane C: I7 (`inject_rx_frame_with_ol_flags`) → Suite 3 (`pressure-counter-parity-offload-matrix`).
Lanes are parallel, but each lane's PRs MUST land sequentially within the lane.
Goal: per-PR signal that catches T17-class bugs and Pattern P4 counter-placement bugs **before merge to master**.
Estimated cost: 2.5–3 agent-days under parallel dispatch.

### Phase A11.2 — Tier 2 (Pre-Stage-2)
Parallel: Suites 4, 5, 6, 13. Infrastructure: I5.
Goal: catch loss-recovery accounting bugs (Part 3 S1, S2), back-pressure invariants, partial-read splits. Reduces Stage-2 multi-thread refactor blast radius.
Estimated cost: 3–4 agent-days.

### Phase A11.3 — Tier 2 narrow (Pre-Stage-2)
Parallel: Suites 7, 8, 14.
Goal: hard-cap correctness for reassembly + SACK + option-negotiation hermeticity.
Estimated cost: 1.5–2 agent-days.

### Phase A11.4 — Tier 3 (forensic / nightly)
Parallel: Suites 9, 10a, 10b, 11, 15. Infrastructure: I6.
Goal: hardening signal under storm + flood + recv-buf saturation + listener exhaustion.
Estimated cost: 2 agent-days.

**Total Stage-1-finalization cost:** ~10.5–12.5 agent-days under parallel dispatch (was 8–11 before missing-suites added).

---

## Open questions back to operator (residuals after codex review)

1. **Suite 1 per-PR locus:** TAP+sudo (slower, more realistic) vs paired-engine (faster, less realistic) for the per-PR smoke bucket. Codex resolved: paired-engine catches mempool-divisor + conn-handle leak deterministically; the K=1MiB stall and ENA-realistic descriptor cleanup are nightly-only. Recommendation: per-PR uses TAP+sudo at the single `N=16, W=16KiB, 10s` bucket; paired-engine via I2 is the faster fallback path if TAP+sudo proves CI-flaky.

2. **Suite 3 land-fail-then-fix vs land-fix-then-test:** the IP NIC-BAD double-bump is in tree today. Should Suite 3 land first as a deliberately-failing test (forcing the next PR to fix the double-bump in `l3_ip.rs:213-220` / `engine.rs:3928-3931`), or should the fix land first and Suite 3 land green? Codex flagged this as an explicit operator decision.

3. **`pressure-test` feature scope:** does enabling `pressure-test` automatically imply `test-inject` and `test-server`, or do they remain three orthogonal features? Recommendation: orthogonal, so I0's metadata gate distinguishes injection plumbing, server-FSM plumbing, and pressure-assertion plumbing independently.

4. **App-starvation overflow semantics (Suite 11b):** during a 5 s app-pause, should `obs.events_dropped` be a hard `== 0` assertion (requires sizing the soft-cap large enough to absorb 5 s) or should the test instead document an expected overflow count and assert post-resume liveness only? This is a spec call, not an implementation call.

5. **New pressure-only counters (`tcp.rx_drop_reassembly_overflow` style):** if a hole-count cap rather than byte-cap is desired for Suite 7, the corresponding counter must be added to public `TcpCounters` and `ALL_COUNTER_NAMES`. Should these become first-class production counters, or is the byte-cap path (which already exists as `tcp.recv_buf_drops`) sufficient?

6. **DSL convergence (deferred):** convergence of `tools/layer-h-correctness/src/scenarios.rs` + `tools/bench-stress/src/scenarios.rs` + new pressure-test DSL into a shared `tools/bench-common/` crate is desirable but pressure tests need direct `AtomicU32` level-counter support too (Layer H's `Snapshot` is delta-only). Codex recommends deferring DSL convergence until after A11.1 ships and the pressure-helper API is stable.

---

## Out of scope

Explicitly NOT in this plan:

- **Performance regression detection.** Numbers (bytes/sec, p99, etc.) remain in `bench-vs-mtcp` AWS sweep + `bench-micro` Criterion runs. Pressure tests are correctness oracles only; assertions are counter relations and lifetime invariants, not throughput thresholds. Performance regressions surface in nightly bench-pair, not pressure suites.
- **Stage-2 multi-thread invariant tests.** Once the engine becomes multi-lcore, every counter-load assumption (Pattern P12) needs a memory-ordering revisit. That is Stage-2 work; the pressure suites here assume the single-lcore borrow-cell invariant.
- **Cross-implementation comparator pressure tests.** mTCP / F-Stack / Linux comparator pressure (vs `dpdk-net`) is `bench-vs-mtcp` territory. The pressure suites here assert engine-internal invariants, not relative behavior vs other stacks.
- **Real-ENA hardware tests.** ENA-specific xstats and offload-runtime behavior already covered by `ahw_smoke_ena_hw.rs` and AWS bench. Pressure suites run on net_tap / paired-engine; ENA-specific pressure remains in nightly AWS sweep.
- **Long-soak (>10 min) tests.** `long_soak_stability.rs` (100k iter, ~10 min) covers the all-day-deployment dimension. Pressure suites stay ≤ 60 s per bucket (per-PR cadence requirement).
- **Fault-injector pressure cross-product.** `bench-stress` already exercises FaultInjector under netem with informational ratio gates; converting those to enforced gates is a separate cleanup (Part 8 S2-14), not new pressure suites.
- **C++/C-ABI consumer pressure.** Once C++ trading client integration begins (Stage 2), a separate pressure surface tests the C ABI under pressure (e.g. caller-driven `dpdk_net_send` storms). That belongs in the Stage-2 C-ABI integration plan, not here.

---

## Cross-references

- Cross-phase retro meta-synthesis: `docs/superpowers/reviews/cross-phase-retro-summary.md` Patterns P3, P4, P9; Themes B, E.
- Part 8 BLOCK-A11 #7: `docs/superpowers/reviews/cross-phase-retro-part-8-synthesis.md` lines 18–21.
- Part 9 STAGE-2 S2-4: `docs/superpowers/reviews/cross-phase-retro-part-9-synthesis.md` lines 41–44.
- Counter inventory: `crates/dpdk-net-core/src/counters.rs` `ALL_COUNTER_NAMES` constant.
- Existing pressure-adjacent tests: `crates/dpdk-net-core/tests/long_soak_stability.rs`, `connect_close_cycle.rs`, `rx_mempool_no_leak_ooo_netem.rs`.
- Existing scenario DSL precedents: `tools/layer-h-correctness/src/scenarios.rs:42-50`, `tools/bench-stress/src/scenarios.rs:38-50`.
- User-feedback memory pointers consulted: `feedback_performance_first_flow_control.md`, `feedback_observability_primitives_only.md`, `feedback_per_task_review_discipline.md`, `feedback_test_timeouts.md`, `feedback_counter_policy.md`.

## Reconciliation notes

This section summarizes how each codex concern was resolved during the DRAFT → FINAL transition. The codex review section below remains unchanged as the audit trail. Format: codex concern → disposition (accepted-and-edited / disputed-with-rationale / partially-accepted) → location of edit.

### Workload-shape sanity
1. **Suite 1 misses K=1MiB / ENA realism (`Suite 1` lines 94-99, 481-482).** Accepted-and-edited. Suite 1 is now a two-tier matrix: per-PR `N=16, W=16KiB, 10s` (one bucket) and nightly `N ∈ {1,16,64} × W ∈ {64B,1KiB,16KiB,1MiB} × 60s` (12 buckets) plus offload-OFF arm. Renamed in-text as "paired/TAP pressure-correctness" with explicit ENA caveat at the top of the suite. Edit location: Suite 1 / Workload shape + Naming caveat.
2. **30 s "continuously" doesn't match once-per-second slow-path sampling (`engine.rs:2502-2534`).** Accepted-and-edited. Replaced "continuously" with "sampled once/sec plus post-close baseline recovery"; promoted `eth.tx_drop_nomem == 0` to the hard real-time tripwire. Edit location: Suite 1 counter table.
3. **Suite 2 rate target 1000 c/s vs 256 conns × 200 ms stagger ceiling.** Accepted-and-edited. The rate is now a measured, logged target — pass criterion is the open/close counter parity, not the rate. Edit location: Suite 2 / Workload shape + Counters.

### Counter-assertion correctness
4. **Suite 1 fixed-5% `tcp.tx_data_mempool_avail` floor wrong.** Accepted-and-edited. Replaced with capacity-formula `min_avail >= tx_data_mempool_size − (N × ceil(send_buffer_bytes / peer_mss) + 2 × tx_ring_size + retrans_headroom)` plus post-close drift ≤ 32. `eth.tx_drop_nomem == 0` retained as hard tripwire. Edit location: Suite 1 counter table.
5. **`tcp.tx_data_mempool_avail` not reachable through `lookup_counter`.** Accepted-and-edited. I1 now requires a typed `read_level_counter_u32(name)` accessor; Layer H's `Snapshot` cannot be reused for level counters. Edit location: I1 + Suite 1 note on level-counter access.
6. **Suite 3 NIC-BAD expected counters wrong (IP NIC-BAD currently double-bumps `ip.rx_csum_bad` via `l3_ip.rs:213-220` + `engine.rs:3928-3931`).** Accepted-and-edited. Suite 3 rewritten as four explicit rows (offload-ON IP NIC-BAD, offload-ON L4 NIC-BAD, offload-OFF software-IP-bad, offload-OFF software-TCP-bad). Row A's expected post-fix delta is `eth += 1, ip += 1, tcp += 0`; Suite 3 will FAIL on current HEAD until the double-bump is fixed. Operator decision (open question #2) on land-fail-then-fix vs land-fix-then-test. Edit location: Suite 3 entirely rewritten.
7. **Suite 3 conflated IP and L4 offload BAD.** Accepted-and-edited. Now four distinct rows naming the exact `RTE_MBUF_F_RX_IP_CKSUM_BAD` vs `RTE_MBUF_F_RX_L4_CKSUM_BAD` flag (`dpdk_consts.rs:35-46`). Edit location: Suite 3 row A vs row B.
8. **`tcp.mbuf_refcnt_drop_unexpected == 0` necessary but not sufficient.** Accepted-and-edited. `obs.events_dropped == 0` is now called out as a global side-check required for every pressure suite. Edit location: Suites 1, 2, 4, 7, 8, 9, 10a-c, 11, 13, 14, 15.
9. **Suite 7 names nonexistent `tcp.rx_drop_reassembly_overflow`.** Accepted-and-edited. Suite 7 now asserts the real `tcp.recv_buf_drops` byte-cap path; adding a hole-count counter is operator-deferred (open question #5). Edit location: Suite 7 / Counters.

### Hosting-crate decision
10. **Tier 1 should stay in `crates/dpdk-net-core/tests/`.** Accepted (matches draft); reinforced. Edit location: Suite 1, 3 hosting headers (now explicit about needing private engine state).
11. **Suite 4 "extend Layer H OR new tools crate" is wrong.** Accepted-and-edited. Pressure suites stay in `dpdk-net-core/tests/` until Stage 2's `dpdk-net-test-support` (Pattern P2) lands. Layer H stays netem-only single-conn smoke per Part 9. Edit location: Suite 4 hosting header.

### Cadence
12. **Per-PR runtime math optimistic.** Accepted-and-edited. Suite 1 per-PR is now a single `N=16, W=16KiB, 10s` bucket (≤ 25 s wall); Suite 3 ≤ 25 s; Suite 2 ≤ 12 s; Suite 10c ≤ 10 s. Tier-1 smoke ≤ 90 s wall total + EAL/TAP overhead. Edit location: Suite 1, 2, 3, 10c cadence.
13. **Move Suites 5-8 and 11 out of per-PR.** Accepted-and-edited. All marked nightly with explicit "promote to per-PR after measured CI wall time exists" rationale. Edit location: Suites 5, 6, 7, 8, 11 cadence.
14. **Cargo invocations should be package-scoped.** Accepted-and-edited. Every cadence header now uses `cargo test -p dpdk-net-core --test 'pressure_*' --features pressure-test[,test-inject]` instead of workspace-wide. Edit location: Suite 1, 3 cadence; cross-references in I0/I1.

### Infrastructure unblocker (I1) shape
15. **I4 (cargo-metadata gate) must precede I1 (`pressure-test` feature).** Accepted-and-edited. I4 is now I0 (hard prerequisite blocking everything else). Old I4 retained as a redirect note. Edit location: Infrastructure / I0 + I4 redirect.
16. **`pressure-test` should not flow to production via workspace unification.** Accepted (matches draft); reinforced via I0 metadata gate explicitly listing `pressure-test`, `test-server`, `test-inject`, `fault-injector`, `obs-none`, `bench-internals`. Edit location: I0 + I1.

### Missing categories
17. **Socket-buffer underrun missing.** Accepted-and-edited. New Suite 13 `pressure-socket-buffer-underrun`, Tier 2, asserts `tcp.rx_partial_read_splits > 0`. Edit location: new Suite 13.
18. **App starvation only partially covered.** Accepted-and-edited. Bucket 11b added with explicit operator-deferred semantics on `obs.events_dropped` (open question #4). Edit location: Suite 11.
19. **Pathological reorder / hole-list saturation underspecified.** Partially-accepted. Suite 7 now asserts the byte-cap path that actually exists; adding a hole-count counter is operator-deferred (open question #5). Edit location: Suite 7.
20. **TCP option negotiation churn missing.** Accepted-and-edited. New Suite 14 `pressure-option-negotiation-churn`, Tier 2. Edit location: new Suite 14.
21. **RST flood DOS too weak / FIN-storm bundled.** Accepted-and-edited. Suite 10 split into 10a (`unmatched-rst-flood`, nightly), 10b (`matched-rst-flood`, nightly), 10c (`fin-storm-deterministic-smoke`, per-PR). Edit location: Suite 10.
22. **Listener-accept-queue exhaustion missing.** Accepted-and-edited. New Suite 15 `pressure-listen-accept-exhaustion`, Tier 3. Edit location: new Suite 15.
23. **Retransmit + reorder + ENOMEM intersection missing.** Accepted-and-edited. Suite 4 now has an explicit ENOMEM bucket with `tx_data_mempool_size_override`; the void-retransmit oracle (Part 3 S1, S2) is asserted under forced allocation failure. Edit location: Suite 4 / Workload shape + Counters.
24. **RTO recovery cliff (Suite 9) imprecise.** Accepted-and-edited. Replaced nonexistent `tcp.tx_data_delta` with per-conn FSM-state recovery + `tcp.tx_payload_bytes` increase under `obs-byte-counters`. Edit location: Suite 9 / Counters.

### Ordering / dependencies
25. **NIC-BAD injection (I7) should not gate I2.** Accepted-and-edited. A11.1 is now three explicit lanes (Lane A: I2 → Suite 1; Lane B: Suites 2 + 10c; Lane C: I7 → Suite 3). I0/I1/I3 form A11.0 prerequisites. Edit location: Phase ordering.
26. **Tier-1 not fully parallel until shared harness exists.** Accepted-and-edited. A11.0 (I0 + I1 + I3) is sequential prerequisite; A11.1 lanes share that harness. Edit location: Phase ordering / A11.0 + A11.1.

### NIC-BAD injection feasibility (verified by codex)
27. **`inject_rx_frame_with_ol_flags` shape under `test-inject`.** Accepted-and-incorporated. I7 deliverable is now the exact signature codex provided, sharing existing alloc/copy body and calling `shim_rte_mbuf_or_ol_flags` before dispatch. No C-side work needed. Edit location: I7.

### Recommendations Claude got right (per codex)
- Middle-layer premise (`Background` lines 18-20).
- Correctness-orientation over throughput thresholds (`Out of scope` lines 478-480).
- Tier-1 hosting under `dpdk-net-core/tests/` (`Suite 1` line 92; Pattern P2).
- `obs.events_dropped` requirement alongside mbuf checks.
- I7 caution about NIC-BAD injection (`Infrastructure / I7`).
- DSL convergence instinct (deferred to post-A11.1 per open question #6).

These are kept as-is in the draft; no edits required.

---

## Status

**FINAL** — codex review incorporated, ready for implementation.
**Date finalized:** 2026-05-05.
**Total suites:** 17 (Suites 1–9, 10a-c, 11, 12, 13, 14, 15; Suite 4 includes ENOMEM bucket; Suite 11 includes app-starvation bucket).
**Implementation order (suites only; infrastructure prereqs in linked plan):**
1. Suite 1 `pressure-max-throughput` (A11.1 Lane A)
2. Suite 2 `pressure-conn-churn` (A11.1 Lane B)
3. Suite 10c `fin-storm-deterministic-smoke` (A11.1 Lane B)
4. Suite 3 `pressure-counter-parity-offload-matrix` (A11.1 Lane C)
5. Suite 4 `pressure-loss-recovery` (A11.2)
6. Suite 5 `pressure-slow-receiver` (A11.2)
7. Suite 6 `pressure-mempool-exhaustion` (A11.2)
8. Suite 13 `pressure-socket-buffer-underrun` (A11.2)
9. Suite 7 `pressure-reassembly-saturation` (A11.3)
10. Suite 8 `pressure-sack-blocks` (A11.3)
11. Suite 14 `pressure-option-negotiation-churn` (A11.3)
12. Suite 9 `pressure-timer-storm` (A11.4)
13. Suite 10a `unmatched-rst-flood` (A11.4)
14. Suite 10b `matched-rst-flood` (A11.4)
15. Suite 11 `pressure-recv-buf-saturation` (A11.4)
16. Suite 15 `pressure-listen-accept-exhaustion` (A11.4)
17. Suite 12 `pressure-pmtu-blackhole` (Layer H placeholder; Stage 2 PMTUD work)

**Total estimated effort:** ~10.5–12.5 agent-days under parallel dispatch (1.5 for A11.0 prereqs, the rest for suites + remaining infrastructure).

**Linked implementation plan:** `docs/superpowers/plans/2026-05-05-pressure-test-plan.md` (T0 through T18).

---

## Codex review (second opinion)

**Reviewer:** codex:codex-rescue
**Reviewed at:** 2026-05-05

### Verdict
NEEDS-REWORK, because the middle-layer premise is right but Suite 1's workload omits the K=1MiB stall shape, Suite 3's NIC-BAD expected counters are wrong for the current IP checksum path, and the feature/hosting plan can reintroduce Pattern P1 unless the cargo-metadata gate lands first.

### Concerns by category

#### Workload-shape sanity
- Suite 1 is too small to claim it catches the full T17 class: the draft says T17 surfaced at `60 s x 64 conns x >=1 KiB` and included the `K=1 MiB per-write stall` (`Background`, lines 14-16), but Suite 1 only sweeps `W in {64 B, 1 KiB, 16 KiB}` for `30 s` after warmup (`Suite 1 / Workload shape`, lines 94-99). Concrete diff shape: add a nightly-only Suite 1 bucket `N=64, W=1MiB, duration=60s` and keep the per-PR bucket at `N=16, W=16KiB, duration=10s`; otherwise the plan should say it catches the mempool-divisor/conn-leak subset but not the K=1MiB stall.
- The current Suite 1 matrix is useful for deterministic pressure, but it is not an ENA realism proxy. The draft says peer is in-host TAP echo and later declares real ENA out of scope (`Suite 1 / Workload shape`, line 98; `Out of scope`, lines 481-482), while the production poll path reads real mbuf `ol_flags`, RSS hash, and hardware timestamp in `dispatch_one_real_mbuf` (`crates/dpdk-net-core/src/engine.rs:3757-3790`). Concrete diff shape: rename Suite 1 to "paired/TAP pressure-correctness" and add an explicit "does not exercise ENA descriptor cleanup, offload stamping, or PMD completion behavior" caveat next to line 98.
- The `30 s` measurement window is not mechanically tied to the slow-path mempool samples. Both `tcp.rx_mempool_avail` and `tcp.tx_data_mempool_avail` are sampled at most once per second (`crates/dpdk-net-core/src/engine.rs:2502-2534`), so a 30-s bucket produces only about 30 level samples; that is enough for post-close recovery/drift but weak for "continuously" in the Suite 1 table (`Suite 1 / Counters`, lines 105-106). Concrete diff shape: replace "continuously" with "sampled once/sec plus post-close baseline recovery" and require `eth.tx_drop_nomem == 0` as the hard real-time failure signal.
- The churn suite target is internally inconsistent: line 135 says 256 conns with a 200 ms stagger, which is about 1,280 opens/sec at best if every slot is active, while line 136 targets `>= 1000 conn open/close / sec` for 30 s. That is plausible only with aggressive concurrent scheduling and no TAP/kernel bottleneck; the draft should make the rate a measured target, not a pass precondition, and assert the actual open/close counters instead (`crates/dpdk-net-core/tests/counter-coverage.rs:801-816` covers `tcp.conn_table_full`, but not this pressure rate).

#### Counter-assertion correctness
- The Suite 1 floor for `tcp.tx_data_mempool_avail` should not be a fixed `5%` of capacity (`Suite 1 / Counters`, line 105). The configured TX data pool formula is capacity-based (`crates/dpdk-net-core/src/engine.rs:409-448`, `crates/dpdk-net-core/src/engine.rs:1216-1246`), while the send path consumes one data mbuf per MSS-sized segment (`crates/dpdk-net-core/src/engine.rs:5336-5378`, `crates/dpdk-net-core/src/engine.rs:5421-5429`). Concrete diff shape: compute the assertion as `min_avail >= tx_data_mempool_size - (N * ceil(send_buffer_bytes / peer_mss) + 2 * tx_ring_size + retrans_headroom)` during the bucket, then require post-close recovery to baseline within a small drift tolerance; keep `eth.tx_drop_nomem == 0` as the regression tripwire.
- `tcp.tx_data_mempool_avail` is not reachable through the generic `lookup_counter` table: the field exists at `crates/dpdk-net-core/src/counters.rs:291-304`, but `ALL_COUNTER_NAMES` only documents the older `rx_mempool_avail` absence and lists `tcp.mbuf_refcnt_drop_unexpected` next (`crates/dpdk-net-core/src/counters.rs:546-551`). Concrete diff shape: add a pressure snapshot helper that directly loads both `AtomicU32` avail fields, or add a typed `read_level_counter_u32` path; do not reuse the Layer H `Snapshot` path as-is.
- Suite 3's offload-ON NIC-BAD expected deltas are wrong for an IP checksum BAD frame. The draft expects `eth.rx_drop_cksum_bad += 1`, `ip.rx_csum_bad += 0`, and `tcp.rx_bad_csum += 0` (`Suite 3 / Counters`, lines 168-173), but current `ip_decode_offload_aware` bumps both `eth.rx_drop_cksum_bad` and `ip.rx_csum_bad` on `RTE_MBUF_F_RX_IP_CKSUM_BAD` (`crates/dpdk-net-core/src/l3_ip.rs:211-219`), and `Engine::handle_ipv4` currently bumps `ip.rx_csum_bad` again for `L3Drop::CsumBad` (`crates/dpdk-net-core/src/engine.rs:3905-3930`). Concrete diff shape: split Suite 3 into IP-NIC-BAD and L4-NIC-BAD rows; expected post-fix IP-NIC-BAD should be `eth += 1, ip += 1, tcp += 0`, and the test should fail today with `ip += 2` until the double-bump is fixed.
- Suite 3 also conflates IP and L4 offload BAD. The TCP offload BAD path drops in `tcp_input` and bumps `eth.rx_drop_cksum_bad` plus `tcp.rx_bad_csum` (`crates/dpdk-net-core/src/engine.rs:4034-4047`), not `ip.rx_csum_bad`; the draft's "NIC-BAD frame" row (`Suite 3`, lines 170-171) must name the exact flag under test (`RTE_MBUF_F_RX_IP_CKSUM_BAD` vs `RTE_MBUF_F_RX_L4_CKSUM_BAD`, constants at `crates/dpdk-net-core/src/dpdk_consts.rs:35-46`).
- `tcp.mbuf_refcnt_drop_unexpected == 0` is necessary but not sufficient for pressure-correctness. It only fires from `MbufHandle::Drop` when `pre == 0` or post-decrement refcount exceeds the threshold (`crates/dpdk-net-core/src/mempool.rs:281-307`); it does not prove the event queue stayed lossless. The draft correctly includes `obs.events_dropped == 0` in Suite 1 (`Suite 1 / Counters`, lines 107-108), and that must remain a global side-check for every pressure suite because the queue drop counter is bumped independently in `EventQueue::push` (`crates/dpdk-net-core/src/tcp_events.rs:153-171`).
- Suite 7 names a likely nonexistent counter: `tcp.rx_drop_reassembly_overflow` (`Suite 7 / Counters`, line 266). Current reassembly overflow feeds `cap_dropped` into `tcp.recv_buf_drops`, while `rx_reassembly_queued` and `rx_reassembly_hole_filled` are the only reassembly counters in the inventory (`crates/dpdk-net-core/src/tcp_reassembly.rs:70-80`, `crates/dpdk-net-core/src/counters.rs:510-514`, `crates/dpdk-net-core/src/engine.rs:885-892`, `crates/dpdk-net-core/src/engine.rs:4586-4590`). Concrete diff shape: either add a new counter to `TcpCounters`/`ALL_COUNTER_NAMES` and bump it at the cap site, or change Suite 7 to assert `tcp.recv_buf_drops` plus a test-only queue-depth accessor.

#### Hosting-crate decision
- Short-term `crates/dpdk-net-core/tests/pressure_*.rs` is the right place for Tier 1 because the assertions need private engine state and direct `AtomicU32` level reads (`Suite 1 / Hosting crate`, line 92; `Suite 3 / Hosting crate`, line 159). A new `tools/pressure-test/` workspace member would repeat the internals leak called out in Pattern P2 (`docs/superpowers/reviews/cross-phase-retro-summary.md:49-60`) unless a `dpdk-net-test-support` split lands first.
- The draft should not leave Suite 4 as "extend Layer H OR new tools crate" (`Suite 4 / Hosting crate`, lines 185-188). Layer H is explicitly netem-only and single-conn in Part 9 (`docs/superpowers/reviews/cross-phase-retro-part-9-synthesis.md:41-45`); pressure-specific suites should either stay in `dpdk-net-core/tests/` behind `pressure-test` or wait for a sibling test-support crate. Concrete diff shape: replace the OR with "A11: core integration tests; Stage 2: move shared harness into `crates/dpdk-net-test-support` and only then add a runner crate."

#### Cadence
- The runtime math is optimistic. Full Suite 1 is `9 buckets x 40 s ~= 6 min` (`Suite 1 / Workload shape`, line 97), not a `cargo test`-friendly job; the per-PR smoke is `80 s` before EAL setup, TAP setup, process startup, and failure-bundle costs (`Suite 1 / Cadence`, line 122). Suite 2 and Suite 3 add at least `~12 s` and `<=30 s` by their own budgets (`Suite 2 / Cadence`, lines 149-151; `Suite 3 / Cadence`, line 179), so the Tier-1 smoke is already about 122 s plus overhead, not "about 2 min" with margin (`Open questions`, line 470).
- Several non-Tier-1 suites are also marked per-PR (`Suite 5`, line 226; `Suite 6`, line 250; `Suite 7`, line 273; `Suite 8`, line 293; `Suite 11`, line 355). Concrete diff shape: make only Tier 1 smoke per-PR; move Suites 5-8 and 11 to nightly until their wall times are measured in CI and a hard budget file exists.
- Cargo build/test invocation should be package-scoped, not workspace-scoped. Pattern P1 exists because workspace feature resolution silently activates test-only branches (`docs/superpowers/reviews/cross-phase-retro-summary.md:29-39`), and `tools/scapy-fuzz-runner` still demonstrates the bad non-optional `dpdk-net-core` + `test-inject` dependency shape (`tools/scapy-fuzz-runner/Cargo.toml:7-8`). Concrete diff shape: change line 122 from generic `cargo test --features pressure-test --features test-server` to `cargo test -p dpdk-net-core --test 'pressure_*' --features pressure-test`.

#### Infrastructure unblocker (I1) shape
- I1 is the right unblocker, but the order is wrong: the cargo-metadata audit must land before adding the `pressure-test` feature. The draft says combine I1 with Pattern P1 CI gate (`Infrastructure / I1`, lines 383-389) and separately lists I4 (`Infrastructure / I4`, lines 407-408); make I4 a prerequisite step inside I1, not a parallel item. Otherwise adding `pressure-test = ["test-server", "test-inject", ...]` to `crates/dpdk-net-core/Cargo.toml` can recreate the same leak class currently described for `test-server` and `test-inject` (`crates/dpdk-net-core/Cargo.toml:63-67`, `crates/dpdk-net-core/Cargo.toml:87-91`).
- The draft's `pressure-test` feature should not be consumed by any workspace tool crate until the Pattern P1 gate proves the production graph stays default-only. Current defaults include all hardware offload features (`crates/dpdk-net-core/Cargo.toml:15-24`) and test features are default-off (`crates/dpdk-net-core/Cargo.toml:63-67`, `crates/dpdk-net-core/Cargo.toml:87-91`); the desired diff is a non-default `pressure-test` used only by `dpdk-net-core` tests plus a metadata assertion that `cargo metadata --workspace --release` does not resolve `pressure-test`, `test-server`, `test-inject`, `fault-injector`, `obs-none`, or `bench-internals` for production binaries.

#### Missing categories
- Socket-buffer underrun is missing. The draft has slow receiver and recv-buffer saturation (`Tier 2`, line 70; `Suite 11`, lines 341-353), but no pressure suite that repeatedly performs tiny application reads/writes to drive partial-read splits and low-occupancy edge cases; the code has a dedicated `tcp.rx_partial_read_splits` counter (`crates/dpdk-net-core/src/counters.rs:264-270`) that should become the oracle. Concrete diff shape: add a Tier-2 `pressure-socket-buffer-underrun` suite with alternating 1-byte/large reads and writes, asserting `rx_partial_read_splits > 0`, no payload loss, and no mempool drift.
- Application-side starvation is only partially covered. Suite 11 slows reads (`Suite 11 / Workload shape`, lines 345-353), but no suite with the application not draining events or reads for a bounded interval; that is the path that can hide `obs.events_dropped` or READABLE queue starvation (`crates/dpdk-net-core/src/tcp_events.rs:153-171`). Concrete diff shape: add an app-starvation bucket to Suite 11: pause event/read drains for 5 s, then resume and assert `obs.events_dropped == 0` or explicitly document the intended overflow semantics.
- Pathological reorder / long hole list is only partially covered and currently underspecified. Suite 7 claims "100 distinct holes" and a "32 holes hard cap" (`Suite 7`, lines 260-268), but current code exposes byte-cap drops via `tcp.recv_buf_drops`, not a hole-count cap or `tcp.rx_drop_reassembly_overflow` (`crates/dpdk-net-core/src/tcp_reassembly.rs:70-80`, `crates/dpdk-net-core/src/counters.rs:510-514`). Concrete diff shape: either implement a hole-count cap + counter first, or restate Suite 7 as byte-cap saturation.
- TCP option negotiation churn is missing. Suite 8 checks SACK block serialization after holes (`Suite 8`, lines 279-291), but no suite repeatedly opens connections with varying MSS/WSCALE/TS/SACK option combinations and confirms negotiated state, counter stability, and no stale `ts_recent`/SACK carryover. Add a `pressure-option-churn` suite under Tier 2 using the existing option parser/proptests as input generators.
- RST flood DOS is partially present but too weak. Suite 10 injects 100k unsolicited RSTs and 10k FINs (`Suite 10`, lines 320-333), but the pass criteria omit event-queue preservation and CPU/liveness under matched RST storms. Concrete diff shape: split Suite 10 into `unmatched-rst-flood` (`tcp.rx_unmatched` delta exact, `conn_close` unchanged) and `matched-rst-flood` (`tcp.rx_rst`, `tcp.conn_rst`, flow-table baseline recovery, `obs.events_dropped == 0`).
- FIN-storm tear-down is partially present in Suite 10 (`Suite 10`, lines 326-333), but it is nightly-only and bundled with RST flood, so it will not isolate FIN teardown regressions. Concrete diff shape: add a deterministic smaller FIN storm per-PR smoke with `N=64` active conns and exact `conn_close`/flow-table baseline checks.
- Listener-accept-queue exhaustion is missing. The draft has concurrent active churn (`Suite 2`, lines 130-145), but no passive-open backlog/accept queue pressure. The engine has test-server listen slots behind `test-server` (`crates/dpdk-net-core/src/engine.rs:854-861`), so the diff should add `pressure-listen-accept-exhaustion` that floods SYNs against one listener and asserts `tcp.conn_table_full`, no listen-slot leak, and no spurious RST for accepted flows.
- Retransmit-plus-reorder simultaneous is only partially covered. Suite 4 combines loss and reorder (`Suite 4`, lines 189-199), but it does not combine loss/reorder with mempool pressure or ENOMEM, which is the void-retransmit counter class in Pattern P4 (`docs/superpowers/reviews/cross-phase-retro-summary.md:101-108`). Concrete diff shape: add one Suite 4 bucket with reduced `tx_data_mempool_size`, reorder gap, and loss, then assert event-vs-counter parity for retransmits plus `eth.tx_drop_nomem` attribution.
- RTO-then-recovery cliff cases are partially present but not precise. Suite 9 says conns resume after peer recovers (`Suite 9`, lines 303-312), but its oracle is `tcp.tx_data_delta > 0`, which is not a listed counter name and does not prove all conns recovered. Concrete diff shape: assert per-conn state returns to `Established`, `tcp.tx_rto > 0`, `tcp.tx_retrans > 0`, `tcp.tx_rst == 0`, and post-recovery `tcp.tx_payload_bytes` increases if `obs-byte-counters` is enabled.

#### Ordering / dependencies
- NIC-BAD injection should not gate the RTC pacer / in-process loopback work. It gates Suite 3 only (`Infrastructure / I7`, lines 427-429; `Suite 3`, lines 157-179), while I2 gates whether Suite 1 can run per-PR without TAP/sudo (`Infrastructure / I2`, lines 390-396). Concrete diff shape: change Phase A11.1 from "Parallel: Suites 1, 2, 3. Infrastructure: I1, I3, I4, I7" (`Phase ordering`, lines 436-439) to: Step 1 `I4+I1`; Step 2 `I3 failure bundles`; Step 3 parallel lanes `I2 -> Suite 1`, `Suite 2`, and `I7 -> Suite 3`.
- Tier-1 should not be described as fully parallel until the common feature gate and failure-bundle helper exist. Suites 1-3 can be implemented in parallel after I1/I3/I4, but they should not merge independently because all three rely on the same feature-gated test support and counter snapshot mechanics (`Infrastructure`, lines 383-422). Concrete diff shape: define an A11.1 merge sequence with shared harness first, then suite PRs.

#### NIC-BAD injection feasibility
- Verified: the mbuf flag is settable from Rust through the existing C shim, but today's public injection APIs do not expose a flagged path. The shim can OR arbitrary `ol_flags` (`crates/dpdk-net-sys/shim.c:170-188`, `crates/dpdk-net-sys/wrapper.h:80-86`), and the RX BAD constants exist (`crates/dpdk-net-core/src/dpdk_consts.rs:35-46`).
- Verified: the production/test-inject dispatch path will consume mbuf `ol_flags` if they are set. `dispatch_one_real_mbuf` reads `shim_rte_mbuf_get_ol_flags` and threads it through `rx_frame` (`crates/dpdk-net-core/src/engine.rs:3757-3790`), and IP/L4 offload classifiers use those bits (`crates/dpdk-net-core/src/l3_ip.rs:151-189`, `crates/dpdk-net-core/src/engine.rs:4034-4047`).
- Verified: current `#[cfg(feature = "test-inject")] inject_rx_frame` allocates/copies/dispatches but never calls `shim_rte_mbuf_or_ol_flags` (`crates/dpdk-net-core/src/engine.rs:6295-6367`), and `inject_rx_chain` also dispatches without a flag setter (`crates/dpdk-net-core/src/engine.rs:6388-6497`). The fallback test-server injection path is worse for NIC-BAD: it bypasses mbuf flag reads entirely and passes `0` for `ol_flags` to `rx_frame` (`crates/dpdk-net-core/src/engine.rs:6724-6732`).
- Concrete diff shape: under `test-inject`, add `pub fn inject_rx_frame_with_ol_flags(&self, frame: &[u8], ol_flags: u64) -> Result<(), InjectErr>` that shares the existing allocation/copy body and calls `unsafe { sys::shim_rte_mbuf_or_ol_flags(mbuf.as_ptr(), ol_flags) }` before `dispatch_one_rx_mbuf`. No new C shim is needed; Suite 3 should require `test-inject` and should not use the fallback `test-server` injector for NIC-BAD.

### Recommendations to incorporate before promoting from DRAFT
1. Replace Suite 1's fixed matrix with a two-tier matrix: per-PR `N=16, W=16KiB, 10s` plus nightly `N=64, W=1MiB, 60s`; document that TAP/paired-engine pressure is not ENA PMD pressure.
2. Replace the `tcp.tx_data_mempool_avail >= 5%` assertion with a capacity-minus-legitimate-outstanding formula and post-close baseline recovery; keep `eth.tx_drop_nomem == 0` as the hard failure.
3. Rewrite Suite 3's counter table into separate IP-NIC-BAD, L4-NIC-BAD, software-IP-bad, and software-TCP-bad rows; make IP-NIC-BAD fail the current double-bump until fixed.
4. Land the cargo metadata P1 gate before adding `pressure-test`, and invoke pressure tests with `cargo test -p dpdk-net-core --test 'pressure_*' --features pressure-test`, not workspace-wide.
5. Add `inject_rx_frame_with_ol_flags` under `test-inject`; do not attempt NIC-BAD tests through the fallback test-server injector.
6. Move Suite 5-8 and Suite 11 out of per-PR until measured CI wall times exist; keep Tier 1 smoke only in per-PR.
7. Add missing suites or buckets for socket-buffer underrun, app starvation, option-negotiation churn, listener accept exhaustion, and RTO-then-recovery recovery cliffs.
8. Replace nonexistent or unverified counters (`tcp.rx_drop_reassembly_overflow`, `tcp.tx_data_delta`) with real counters or add the counters in `TcpCounters` and `ALL_COUNTER_NAMES` as part of the suite patch.

### Recommendations Claude got right
- The plan correctly identifies the missing pressure-correctness layer between `cargo test` and AWS maxtp; Part 8 independently says every T17 bug was caught by sustained-throughput AWS runs rather than `cargo test` (`docs/superpowers/reviews/cross-phase-retro-part-8-synthesis.md:10-24`).
- Keeping early pressure suites correctness-oriented rather than throughput-threshold-oriented is right (`Background`, lines 18-20; `Out of scope`, lines 478-480).
- Colocating the first Tier-1 suites under `crates/dpdk-net-core/tests/` is pragmatic because the needed counters, mempool levels, and diag accessors are engine-internal today (`Suite 1`, line 92; Pattern P2 at `docs/superpowers/reviews/cross-phase-retro-summary.md:49-60`).
- Requiring `obs.events_dropped == 0` alongside mbuf/mempool checks is right; `mbuf_refcnt_drop_unexpected` and event-queue overflow are separate failure channels (`crates/dpdk-net-core/src/mempool.rs:281-307`, `crates/dpdk-net-core/src/tcp_events.rs:153-171`).
- The I7 caution is correct: NIC-BAD cannot be claimed until the injector can set `RTE_MBUF_F_RX_IP_CKSUM_BAD` (`Infrastructure / I7`, lines 427-429).
- The DSL convergence instinct is reasonable: Layer H and bench-stress already encode counter relations in nearly the same static matrix shape (`tools/layer-h-correctness/src/scenarios.rs:35-41`, `tools/bench-stress/src/scenarios.rs:46-50`), but pressure tests need direct `AtomicU32` level-counter support too.

### Open questions back to operator
- Should Suite 1 be allowed to require TAP/sudo for its per-PR smoke, or should per-PR be paired-engine only with a documented "not K=1MiB/ENA-realistic" caveat?
- Do you want the current IP NIC-BAD double-bump fixed before Suite 3 lands, or should Suite 3 intentionally land as a failing regression test first?
- Is `pressure-test` allowed to imply `test-inject` and `test-server`, or do you want separate feature flags so the metadata gate can distinguish injection, server FSM, and pressure assertions?
- Should new pressure-only counters such as `tcp.rx_drop_reassembly_overflow` be added to the public Rust counter inventory, or should these remain test-only diagnostics?
