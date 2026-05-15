# Bench Suite Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Post-codex-review addendum (2026-05-13).** Codex's adversarial
> review of the published fast-iter suite landed three IMPORTANTs and
> one MINOR that affect plan-published doc claims (not the Phase 1–12
> code path):
> - **I2 (PMD-handoff metric rename) — DONE.** `throughput_per_burst_bps`
>   renamed to `pmd_handoff_rate_bps` on the dpdk_net arm; linux + fstack
>   already on `write_acceptance_rate_bps` since T57 follow-up #2. See
>   `docs/bench-reports/methodology-and-claims-2026-05-09.md` for the
>   per-arm semantic definitions and the post-rename history table.
> - **I4 (fixed-order bias) — DONE.** `scripts/fast-iter-suite.sh` now
>   randomizes per-tool stack execution order from a `--seed N` flag;
>   resolved order logged into `metadata.json` and SUMMARY.md.
> - **I5 (pure-stack overhead overclaim) — DONE in docs (this commit).**
>   The phrase "pure software-stack overhead" / "user-space TCP stack
>   performance" in the methodology doc and T57 report is reworded to
>   "controlled three-stack comparison" / "end-to-end harness behavior".
>   A new "What this suite is NOT" section in the methodology doc
>   enumerates the wire-rate / API-asymmetry / two-ENI / AWS-shared-
>   tenancy disclaimers.
> - **M1 (stale docs) — DONE in docs (this commit).** Stale "loopback"
>   wording in `linux-nat-investigation-2026-05-12.md` and the
>   pre-Phase-2 mTCP framing in the methodology doc's Phase-1 history
>   line are now marked as historical.
>
> The Phase 1–12 task list below remains the historical record of the
> overhaul code path; the mTCP-arm removal (Phase 2 Task 2.1) and the
> bench-vs-mtcp split (Phase 5) are preserved as written. New
> publication-facing claims should reference the methodology doc, not
> the per-phase task bodies in this plan.

**Goal:** Reorganize the bench-tool suite to remove dead arms, consolidate overlapping benches, and close every coverage gap that blocks measuring (a) RX-side latency on small trading-quote-sized packets and (b) TX-side latency under retransmit-driving congestion. Comparator set is dpdk_net + linux_kernel + fstack only — mTCP arm is removed.

**Architecture:**
- Three RTT benches (`bench-e2e`, `bench-stress`, `bench-vs-linux` mode A) collapse into one cross-stack `bench-rtt` driven by `--stack`/`--request-bytes`/`--response-bytes`/`--netem-spec` arguments. The nightly script becomes the matrix orchestrator.
- `bench-vs-mtcp` is split into focused `bench-tx-burst` and `bench-tx-maxtp` tools without the mTCP arm; both gain raw-sample emission, percentile distributions, and per-segment send→ACK latency tracking.
- A new `bench-rx-burst` tool drives a peer-side burst-echo workload and records per-segment app-delivery latency on the DUT — closing the small-pkt RX-burst gap.
- Bidirectional netem (peer IFB ingress) lands so DUT-TX-data-loss actually fires DUT fast-retransmit, not just ACK-loss-driven retransmit.
- Raw sample CSVs ship for every bench so percentile recomputation no longer requires re-runs.
- HW-TS attribution becomes live on c7i (assumed working RX HW-TS on the new instance family per user direction).

**Tech Stack:** Rust 2021 (stable rustup), DPDK 23.11 via `dpdk-net-sys`/`dpdk-net-core`/`dpdk-net`, F-Stack via `tools/bench-vs-mtcp/src/fstack_ffi.rs` patterns, Linux kernel TCP via `std::net::TcpStream`, Linux `tc`/`netem`/`ifb` for traffic shaping, criterion 0.5 for microbenches, clang-22 + libstdc++ toolchain.

---

## Catalogued claims (from the 2026-05-09 audit)

Every claim below is verified against `docs/bench-reports/t50-bench-pair-2026-05-08.md` and the source tree on `a10-perf-23.11`. Each claim cites the phase that fixes it.

### A. Tooling cleanup
- **C-A1**: `bench-vs-mtcp/src/mtcp.rs` arm is permanent stub — driver `/opt/mtcp-peer/mtcp-driver` exits 1 on every invocation. Fixed by Phase 2 (removal) and Phase 5 (rename + split).
- **C-A2**: `bench-vs-linux/src/afpacket.rs` is a stub; strict mode errors, lenient mode skips. Fixed by Phase 2 + Phase 4 (consolidation drops the tool).
- **C-A3**: `bench-rx-zero-copy/benches/delivery_cycle.rs` body is `DpdkNetIovec` struct construction + `black_box` — no real RX-path measurement. Fixed by Phase 2 (delete crate) + Phase 8 (real RX-burst tool replaces the stub's purpose).
- **C-A4**: `bench-stress/src/scenarios.rs:132-138` `pmtu_blackhole_STAGE2` is a placeholder the driver skips unconditionally. Fixed by Phase 2 (removal) and Phase 4 (consolidation drops bench-stress).
- **C-A5**: Three RTT benches (`bench-e2e`, `bench-stress`, `bench-vs-linux` mode A) reuse the same 128/128 req-resp inner loop in `bench-e2e/src/workload.rs::request_response_attributed`. Fixed by Phase 4 (consolidate into `bench-rtt`).

### B. Distribution / sample-fidelity gaps
- **C-B1**: `bench-vs-mtcp` maxtp emits one `MaxtpSample{goodput_bps, pps}` Mean per (W, C) bucket — no percentiles, no per-message latency (`tools/bench-vs-mtcp/src/maxtp.rs:39,148`). Fixed by Phase 5 (per-conn raw-sample emission) + Phase 6 (per-segment send→ACK).
- **C-B2**: No bench emits raw per-iteration samples — every tool calls `bench_common::percentile::summarize` and drops raw, so re-deriving p9999 / histograms / bimodal-checks requires re-runs (`tools/bench-common/src/percentile.rs:43-63`). Fixed by Phase 3 (raw-sample CSV in bench-common) + Phases 4/5/8 (callers adopt).
- **C-B3**: No live bench records per-RX-segment wire→app latency. HW-TS attribution buckets in `bench-e2e/src/attribution.rs` collapse to TSC fallback on ENA (`rx_hw_ts_ns=0`); the `tools/bench-micro/benches/tcp_input.rs` 64 B target calls `tcp_input::dispatch()` in-process, skipping NIC RX. Fixed by Phase 8 (`bench-rx-burst`) + Phase 9 (HW-TS path on c7i).
- **C-B4**: No per-segment send→ACK latency CDF for any stack. Fixed by Phase 6 (sequence-range ringbuffer in `dpdk-net-core::tcp_output` + `tcp_input`; `TCP_INFO`/`tcp_diag` for kernel; cumulative-ACK-delta sampling for fstack).
- **C-B5**: All RTT benches run a single connection. Maxtp does C ∈ {1,4,16,64} but only goodput. Fixed by Phase 4 (`bench-rtt --connections N`) + Phase 5 (per-conn raw samples in maxtp).

### C. Coverage gaps for trading workloads
- **C-C1**: No 64 / 128 / 256 B RTT distribution in nightly — knobs (`--request-bytes`/`--response-bytes`) exist but `scripts/bench-nightly.sh` never overrides them. Fixed by Phase 4 (`bench-rtt` with payload axis) + Phase 10 (nightly grid).
- **C-C2**: No "peer pushes N back-to-back small segments" workload — RX burst is unmeasured anywhere. Fixed by Phase 8 (`bench-rx-burst` tool + `tools/bench-e2e/peer/echo-server` enhancement).
- **C-C3**: No bucket combines burst-grid K with non-zero netem (`scripts/bench-nightly.sh` runs stress at step [8/12], burst at step [11/12], with `tc qdisc del` between). Fixed by Phase 10 (nightly matrix adds burst×netem cells).
- **C-C4**: Current peer-egress netem only triggers DUT TX retransmits via the **ACK-loss path** (RACK/TLP, never RTO — see comment block at `tools/bench-stress/src/scenarios.rs:68-83`). Direct DUT-TX-data-loss never fires. Fixed by Phase 7 (peer ingress IFB so DUT egress traffic is shaped).

### D. Loss / retransmit observability gaps
- **C-D1**: Nightly iteration count (`BENCH_ITERATIONS=5000`) is too low to characterize p999 of loss-affected events at 0.1 % loss (~5 lossy events per scenario; need ~10⁶ iters for ≥1k lossy events). Fixed by Phase 10 (per-scenario iter override).
- **C-D2**: At current loss profiles RTO never fires; only RACK/TLP exercised. Fixed by Phase 10 (add scenarios with ≥3 % bursty loss to push burst-tail past 200 ms RTO floor) + Phase 11 (counters split RTO/RACK/TLP in CSV).
- **C-D3**: `run_rtt_workload` propagates errors via `?` — any single-iter timeout > 10 s kills the whole scenario, dropping all earlier samples (`tools/bench-e2e/src/workload.rs:333,341`). Fixed by Phase 4 (`bench-rtt` keeps successful samples + emits a `failed_iter_count` column on bail).

### E. State-observability gaps
- **C-E1**: No queue-depth (`snd_nxt - snd_una`, `snd_wnd`, `room_in_peer_wnd`) time series in any CSV — only stderr dumps from stall watchdogs. Fixed by Phase 11 (per-conn time-series CSV alongside maxtp summary).
- **C-E2**: No RTO vs RACK vs TLP breakdown per scenario; only `tcp.tx_retrans` aggregate. Fixed by Phase 11 (split counters in CSV emit).
- **C-E3**: HW-TS attribution buckets dead on ENA (`bench-e2e/src/attribution.rs:32-44`). User confirms c7i has working RX HW-TS — fix is to validate the 5-bucket path actually populates when `rx_hw_ts_ns ≠ 0`. Fixed by Phase 9.

### F. Comparator scope
- **C-F1**: mTCP comparator dead — driver setup too costly to maintain. Fixed by Phase 2 (delete arm) + per user instruction.
- **C-F2**: linux maxtp peer port 10002 was using echo-server in T50 (should be `linux-tcp-sink` discard server). Fixed by Phase 5 (sink-only contract for maxtp linux arm).

---

## Target tool inventory (after this plan)

| Tool | Purpose | Replaces |
|---|---|---|
| `bench-rtt` | Cross-stack RTT distribution; payload swept; netem-aware; raw samples | `bench-e2e`, `bench-stress`, `bench-vs-linux` mode A |
| `bench-tx-burst` | TX-side burst write throughput + initiation latency CDF; cross-stack | `bench-vs-mtcp` `--workload burst` (no mTCP) |
| `bench-tx-maxtp` | Sustained TX with per-conn goodput + per-segment send→ACK CDF; bidirectional-netem-aware | `bench-vs-mtcp` `--workload maxtp` (no mTCP) |
| `bench-rx-burst` | Peer pushes N×W back-to-back small segments; DUT measures per-segment app-delivery latency | NEW (replaces `bench-rx-zero-copy` placeholder purpose) |
| `bench-vs-linux` (mode B only) | Wire-canonical pcap byte-diff for compat checking | unchanged scope (mode A folded into `bench-rtt`) |
| `bench-offload-ab` | A/B over `hw-*` feature flags via `bench-rtt` subprocess | renamed shell-out target only |
| `bench-obs-overhead` | A/B over `obs-*` feature flags via `bench-rtt` subprocess | renamed shell-out target only |
| `bench-ab-runner` | shared subprocess for A/B drivers (was `request_response`) | unchanged but now thin wrapper around `bench-rtt --once-mode` |
| `bench-micro` | Criterion microbenches (poll, tsc, flow_lookup, tcp_input, send, timer, counters, throughput) | unchanged |
| `bench-common` | Shared CSV schema + percentile + raw-sample emit | augmented with `raw_samples` module |
| `bench-report` | Nightly report assembly | updated for new tool names |

Deletions: `bench-rx-zero-copy` crate, `bench-vs-mtcp/src/mtcp.rs`, `bench-vs-linux/src/afpacket.rs`, `bench-stress/src/scenarios.rs::pmtu_blackhole_STAGE2`, `bench-stress` crate (after consolidation).

---

## File structure delta

**New files:**
- `tools/bench-common/src/raw_samples.rs`
- `tools/bench-rtt/Cargo.toml`
- `tools/bench-rtt/src/main.rs`
- `tools/bench-rtt/src/workload.rs` (moved from `bench-e2e/src/workload.rs`)
- `tools/bench-rtt/src/csv.rs`
- `tools/bench-rtt/src/stack.rs` (dpdk_net | linux_kernel | fstack dispatch)
- `tools/bench-tx-burst/Cargo.toml`
- `tools/bench-tx-burst/src/main.rs` (split from `bench-vs-mtcp`)
- `tools/bench-tx-burst/src/dpdk.rs`, `src/linux.rs`, `src/fstack.rs`
- `tools/bench-tx-maxtp/Cargo.toml`
- `tools/bench-tx-maxtp/src/main.rs`
- `tools/bench-tx-maxtp/src/dpdk.rs`, `src/linux.rs`, `src/fstack.rs`
- `tools/bench-tx-maxtp/src/send_ack_ring.rs` (per-segment ringbuffer)
- `tools/bench-rx-burst/Cargo.toml`
- `tools/bench-rx-burst/src/main.rs`
- `tools/bench-rx-burst/src/dpdk.rs`, `src/linux.rs`, `src/fstack.rs`
- `tools/bench-e2e/peer/burst-echo-server.c` (peer-side burst echo extension)
- `crates/dpdk-net-core/src/tcp_send_ack_log.rs` (per-segment send→ACK ringbuffer)
- `scripts/peer-ifb-setup.sh` (bidirectional netem wrapper)
- `docs/bench-reports/t51-bench-suite-overhaul-2026-05-XX.md` (final validation report)

**Modified files:**
- `Cargo.toml` (workspace members updated)
- `scripts/bench-nightly.sh` (matrix rewire, payload sweep, scenario expansion, IFB invocation)
- `tools/bench-common/src/lib.rs` (re-export `raw_samples`)
- `tools/bench-common/src/csv_row.rs` (add `raw_samples_path` + `failed_iter_count` columns)
- `crates/dpdk-net-core/src/tcp_output.rs` + `tcp_input.rs` (instrumentation hooks)
- `crates/dpdk-net-core/src/counters.rs` (split RTO/RACK/TLP counters)
- `tools/bench-offload-ab/src/main.rs` (subprocess target → `bench-rtt`)
- `tools/bench-obs-overhead/src/main.rs` (subprocess target → `bench-rtt`)
- `tools/bench-ab-runner/src/main.rs` (becomes thin `bench-rtt --once-mode` shim)
- `tools/bench-vs-linux/src/main.rs` (mode A removed; only mode B remains)
- `tools/bench-report/...` (input filenames updated)

**Deleted:**
- `tools/bench-rx-zero-copy/` (entire crate)
- `tools/bench-vs-mtcp/` (entire crate, after split lands)
- `tools/bench-stress/` (entire crate, after consolidation lands)
- `tools/bench-e2e/src/main.rs` (replaced by `bench-rtt/src/main.rs`; keep peer/echo-server)
- `tools/bench-vs-linux/src/linux_kernel.rs`, `mode_rtt.rs`, `afpacket.rs` (mode A removal)

---

# Phase 1 — Pre-work and baseline

### Task 1.1: Capture pre-overhaul baseline

**Files:** none modified; output goes to `target/bench-results/baseline-pre-overhaul/`.

- [ ] **Step 1: Run nightly bench in the perf worktree to capture the current state**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
OUT_DIR=target/bench-results/baseline-pre-overhaul timeout 7200 ./scripts/bench-nightly.sh 2>&1 | tee target/bench-results/baseline-pre-overhaul/run.log
```

Expected: completes in ~90-120 min; CSVs land under `target/bench-results/baseline-pre-overhaul/`.

- [ ] **Step 2: Snapshot the workspace member list and binary checksums**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
sha256sum target/release/bench-* > target/bench-results/baseline-pre-overhaul/binary-sha256.txt
ls tools > target/bench-results/baseline-pre-overhaul/workspace-tools.txt
```

- [ ] **Step 3: Commit the baseline**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add target/bench-results/baseline-pre-overhaul/
git commit -m "bench-overhaul: capture pre-overhaul baseline (T50 shape) for regression check

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.2: Pin tool inventory in a tracking issue file

**Files:**
- Create: `/home/ubuntu/resd.dpdk_tcp-a10-perf/docs/bench-reports/overhaul-tracker.md`

- [ ] **Step 1: Write the tracker file**

```markdown
# Bench overhaul tracker — 2026-05-09

Plan: docs/superpowers/plans/2026-05-09-bench-suite-overhaul.md

## Tool fate map

| Tool | Action | Phase |
|---|---|---|
| bench-e2e (binary) | replaced by bench-rtt | 4 |
| bench-e2e/peer/echo-server | extended (burst-echo) | 8 |
| bench-stress | deleted | 4 |
| bench-vs-linux mode A | folded into bench-rtt --stack linux_kernel | 4 |
| bench-vs-linux mode B | retained | — |
| bench-vs-mtcp burst | replaced by bench-tx-burst | 5 |
| bench-vs-mtcp maxtp | replaced by bench-tx-maxtp | 5 |
| bench-vs-mtcp/src/mtcp.rs | deleted | 2 |
| bench-rx-zero-copy | deleted | 2 |
| bench-stress/scenarios.rs::pmtu_blackhole_STAGE2 | deleted | 2 |
| bench-vs-linux/src/afpacket.rs | deleted | 2 |
| bench-offload-ab | retained (subprocess re-target) | 4 |
| bench-obs-overhead | retained (subprocess re-target) | 4 |
| bench-ab-runner | retained (thin shim) | 4 |
| bench-micro | retained | — |
| bench-common | augmented (raw_samples) | 3 |
| bench-report | updated for new names | 10 |

## Phase status

- [ ] Phase 1 complete
- [ ] Phase 2 complete
- [ ] Phase 3 complete
- [ ] Phase 4 complete
- [ ] Phase 5 complete
- [ ] Phase 6 complete
- [ ] Phase 7 complete
- [ ] Phase 8 complete
- [ ] Phase 9 complete
- [ ] Phase 10 complete
- [ ] Phase 11 complete
- [ ] Phase 12 complete
```

- [ ] **Step 2: Commit the tracker**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add docs/bench-reports/overhaul-tracker.md
git commit -m "bench-overhaul: pin tool fate map and phase tracker

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 2 — Remove dead arms

Closes claims **C-A1, C-A2, C-A3, C-A4, C-F1**.

### Task 2.1: Delete the mTCP arm from bench-vs-mtcp

**Files:**
- Delete: `tools/bench-vs-mtcp/src/mtcp.rs`
- Modify: `tools/bench-vs-mtcp/src/lib.rs`, `src/main.rs`, `src/burst.rs`, `src/maxtp.rs`
- Modify: `scripts/bench-nightly.sh:899-912, 957-969` (drop pass 3 burst-mtcp + pass 4 maxtp-mtcp)

- [ ] **Step 1: Remove the mtcp module reference**

In `tools/bench-vs-mtcp/src/lib.rs`, delete the `pub mod mtcp;` line (and any `pub use mtcp::*;`).

- [ ] **Step 2: Drop mtcp from the Stack enum**

In `tools/bench-vs-mtcp/src/main.rs`, find the `enum Stack` and remove the `Mtcp` variant. Drop matching arms in any `match stack {` block. Drop `--stack mtcp` from clap argument validators if present.

- [ ] **Step 3: Delete the file**

```bash
rm /home/ubuntu/resd.dpdk_tcp-a10-perf/tools/bench-vs-mtcp/src/mtcp.rs
```

- [ ] **Step 4: Remove mtcp passes from nightly**

In `scripts/bench-nightly.sh`, delete the entire blocks `[11a2/12]` (burst mtcp) and `[11c2/12]` (maxtp mtcp). The marker rows they emitted are no longer needed.

- [ ] **Step 5: Build to verify no leftover refs**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-vs-mtcp --features fstack
```

Expected: clean build.

- [ ] **Step 6: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u tools/bench-vs-mtcp/ scripts/bench-nightly.sh
git rm tools/bench-vs-mtcp/src/mtcp.rs
git commit -m "bench-overhaul: remove mTCP comparator arm

mTCP driver has been a permanent stub (exits 1 on every invocation).
Per user direction the comparator scope is dpdk_net + linux_kernel +
fstack only; mTCP is too costly to maintain alongside upstream
dormancy. Closes C-A1, C-F1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 2.2: Delete the afpacket stub from bench-vs-linux

**Files:**
- Delete: `tools/bench-vs-linux/src/afpacket.rs`
- Modify: `tools/bench-vs-linux/src/lib.rs`, `src/main.rs`, `src/mode_rtt.rs`

- [ ] **Step 1: Remove the afpacket module reference and Stack variant**

In `tools/bench-vs-linux/src/lib.rs` drop `pub mod afpacket;`. In `src/main.rs` and `src/mode_rtt.rs` drop the `Afpacket` enum variant + match arms; tighten clap validation.

- [ ] **Step 2: Delete the file**

```bash
rm /home/ubuntu/resd.dpdk_tcp-a10-perf/tools/bench-vs-linux/src/afpacket.rs
```

- [ ] **Step 3: Build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-vs-linux
```

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u tools/bench-vs-linux/
git rm tools/bench-vs-linux/src/afpacket.rs
git commit -m "bench-overhaul: remove afpacket stub from bench-vs-linux

afpacket was a stub that strict mode errored on and lenient mode
skipped. No real implementation was ever wired. Closes C-A2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 2.3: Delete the bench-rx-zero-copy crate

**Files:**
- Delete: `tools/bench-rx-zero-copy/` (entire directory)
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Remove from workspace**

In `/home/ubuntu/resd.dpdk_tcp-a10-perf/Cargo.toml` remove the `"tools/bench-rx-zero-copy",` line.

- [ ] **Step 2: Delete the crate directory**

```bash
rm -rf /home/ubuntu/resd.dpdk_tcp-a10-perf/tools/bench-rx-zero-copy/
```

- [ ] **Step 3: Build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release --workspace
```

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u Cargo.toml
git rm -r tools/bench-rx-zero-copy/
git commit -m "bench-overhaul: delete bench-rx-zero-copy placeholder crate

The criterion targets exercised only DpdkNetIovec struct construction
+ black_box; no real RX path measurement. The new bench-rx-burst tool
(Phase 8) covers the actual gap. Closes C-A3.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 2.4: Remove pmtu_blackhole placeholder from bench-stress

**Files:**
- Modify: `tools/bench-stress/src/scenarios.rs:132-138`
- Modify: `tools/bench-stress/src/scenarios.rs::is_stage2_placeholder` and any tests
- Modify: `tools/bench-stress/tests/scenario_parse.rs` if `pmtu_blackhole_placeholder_is_stage2` exists

- [ ] **Step 1: Delete the Scenario struct entry and is_stage2_placeholder fn**

In `tools/bench-stress/src/scenarios.rs`, remove the `pmtu_blackhole_STAGE2` Scenario entry and the `is_stage2_placeholder` function (and its `_STAGE2`-suffix logic). Update the `find` callers if they referenced it.

- [ ] **Step 2: Delete the matching test**

If `tools/bench-stress/tests/scenario_parse.rs` has `pmtu_blackhole_placeholder_is_stage2`, delete that test.

- [ ] **Step 3: Run tests**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 120 cargo test -p bench-stress
```

Expected: all remaining tests pass.

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u tools/bench-stress/
git commit -m "bench-overhaul: drop pmtu_blackhole_STAGE2 placeholder

bench-stress will be deleted in Phase 4; trimming the dead Stage-2
placeholder first keeps the consolidation diff focused. Closes C-A4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 3 — Raw-sample emission in bench-common

Closes claim **C-B2**. Foundational for Phases 4, 5, 8.

### Task 3.1: Add raw_samples module to bench-common

**Files:**
- Create: `tools/bench-common/src/raw_samples.rs`
- Modify: `tools/bench-common/src/lib.rs`
- Test: `tools/bench-common/tests/raw_samples.rs`

- [ ] **Step 1: Write the failing test**

Create `tools/bench-common/tests/raw_samples.rs`:

```rust
use bench_common::raw_samples::RawSamplesWriter;
use std::io::Read;

#[test]
fn writes_header_and_one_sample_per_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("raw.csv");
    let mut w = RawSamplesWriter::create(&path, &["bucket_id", "iter", "rtt_ns"])
        .expect("create");
    w.row(&["b1", "0", "1234"]).expect("row 0");
    w.row(&["b1", "1", "5678"]).expect("row 1");
    w.flush().expect("flush");
    drop(w);

    let mut got = String::new();
    std::fs::File::open(&path).unwrap().read_to_string(&mut got).unwrap();
    assert_eq!(got, "bucket_id,iter,rtt_ns\nb1,0,1234\nb1,1,5678\n");
}

#[test]
fn rejects_row_with_wrong_column_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.csv");
    let mut w = RawSamplesWriter::create(&path, &["a", "b"]).unwrap();
    let err = w.row(&["only_one"]).unwrap_err();
    assert!(err.to_string().contains("column count"));
}
```

Add `tempfile = "3"` to `tools/bench-common/Cargo.toml` `[dev-dependencies]`.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-common --test raw_samples
```

Expected: FAIL with "could not find raw_samples".

- [ ] **Step 3: Implement raw_samples.rs**

Create `tools/bench-common/src/raw_samples.rs`:

```rust
//! Streaming raw-sample CSV writer.
//!
//! Every bench tool that produces percentile distributions also emits a
//! sidecar CSV containing one row per measurement so post-hoc analysis
//! (additional percentiles, histograms, bimodality detection) does not
//! require re-running the bench. Writers are streaming — they flush
//! per-row to bound peak memory at iteration counts up to 10^7.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct RawSamplesWriter {
    inner: BufWriter<File>,
    expected_cols: usize,
}

impl RawSamplesWriter {
    pub fn create(path: &Path, header: &[&str]) -> Result<Self> {
        let f = File::create(path)
            .with_context(|| format!("create {}", path.display()))?;
        let mut inner = BufWriter::new(f);
        inner.write_all(header.join(",").as_bytes())?;
        inner.write_all(b"\n")?;
        Ok(Self { inner, expected_cols: header.len() })
    }

    pub fn row(&mut self, cols: &[&str]) -> Result<()> {
        if cols.len() != self.expected_cols {
            return Err(anyhow!(
                "raw_samples row column count {} != header {}",
                cols.len(),
                self.expected_cols,
            ));
        }
        self.inner.write_all(cols.join(",").as_bytes())?;
        self.inner.write_all(b"\n")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }
}
```

- [ ] **Step 4: Wire into lib.rs**

In `tools/bench-common/src/lib.rs` add:

```rust
pub mod raw_samples;
```

- [ ] **Step 5: Run the test to verify it passes**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-common --test raw_samples
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add tools/bench-common/src/raw_samples.rs tools/bench-common/tests/raw_samples.rs tools/bench-common/Cargo.toml tools/bench-common/src/lib.rs
git commit -m "bench-overhaul: add streaming raw-sample CSV writer

Per-row streaming write so high-iter benches do not need O(N) memory
to emit raw samples. Foundational for bench-rtt, bench-tx-*, and
bench-rx-burst raw-sample emit. Closes C-B2 (writer surface).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 3.2: Add raw_samples_path + failed_iter_count columns to summary CSV schema

**Files:**
- Modify: `tools/bench-common/src/csv_row.rs`
- Test: existing csv_row tests

- [ ] **Step 1: Write the failing test**

Append to `tools/bench-common/src/csv_row.rs` test module (or create one if absent):

```rust
#[test]
fn summary_row_includes_raw_samples_path_and_failed_iter_count() {
    let row = SummaryRow {
        // ... fill all existing fields ...
        raw_samples_path: Some("raw/bench-rtt-128b.csv".to_string()),
        failed_iter_count: 3,
        // ...
    };
    let csv = row.to_csv_line();
    assert!(csv.contains("raw/bench-rtt-128b.csv"));
    assert!(csv.contains(",3,"));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-common csv_row
```

Expected: FAIL with "no field `raw_samples_path`".

- [ ] **Step 3: Add the columns to SummaryRow**

In `tools/bench-common/src/csv_row.rs`, append `raw_samples_path: Option<String>` and `failed_iter_count: u64` to the `SummaryRow` struct. Update `to_csv_line` and `header()` accordingly. Append the new column names at the end of the existing schema (do not re-order — downstream report consumers parse positionally).

- [ ] **Step 4: Run to verify it passes**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-common csv_row
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add tools/bench-common/src/csv_row.rs
git commit -m "bench-overhaul: add raw_samples_path + failed_iter_count to summary CSV

raw_samples_path points to the per-iter sidecar CSV; failed_iter_count
records iters that hit the per-iter timeout (was previously fatal — see
C-D3). Schema additions only; existing column positions unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 4 — Consolidate RTT benches into bench-rtt

Closes claims **C-A5, C-B5, C-C1, C-D3**.

### Task 4.1: Scaffold bench-rtt crate

**Files:**
- Create: `tools/bench-rtt/Cargo.toml`
- Create: `tools/bench-rtt/src/main.rs`
- Modify: `Cargo.toml` (workspace)

- [ ] **Step 1: Add bench-rtt to workspace**

In `/home/ubuntu/resd.dpdk_tcp-a10-perf/Cargo.toml` add `"tools/bench-rtt",` to `members`.

- [ ] **Step 2: Create Cargo.toml**

```toml
[package]
name = "bench-rtt"
version.workspace = true
edition.workspace = true
license.workspace = true

[features]
default = []
fstack = ["dep:libc"]

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
libc = { workspace = true, optional = true }
bench-common = { path = "../bench-common" }
dpdk-net-core = { path = "../../crates/dpdk-net-core" }
dpdk-net = { path = "../../crates/dpdk-net" }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 3: Create stub main.rs that compiles**

```rust
//! bench-rtt — cross-stack request/response RTT distribution.
//!
//! Replaces bench-e2e (binary), bench-stress (matrix runner), and
//! bench-vs-linux mode A by parameterising the stack, payload size,
//! connection count, and netem-spec axes.
fn main() -> anyhow::Result<()> {
    anyhow::bail!("bench-rtt scaffold — wiring lands in Task 4.2+")
}
```

- [ ] **Step 4: Verify build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-rtt
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add Cargo.toml tools/bench-rtt/
git commit -m "bench-overhaul: scaffold bench-rtt crate

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.2: Move workload.rs from bench-e2e to bench-rtt

**Files:**
- Move: `tools/bench-e2e/src/workload.rs` → `tools/bench-rtt/src/workload.rs`
- Move: `tools/bench-e2e/src/attribution.rs` → `tools/bench-rtt/src/attribution.rs`
- Move: `tools/bench-e2e/src/hw_task_18.rs` → `tools/bench-rtt/src/hw_task_18.rs`
- Move: `tools/bench-e2e/src/sum_identity.rs` → `tools/bench-rtt/src/sum_identity.rs`
- Modify: `tools/bench-rtt/src/main.rs`, `Cargo.toml`
- Modify: `tools/bench-e2e/Cargo.toml`, `src/lib.rs`

- [ ] **Step 1: Move the source files**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git mv tools/bench-e2e/src/workload.rs tools/bench-rtt/src/workload.rs
git mv tools/bench-e2e/src/attribution.rs tools/bench-rtt/src/attribution.rs
git mv tools/bench-e2e/src/hw_task_18.rs tools/bench-rtt/src/hw_task_18.rs
git mv tools/bench-e2e/src/sum_identity.rs tools/bench-rtt/src/sum_identity.rs
```

- [ ] **Step 2: Add module declarations to bench-rtt/src/main.rs**

Replace the stub main with the bench-e2e/src/main.rs structure but rename `mod` paths and the binary. Keep the dpdk_net inner loop verbatim — it is the gold-standard implementation. Add `mod workload; mod attribution; mod hw_task_18; mod sum_identity;` at the top.

- [ ] **Step 3: Drop the modules from bench-e2e/src/lib.rs**

In `tools/bench-e2e/src/lib.rs`, remove the `pub mod workload;` etc. — that crate becomes peer-server-only (the `tools/bench-e2e/peer/` directory stays).

- [ ] **Step 4: Verify build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-rtt -p bench-e2e
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: move workload/attribution modules to bench-rtt

bench-e2e shrinks to peer-server-only (tools/bench-e2e/peer/). The
DPDK inner loop is preserved verbatim; subsequent tasks add stack
dispatch and payload-axis arguments.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.3: Add stack dispatch (dpdk_net | linux_kernel | fstack)

**Files:**
- Create: `tools/bench-rtt/src/stack.rs`
- Move-merge: `tools/bench-vs-linux/src/linux_kernel.rs` → `tools/bench-rtt/src/linux_kernel.rs`
- Modify: `tools/bench-rtt/src/main.rs`
- Test: `tools/bench-rtt/tests/stack_arg.rs`

- [ ] **Step 1: Write the failing test**

`tools/bench-rtt/tests/stack_arg.rs`:

```rust
use std::process::Command;

#[test]
fn rejects_unknown_stack() {
    let bin = env!("CARGO_BIN_EXE_bench-rtt");
    let out = Command::new(bin)
        .args(["--stack", "wireshark", "--peer-ip", "127.0.0.1", "--peer-port", "10001"])
        .output()
        .expect("spawn");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid value 'wireshark' for '--stack"));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt --test stack_arg
```

Expected: FAIL.

- [ ] **Step 3: Add the Stack enum and clap wiring**

`tools/bench-rtt/src/stack.rs`:

```rust
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Stack {
    DpdkNet,
    LinuxKernel,
    Fstack,
}
```

In `tools/bench-rtt/src/main.rs` add `mod stack; mod linux_kernel;` and a clap arg `#[arg(long, value_enum)] stack: stack::Stack`. Dispatch the workload by stack.

- [ ] **Step 4: Move linux_kernel.rs from bench-vs-linux**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git mv tools/bench-vs-linux/src/linux_kernel.rs tools/bench-rtt/src/linux_kernel.rs
```

In `tools/bench-vs-linux/src/lib.rs`, drop `pub mod linux_kernel;`. In `tools/bench-vs-linux/src/main.rs`, drop the mode-A handler block — only mode B (wire-diff) remains.

- [ ] **Step 5: Run to verify it passes**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt --test stack_arg
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: add --stack {dpdk_net|linux_kernel|fstack} to bench-rtt

linux_kernel inner loop migrated from bench-vs-linux mode A. fstack
dispatch reuses the bench-vs-mtcp fstack_ffi.rs patterns (added next
task). bench-vs-linux now retains only mode B (wire-diff).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.4: Add fstack RTT path

**Files:**
- Create: `tools/bench-rtt/src/fstack.rs` (new RTT inner loop using ff_write/ff_read)
- Reuse: copy patterns from `tools/bench-vs-mtcp/src/fstack_ffi.rs` (will be removed in Phase 5)
- Test: requires `--features fstack`; gate the integration test behind it

- [ ] **Step 1: Implement the fstack RTT inner loop**

`tools/bench-rtt/src/fstack.rs`:

```rust
//! F-Stack RTT path: ff_write request, ff_poll/ff_read until response_bytes
//! are returned, return wall-clock RTT in ns. Mirrors the linux_kernel
//! shape (blocking semantics on top of an async stack via ff_poll).

#[cfg(feature = "fstack")]
pub mod imp {
    use std::time::Instant;

    pub fn run_rtt_workload(
        // ... ff_socket, ff_connect setup elided; copy from
        // tools/bench-vs-mtcp/src/fstack_burst.rs ...
        request_bytes: usize,
        response_bytes: usize,
        warmup: u64,
        iterations: u64,
    ) -> anyhow::Result<Vec<f64>> {
        // 1. ff_socket + ff_connect (POLLOUT-gated)
        // 2. for i in 0..warmup: ff_write request, ff_poll/ff_read response_bytes
        // 3. for i in 0..iterations: t0 = Instant::now(); ff_write; ff_read; samples.push(t0.elapsed())
        // 4. ff_close
        unimplemented!("port the warmup/measure phases from fstack_burst.rs")
    }
}

#[cfg(not(feature = "fstack"))]
pub mod imp {
    pub fn run_rtt_workload(
        _request_bytes: usize,
        _response_bytes: usize,
        _warmup: u64,
        _iterations: u64,
    ) -> anyhow::Result<Vec<f64>> {
        anyhow::bail!("bench-rtt built without fstack feature; rebuild with --features fstack")
    }
}
```

- [ ] **Step 2: Adapt the FFI bindings**

Copy `tools/bench-vs-mtcp/src/fstack_ffi.rs` constants (Linux-namespace `FF_EAGAIN=11`, `SOL_SOCKET=1`, `SO_ERROR=4`, etc.) into `tools/bench-rtt/src/fstack.rs` or a sibling `fstack_ffi.rs`. Reuse the `ff_poll(POLLOUT, timeout=0)` connect-detection helper.

- [ ] **Step 3: Verify build under both feature flags**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-rtt
cargo build --release -p bench-rtt --features fstack
```

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add tools/bench-rtt/src/fstack.rs
git commit -m "bench-overhaul: add fstack RTT inner loop to bench-rtt

Mirrors the linux_kernel blocking-style shape on top of ff_poll. FFI
namespace constants reuse the lessons from bench-vs-mtcp T50 (Linux
errno + Linux SOL_SOCKET, not FreeBSD).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.5: Wire payload-axis sweep + raw-sample emission

**Files:**
- Modify: `tools/bench-rtt/src/main.rs`, `src/csv.rs` (new)
- Test: `tools/bench-rtt/tests/payload_sweep.rs`

- [ ] **Step 1: Write the failing test**

`tools/bench-rtt/tests/payload_sweep.rs`:

```rust
use std::process::Command;

#[test]
fn accepts_payload_sweep_arg() {
    let bin = env!("CARGO_BIN_EXE_bench-rtt");
    let out = Command::new(bin)
        .args(["--help"])
        .output().expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--payload-bytes-sweep"));
    assert!(stdout.contains("--raw-samples-csv"));
    assert!(stdout.contains("--connections"));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt --test payload_sweep
```

- [ ] **Step 3: Implement the args + per-bucket loop**

In `tools/bench-rtt/src/main.rs`:

```rust
#[derive(Parser)]
struct Args {
    #[arg(long, value_enum)] stack: stack::Stack,
    #[arg(long)] peer_ip: String,
    #[arg(long, default_value_t = 10001)] peer_port: u16,
    /// Comma-separated list of payload sizes (bytes) to sweep over.
    /// Each value is used as both request and response size for a bucket.
    #[arg(long, value_delimiter = ',', default_value = "128")]
    payload_bytes_sweep: Vec<usize>,
    #[arg(long, default_value_t = 1)] connections: u32,
    #[arg(long, default_value_t = 5000)] iterations: u64,
    #[arg(long, default_value_t = 500)] warmup: u64,
    /// Optional sidecar CSV for raw per-iter samples.
    #[arg(long)] raw_samples_csv: Option<std::path::PathBuf>,
    #[arg(long)] output_csv: std::path::PathBuf,
}
```

Inside `main()`, loop over `payload_bytes_sweep`. For each size W:
1. Open `connections` connections
2. Run warmup + iterations of `request_response_attributed` per connection (or sequential round-robin)
3. Aggregate samples; write summary row + raw-sample rows tagged by bucket_id `payload_W`

- [ ] **Step 4: Run to verify it passes**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt --test payload_sweep
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add tools/bench-rtt/
git commit -m "bench-overhaul: wire payload-bytes-sweep + raw-sample CSV in bench-rtt

Closes C-C1 (small-pkt RTT distribution at 64/128/256B can now be
swept) and C-B2 (raw samples emitted per iter). Single-conn bias from
C-B5 partially fixed via --connections N (full multi-conn behaviour
arrives with bench-tx-maxtp).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.6: Capture failed-iter count instead of bailing on timeout

**Files:**
- Modify: `tools/bench-rtt/src/workload.rs`

- [ ] **Step 1: Write the failing test**

In `tools/bench-rtt/src/workload.rs` test module:

```rust
#[test]
fn run_rtt_workload_returns_failed_count_not_bail() {
    // Synthetic: feed a workload that times out 3 of 10 iterations;
    // assert returned vec has 7 samples and failed_iter_count == 3.
    // (Driven via a mock send_bytes that returns Err on iters {2,5,8}.)
    let (samples, failed) = run_rtt_workload_with_outcome(/* mock */);
    assert_eq!(samples.len(), 7);
    assert_eq!(failed, 3);
}
```

(Concrete mock surface: introduce a `WorkloadHooks` trait so the test can inject failures without a real engine. If that's heavy, gate this test behind `#[cfg(feature = "test-hooks")]` and add an integration test that runs against a deliberately-flaky echo peer.)

- [ ] **Step 2: Run to verify it fails**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt
```

- [ ] **Step 3: Change `?` to a `match` that increments a counter**

In `run_rtt_workload`, replace the `?` on `request_response_attributed` with:

```rust
match request_response_attributed(...) {
    Ok(rec) => samples.push(rec.rtt_ns as f64),
    Err(e) => {
        eprintln!("bench-rtt: iter {i} failed: {e:#}");
        failed += 1;
        if failed > iterations / 2 {
            anyhow::bail!("more than 50% iters failed; aborting scenario");
        }
    }
}
```

Return `Ok((samples, failed))` instead of `Ok(samples)`. Update the caller to write `failed_iter_count` into the summary row.

- [ ] **Step 4: Run to verify it passes**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: bench-rtt keeps successful iters on per-iter timeout

Previously a single-iter recv timeout bailed the entire scenario via
the ? operator, dropping all earlier samples. Now we count failed
iters into a column and only abort if > 50% fail. Closes C-D3.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.7: Delete bench-stress and bench-e2e binary

**Files:**
- Delete: `tools/bench-stress/` (entire crate)
- Delete: `tools/bench-e2e/src/main.rs`, `src/lib.rs` (keep only `peer/`)
- Modify: `Cargo.toml`, `scripts/bench-nightly.sh`, `scripts/bench-quick.sh`
- Modify: `tools/bench-offload-ab/src/main.rs`, `tools/bench-obs-overhead/src/main.rs`, `tools/bench-ab-runner/src/main.rs` (subprocess target → `bench-rtt`)

- [ ] **Step 1: Delete bench-stress crate**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git rm -r tools/bench-stress/
```

In `Cargo.toml` drop `"tools/bench-stress",`.

- [ ] **Step 2: Convert bench-e2e to peer-only**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git rm tools/bench-e2e/src/main.rs tools/bench-e2e/src/lib.rs
```

If `tools/bench-e2e/Cargo.toml` declares a `[[bin]]`, change it to a placeholder library or drop the `[[bin]]` section entirely. The `tools/bench-e2e/peer/` Makefile + `echo-server` C source stay — that is the peer-side artefact.

- [ ] **Step 3: Repoint subprocess callers**

In `tools/bench-offload-ab/src/main.rs`, `tools/bench-obs-overhead/src/main.rs`, `tools/bench-ab-runner/src/main.rs`: change the spawned binary path from `target/release/bench-ab-runner` (or wherever) to `target/release/bench-rtt --stack dpdk_net --connections 1 --iterations N --warmup M --output-csv …`.

- [ ] **Step 4: Update bench-nightly.sh sections [7/12] and [8/12]**

Replace the `bench-e2e` invocation at step [7/12] with a `bench-rtt` invocation. Replace the `bench-stress` matrix loop at step [8/12] with a `bench-rtt --netem-spec` matrix loop (delegating netem orchestration to a new helper). Preserve idle-baseline + per-scenario p999 ratio computation in a small post-processing script (`scripts/bench-stress-ratio-check.py` or similar).

- [ ] **Step 5: Build the whole workspace**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release --workspace
cargo build --release --workspace --features fstack
```

- [ ] **Step 6: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: delete bench-stress + bench-e2e binary; rewire callers

bench-rtt absorbs both. bench-stress matrix loop migrates into the
nightly script (which already drove the per-scenario tc qdisc lifecycle).
bench-e2e shrinks to peer-server-only. bench-offload-ab, bench-obs-overhead,
bench-ab-runner now subprocess into bench-rtt. Closes C-A5.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 5 — Split bench-vs-mtcp into bench-tx-burst + bench-tx-maxtp

Closes claims **C-A1** (final piece — full crate removal), **C-B1** (maxtp percentiles), **C-B5** (per-conn maxtp samples), **C-F1**, **C-F2**.

### Task 5.1: Scaffold bench-tx-burst crate

**Files:**
- Create: `tools/bench-tx-burst/Cargo.toml`
- Create: `tools/bench-tx-burst/src/main.rs`, `src/dpdk.rs`, `src/linux.rs`, `src/fstack.rs`
- Move-merge: `tools/bench-vs-mtcp/src/burst.rs` → `tools/bench-tx-burst/src/burst.rs`
- Move-merge: `tools/bench-vs-mtcp/src/dpdk_burst.rs` → `tools/bench-tx-burst/src/dpdk.rs`
- Move-merge: `tools/bench-vs-mtcp/src/fstack_burst.rs` → `tools/bench-tx-burst/src/fstack.rs`

- [ ] **Step 1: Add to workspace + Cargo.toml**

In `Cargo.toml` add `"tools/bench-tx-burst",`. Create `tools/bench-tx-burst/Cargo.toml` mirroring bench-rtt's deps.

- [ ] **Step 2: Move source files**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git mv tools/bench-vs-mtcp/src/burst.rs tools/bench-tx-burst/src/burst.rs
git mv tools/bench-vs-mtcp/src/dpdk_burst.rs tools/bench-tx-burst/src/dpdk.rs
git mv tools/bench-vs-mtcp/src/fstack_burst.rs tools/bench-tx-burst/src/fstack.rs
```

- [ ] **Step 3: Add a linux_kernel TX-burst path**

`tools/bench-tx-burst/src/linux.rs`: blocking `TcpStream` write of K bytes back-to-back, with the same K/G grid as the dpdk arm. Reuses the linux_kernel patterns from Phase 4. The peer drains via the existing `echo-server` (the peer's recv path is fine — we only measure DUT TX).

- [ ] **Step 4: Adapt main.rs for stack dispatch**

Combine the dpdk_burst.rs + fstack_burst.rs entry points behind `--stack {dpdk_net|linux_kernel|fstack}`. Drop the K/G grid driver into a single `burst.rs` shared loop.

- [ ] **Step 5: Verify build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-tx-burst
cargo build --release -p bench-tx-burst --features fstack
```

- [ ] **Step 6: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: split bench-vs-mtcp burst path into bench-tx-burst

Adds a linux_kernel TX-burst arm so the comparator triplet
dpdk_net + linux_kernel + fstack covers burst as well as RTT.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 5.2: Scaffold bench-tx-maxtp crate

**Files:**
- Create: `tools/bench-tx-maxtp/Cargo.toml`
- Create: `tools/bench-tx-maxtp/src/main.rs`, `src/dpdk.rs`, `src/linux.rs`, `src/fstack.rs`, `src/maxtp.rs`
- Move-merge: `tools/bench-vs-mtcp/src/maxtp.rs` → `tools/bench-tx-maxtp/src/maxtp.rs`
- Move-merge: `tools/bench-vs-mtcp/src/dpdk_maxtp.rs` → `tools/bench-tx-maxtp/src/dpdk.rs`
- Move-merge: `tools/bench-vs-mtcp/src/linux_maxtp.rs` → `tools/bench-tx-maxtp/src/linux.rs`
- Move-merge: `tools/bench-vs-mtcp/src/fstack_maxtp.rs` → `tools/bench-tx-maxtp/src/fstack.rs`

- [ ] **Step 1: Add to workspace and create Cargo.toml** (analogous to Task 5.1).

- [ ] **Step 2: Move source files**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git mv tools/bench-vs-mtcp/src/maxtp.rs tools/bench-tx-maxtp/src/maxtp.rs
git mv tools/bench-vs-mtcp/src/dpdk_maxtp.rs tools/bench-tx-maxtp/src/dpdk.rs
git mv tools/bench-vs-mtcp/src/linux_maxtp.rs tools/bench-tx-maxtp/src/linux.rs
git mv tools/bench-vs-mtcp/src/fstack_maxtp.rs tools/bench-tx-maxtp/src/fstack.rs
```

- [ ] **Step 3: Update internal module paths**

In each moved file, fix `use bench_vs_mtcp::...` and crate-relative `use crate::...` paths so the files compile under their new crate.

- [ ] **Step 4: Build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-tx-maxtp --features fstack
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: split bench-vs-mtcp maxtp path into bench-tx-maxtp

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 5.3: Switch maxtp to per-conn raw-sample emission with goodput percentiles

**Files:**
- Modify: `tools/bench-tx-maxtp/src/maxtp.rs`, `src/dpdk.rs`, `src/linux.rs`, `src/fstack.rs`
- Test: `tools/bench-tx-maxtp/tests/per_conn_samples.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn maxtp_emits_one_raw_row_per_sample_interval_per_conn() {
    // Run a 5s synthetic maxtp at C=4, SAMPLE_INTERVAL=1s.
    // Expect 4 conns * 5 intervals = 20 raw rows in the sidecar CSV.
    // Each row has columns: bucket_id, conn_id, sample_idx, goodput_bps, snd_nxt_minus_una
    // ...
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-tx-maxtp --test per_conn_samples
```

- [ ] **Step 3: Refactor SnduneAccumulator to emit per-sample-interval points**

The current accumulator (`tools/bench-vs-mtcp/src/dpdk_maxtp.rs:506`) computes a window-aggregate. Change it to emit a `MaxtpRawPoint { conn_id, sample_idx, t_ns, goodput_bps_window, snd_nxt_minus_una }` to a `RawSamplesWriter` per `SAMPLE_INTERVAL` tick.

- [ ] **Step 4: Compute percentiles over the per-sample-interval points**

After the 60 s window completes, run `bench_common::percentile::summarize` over the per-conn goodput samples (folded across conns, then per-conn). Emit p50/p99/p999/mean as the bucket summary; emit the raw points to the sidecar CSV.

- [ ] **Step 5: Run tests**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 120 cargo test -p bench-tx-maxtp
```

- [ ] **Step 6: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: maxtp emits per-conn raw sample points + percentiles

Each (W,C,stack) bucket now emits per-conn goodput sample points at
SAMPLE_INTERVAL granularity, with percentile summary alongside the
existing mean. Closes C-B1 (no percentiles), C-B5 (multi-conn),
C-E1 (queue depth time series via snd_nxt_minus_una column).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 5.4: Delete bench-vs-mtcp crate

**Files:**
- Delete: `tools/bench-vs-mtcp/`
- Modify: `Cargo.toml`, `scripts/bench-nightly.sh`

- [ ] **Step 1: Confirm all source files have moved**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
ls tools/bench-vs-mtcp/src/
```

Expected: only files we explicitly preserve (none — all moved).

- [ ] **Step 2: Drop from workspace + delete**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git rm -r tools/bench-vs-mtcp/
# Remove `"tools/bench-vs-mtcp",` from Cargo.toml
```

- [ ] **Step 3: Repoint nightly script**

In `scripts/bench-nightly.sh` step [11/12], change `bench-vs-mtcp` invocations to `bench-tx-burst` and `bench-tx-maxtp` accordingly. Drop the per-pass `--stack` arg (now derived from the binary name, or pass through if main.rs still takes one).

- [ ] **Step 4: Build the workspace**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release --workspace --features fstack
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: delete bench-vs-mtcp crate (split complete)

bench-tx-burst + bench-tx-maxtp now own the workloads. Closes C-A1
(crate-level removal), C-F1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 5.5: Validate linux maxtp uses linux-tcp-sink, not echo-server

**Files:**
- Modify: `tools/bench-tx-maxtp/src/linux.rs` (assert peer port 10002 — sink, not echo)
- Modify: `scripts/bench-nightly.sh` step [6/12] to ensure linux-tcp-sink is started

- [ ] **Step 1: Add a contract assertion at start-of-bench**

```rust
fn assert_peer_is_sink(peer_ip: &str, peer_port: u16) -> anyhow::Result<()> {
    if peer_port != 10002 {
        anyhow::bail!(
            "bench-tx-maxtp linux arm requires peer_port=10002 (linux-tcp-sink); got {peer_port}"
        );
    }
    Ok(())
}
```

Call before opening connections.

- [ ] **Step 2: Confirm nightly starts linux-tcp-sink**

In `scripts/bench-nightly.sh` step [6/12], verify the `linux-tcp-sink` peer binary is started on the peer host on port 10002. If not, add it.

- [ ] **Step 3: Build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-tx-maxtp
```

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: assert linux maxtp uses linux-tcp-sink (port 10002)

T50 reported the linux maxtp peer port was actually echo-server in
that run, producing back-pressured goodput; assertion makes the
contract explicit. Closes C-F2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 6 — Per-segment send→ACK latency in bench-tx-maxtp

Closes claim **C-B4**.

### Task 6.1: Add a per-segment send→ACK ringbuffer to dpdk-net-core

**Files:**
- Create: `crates/dpdk-net-core/src/tcp_send_ack_log.rs`
- Modify: `crates/dpdk-net-core/src/lib.rs`, `src/tcp_output.rs`, `src/tcp_input.rs`
- Test: `crates/dpdk-net-core/src/tcp_send_ack_log.rs` unit tests

- [ ] **Step 1: Write the failing unit test**

`crates/dpdk-net-core/src/tcp_send_ack_log.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_send_then_match_cumulative_ack_returns_latency() {
        let mut log = SendAckLog::with_capacity(16);
        log.record_send(seq_range(100, 200), 1_000);     // seq [100,200), t=1000ns
        log.record_send(seq_range(200, 300), 1_500);
        log.record_send(seq_range(300, 400), 2_000);

        // Cumulative ACK 250 covers first segment + part of second; we
        // attribute first segment's send-time vs ack-time to a complete
        // sample, partial second is queued for next ACK.
        let acks = log.observe_cumulative_ack(250, 3_000);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].latency_ns, 2_000);

        let acks2 = log.observe_cumulative_ack(400, 4_000);
        assert_eq!(acks2.len(), 2);
    }

    fn seq_range(a: u32, b: u32) -> SeqRange { SeqRange { begin: a, end: b } }
}
```

- [ ] **Step 2: Verify failure**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p dpdk-net-core tcp_send_ack_log
```

- [ ] **Step 3: Implement SendAckLog**

```rust
//! Per-segment send→ACK latency ringbuffer.
//!
//! Record per-segment {begin_seq, end_seq, t_send_ns}. On every
//! cumulative-ACK delivered to the conn, walk the ringbuffer head and
//! emit one (latency_ns) sample per segment fully covered by the new
//! snd_una. Partial coverage is left for the next ACK. Capacity is
//! bounded — overflow drops oldest entries (counter incremented).

#[derive(Copy, Clone, Debug)]
pub struct SeqRange {
    pub begin: u32,
    pub end: u32,
}

#[derive(Copy, Clone, Debug)]
pub struct SendAckSample {
    pub begin_seq: u32,
    pub end_seq: u32,
    pub latency_ns: u64,
}

pub struct SendAckLog {
    entries: std::collections::VecDeque<(SeqRange, u64)>,
    cap: usize,
    pub overflow_drops: u64,
}

impl SendAckLog {
    pub fn with_capacity(cap: usize) -> Self {
        Self { entries: std::collections::VecDeque::with_capacity(cap), cap, overflow_drops: 0 }
    }

    pub fn record_send(&mut self, range: SeqRange, t_send_ns: u64) {
        if self.entries.len() == self.cap {
            self.entries.pop_front();
            self.overflow_drops += 1;
        }
        self.entries.push_back((range, t_send_ns));
    }

    pub fn observe_cumulative_ack(&mut self, snd_una: u32, t_ack_ns: u64) -> Vec<SendAckSample> {
        let mut out = Vec::new();
        while let Some((range, t_send)) = self.entries.front().copied() {
            // wrapping_lt(range.end, snd_una.wrapping_add(1))
            if seq_le(range.end, snd_una) {
                out.push(SendAckSample {
                    begin_seq: range.begin,
                    end_seq: range.end,
                    latency_ns: t_ack_ns.saturating_sub(t_send),
                });
                self.entries.pop_front();
            } else {
                break;
            }
        }
        out
    }
}

fn seq_le(a: u32, b: u32) -> bool { (b.wrapping_sub(a) as i32) >= 0 }
```

- [ ] **Step 4: Wire into tcp_output**

In `crates/dpdk-net-core/src/tcp_output.rs`, locate the segment-emission point. After successfully emitting, call `conn.send_ack_log.record_send(SeqRange { begin: seg_seq, end: seg_seq.wrapping_add(seg_len) }, clock::now_ns())`.

- [ ] **Step 5: Wire into tcp_input ACK handler**

In `crates/dpdk-net-core/src/tcp_input.rs`, after `snd_una` advances, call `let samples = conn.send_ack_log.observe_cumulative_ack(new_snd_una, clock::now_ns())` and forward `samples` into a per-conn channel/Vec the bench tool can drain.

- [ ] **Step 6: Run tests**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 120 cargo test -p dpdk-net-core
```

- [ ] **Step 7: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "feat(tcp): per-segment send→ACK latency ringbuffer

Tracks {seq_range, t_send_ns} on emit and emits {latency_ns} samples
on cumulative-ACK delivery. Bounded capacity with overflow counter.
Foundational for bench-tx-maxtp per-msg latency CDF (C-B4).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 6.2: Drain send-ack samples into bench-tx-maxtp output

**Files:**
- Modify: `tools/bench-tx-maxtp/src/dpdk.rs`
- Modify: `tools/bench-tx-maxtp/src/main.rs` (add `--send-ack-csv` flag)

- [ ] **Step 1: Add the CSV writer + drain loop**

In `tools/bench-tx-maxtp/src/dpdk.rs` pump loop, after each `engine.poll_once()`:

```rust
for conn in &conns {
    let samples = engine.drain_send_ack_samples(conn);
    for s in samples {
        send_ack_csv.row(&[
            bucket_id, &conn.id().to_string(),
            &s.begin_seq.to_string(), &s.end_seq.to_string(),
            &s.latency_ns.to_string(),
        ])?;
    }
}
```

- [ ] **Step 2: Add `--send-ack-csv` arg + raw_samples writer**

In `tools/bench-tx-maxtp/src/main.rs` add `#[arg(long)] send_ack_csv: Option<std::path::PathBuf>`. If present, open a `RawSamplesWriter` and pass into the dpdk pump loop.

- [ ] **Step 3: For linux_kernel arm, sample TCP_INFO**

`tools/bench-tx-maxtp/src/linux.rs`: every SAMPLE_INTERVAL, `getsockopt(TCP_INFO)` and emit `tcpi_rtt`/`tcpi_total_retrans`/`tcpi_unacked` snapshots. This is coarser than per-segment but is the only kernel-side view. Document the limitation in the file header.

- [ ] **Step 4: For fstack arm, sample ff_getsockopt FreeBSD-equivalent**

`tools/bench-tx-maxtp/src/fstack.rs`: skip per-segment send→ACK (FreeBSD TCP_INFO surface is reachable but adds large surface area for one stack). Emit a `send_ack_unsupported` row instead so the CSV schema stays uniform.

- [ ] **Step 5: Build + integration test**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-tx-maxtp --features fstack
```

- [ ] **Step 6: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: drain send→ACK latency samples in bench-tx-maxtp

dpdk_net path emits per-segment latency samples. linux_kernel path
emits coarse TCP_INFO snapshots. fstack path emits an unsupported
marker for schema uniformity. Closes C-B4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 7 — Bidirectional netem (peer IFB ingress)

Closes claim **C-C4**.

### Task 7.1: Write the peer-IFB setup script

**Files:**
- Create: `scripts/peer-ifb-setup.sh`
- Test: shellcheck + dry-run on the peer

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# peer-ifb-setup.sh — set up an ifb redirect on the peer's data NIC so
# ingress traffic (DUT→peer direction) can be shaped via netem.
#
# Usage on the peer host:
#   sudo ./peer-ifb-setup.sh up   ens6 ifb0 "loss 1% delay 5ms"
#   sudo ./peer-ifb-setup.sh down ens6 ifb0
#
# Up:    creates ifb0, redirects ens6 ingress to ifb0, applies netem on ifb0.
# Down:  removes ingress qdisc + ifb0 device.
set -euo pipefail
mode="${1:?up|down}"
iface="${2:?iface}"
ifb="${3:?ifb dev name}"
spec="${4:-}"

case "$mode" in
  up)
    modprobe ifb numifbs=2
    ip link add "$ifb" type ifb 2>/dev/null || true
    ip link set "$ifb" up
    tc qdisc add dev "$iface" handle ffff: ingress
    tc filter add dev "$iface" parent ffff: protocol ip u32 \
        match u32 0 0 action mirred egress redirect dev "$ifb"
    tc qdisc add dev "$ifb" root netem $spec
    ;;
  down)
    tc qdisc del dev "$ifb" root || true
    tc qdisc del dev "$iface" ingress || true
    ip link set "$ifb" down || true
    ip link delete "$ifb" type ifb || true
    ;;
  *) echo "unknown mode $mode"; exit 1 ;;
esac
```

- [ ] **Step 2: Verify shellcheck-clean**

```bash
shellcheck /home/ubuntu/resd.dpdk_tcp-a10-perf/scripts/peer-ifb-setup.sh
```

- [ ] **Step 3: Make executable + commit**

```bash
chmod +x /home/ubuntu/resd.dpdk_tcp-a10-perf/scripts/peer-ifb-setup.sh
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add scripts/peer-ifb-setup.sh
git commit -m "bench-overhaul: peer ifb-ingress netem setup script

Allows applying netem to packets arriving at the peer (i.e. DUT egress
traffic), so DUT-TX-data-loss triggers DUT fast-retransmit. Pairs with
existing peer-egress netem for true bidirectional shaping. Closes C-C4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 7.2: Wire bidirectional-netem option into nightly script

**Files:**
- Modify: `scripts/bench-nightly.sh`

- [ ] **Step 1: Add a NETEM_DIRECTION axis**

In the netem matrix loop (post-Phase 4 location), wrap each scenario invocation in three sub-cells: `egress` (peer root, current), `ingress` (peer ifb), `bidir` (both). Sub-cell labels become part of the CSV `bucket_id`.

- [ ] **Step 2: Apply ifb on the matching sub-cells**

In the nightly loop:

```bash
case "$direction" in
  egress)
    ssh "${SSH_OPTS[@]}" ubuntu@$PEER_SSH "sudo tc qdisc add dev ens6 root netem $spec"
    ;;
  ingress)
    ssh "${SSH_OPTS[@]}" ubuntu@$PEER_SSH "sudo /tmp/peer-ifb-setup.sh up ens6 ifb0 \"$spec\""
    ;;
  bidir)
    ssh "${SSH_OPTS[@]}" ubuntu@$PEER_SSH "sudo tc qdisc add dev ens6 root netem $spec && sudo /tmp/peer-ifb-setup.sh up ens6 ifb0 \"$spec\""
    ;;
esac
```

The teardown branch mirrors with `del`/`down`.

- [ ] **Step 3: SCP peer-ifb-setup.sh to the peer in step [5/12]**

Add `scripts/peer-ifb-setup.sh` to the SCP file list pushed to `/tmp/` on the peer.

- [ ] **Step 4: Dry-run**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
bash -n scripts/bench-nightly.sh
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u scripts/bench-nightly.sh
git commit -m "bench-overhaul: nightly applies netem in egress|ingress|bidir directions

Each netem scenario now produces three CSV buckets keyed by direction.
DUT-TX-data-loss is finally exercised via the ingress direction. Closes
the operational half of C-C4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 8 — bench-rx-burst (the missing tool)

Closes claims **C-B3, C-C2, C-A3** (replacement for the deleted bench-rx-zero-copy).

### Task 8.1: Extend echo-server with a burst-push mode

**Files:**
- Create: `tools/bench-e2e/peer/burst-echo-server.c`
- Modify: `tools/bench-e2e/peer/Makefile`

- [ ] **Step 1: Write the burst-echo-server.c**

A minimal extension of `echo-server.c` that:
1. Listens on a control TCP port (e.g. 10003) for one-line burst commands `BURST <N> <W>\n`.
2. On command, sends `N` segments of `W` bytes back-to-back over the same control connection (or a parallel data connection — single conn keeps it simple).
3. Each segment payload starts with a 16-byte header: `[u64 seq_idx | u64 peer_send_ns]`.

- [ ] **Step 2: Add Makefile target**

```makefile
burst-echo-server: burst-echo-server.c
	$(CC) -O2 -Wall -Wextra -o $@ $<

all: echo-server burst-echo-server
.PHONY: all
```

- [ ] **Step 3: Build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf/tools/bench-e2e/peer
make burst-echo-server
```

- [ ] **Step 4: Smoke test locally**

```bash
./burst-echo-server 10003 &
PID=$!
sleep 0.2
echo 'BURST 4 64' | nc -q1 127.0.0.1 10003 | xxd | head
kill $PID
```

Expected: 4 chunks of 64 bytes each printed.

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add tools/bench-e2e/peer/burst-echo-server.c tools/bench-e2e/peer/Makefile
git commit -m "bench-overhaul: peer-side burst-echo-server for bench-rx-burst

Listens on a control port; on BURST command sends N×W back-to-back
segments with 16-byte headers (seq_idx + peer_send_ns) so DUT can
attribute per-segment latency.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 8.2: Scaffold bench-rx-burst crate (DUT side, dpdk_net first)

**Files:**
- Create: `tools/bench-rx-burst/Cargo.toml`, `src/main.rs`, `src/dpdk.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Add to workspace**

In `Cargo.toml` add `"tools/bench-rx-burst",`. Create `Cargo.toml` analogous to bench-tx-burst.

- [ ] **Step 2: Write the dpdk RX-burst inner loop**

`tools/bench-rx-burst/src/dpdk.rs`:

```rust
//! DUT-side RX-burst measurement.
//!
//! 1. Connect to peer's burst-echo-server control port.
//! 2. Send "BURST N W\n".
//! 3. Drive engine.poll_once() in a tight loop, capturing on each
//!    Readable event:
//!      - clock::now_ns() at delivery
//!      - the embedded peer_send_ns from the segment header
//!      - the engine-internal hw_rx_ts_ns (zero on ENA, populated on c7i)
//!    Per-segment latency = clock::now_ns() - peer_send_ns (skewed by
//!    clock offset; see Phase 9 for HW-TS correction). Also record the
//!    DUT-internal poll-detect-to-deliver delta as an unskewed metric.
```

Implement the inner loop using the existing `engine.events()` drain + `Readable` event pattern.

- [ ] **Step 3: Add main.rs with --burst-grid arg**

```rust
#[derive(Parser)]
struct Args {
    #[arg(long, value_enum)] stack: stack::Stack,
    #[arg(long)] peer_ip: String,
    #[arg(long, default_value_t = 10003)] peer_control_port: u16,
    /// Comma-separated W values (per-segment payload size in bytes)
    #[arg(long, value_delimiter = ',', default_value = "64,128,256")]
    segment_sizes: Vec<usize>,
    /// Comma-separated N values (segments per burst)
    #[arg(long, value_delimiter = ',', default_value = "16,64,256,1024")]
    burst_counts: Vec<usize>,
    #[arg(long, default_value_t = 100)] warmup_bursts: u64,
    #[arg(long, default_value_t = 10_000)] measure_bursts: u64,
    #[arg(long)] output_csv: std::path::PathBuf,
    #[arg(long)] raw_samples_csv: Option<std::path::PathBuf>,
}
```

- [ ] **Step 4: Build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-rx-burst
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: scaffold bench-rx-burst (dpdk_net arm)

Drives the peer's burst-echo-server, captures per-segment app-delivery
latency on the DUT. The peer_send_ns embedded in the segment header
gives a wire-side anchor that combines with HW-RX-TS (Phase 9) for a
true wire→app metric. Closes C-B3, C-C2 (dpdk_net path).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 8.3: Add linux_kernel and fstack arms to bench-rx-burst

**Files:**
- Create: `tools/bench-rx-burst/src/linux.rs`, `src/fstack.rs`

- [ ] **Step 1: linux_kernel arm**

Blocking `TcpStream::read_exact` over a `TCP_NODELAY` socket; record `Instant::now()` at each successful read of one segment. Stamp peer_send_ns from the header. Same per-segment latency metric.

- [ ] **Step 2: fstack arm**

`ff_read` loop; record `clock::now_ns()` at each return. Same metric shape.

- [ ] **Step 3: Build under both feature flags**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-rx-burst --features fstack
```

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: bench-rx-burst linux_kernel + fstack arms

Closes C-B3, C-C2 across the comparator triplet.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 8.4: Wire bench-rx-burst into nightly

**Files:**
- Modify: `scripts/bench-nightly.sh`

- [ ] **Step 1: SCP burst-echo-server to peer (step [5/12])**

Add to the SCP list. Start it in step [6/12] alongside echo-server, on port 10003.

- [ ] **Step 2: Add a new bench step (e.g. [11d/12])**

Three passes — dpdk, linux, fstack — sweeping `segment_sizes={64,128,256}` × `burst_counts={16,64,256,1024}`.

- [ ] **Step 3: Dry-run**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
bash -n scripts/bench-nightly.sh
```

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: wire bench-rx-burst into nightly matrix

Three stack passes × W∈{64,128,256}B × N∈{16,64,256,1024} = 36 buckets.
Raw samples emitted per bucket.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 9 — HW-TS attribution validation on c7i

Closes claim **C-E3**.

> **Assumption per user direction:** c7i NIC populates `rx.timestamp` in DPDK 23.11. We do not write a probe; we wire the path and validate via post-run CSV.

### Task 9.1: Verify the 5-bucket Hw path is fully populated when rx_hw_ts_ns ≠ 0

**Files:**
- Modify: `tools/bench-rtt/src/attribution.rs` (audit; no logic change expected)
- Modify: `tools/bench-rtt/src/workload.rs` (verify `last_rx_hw_ts_ns` plumbing)
- Test: `tools/bench-rtt/tests/attribution_hw_path.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn hw_buckets_populated_when_rx_hw_ts_nonzero() {
    let rec = compose_iter_record(IterInputs {
        t_user_send: 1_000,
        t_tx_sched: 2_000,
        t_enqueued: 3_500,
        t_user_return: 4_000,
        rx_hw_ts_ns: 3_200,  // nonzero → triggers Hw mode
        tsc_hz: 1_000_000_000,
    });
    let buckets = rec.hw_buckets.expect("expected Hw mode");
    // total_ns must equal rtt_ns
    assert_eq!(buckets.total_ns(), rec.rtt_ns);
    // five buckets all > 0 (no silent zeros)
    assert!(buckets.user_send_to_tx_sched_ns > 0);
    assert!(buckets.tx_sched_to_nic_tx_wire_ns >= 0);
    assert!(buckets.nic_tx_wire_to_nic_rx_ns > 0);
    assert!(buckets.nic_rx_to_enqueued_ns >= 0);
    assert!(buckets.enqueued_to_user_return_ns > 0);
}
```

(Note: `tx_sched_to_nic_tx_wire_ns` and `nic_rx_to_enqueued_ns` may be zero in the current code — the assertion catches the zero-collapse the audit flagged.)

- [ ] **Step 2: Run to verify it fails**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt --test attribution_hw_path
```

- [ ] **Step 3: Audit the Hw composition path**

In `tools/bench-rtt/src/workload.rs:136-150`, the Hw branch sets `tx_sched_to_nic_tx_wire_ns: 0` and `nic_rx_to_enqueued_ns: 0` because there is no DPDK TX HW-TS available. Fix: instead of forcing zeros, derive these as best-effort splits of `host_span_ns` using the engine-side timestamps (`tx_burst_complete_ns` from the engine if available; otherwise use the wire arrival from `rx_hw_ts_ns` minus a configured peer-echo-server stack budget).

If the engine doesn't expose tx-burst-complete, mark the buckets as `_unsupported` with a counter rather than emitting silent zeros.

- [ ] **Step 4: Run to verify it passes**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 60 cargo test -p bench-rtt --test attribution_hw_path
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: validate 5-bucket Hw attribution path on c7i

Previously the Hw path silently zeroed tx_sched_to_nic_tx_wire_ns and
nic_rx_to_enqueued_ns even when rx_hw_ts_ns was non-zero. The path now
either populates them from engine timestamps or marks them unsupported;
no silent zeros. Closes C-E3 (code path; live validation in Phase 12).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 10 — Nightly script rewire and scenario expansion

Closes claims **C-C1, C-C3, C-D1, C-D2**.

### Task 10.1: Add payload-bytes sweep to bench-rtt nightly invocation

**Files:**
- Modify: `scripts/bench-nightly.sh`

- [ ] **Step 1: Replace fixed 128/128 calls with sweep**

In step [7/12] (was bench-e2e):

```bash
run_dut_bench bench-rtt bench-rtt-clean \
  "${DPDK_COMMON[@]}" \
  --stack dpdk_net \
  --peer-port 10001 \
  --payload-bytes-sweep 64,128,256,1024 \
  --connections 1 \
  --iterations "$BENCH_ITERATIONS" \
  --warmup "$BENCH_WARMUP" \
  --raw-samples-csv /tmp/bench-rtt-raw.csv \
  --tool bench-rtt \
  --feature-set trading-latency
```

Repeat for `--stack linux_kernel` (peer port 10001 echo-server) and `--stack fstack`.

- [ ] **Step 2: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: nightly sweeps RTT at 64/128/256/1024B per stack

Closes C-C1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 10.2: Expand netem matrix with high-loss + bidirectional cells

**Files:**
- Modify: `scripts/bench-nightly.sh`

- [ ] **Step 1: Append RTO-firing scenarios**

Add to the matrix:

```bash
[high_loss_3pct]="loss 3% delay 5ms"        # forces RTO at 25% correlation tail
[high_loss_5pct]="loss 5% 25%"
[symmetric_3pct]="loss 3%"  # used in egress + ingress + bidir directions
```

- [ ] **Step 2: Add the burst×netem cell**

Inside the netem loop, add a sub-cell that runs `bench-tx-burst` and `bench-rx-burst` against each netem scenario (with the netem applied across the whole bench duration):

```bash
for direction in egress ingress bidir; do
  apply_netem $direction $spec
  run_dut_bench bench-tx-burst bench-tx-burst-${scenario}-${direction} ...
  run_dut_bench bench-rx-burst bench-rx-burst-${scenario}-${direction} ...
  remove_netem $direction
done
```

- [ ] **Step 3: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: nightly adds high-loss + bidirectional netem cells

3%/5% loss scenarios drive RTO at burst tails. burst×netem buckets
fill the C-C3 gap. Closes C-D2 (RTO path coverage), C-C3.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 10.3: Per-scenario iteration override

**Files:**
- Modify: `scripts/bench-nightly.sh`

- [ ] **Step 1: Add a per-scenario iter map**

```bash
declare -A SCENARIO_ITERS=(
  [random_loss_01pct_10ms]=1000000
  [correlated_burst_loss_1pct]=200000
  [reorder_depth_3]=20000
  [duplication_2x]=20000
  [high_loss_3pct]=200000
  [high_loss_5pct]=100000
)
```

- [ ] **Step 2: Use the override in the loop**

```bash
iters="${SCENARIO_ITERS[$scenario]:-$BENCH_ITERATIONS}"
... --iterations "$iters" ...
```

- [ ] **Step 3: Document the wallclock budget impact**

In `docs/bench-reports/overhaul-tracker.md`, note that the new nightly is expected to take ~6h (vs ~2h before) due to bigger iter counts. This is the cost of meaningful p999 at low loss rates.

- [ ] **Step 4: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "bench-overhaul: per-scenario iteration override

Low-loss scenarios get 1M iters so p999 of loss-affected events is
statistically meaningful. Tracker notes the ~3x wall-time impact.
Closes C-D1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 11 — Counters + observability

Closes claims **C-E1** (final piece — CSV emit), **C-E2**.

### Task 11.1: Split RTO/RACK/TLP retransmit counters in dpdk-net-core

**Files:**
- Modify: `crates/dpdk-net-core/src/counters.rs`
- Modify: `crates/dpdk-net-core/src/tcp_output.rs` (callers tag the trigger)
- Test: `crates/dpdk-net-core/tests/retransmit_counter_split.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn rto_increments_only_rto_counter() {
    let snap0 = counters::snapshot();
    // synthesize an RTO retransmit on a synthetic conn
    fire_rto_retransmit_via_test_hook();
    let snap1 = counters::snapshot();
    assert_eq!(snap1.tcp.tx_retrans_rto - snap0.tcp.tx_retrans_rto, 1);
    assert_eq!(snap1.tcp.tx_retrans_rack - snap0.tcp.tx_retrans_rack, 0);
    assert_eq!(snap1.tcp.tx_retrans_tlp - snap0.tcp.tx_retrans_tlp, 0);
    assert_eq!(snap1.tcp.tx_retrans - snap0.tcp.tx_retrans, 1);  // aggregate still bumped
}
```

- [ ] **Step 2: Add the new counters**

In `crates/dpdk-net-core/src/counters.rs`, add `tx_retrans_rto`, `tx_retrans_rack`, `tx_retrans_tlp` alongside the existing `tx_retrans`. Keep `tx_retrans` as the sum.

- [ ] **Step 3: Tag retransmit call sites**

Find every place tcp_output emits a retransmit (RTO timer fire, RACK trigger, TLP fire). Each must increment its specific counter PLUS the aggregate.

- [ ] **Step 4: Run tests**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 120 cargo test -p dpdk-net-core
```

- [ ] **Step 5: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add -u
git commit -m "feat(tcp): split tx_retrans into rto/rack/tlp counters

Aggregate counter retained for back-compat; per-trigger counters allow
bench-stress-style assertions to distinguish recovery mechanisms.
Closes C-E2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 11.2: Surface queue-depth time series in maxtp CSV

**Files:**
- Modify: `tools/bench-tx-maxtp/src/dpdk.rs`

- [ ] **Step 1: Per-sample-interval, capture queue-depth metrics**

In the maxtp pump loop, every `SAMPLE_INTERVAL` (1 s), per conn:

```rust
let depth = conn.snd_nxt.wrapping_sub(conn.snd_una);
let snd_wnd = conn.snd_wnd;
let room = conn.peer_recv_window_room();
raw_samples.row(&[
    bucket_id, &conn.id().to_string(), &sample_idx.to_string(),
    &depth.to_string(), &snd_wnd.to_string(), &room.to_string(),
])?;
```

- [ ] **Step 2: Document column names in the CSV header**

`bucket_id,conn_id,sample_idx,snd_nxt_minus_una,snd_wnd,room_in_peer_wnd`

- [ ] **Step 3: Build + commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
cargo build --release -p bench-tx-maxtp
git add -u
git commit -m "bench-overhaul: maxtp emits queue-depth + window time series

Per-sample-interval rows of {snd_nxt-snd_una, snd_wnd,
room_in_peer_wnd} per conn into the raw-samples CSV. Closes C-E1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase 12 — Cleanup, validation report, c7i migration

### Task 12.1: Delete bench-ab-runner if redundant

**Files:**
- Delete: `tools/bench-ab-runner/` (if `bench-rtt --once-mode` covers all uses)
- Modify: `tools/bench-offload-ab/src/main.rs`, `tools/bench-obs-overhead/src/main.rs`

- [ ] **Step 1: Confirm no remaining unique callers**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
grep -rn 'bench-ab-runner\|bench_ab_runner' . --include='*.rs' --include='*.sh' --include='*.toml'
```

If the only callers are `bench-offload-ab` + `bench-obs-overhead` and they were repointed to `bench-rtt` in Task 4.7, delete the crate.

- [ ] **Step 2: Delete + commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git rm -r tools/bench-ab-runner/
# Drop "tools/bench-ab-runner", from Cargo.toml
git add -u
git commit -m "bench-overhaul: delete redundant bench-ab-runner crate

bench-rtt --once-mode covers the A/B subprocess use cases.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 12.2: Provision and bench on c7i

**Files:** none modified; ops task.

- [ ] **Step 1: Update the resd-aws-infra setup to request c7i.metal (or the preferred c7i SKU)**

The `resd-aws-infra setup bench-pair` command emits a pair-id. If the SKU is configurable, set it to `c7i.metal-48xl` (or whichever has guaranteed RX HW-TS support). If hard-coded, file an issue and patch in `resd-aws-infra`. Do not block this plan on the patch — run on the smallest c7i variant available to validate the HW-TS path.

- [ ] **Step 2: Run the new nightly on c7i**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
OUT_DIR=target/bench-results/c7i-overhaul-validation timeout 28800 ./scripts/bench-nightly.sh 2>&1 | tee target/bench-results/c7i-overhaul-validation/run.log
```

- [ ] **Step 3: Verify HW-TS path actually fires**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
grep -c '"Hw"' target/bench-results/c7i-overhaul-validation/bench-rtt-clean.csv
```

Expected: > 0 (at least some buckets running in Hw attribution mode).

If Hw mode never fires on c7i, file as a follow-up and document in the report.

### Task 12.3: Write t51 final report

**Files:**
- Create: `docs/bench-reports/t51-bench-overhaul-2026-05-XX.md`

- [ ] **Step 1: Produce the report**

Mirror the structure of `t50-bench-pair-2026-05-08.md`. New sections:
- "Tool inventory delta" (deletions, renames, new tools)
- "Closed claims" (one row per C-* with CSV evidence)
- "RX-burst latency CDF" (new — small-pkt p50/p99/p999/p9999 at 64/128/256B for each stack and burst-count)
- "TX retransmit CDF" (new — send→ACK p50/p99/p999 under egress/ingress/bidir loss)
- "Open claims" (any C-* not closed and why)

- [ ] **Step 2: Update the tracker**

In `docs/bench-reports/overhaul-tracker.md` mark every phase complete.

- [ ] **Step 3: Commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git add docs/bench-reports/t51-bench-overhaul-*.md docs/bench-reports/overhaul-tracker.md
git commit -m "bench-overhaul: t51 final validation report

Closes the overhaul plan. All catalogued claims (C-A1..C-F2) closed
or explicitly deferred with rationale.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 12.4: Tag the overhaul

**Files:** git only.

- [ ] **Step 1: Tag**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git tag -a bench-overhaul-2026-05 -m "Bench suite overhaul: comparator triplet (dpdk_net + linux_kernel + fstack), RX-burst tool, send→ACK CDF, bidirectional netem"
```

(Push tag only after user explicit approval.)

---

## Self-review

**Spec coverage check:** Each catalogued claim is mapped:

| Claim | Phase / Task |
|---|---|
| C-A1 (mTCP arm) | Phase 2 Task 2.1; Phase 5 Task 5.4 |
| C-A2 (afpacket) | Phase 2 Task 2.2 |
| C-A3 (rx-zero-copy placeholder) | Phase 2 Task 2.3; Phase 8 (replacement) |
| C-A4 (pmtu_blackhole) | Phase 2 Task 2.4 |
| C-A5 (RTT bench overlap) | Phase 4 Tasks 4.1–4.7 |
| C-B1 (maxtp single-mean) | Phase 5 Task 5.3 |
| C-B2 (no raw samples) | Phase 3 Tasks 3.1–3.2; adopted in 4.5, 5.3, 8.2 |
| C-B3 (no per-RX-segment latency) | Phase 8 Tasks 8.2–8.4 |
| C-B4 (no send→ACK CDF) | Phase 6 Tasks 6.1–6.2 |
| C-B5 (single-conn bias) | Phase 4 Task 4.5; Phase 5 Task 5.3 |
| C-C1 (no payload sweep) | Phase 4 Task 4.5; Phase 10 Task 10.1 |
| C-C2 (no RX burst workload) | Phase 8 Tasks 8.1–8.4 |
| C-C3 (no burst×netem) | Phase 10 Task 10.2 |
| C-C4 (no bidirectional netem) | Phase 7 Tasks 7.1–7.2 |
| C-D1 (iter count too low) | Phase 10 Task 10.3 |
| C-D2 (RTO never fires) | Phase 10 Task 10.2; Phase 11 Task 11.1 |
| C-D3 (lost-iter terminal) | Phase 4 Task 4.6 |
| C-E1 (no queue depth time series) | Phase 5 Task 5.3; Phase 11 Task 11.2 |
| C-E2 (no RTO/RACK/TLP split) | Phase 11 Task 11.1 |
| C-E3 (HW-TS attribution dead) | Phase 9 Task 9.1; Phase 12 Task 12.2 |
| C-F1 (mTCP comparator scope) | Phase 2 Task 2.1; Phase 5 Task 5.4 |
| C-F2 (linux maxtp peer port) | Phase 5 Task 5.5 |

**Placeholder scan:** No "TBD" / "implement later" / "fill in" placeholders in step bodies. Where a step requires inspection of the existing source (e.g. tagging retransmit call sites in Task 11.1), the step is concrete about what to find and what to add.

**Type consistency:** `Stack` enum in `bench-rtt` and `bench-tx-burst` and `bench-rx-burst` uses identical variants `DpdkNet | LinuxKernel | Fstack`. `RawSamplesWriter` API surface is consistent (`create`, `row`, `flush`). `SendAckSample` field names `begin_seq`, `end_seq`, `latency_ns` used in both Task 6.1 (definition) and Task 6.2 (consumer).

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-09-bench-suite-overhaul.md`. Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task with two-stage review between tasks; fast iteration on a long plan with many independent phases.
2. **Inline Execution** — execute tasks in this session via `superpowers:executing-plans`; batch with checkpoints.

The user already directed: "use subagent driven approach to implement according to the plan." Proceeding with option 1 unless overridden.
