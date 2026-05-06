# Pressure-Test Plan — Implementation

**Status:** READY-FOR-IMPLEMENTATION
**Author:** general-purpose subagent (opus 4.7) — synthesized from Claude draft + Codex critique
**Created:** 2026-05-05
**Trigger:** Cross-phase retro Pattern P9 (test-pyramid middle-layer gap) + user observation (max-throughput bench surfaces real bugs)
**Linked design:** `docs/superpowers/specs/2026-05-05-pressure-test-plan-design.md` (FINAL)

## Phase boundaries

This plan introduces a new umbrella phase **A11 (pressure-correctness layer)** with four sub-phases A11.0 → A11.4. A11.0 is a hard prerequisite that gates all subsequent suite work.

- **A11.0** — workspace-feature CI gate + `pressure-test` feature + failure-bundle helper. Sequential. Mechanical.
- **A11.1** — Tier 1 (BLOCK-A11): per-PR signal that catches T17-class bugs and Pattern P4 counter-placement bugs.
- **A11.2** — Tier 2 (Pre-Stage-2): loss-recovery, back-pressure, mempool-exhaustion, socket-buffer underrun.
- **A11.3** — Tier 2 narrow: reassembly + SACK + option-negotiation hermeticity.
- **A11.4** — Tier 3 (forensic / nightly): storm + flood + saturation + listener exhaustion.

A11.0 can begin immediately — it is mechanical and parallelizable with any remaining Stage 1 wrap-up work. A11.1 cannot start until A11.0 ships because every suite depends on the `pressure-test` feature gate and the failure-bundle helper.

## Tasks (ordered)

### T0 — workspace-feature CI gate (Pattern P1 prerequisite, A11.0 step 1)

**Why first:** without this gate, the new `pressure-test` cargo feature can leak into production binaries via workspace-feature unification — the same antipattern that produced the `tools/scapy-fuzz-runner` regression (`tools/scapy-fuzz-runner/Cargo.toml:7-8` has a non-optional `dpdk-net-core` dependency with `test-inject` enabled, which `cargo metadata --workspace` then promotes into the production graph). T0 must land BEFORE T1 because adding `pressure-test = [...]` to `crates/dpdk-net-core/Cargo.toml` without the gate recreates the leak class.

**Deliverable:**
- `scripts/check-workspace-features.sh` — 5–15 lines. Runs `cargo metadata --format-version 1 --workspace --release`, parses the resolved feature set for `dpdk-net-core`, asserts none of `pressure-test`, `test-server`, `test-inject`, `fault-injector`, `obs-none`, `bench-internals` is enabled in production binaries (allowlist = test/bench-only crates).
- CI integration: a new step / job in the existing GitHub Actions / Jenkins pipeline that runs the script on every push to master and every PR. Fail the build if the script exits non-zero.
- A synthetic feature-leak commit on a throwaway branch to prove the gate is red on violations; revert before merging T0.

**Pass criteria:**
- CI green on master with the gate active.
- CI red on a feature-leak branch (proof of life).
- Script handles offline-friendly invocation (cargo metadata works without network).

**Estimate:** 0.5 agent-day.

**Dependencies:** none. Can start IMMEDIATELY in parallel with any Stage 1 wrap-up.

---

### T1 — `pressure-test` cargo feature + level-counter accessor (A11.0 step 2)

**Why before suites:** every suite needs the feature gate and the typed level-counter accessor. The generic `lookup_counter` (`counters.rs:546-551`) cannot read `tcp.tx_data_mempool_avail` or `tcp.rx_mempool_avail` because they are `AtomicU32` level fields, not delta counters. The Layer H `Snapshot` type is delta-only and cannot be reused for level reads.

**Deliverable:**
- New non-default feature in `crates/dpdk-net-core/Cargo.toml`:
  ```toml
  pressure-test = []
  ```
  Decision (open question #3 in design): keep orthogonal from `test-inject` and `test-server`; suites that need both pass them explicitly.
- Under `#[cfg(feature = "pressure-test")]`:
  - Test-only `EngineConfig` overrides for `rx_mempool_size`, `tx_data_mempool_size`, `tx_hdr_mempool_size`.
  - Test-only diag accessors: `flow_table().active_conns()`, `flow_table().states()`, `reassembly_byte_occupancy()`.
  - Typed level-counter accessor on `Counters`: `read_level_counter_u32(&self, name: &str) -> Option<u32>` covering `tcp.tx_data_mempool_avail` and `tcp.rx_mempool_avail`.
- Re-run T0's CI gate: `pressure-test` must NOT show up in any production binary's resolved feature set.

**Pass criteria:**
- `cargo build -p dpdk-net-core` (no features) succeeds.
- `cargo test -p dpdk-net-core --features pressure-test` succeeds.
- T0 metadata gate confirms `pressure-test` is non-default and isolated.
- The new accessors are reachable from a smoke test under `#[cfg(feature = "pressure-test")]`.

**Estimate:** 0.5 agent-day.

**Dependencies:** T0.

---

### T2 — Failure-bundle helper + counter-snapshot DSL (A11.0 step 3)

**Why before suites:** all suites use the same failure-dump format and counter-delta assertions; building it once avoids 17 copies.

**Deliverable:**
- `crates/dpdk-net-core/tests/common/pressure.rs` (new module):
  - `pub struct CounterSnapshot { ... }` cloning the full `Counters` state plus level-counter reads.
  - `impl CounterSnapshot { pub fn delta_since(&self, before: &Self) -> CounterDelta }`.
  - `pub fn assert_delta(d: &CounterDelta, name: &str, rel: Relation)` — relations: `Eq(n)`, `Gt(n)`, `Ge(n)`, `Le(n)`, `Range(lo, hi)`.
  - `pub fn dump_failure_bundle(suite: &str, bucket: &str, ctx: &FailureCtx)` — writes last 1024 `InternalEvent`s, full counter snapshot, diag snapshot, `EngineConfig` to `target/pressure-test/<suite>/<bucket>/<timestamp>/`.
- Suite-side helper: `pub struct PressureBucket { ... }` carrying engine handle + before-snapshot + per-bucket label.

**Pass criteria:**
- A trivial smoke test exercises the helper end-to-end: opens an engine, takes snapshot, fires a known counter bump, asserts delta == 1, deliberately fails an assertion to confirm the bundle gets written.

**Estimate:** 0.5 agent-day.

**Dependencies:** T1.

**This concludes A11.0.** The next tasks (T3 onwards) are A11.1 lanes.

---

### T3 — Suite #1 `pressure-max-throughput` (A11.1 Lane A, BLOCK-A11)

**Why:** highest bug-yield. Catches T17 mempool-divisor + conn-handle-leak deterministically; nightly bucket also catches the K=1 MiB per-write stall.

**Deliverable:**
- `crates/dpdk-net-core/tests/pressure_max_throughput.rs`.
- Per-PR bucket: `N=16, W=16KiB, duration=10s` over TAP+kernel-echo. Wall budget ≤ 25 s incl. EAL/TAP setup.
- Nightly buckets: 12 × `N ∈ {1,16,64} × W ∈ {64B,1KiB,16KiB,1MiB} × 60s` plus offload-OFF arm.
- Counter assertions (full table in design doc Suite 1):
  - `tcp.tx_data_mempool_avail` capacity-formula floor + post-close drift ≤ 32.
  - `eth.tx_drop_nomem == 0` (hard tripwire).
  - `tcp.mbuf_refcnt_drop_unexpected == 0`, `obs.events_dropped == 0`.
  - `eth.tx_drop_full_ring / eth.tx_pkts_delta < 0.001`.
  - Conn-handle baseline recovery; timer-wheel slot growth ≤ 64.
  - `tcp.tx_payload_bytes >= 0.95 × emitted` (requires `obs-byte-counters`).
- Failure-bundle integration via T2's helper.
- Optional follow-up sub-task: I2 (in-process loopback driver) — paired-engine via `inject_rx_chain` if TAP+sudo proves CI-flaky. Not required for initial landing if TAP+sudo CI works.

**Pass criteria:** all counter relations hold per-bucket; CI green on master; deliberate-fail run produces a complete failure bundle.

**Estimate:** 1.5 agent-days.

**Dependencies:** T2.

---

### T4 — Suite #2 `pressure-conn-churn` (A11.1 Lane B, BLOCK-A11)

**Why:** catches conn-table-full accounting, time-wait reaper races, slot-recycle regressions invisible to single-conn `connect_close_cycle.rs`.

**Deliverable:**
- `crates/dpdk-net-core/tests/pressure_conn_churn.rs`.
- Per-PR: `N=64, 10 s`. Nightly: `N=256, 30 s`.
- Counter assertions:
  - `tcp.conn_open` delta == `tcp.conn_close` delta exactly (post-drain).
  - `tcp.conn_table_full == 0`.
  - `tcp.conn_time_wait_reaped` monotonic; no leak past `tcp_msl_ms × 4`.
  - All four mempool drifts ≤ 32; `tcp.mbuf_refcnt_drop_unexpected == 0`; `obs.events_dropped == 0`.
  - `tcp.tx_rst == 0`; timer-wheel slots growth ≤ 64.
  - Logged-only: actual measured open-per-sec rate.

**Pass criteria:** counter parity + leak invariants.

**Estimate:** 0.75 agent-day.

**Dependencies:** T2. Parallel with T3, T5, T6.

---

### T5 — Suite #10c `fin-storm-deterministic-smoke` (A11.1 Lane B, BLOCK-A11)

**Why:** isolates FIN teardown regressions that the bundled RST flood (Suite 10) masks.

**Deliverable:**
- `crates/dpdk-net-core/tests/pressure_fin_storm_smoke.rs` (separate file from 10a/10b which land in A11.4).
- 64 active conns, mid-transfer; peer FINs each; engine half-closes, app closes, conns drain. 5 s wall.
- Counter assertions:
  - `tcp.conn_open` delta == `tcp.conn_close` delta exactly.
  - Flow-table baseline post-drain.
  - No spurious RST. `obs.events_dropped == 0`.

**Pass criteria:** clean FIN-teardown semantics under deterministic load.

**Estimate:** 0.5 agent-day.

**Dependencies:** T2. Parallel with T3, T4, T6.

---

### T6 — I7 (`inject_rx_frame_with_ol_flags`) + Suite #3 `pressure-counter-parity-offload-matrix` (A11.1 Lane C, BLOCK-A11)

**Why:** the only suite that catches Pattern P4 counter-placement bugs mechanically. Will land as a deliberately failing test that pins the IP NIC-BAD double-bump in `l3_ip.rs:213-220` + `engine.rs:3928-3931` until that bug is fixed.

**Deliverable (split into two PRs within the lane):**

- **PR 6a:** add `inject_rx_frame_with_ol_flags` to `engine.rs` under `#[cfg(feature = "test-inject")]`. Shares the existing alloc/copy body of `inject_rx_frame` (`engine.rs:6295-6367`); inserts `unsafe { sys::shim_rte_mbuf_or_ol_flags(mbuf.as_ptr(), ol_flags) }` before `dispatch_one_rx_mbuf`. Smoke test under `#[cfg(test)]` confirms the OR-ed bit reaches `dispatch_one_real_mbuf`'s `ol_flags` read (`engine.rs:3757-3790`).

- **PR 6b:** `crates/dpdk-net-core/tests/pressure_counter_parity.rs`.
  - Four rows: A (offload-ON IP NIC-BAD), B (offload-ON L4 NIC-BAD), C (offload-OFF software-IP-bad), D (offload-OFF software-TCP-bad). 1000 frames per row, ≤ 5 s wall per row.
  - Expected post-fix counter deltas (Row A `eth+=1, ip+=1, tcp+=0`; Row B `eth+=1, ip+=0, tcp+=1`; Row C `eth+=0, ip+=1, tcp+=0`; Row D `eth+=0, ip+=0, tcp+=1`).
  - Side gates: `obs.events_dropped == 0`, `tcp.mbuf_refcnt_drop_unexpected == 0`.

**Operator decision required before PR 6b lands** (open question #2 in design): land Suite 3 as a deliberately failing regression test (forces a follow-up PR to fix the double-bump) OR fix the double-bump first then land Suite 3 green. Default if no decision: land 6b green by fixing `l3_ip.rs` to not bump `ip.rx_csum_bad` (since `engine.rs:3928-3931` already bumps it on `L3Drop::CsumBad`), preserving the post-fix expectation in 6b.

**Pass criteria:** all four rows assert exact deltas; CI green after the double-bump fix lands.

**Estimate:** 1.5 agent-days.

**Dependencies:** T2. Parallel with T3, T4, T5.

---

### T7 — I2 in-process loopback driver (optional, A11.1 Lane A follow-up if TAP+sudo flaky)

**Why:** if T3's TAP+sudo per-PR cadence proves CI-flaky, fall back to paired-engine via `inject_rx_chain` (Option A from design I2). Not blocking A11.1 if TAP+sudo CI is green.

**Deliverable:** virtual-clock + paired engine instances exchanging frames via `inject_rx_chain`. Reuses `engine_no_eal_harness.rs` scaffold.

**Pass criteria:** Suite 1's per-PR bucket runs on paired-engine in ≤ 25 s without sudo.

**Estimate:** 1.5 agent-days.

**Dependencies:** T2; only triggered if T3 CI is flaky.

---

### T8 — Suite #4 `pressure-loss-recovery` + ENOMEM bucket (A11.2)

**Why:** catches the void-retransmit accounting class (Part 3 STAGE-2 S1, S2) that current TAP smoke can't exercise.

**Deliverable:**
- `crates/dpdk-net-core/tests/pressure_loss_recovery.rs`. Nightly only.
- 12 baseline buckets (loss × delay × reorder) + 1 ENOMEM bucket (`tx_data_mempool_size_override` small, 1% loss + reorder).
- Counter assertions:
  - `tcp.tx_retrans > 0`, `tcp.tx_rto + tcp.tx_tlp > 0`.
  - **Void-retransmit oracle:** `InternalEvent::TcpRetrans` delta vs `tcp.tx_retrans` delta MUST match — under ENOMEM bucket, an event that fired but failed to allocate MUST NOT bump `tcp.tx_retrans`.
  - ENOMEM bucket: `eth.tx_drop_nomem > 0`, `tcp.tx_retrans` matches successful-only count.
  - `tcp.tx_payload_bytes` within 5%; `obs.events_dropped == 0`; `tcp.tx_rst == 0`.

**Pass criteria:** event-vs-counter parity holds even under forced allocation failure.

**Estimate:** 1.5 agent-days.

**Dependencies:** T2.

---

### T9 — Suite #5 `pressure-slow-receiver` (A11.2)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_slow_receiver.rs`. Nightly. 1 conn × 64 KiB writes, peer drains at 10 MB/s, 30 s.

**Counters:** `tcp.send_buf_full > 0`, `tcp.tx_zero_window` / `tcp.rx_zero_window` consistent, `tcp.tx_window_update > 0` post-drain, no spurious RST/conn-close, `tcp.tx_payload_bytes` matches drained.

**Estimate:** 0.75 agent-day. **Dependencies:** T2.

---

### T10 — Suite #6 `pressure-mempool-exhaustion` (A11.2)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_mempool_exhaustion.rs`. Nightly. Small RX mempool (256 mbufs), peer floods, app drains slowly, 30 s with 5 s mid-test pause.

**Counters:** `eth.rx_drop_nomem` monotonic; `tcp.rx_mempool_avail > 0` always; recovery within 10% of baseline post-pause; `tcp.mbuf_refcnt_drop_unexpected == 0`; `tcp.rx_pkts > 0` post-pause.

**Estimate:** 0.75 agent-day. **Dependencies:** T2.

---

### T11 — Suite #13 `pressure-socket-buffer-underrun` (A11.2)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_socket_buffer_underrun.rs`. Nightly. 1 conn, alternating 1-byte / 64 KiB reads/writes, 30 s.

**Counters:** `tcp.rx_partial_read_splits > 0` (`counters.rs:264-270`); no payload loss (CRC); no mempool drift; `obs.events_dropped == 0`; `tcp.mbuf_refcnt_drop_unexpected == 0`.

**Estimate:** 0.75 agent-day. **Dependencies:** T2.

---

### T12 — Suite #7 `pressure-reassembly-saturation` + I5 DSL polish (A11.3)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_reassembly_saturation.rs`. Nightly. Single conn, sustained reorder via `inject_rx_frame` to push byte-cap.

**Counters (corrected from design draft):** `tcp.recv_buf_drops > 0`; reassembly byte-occupancy never exceeds spec cap (test-only accessor from T1); `tcp.rx_reassembly_queued + tcp.rx_reassembly_hole_filled` parity; `tcp.mbuf_refcnt_drop_unexpected == 0`; `obs.events_dropped == 0`.

**Polish task:** review I5 DSL maturity after 7 suites land; defer convergence with `tools/bench-common/` to post-A11.1 (open question #6).

**Estimate:** 1 agent-day. **Dependencies:** T2.

---

### T13 — Suite #8 `pressure-sack-blocks` (A11.3)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_sack_blocks.rs`. Nightly. Inject 16 distinct holes, ≤ 2 s wall.

**Counters:** outgoing SACK ≤ 4 blocks; `tcp.tx_sack_blocks` exact emission count; oldest-block-evicted policy; no panic; no `tcp.rx_bad_option`; `obs.events_dropped == 0`.

**Estimate:** 0.5 agent-day. **Dependencies:** T2.

---

### T14 — Suite #14 `pressure-option-negotiation-churn` (A11.3)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_option_churn.rs`. Nightly. 256 sequential conn-open/close cycles with random {MSS, WSCALE, TS, SACK} combinations, 30 s.

**Counters:** negotiated state matches peer offer per cycle; no `ts_recent` carryover (test-only diag); no SACK carryover; `tcp.rx_bad_option == 0`; `obs.events_dropped == 0`.

**Estimate:** 1 agent-day. **Dependencies:** T2.

---

### T15 — Suite #9 `pressure-timer-storm` (A11.4)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_timer_storm.rs`. Nightly. 256 conns, force RTO storm via netem disconnect, 30 s.

**Counters (corrected):** `tcp.tx_rto > 0`, `tcp.tx_retrans > 0`; per-conn FSM state returns to Established post-recovery (test-only `flow_table().states()`); `tcp.tx_payload_bytes` strictly increases post-recovery; timer-wheel slot growth ≤ 64; `tcp.tx_rst == 0`; `obs.events_dropped == 0`.

**Estimate:** 1 agent-day. **Dependencies:** T2.

---

### T16 — Suite #10a + #10b `unmatched-rst-flood` + `matched-rst-flood` (A11.4)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_rst_flood.rs` (two `#[test]` fns: `unmatched_rst_flood`, `matched_rst_flood`). Nightly.
- 10a: 100k unsolicited RSTs at random 4-tuples; `tcp.rx_unmatched` exact, `tcp.conn_close == 0`, `obs.events_dropped == 0`, flow-table baseline.
- 10b: 64 active conns, RST per conn every 100 ms; `tcp.rx_rst` exact match count, `tcp.conn_rst` consistent, baseline recovery, `obs.events_dropped == 0`.

**Estimate:** 1 agent-day. **Dependencies:** T2.

---

### T17 — Suite #11 `pressure-recv-buf-saturation` (A11.4)

**Deliverable:** `crates/dpdk-net-core/tests/pressure_recv_buf_saturation.rs`. Nightly. Two buckets: 11a sustained slow-drain, 11b app-starvation pause.

**Counters:** see design Suite 11. App-starvation overflow semantics is operator-deferred (open question #4); document the chosen semantics in the suite header.

**Estimate:** 1 agent-day. **Dependencies:** T2.

---

### T18 — Suite #15 `pressure-listen-accept-exhaustion` + I6 ASAN axis (A11.4)

**Deliverable:**
- `crates/dpdk-net-core/tests/pressure_listen_accept_exhaustion.rs` (requires `test-server`). Nightly. 100k SYNs against 1 listener, 10 s wall.
- Counters: `tcp.conn_table_full > 0`; no listen-slot leak; accepted-flow three-way completes cleanly; `obs.events_dropped == 0`.
- I6: promote ASAN to a CI matrix axis. One green-CI run/day under `-Zsanitizer=address` for Suites 9, 10a, 10b, 11, 15.

**Estimate:** 1.5 agent-days. **Dependencies:** T2.

---

## Total estimate

| Phase | Tasks | Agent-days |
|-------|-------|------------|
| A11.0 | T0, T1, T2 | 1.5 |
| A11.1 | T3, T4, T5, T6 (T7 conditional) | 4.25 (+1.5 if T7) |
| A11.2 | T8, T9, T10, T11 | 3.75 |
| A11.3 | T12, T13, T14 | 2.5 |
| A11.4 | T15, T16, T17, T18 | 4.5 |
| **Total** | **18 tasks (19 with T7)** | **~16.5 agent-days** (~18 with T7) |

Under parallel dispatch (3 lanes within each sub-phase), wall time ≈ **6–7 calendar days** of focused work, gated by review checkpoints between phases.

---

## Per-PR vs nightly cadence

| Suite | Per-PR? | Nightly? | Wall budget per-PR |
|-------|---------|----------|--------------------|
| 1 `pressure-max-throughput` (1 bucket) | Yes | Yes (12 buckets × 2 offload arms) | ≤ 25 s |
| 2 `pressure-conn-churn` (N=64) | Yes | Yes (N=256) | ≤ 12 s |
| 3 `pressure-counter-parity-offload-matrix` | Yes | (same) | ≤ 25 s |
| 10c `fin-storm-deterministic-smoke` | Yes | (same) | ≤ 10 s |
| 4 `pressure-loss-recovery` | No | Yes | n/a |
| 5 `pressure-slow-receiver` | No | Yes | n/a |
| 6 `pressure-mempool-exhaustion` | No | Yes | n/a |
| 13 `pressure-socket-buffer-underrun` | No | Yes | n/a |
| 7 `pressure-reassembly-saturation` | No | Yes | n/a |
| 8 `pressure-sack-blocks` | No | Yes | n/a |
| 14 `pressure-option-negotiation-churn` | No | Yes | n/a |
| 9 `pressure-timer-storm` | No | Yes | n/a |
| 10a `unmatched-rst-flood` | No | Yes | n/a |
| 10b `matched-rst-flood` | No | Yes | n/a |
| 11 `pressure-recv-buf-saturation` | No | Yes | n/a |
| 15 `pressure-listen-accept-exhaustion` | No | Yes | n/a |
| 12 `pressure-pmtu-blackhole` | n/a (Layer H placeholder) | Layer H nightly | n/a |

**Per-PR Tier-1 wall-time total:** ≤ 90 s pressure-suite wall + EAL/TAP overhead. Invocation: `cargo test -p dpdk-net-core --test 'pressure_*' --features pressure-test,test-inject,test-server`.

**Codex #5 caveat:** Suites 4–11 and 13–15 will be re-evaluated for per-PR promotion after measured CI wall times exist; do not promote without data.

---

## Open operator decisions

These are residuals from codex review that block specific tasks; default behavior listed where no decision is required to start work.

1. **Suite 1 per-PR locus (T3):** TAP+sudo (default, recommended) vs paired-engine via I2 (T7) only if TAP+sudo CI flaky.
2. **Suite 3 land-fail-then-fix vs land-fix-then-test (T6):** default is land-fix-then-test — fix the IP NIC-BAD double-bump (`l3_ip.rs:213-220`: drop the `ip.rx_csum_bad.fetch_add` since `engine.rs:3928-3931` already bumps it on `L3Drop::CsumBad`) in PR 6b alongside Suite 3.
3. **`pressure-test` feature scope (T1):** default is orthogonal to `test-inject` and `test-server`; suites pass `--features pressure-test,test-inject` or `--features pressure-test,test-server` explicitly.
4. **App-starvation overflow semantics (T17):** default is hard `obs.events_dropped == 0` (size soft-cap to absorb 5 s pause); operator may instead choose to assert post-resume liveness only.
5. **Pressure-only counters (T12):** default is do NOT add `tcp.rx_drop_reassembly_overflow`; assert byte-cap path (`tcp.recv_buf_drops`) only. Operator may opt in to a hole-count counter as a follow-up.
6. **DSL convergence with `tools/bench-common/`:** deferred to post-A11.1; revisit after T12 ships.
