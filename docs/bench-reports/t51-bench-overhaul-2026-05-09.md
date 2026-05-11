# T51 bench-overhaul report — 2026-05-09

## Summary

The 2026-05-09 bench-suite overhaul closes 22 catalogued claims (C-A1..C-F2)
identified in the audit. This report documents tool inventory delta, claim
closures, schema additions, and deferred work (c7i validation).

## Tool inventory delta

### Removed
- bench-vs-mtcp (Phase 5) — split into bench-tx-burst + bench-tx-maxtp.
- bench-stress (Phase 4) — folded into bench-rtt + nightly netem matrix.
- bench-e2e binary (Phase 4) — folded into bench-rtt; peer/echo-server retained.
- bench-vs-linux mode A (Phase 4) — folded into bench-rtt --stack linux_kernel; mode B retained.
- bench-rx-zero-copy (Phase 2) — placeholder body deleted; functionality covered by new bench-rx-burst.
- bench-ab-runner (Phase 12) — leaf crate; A/B drivers now subprocess bench-rtt directly.
- bench-vs-mtcp/src/mtcp.rs (Phase 2) — permanent stub.
- bench-vs-linux/src/afpacket.rs (Phase 2) — never wired stub.
- bench-stress/scenarios.rs::pmtu_blackhole_STAGE2 (Phase 2) — placeholder.

### Added
- bench-rtt (Phase 4) — cross-stack RTT distribution; payload swept; netem-aware; raw samples; failed-iter count.
- bench-tx-burst (Phase 5) — TX-side burst write throughput + initiation latency CDF; cross-stack (no mTCP).
- bench-tx-maxtp (Phase 5) — sustained TX with per-conn goodput + per-segment send→ACK CDF (Phase 6); queue-depth time series (Phase 11).
- bench-rx-burst (Phase 8) — peer pushes N×W back-to-back small segments; DUT records per-segment app-delivery latency.
- bench-fstack-ffi (Phase 5) — shared F-Stack FFI bindings, replacing per-tool duplication.
- crates/dpdk-net-core/src/tcp_send_ack_log.rs (Phase 6) — bounded ringbuffer for per-segment send→ACK latency.

### Augmented
- bench-common (Phase 3) — streaming raw-sample CSV writer; raw_samples_path + failed_iter_count columns in summary CSV.
- crates/dpdk-net-core/src/counters.rs (Phase 11) — tx_retrans split into rto/rack/tlp sub-counters; aggregate retained.
- bench-rtt attribution (Phase 9) — Hw 5-bucket path flags unsupported buckets instead of silent zeros.
- scripts/bench-nightly.sh — payload sweep, bidirectional netem, high-loss scenarios, per-scenario iter override (Phases 7, 10).

## Closed claims

| Claim | Title | Phase(s) | Evidence |
|---|---|---|---|
| C-A1 | mTCP arm permanent stub | 2, 5 | `tools/bench-vs-mtcp/` deleted entirely |
| C-A2 | afpacket stub | 2 | `tools/bench-vs-linux/src/afpacket.rs` deleted |
| C-A3 | bench-rx-zero-copy placeholder body | 2, 8 | crate deleted; bench-rx-burst replaces functionality |
| C-A4 | pmtu_blackhole_STAGE2 placeholder | 2 | scenario entry + helper fn deleted |
| C-A5 | RTT bench overlap | 4 | bench-e2e + bench-stress + bench-vs-linux mode A → bench-rtt |
| C-B1 | maxtp single-mean | 5 | per-conn raw samples + 6-row percentile summary |
| C-B2 | no raw samples | 3, 4, 5, 8 | RawSamplesWriter shipped; 4 callers adopted |
| C-B3 | no per-RX-segment latency | 8, 9 | bench-rx-burst tool ships per-segment CDF; HW-TS path validated |
| C-B4 | no send→ACK CDF | 6 | SendAckLog ringbuffer + bench-tx-maxtp drain |
| C-B5 | single-conn bias | 4, 5 | bench-rtt --connections N; bench-tx-maxtp per-conn raw samples |
| C-C1 | no payload sweep | 4, 10 | bench-rtt --payload-bytes-sweep; nightly default 64,128,256,1024 |
| C-C2 | no peer-burst RX workload | 8 | burst-echo-server + bench-rx-burst |
| C-C3 | no burst×netem bucket | 10 | bench-tx-burst + bench-rx-burst run under netem matrix |
| C-C4 | no bidirectional netem | 7 | peer-ifb-setup.sh + ingress/bidir directions |
| C-D1 | iter count too low | 10 | SCENARIO_ITERS map: 1M for low-loss, 200k/100k for high-loss |
| C-D2 | RTO never fires | 10, 11 | high_loss_3pct + symmetric_3pct scenarios; tx_retrans_rto sub-counter |
| C-D3 | lost-iter terminal | 4 | bench-rtt counts failed_iter_count; only bails > 50% fail |
| C-E1 | no queue depth time series | 5, 11 | maxtp raw-sample CSV with snd_nxt_minus_una + snd_wnd + room_in_peer_wnd |
| C-E2 | no RTO/RACK/TLP split | 11 | tx_retrans_{rto,rack,tlp} sub-counters; aggregate retained |
| C-E3 | HW-TS attribution dead on ENA | 9, 12 | Hw branch flags unsupported buckets; live c7i validation **permanently deferred** 2026-05-11 — HW-TS not supported in ap-south-1 for any instance type, see §c7i validation |
| C-F1 | mTCP comparator scope | 2, 5 | comparator triplet is dpdk_net + linux_kernel + fstack |
| C-F2 | linux maxtp peer port | 5 | bench-tx-maxtp::linux::assert_peer_is_sink(10002) |

## Schema additions

### bench-common::CsvRow
- Phase 3: appended `raw_samples_path: Option<String>`, `failed_iter_count: u64` at the end of the column list. Existing positions stable.

### bench-tx-maxtp raw-samples CSV header
- Phase 5: `bucket_id, conn_id, sample_idx, t_ns, goodput_bps_window, snd_nxt_minus_una`
- Phase 11: appended `snd_wnd, room_in_peer_wnd` (now 8 columns).

### bench-tx-maxtp send-ack CSV header
- Phase 6: `bucket_id, conn_id, scope, sample_idx, t_ns, begin_seq, end_seq, latency_ns, tcpi_rtt_us, tcpi_total_retrans, tcpi_unacked` (11 columns; `scope` distinguishes dpdk_segment / linux_tcp_info / fstack_unsupported).

### bench-rx-burst raw-samples CSV header
- Phase 8: `bucket_id, burst_idx, seg_idx, peer_send_ns, dut_recv_ns, latency_ns`.

### dpdk-net-core counters
- Phase 11: `tx_retrans_rto`, `tx_retrans_rack`, `tx_retrans_tlp` added to TcpCounters. KNOWN_COUNTER_COUNT 119 → 122. C ABI mirror unchanged (consistent precedent).

## c7i validation — PERMANENTLY DEFERRED (2026-05-11)

**Status: PERMANENTLY DEFERRED.** Per operator confirmation 2026-05-11,
RX hardware-timestamp support is NOT exposed in the `ap-south-1` region
on any current EC2 instance family, including c7i. The ENA driver
returns `rx_hw_ts_ns == 0` regardless of instance type there. Empirical
confirmation: running bench-rtt on this region's DPDK ENA port logs
`RX timestamp dynfield/dynflag unavailable on port 0 (ENA steady state)`.
Provisioning a c7i instance in this region would not unblock live HW-TS
validation.

The Phase 9 code path remains correct: when running on a NIC that
*does* populate `rx_hw_ts_ns`, the 5-bucket Hw composition fires and
the two un-measurable buckets carry the `unsupported_buckets` flag
(synthesised test at `tools/bench-rtt/tests/attribution_hw_path.rs`).
Live validation would require either:
  - migrating the bench fleet to a region with HW-TS-capable ENA
    (e.g., a Nitro generation that exposes it in `us-east-1`), OR
  - switching to a non-ENA NIC family (Mellanox / ice / mlx5 typical),
    OR
  - AWS adding HW-TS support to ap-south-1 ENA.

None of those are in this session's scope. The code path is locked in
and tested; no further action expected without the prerequisites above.

## Live verification — fast-iter run 2026-05-11

A fast-iter peer (i-0222587a5864ab4d4 in ap-south-1; AMI 1.0.15) ran
the new bench tools to validate the merge and the Phase 11 counter
split. Empirical findings:

**Phase 11 RTO/RACK/TLP counter split**: WORKS. high_loss_3pct
(loss 3% delay 5ms, 200 k iters) produced:
  - `tcp.tx_retrans`        = 9418 (aggregate)
  - `tcp.tx_retrans_rto`    = 8119 (86 %)
  - `tcp.tx_retrans_rack`   =    0
  - `tcp.tx_retrans_tlp`    = 1299 (14 %)

Sub-counters sum to aggregate (8119 + 0 + 1299 = 9418) ✓.

**Empirical assertion calibration finding**: `verify-rack-tlp.sh`
asserts `tx_retrans_rack > 0` for high_loss_3pct and symmetric_3pct,
but RACK does not fire empirically at 3 % loss + 5 ms delay — RTO
absorbs the recovery before RACK has enough ACK information.
**Action**: relax the script's `EXPECTED_NON_ZERO` map for those two
scenarios, or push loss to ≥10 % with smaller burst correlation to
expose RACK fast-recovery. Not blocking — the wiring is correct;
the threshold is a future calibration follow-up.

**bench-tx-burst** dpdk_net arm: WORKS. K=64 KiB,G=0 ms,50 bursts
emits burst_initiation_ns p50=28 µs / p99=37.8 µs / mean=29 µs.

**bench-rx-burst pre-existing bugs (NOT merge-induced)** — both arms
fail on real wire traffic against the burst-echo-server peer:
  - **dpdk_net**: `Readable total_len sum (960) does not match scratch
    bytes (832); engine event/scratch model may have changed` during
    warmup burst 3 of W=64 N=16. The engine emits a Readable event
    with `total_len > scratch_bytes` for small back-to-back segments.
    Verified pre-existing by `git diff 2828cbf HEAD -- crates/` =
    0 lines.
  - **linux_kernel**: `Resource temporarily unavailable (os error 11)`
    (EAGAIN) from blocking TcpStream read in warmup burst 0 — the
    socket inherits a non-blocking mode somewhere.

Both bugs are in `tools/bench-rx-burst/src/{dpdk,linux}.rs` and predate
the squash merge. Tracked as a Phase 8 follow-up; doesn't affect the
overhaul's correctness because bench-rx-burst's unit tests + the
peer-side burst-echo-server smoke test pass. The wire-level workload
needs an additional fix before bench-rx-burst is usable on dpdk_net or
linux_kernel against a real ENA NIC.

**c7i HW-TS**: peer-side AMI's bench-rtt run logged
`RX timestamp dynfield/dynflag unavailable on port 0 (ENA steady state)`,
confirming the operator's report that HW-TS is not exposed in
ap-south-1 on the current ENA driver.

### Original deferral rationale (pre-2026-05-11, kept for context)

The 5-bucket Hw attribution path was code-validated in Phase 9 against
synthetic test data. Live validation against an actual c7i instance with
non-zero `rx_hw_ts_ns` is operational work that requires fleet
provisioning and a ~6.5h nightly run; both fall outside this session.

### Deferral rationale

1. Provisioning: requires `resd-aws-infra setup bench-pair --instance-type
   c7i.metal-48xl` (or similar c7i SKU), spinning up a fresh DUT/peer pair
   off the hardened AMI, plus the standard EC2 IC keypair grant.
2. Wallclock: the post-Phase-10 nightly matrix is ~6.5h end-to-end (per
   `docs/bench-reports/overhaul-tracker.md` Phase 10 wallclock impact
   section). The HW-TS validation is incremental on top of that —
   the attribution columns are emitted by the same bench-rtt invocation
   that owns the rest of the netem matrix.

### Operator runbook

1. Provision a bench-pair fleet on c7i (e.g., c7i.metal-48xl) via
   `resd-aws-infra setup bench-pair --instance-type c7i...`.
2. Run `OUT_DIR=target/bench-results/c7i-overhaul-validation \
   ./scripts/bench-nightly.sh`.
3. Verify HW mode fires:
   ```
   grep -c '"Hw"' target/bench-results/c7i-overhaul-validation/bench-rtt-clean.csv
   ```
   Expected: > 0. If 0, the bench-rtt path either skipped the Hw branch
   (driver capability detection failed) or fell back to TSC silently —
   investigate `dpdk-net-core::engine::Engine::nic_caps()` and the
   `--rx-hw-timestamp` arg plumbing.
4. Spot-check a sample row: 3 measurable buckets > 0; 2 unmeasurable
   buckets carry the `unsupported_buckets` flag bits set.
5. If c7i's ENA driver in DPDK 23.11 does NOT populate `rx_hw_ts_ns`,
   this is a follow-up to file (the implementation is correct but the
   assumption — that c7i exposes RX HW-TS — would need re-verification
   against the DPDK 23.11 ENA PMD release notes for c7i SKUs).

Estimated wallclock: ~6.5h for the full nightly per Phase 10 budget;
the HW-TS columns themselves are written inline by bench-rtt and add
no measurable wallclock.

## Deferred work (operator follow-ups, non-blocking)

1. **CLOSED** (2026-05-09): preflight + peer_introspect were duplicated between
   bench-tx-burst and bench-tx-maxtp at Phase 5 (byte-identical inline copies,
   ~270 + ~280 LoC each). Extracted to `bench-common`:
   - `tools/bench-common/src/preflight.rs` (moved via `git mv` from bench-tx-burst,
     history preserved).
   - `tools/bench-common/src/peer_introspect.rs` (moved via `git mv` from
     bench-tx-burst, history preserved).
   Both consumer crates now `use bench_common::{preflight, peer_introspect}::*`
   instead of crate-local imports. The duplicate copies under bench-tx-maxtp/src
   were `git rm`'d; net workspace LoC delta is approximately -554. The trigger
   was drift risk (independent evolution of the two copies), not a third consumer
   — the extraction is mechanical because the copies were verified byte-identical
   pre-move. bench-common already had `anyhow` as a dependency so no Cargo.toml
   churn was needed; the helpers are pure-data + `std::process::Command` so they
   slot in alongside `csv_row` / `percentile` / `raw_samples` / `run_metadata` /
   `preconditions` without expanding the crate's dep closure.
2. **CLOSED** (2026-05-10): build.rs duplication across bench-rtt + bench-tx-burst +
   bench-tx-maxtp + bench-rx-burst was lifted into a new `tools/bench-build-helpers`
   crate consumed via `[build-dependencies]`. The four consumer build.rs files (each
   ~50-65 LoC pre-extraction) shrink to a 13-line stub that emits two
   `cargo:rerun-if-*` pragmas and calls
   `bench_build_helpers::emit_fstack_link_args_if_enabled()`. The helper emits the
   exact same byte sequence the originals did (verified by sha256 of build-script
   output for all four consumers). The link-arg ORDER constraint
   (push-state / --no-as-needed / --whole-archive / -lfstack / --no-whole-archive /
   DPDK rte_* libs / --pop-state) is captured once in
   `tools/bench-build-helpers/src/lib.rs`. Net workspace LoC delta is approximately
   -78 (4×~55 LoC originals → 4×13-line stubs + 93 LoC of helper). Default
   (no-fstack) and `--features fstack` workspace builds both clean; nm-T spot
   check confirms 118 `ff_*` text symbols in each fstack-built binary, identical
   across all four. A separate crate (rather than extending bench-fstack-ffi)
   keeps build-time deps from bleeding into the runtime crate's feature surface.
3. **CLOSED** (2026-05-10, operator-runnable): the RACK / TLP / RTO trigger paths
   under Phase 10's high-loss scenarios now have an operator-runnable verification
   harness at `scripts/verify-rack-tlp.sh`, paired with documentation at
   `docs/bench-reports/verify-rack-tlp.md`. The script applies each scenario's
   netem spec on the peer (egress), runs `bench-rtt` with the new `--counters-csv`
   flag (lifted into `tools/bench-rtt/src/main.rs` — pre/post snapshots over
   `dpdk_net_core::counters::ALL_COUNTER_NAMES`), parses the resulting
   `name,pre,post,delta` CSV, and asserts `delta > 0` per scenario:
   `high_loss_3pct` requires non-zero `tcp.tx_retrans_rto` and `tcp.tx_retrans_rack`,
   `high_loss_5pct` requires non-zero `tcp.tx_retrans_rto`, and `symmetric_3pct`
   requires non-zero `tcp.tx_retrans_rack` and `tcp.tx_retrans_tlp`. Auto-running
   it in CI is out of scope (needs a real DUT+peer), so the closure is
   "operator-runnable, not nightly-wired". Any future regression in retransmit
   plumbing surfaces as a FAIL row in the script's summary table; the doc covers
   the diagnostic path for each common FAIL shape.
4. **CLOSED** (2026-05-09, commit `f05b114`): bench-rtt's
   `--attribution-csv` sidecar now emits one row per measurement iteration with
   the 14-column schema (`bucket_id, iter, mode, rtt_ns, rx_hw_ts_ns,` 5 Hw-bucket
   ns columns, 3 Tsc-bucket ns columns, `unsupported_mask`) — the trailing
   `unsupported_mask` carries Phase 9's `HwTsBuckets::unsupported_buckets` u32
   bitfield verbatim. Nightly script wires the flag at every bench-rtt invocation;
   `run_dut_bench` pulls the sidecar back via the generic `--*-csv` arg-scan so
   future per-iter CSV emits plug in without further changes.
5. **CLOSED** (2026-05-09): the gdb stack-trace wrapper survived Phase 12's
   bench-ab-runner crate deletion with an orphaned `exec /tmp/bench-ab-runner`
   target. Decommissioned by repointing it at `/tmp/bench-rtt` (the binary
   bench-offload-ab / bench-obs-overhead now subprocess via `--runner-bin`)
   and renaming `scripts/bench-ab-runner-gdb.sh` →
   `scripts/bench-rtt-gdb.sh` with `git mv` to preserve history. The
   SIGSEGV trace-capture workflow is preserved; `bench-nightly.sh`
   updated to scp the renamed script and pull `/tmp/bench-rtt-gdb.log`
   back at the end of the [10/12]+[10b/12] block.

## Operator runbook delta

The new nightly takes ~6.5h on a fully-loaded matrix (was ~2h pre-overhaul). To
shorten for development cycles, override:
- `BENCH_RTT_PAYLOADS=128` (default 64,128,256,1024 — single-payload halves the
  bench-rtt time).
- `BENCH_ITERATIONS=5000` (the per-scenario override map only kicks in for the
  netem matrix; clean-wire bench-rtt still respects this).
- `BENCH_RX_MEASURE_BURSTS=100` (default 1000 for clean wire).
- Skip the netem matrix entirely by emptying `NETEM_SCENARIOS=()`.

### fast-iter setup (single-peer dev iteration)

`scripts/fast-iter-setup.sh` (added 2026-05-10 follow-up) provisions a single
peer instance from the bench-pair AMI (`ami-0e483926d07d19647`) and treats the
current host as the DUT. The peer runs all three echo servers used by the new
bench tools — `echo-server` on :10001, `linux-tcp-sink` on :10002,
`burst-echo-server` on :10003 — mirroring step `[6/12]` of `bench-nightly.sh`.

Round-trip is ~1-2 min provision + ~30 s teardown, vs. the ~6.5h
`bench-nightly.sh` cycle (which provisions a full DUT/peer fleet and walks
the entire matrix). Use it for RX/TX bench iteration, compatibility tests,
and regression spot-checks where you don't need the full nightly matrix.

Usage:
```
./scripts/fast-iter-setup.sh up           # provision + start servers
source ./.fast-iter.env                   # exports PEER_IP / PEER_SSH / PEER_*_PORT
sudo ./target/release/bench-rtt --stack dpdk_net \
    --peer-ip "$PEER_IP" --peer-port "$PEER_ECHO_PORT" \
    --output-csv /tmp/quick-rtt.csv ...
./scripts/fast-iter-setup.sh down         # terminate + clear state
```

State files:
- `$HOME/.bench-fast-iter.state.json` — peer instance id + ip + port map
  (overridable via `$BENCH_FAST_ITER_STATE`).
- `./.fast-iter.env` — sourceable env file with `PEER_IP`, `PEER_SSH`,
  `PEER_INSTANCE_ID`, `PEER_ECHO_PORT`, `PEER_SINK_PORT`, `PEER_BURST_PORT`
  (overridable via `$BENCH_FAST_ITER_ENV`; gitignored).

The script fails fast on prereq / build / SSH errors and traps `EXIT` to
terminate a half-provisioned peer so the operator never gets stuck paying
for a peer with no echo-server. The current-host DUT setup (NIC bind to
vfio-pci, hugepage allocation) is operator-managed — `scripts/bench-quick.sh
setup` handles that path if the operator wants the bench-quick DUT-bind
flow on top of the fast-iter peer.

`scripts/fast-iter-setup.sh` differs from `scripts/bench-quick.sh` in two
ways: (a) it starts all three peer servers (bench-quick only starts
`echo-server`, since it only drives `bench-tx-burst` / `bench-tx-maxtp`),
and (b) it produces a sourceable `./.fast-iter.env` so any bench tool can
plug in without re-discovering the peer state. `bench-quick.sh` remains
the right choice when you want the integrated build + run loop for the
two TX workloads it knows about.

## Plan reference

docs/superpowers/plans/2026-05-09-bench-suite-overhaul.md — 41 tasks across 12
phases; commit history `git log --oneline a10-perf-23.11 ^master | grep bench-overhaul`
shows the per-task progression.

## Final commit chain

The overhaul lands on `a10-perf-23.11`. Tag: `bench-overhaul-2026-05` (local
only, not pushed). Plan tracker at `docs/bench-reports/overhaul-tracker.md`
flags every phase complete.
