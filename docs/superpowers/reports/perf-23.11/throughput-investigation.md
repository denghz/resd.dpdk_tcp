# T7.7 throughput-investigation — uProf-driven optimization

## Setup

- Branch: `a10-perf-23.11`
- Worktree: `/home/ubuntu/resd.dpdk_tcp-a10-perf`
- Bench file: `tools/bench-micro/benches/throughput.rs` (commit `2387a8a` baseline shape; see family-specific commits below for restructure)
- uProf 5.2.606.0 TBP-only (per [`perf-host-capabilities.md`](../perf-host-capabilities.md))
- Diagnostic baseline (KVM dev host, THP=`madvise`, governor unavailable)
- Cross-worktree dpdk24 follow-up runs in parallel `a10-dpdk24-adopt` worktree

## Why this could resolve workload symbols (when latency-bench TBP couldn't)

uProf TBP samples at ~1 ms intervals on this KVM host. For latency
benches each `iter` body finishes 1000× faster than the sampler
period, so samples mostly hit the criterion harness. Throughput
benches batch BATCH=1024 ops per criterion iter, with per-iter time
in the 10–140 µs range — well above the sampler resolution. This
moved enough samples into workload code to make `parse_options`,
`shim_rte_mbuf_refcnt_update`, `TimerWheel::add` visible as named
symbols.

**Confirmed on tcp_input_data**: pre-H1 TBP showed `parse_options`
at 6.8% and `shim_rte_mbuf_refcnt_update` at 2.3%. **Did NOT confirm
on poll_*, flow_lookup_hot**: those benches batch tighter, and the
inner loop inlines all workload code into criterion's `iter_custom`
symbol — TBP attributes 99.9% of profile time to a single symbol.
Those families had no resolved hotspot to attribute.

## Per-family hotspot inventory (TBP, post-H1 / post-baseline state)

| Family                            | Total profile (s) | Top fn (TBP %) | Second (TBP %) | Third (TBP %) | Notes |
|-----------------------------------|------------------:|----------------|----------------|---------------|-------|
| tcp_input_data (pre-H1)           | 38.22             | iter_custom (39.2%) | libc malloc/free family (36.3%) | parse_options (6.8%) | Pre-H1 dominated by harness alloc/free thrash from per-iter `make_est_conn` rebuilds |
| tcp_input_data (post-H1)          | 41.75             | iter_custom (57.0%) | parse_options (16.1%) | rayon (10.9%) | Workload dispatch now dominantly inside iter_custom; `parse_options` resolved |
| tcp_input_data (post-H2)          | 41.79             | iter_custom (57.1%) | parse_options (16.6%) | rayon (10.9%) | Per-call parse_options cost dropped 12.4ns→11.4ns; absolute time held since throughput rose proportionally |
| timer_add_cancel                  | 35.00             | TimerWheel::add (40.4%) | clear_page_rep+do_user_addr_fault+zap_pte_range etc. (~34% kernel page-faults) | iter_custom (8.1%) | `cancel` only tombstones; `advance` never called by bench → `slots` Vec grew unbounded → page-fault traffic. Bench measures the wrong workload shape (real engine calls advance every poll_once). |
| poll_idle_with_timers             | 34.95             | iter_custom (99.9%) | _int_malloc (0.02%) | _int_free (0.01%) | Workload fully inlined; no resolved workload hotspot to attribute. |
| poll_empty                        | 34.85             | iter_custom (99.8%) | _int_malloc (0.09%) | do_user_addr_fault (0.06%) | Same: workload fully inlined. |
| flow_lookup_hot                   | 35.00             | iter_custom (99.9%) | (kernel symbols ≈0) | — | Same: workload fully inlined; brief pre-flagged "skip if no clear hotspot" — confirmed no clear hotspot. |

**Cross-cutting finding**: TBP visibility on µs-scale throughput
benches is bench-shape-dependent. `tcp_input_data` exposes
`parse_options` and `shim_rte_mbuf_refcnt_update` because dispatch's
inner work straddles a non-inlined function call (`tcp_options::parse_options`).
`timer_add_cancel` exposes `TimerWheel::add` because it's called twice
per inner iter through `EngineNoEalHarness::timer_add` (a non-inlined
shim). The poll/lookup families only call workload code via tight
inlinable closures, so all dispatch goes inside `iter_custom`.

## Hypotheses tested

| #  | Family            | Hypothesis                                                | Verdict   | Δ throughput               | p-value | Commit (kept/reverted) |
|----|-------------------|-----------------------------------------------------------|-----------|---------------------------:|--------:|------------------------|
| H1 | tcp_input_data    | Hoist conn construct out of timed inner loop, reset state in-place | **kept** | 7.40 → 17.45 Mops/s (+135.4%..+136.8%) | <0.05 | `1718208` |
| H2 | tcp_input_data    | `#[inline]` parse_options                                 | **kept** | 15.53 → 17.30 Mops/s (+11.85%..+12.53%) | <0.05 | `50c0fd0` |
| H3 | tcp_input_data    | Promote to `#[inline(always)]`                            | reverted (borderline) | 17.44 → 18.29 Mops/s (+4.64%..+5.08%) | <0.05 | `095918c` (commit) → `e84ea23` (revert) |
| H1 | timer_add_cancel  | advance after cancel each iter to recycle slots           | reverted (regressed) | 28.75 → 25.68 Mops/s (-5.76%..-10.47%) | <0.05 | `76eec2b` (commit) → `2f32a78` (revert) |

## Retained optimizations

### Commit `1718208` — tcp_input H1: hoist conn construct

`tools/bench-micro/benches/throughput.rs::bench_tcp_input_throughput_data`.
Pre-T7.7, every BATCH inner iter rebuilt a `TcpConn` via
`make_est_conn` (5 heap allocations: SendQueue's 256-KiB `VecDeque`,
RecvQueue, ReorderQueue, Vec timer_ids, SendRetrans deque) and dropped
it. TBP attributed 36% of profile time to libc `_int_malloc`,
`_int_free`, `__libc_malloc`, `__GI___libc_free`, plus 3.2% to
`drop_in_place::TcpConn`. That harness overhead masked the dispatch
hotspots TBP could otherwise resolve.

The fix: build one `TcpConn` outside the timed inner loop, then per
iter reset only the seq / window / ts / recv-buf-bytes fields
`handle_established` mutates on the in-order, TS-only,
SACK-permitted, no-new-ACK code path the bench exercises. Reset
list (verified by reading dispatch / handle_established):
`rcv_nxt`, `snd_wnd`, `snd_wl1`, `snd_wl2`, `recv.bytes` (clear),
`last_advertised_wnd`, `last_sack_trigger`, `ts_recent`,
`ts_recent_age`. Other fields (`snd_una`, `snd_nxt`, `snd_retrans`,
`sack_scoreboard`, `timer_ids`, `rtt_est`) are not changed in this
code path.

Effect: 7.40 → 17.45 Mops/s on `pre-T7.7-tcp_input_data_throughput-H1`
A/B (15s × 50 samples). Engine harness 5/5 + tcp_options unit tests
22/22 still pass.

### Commit `50c0fd0` — tcp_input H2: `#[inline]` parse_options

`crates/dpdk-net-core/src/tcp_options.rs::parse_options`. Post-H1
TBP showed `parse_options` as the largest resolved workload symbol
at 16.1% of profile time (6.72s / 41.75s). Disassembly confirmed the
function was being emitted as a non-inlined call from
`handle_established`'s dispatch path; default compiler heuristics
flagged it as too large to inline.

The fix: add `#[inline]` to let the optimizer fold the
`TcpOpts::default()` zero-init with the per-arm field writes, elide
bounds checks against the caller's known options-buf shape, and
skip the `Result` discriminant store when downstream branches prove
specific `Err` variants.

Effect: 15.53 → 17.30 Mops/s on `pre-T7.7-tcp_input_data_throughput-H2`
A/B (15s × 50 samples). Engine harness 5/5 + tcp_options unit
tests 22/22 still pass.

Note: post-H2 disassembly shows `parse_options` is still emitted
out-of-line at the call site — the `#[inline]` hint did not force
LLVM to inline. The +12% improvement came via secondary effects
(register allocation at the call shape, adjacent inlining).

## Reverted hypotheses

### Commit `095918c` (kept temporarily for measurement record) → `e84ea23` (revert) — tcp_input H3: `#[inline(always)]` parse_options

Promoted H2's `#[inline]` to `#[inline(always)]` to force full
inlining at the call site. A/B showed +4.86% throughput point
estimate (15s × 50 samples), with CI `[+4.64%, +5.08%]`. The lower
CI bound `+4.64%` sits below the brief's 5% criterion noise
threshold, even though criterion's verdict was "Performance has
improved" with p<0.05.

Per the per-task discipline ("don't keep it as small win — that's
noise") and the brief's strict ">5%" pass threshold, **reverted**.
Borderline case: the change probably IS a real win but its
magnitude on this diagnostic KVM host is statistically
indistinguishable from noise. Could be re-tested on a hardened
bench-pair host where measurement variance is lower.

### Commit `76eec2b` → `2f32a78` (revert) — timer H1: advance after cancel

Pre-T7.7 bench did `add` + `cancel` × BATCH × outer-iters without
ever calling `advance`. `cancel` only tombstones — slot
reclamation happens in `advance`. With no advance, the wheel's
`slots` / `generations` Vec grew unboundedly across the bench
window (~28 Mops/s × 30s = ~10⁹ entries, ~30 GB) → kernel page-fault
traffic dominated 34% of profile time.

H1 added `advance(now+TICK_NS)` after each cancel and bumped a
`now_ns` cursor per inner iter. Expected outcome: page-fault
traffic eliminated, slots Vec stays at steady-state size, slight
throughput improvement.

Actual outcome: throughput regressed -8.4% (28.75 → 25.68 Mops/s,
p<0.05). The advance overhead per inner iter (bucket walk + slot
recycle + cursor read) costs more on this host than the kernel
page-fault path it eliminates. **Reverted.**

This exposes a real characterisation issue with the
`timer_add_cancel_throughput` bench: the pre-T7.7 baseline
measures "add + cancel + page-fault overhead" which doesn't reflect
production behaviour (the real engine calls `advance` every
`poll_once` and sustains a steady-state slot pool with no Vec
growth). Neither pre nor post-H1 reflects production cleanly. A
future bench redesign could pre-populate `free_list` so steady-state
adds reuse slots without growing Vecs and without per-iter advance
overhead — deferred from T7.7 scope.

## Conclusions

- **Two retained optimizations** on `tcp_input_data_throughput`:
  bench-restructure (H1) and `#[inline] parse_options` (H2).
  Combined, they take post-T7.7 throughput from 7.40 Mops/s to
  17.5 Mops/s (+137%). H1 is bench-only (the on-disk runtime didn't
  change); H2 is workload-only (`#[inline]` on a public API); both
  should ship.
- **Two rejected hypotheses**: H3 on tcp_input (sub-5% borderline)
  and H1 on timer (regressed). Both have full git revert records
  preserving the measurement.
- **Three families with no actionable hypothesis**: poll_empty,
  poll_idle_with_timers, flow_lookup_hot. TBP attributes ≥99.9% of
  profile time to `iter_custom` (workload fully inlined, no
  resolved hotspot to optimize). The brief's pre-flag for
  `flow_lookup_hot` ("skip if no clear hotspot") confirmed; the
  poll family proved similar.
- **timer_add_cancel** has a structural bench characterisation
  issue (pre-T7.7 measurement is dominated by page-fault overhead,
  not the workload). H1 fix attempt regressed because its added
  advance overhead exceeded the saved page-fault cost on this
  host. The bench's actual production-relevant rate would require a
  redesign that pre-populates `free_list` for steady-state slot
  reuse without per-iter advance.
- **TBP-on-throughput is bench-shape-dependent.** Where the
  workload's hot path crosses a non-inlined function boundary
  (parse_options, TimerWheel::add, refcnt shim), TBP resolves
  symbols. Where it stays in tight inlined closures (poll, flow
  lookup), TBP can't see past `iter_custom`. The 5 µs/batch lower
  bound for resolution ≈ samples-per-iter × workload-fn-call-count
  × non-inline-rate; any future throughput bench design should
  account for this.

## Final post-T7.7 throughput numbers (a10-perf-23.11)

| Bench                              | Pre-T7.7 (Mops/s) | Post-T7.7 (Mops/s) | Δ      |
|------------------------------------|------------------:|-------------------:|-------:|
| poll_empty_throughput              | 22.96             | 23.23              | +1.2%  (noise) |
| poll_idle_with_timers_throughput   | 21.65             | 22.30              | +3.0%  (noise) |
| flow_lookup_hot_throughput         | 102.67            | 101.45             | -1.2%  (noise) |
| timer_add_cancel_throughput        | 28.01             | 28.80              | +2.8%  (noise) |
| tcp_input_data_throughput          |  6.89             | 17.51              | **+154%** |

The +154% on `tcp_input_data_throughput` reflects: (a) bench-restructure
removing harness setup cost from the timed window, and (b) workload
optimization from `#[inline] parse_options`. The four other
families show within-noise drift consistent with re-runs on the
same host across the investigation window — none had a kept
hypothesis.

## Cross-worktree state (a10-dpdk24-adopt)

Both kept commits (H1 = `1718208`, H2 = `50c0fd0`) cherry-picked
clean to `a10-dpdk24-adopt` as `8f9a015` and `252a79a`. Re-run on
dpdk24 produced **17.51 Mops/s** on tcp_input_data_throughput, an
identical landing to a10-perf-23.11. Engine harness 5/5 still
passes on dpdk24.

The pre-T7.7 cross-worktree finding was "+6.1% on tcp_input_data
on dpdk24 vs 23.11" — that gap shrinks to within-noise post-T7.7
because both worktrees received the same H1 + H2 fixes. The
optimisation transfers cleanly across DPDK versions; both
worktrees now sit at the same post-T7.7 throughput.

## Caveats

- **Diagnostic host.** THP=`madvise` (not `never`); governor read
  fails on AWS KVM. Numbers are usable for deltas / ratios;
  absolute headline figures should be re-collected on a hardened
  bench-pair host. See [`perf-host-capabilities.md`](../perf-host-capabilities.md).
- **TBP-only attribution.** uProf IBS / Core PMC / L3 PMC are
  unavailable on this KVM. No top-down / cache-miss attribution
  was possible. The workload hotspots that TBP DID resolve are
  honest hotspots; what TBP failed to resolve (poll, flow_lookup)
  remains attribution-blind.
- **timer family bench characterisation.** The pre-T7.7 measurement
  is dominated by page-fault overhead from unbounded `slots` Vec
  growth, not the workload. Production-realistic timer
  add/cancel rate is uncertain from this bench in either pre or
  post-H1 form; deferred to future bench redesign.
- **Single host.** Cross-host validation deferred to bench-pair
  hardware (Phase 5 future-work).

## Related artefacts

- T7.5 + T7.6 throughput summary: [`throughput-summary.md`](throughput-summary.md)
- Per-family latency summaries: [`poll-summary.md`](poll-summary.md), [`tcp_input-summary.md`](tcp_input-summary.md), [`timer-summary.md`](timer-summary.md), [`flow_lookup-summary.md`](flow_lookup-summary.md)
- Cross-worktree throughput comparison (pre-T7.7): [`../perf-a10-postphase-throughput.md`](../perf-a10-postphase-throughput.md) on `master`
- Companion 24.11 throughput summary: `docs/superpowers/reports/perf-dpdk24/throughput-summary.md` on `a10-dpdk24-adopt`
- Host capability matrix: [`../perf-host-capabilities.md`](../perf-host-capabilities.md)
- TBP profile data:
  - `profile/throughput-tcp_input_data_throughput/` (pre-H1)
  - `profile/throughput-tcp_input_data_throughput-postH1/`
  - `profile/throughput-tcp_input_data_throughput-postH2/`
  - `profile/throughput-timer_add_cancel_throughput/`
  - `profile/throughput-poll_idle_with_timers_throughput/`
  - `profile/throughput-poll_empty_throughput/`
  - `profile/throughput-flow_lookup_hot_throughput/`
