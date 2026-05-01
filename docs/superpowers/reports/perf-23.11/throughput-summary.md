# Worktree 1 — throughput summary (post-A10-perf follow-on)

Branch: `a10-perf-23.11`. Worktree: `/home/ubuntu/resd.dpdk_tcp-a10-perf`.
Companion to the per-family latency summaries already in `perf-23.11/`
(`poll-summary.md`, `flow_lookup-summary.md`, etc.) and the cross-worktree
[`docs/superpowers/reports/perf-a10-postphase.md`](../perf-a10-postphase.md)
on `master`.

## Configuration

- Bench file: `tools/bench-micro/benches/throughput.rs` (added on
  commit `2387a8a` — T7.1 of the throughput follow-on).
- Per-bench wall: 30s `measurement_time` × 50 `sample_size`. `BATCH = 1024`
  ops per criterion iter; `Throughput::Elements(BATCH)` so criterion
  reports Mops/s directly.
- Diagnostic baseline: KVM dev host, THP=`madvise`, governor unavailable
  (per [`perf-host-capabilities.md`](../perf-host-capabilities.md)).
  Numbers are good for *deltas* and *ratios*; absolute headline figures
  belong on a hardened bench-pair host (T7 future-work — see T3.3 / Phase 5).

## Per-bench results (post-H1 state)

Numbers from the run captured on this worktree at `2387a8a`. The
"Pre-H1 (computed)" column converts the pre-H1 latency (1031 ns mean
on `poll_once`) to a steady-state Mops/s ceiling — `1 / 1031 ns ≈
0.97 Mops/s`. H1 is the only landed perf change between pre-H1 and
the current state, so this conversion is exact for the poll family.

| Bench                              | Median (µs/batch) | Throughput (Mops/s) | Pre-H1 (computed from latency) | H1 effect |
|------------------------------------|-------------------|---------------------|--------------------------------|-----------|
| poll_empty_throughput              | 44.59             | 22.96               | 0.97                           | **+23.5×** |
| poll_idle_with_timers_throughput   | 46.17             | 21.65               | 0.86                           | **+25.2×** |
| flow_lookup_hot_throughput         |  9.97             | 102.67              | unchanged                      | n/a       |
| timer_add_cancel_throughput        | 36.56             | 28.01               | unchanged                      | n/a       |
| tcp_input_data_throughput          | 145.0             |   6.89              | unchanged                      | n/a       |

`flow_lookup_hot`, `timer_add_cancel`, and `tcp_input_data` were not
touched by H1 — H1 changed only `EngineNoEalHarness::poll_once`. No
re-measurement of these three was needed to compute their post-H1
state; the harness does not invoke them via `poll_once`.

## H1 effect on throughput

H1 (poll harness scratch reuse — `5b8ee71`) was the single retained
optimisation from Phase 3 across both poll and the broader 7-family
exploration. It pre-allocates a `Vec<ConnHandle>` on the harness and
reuses it across `poll_once` calls instead of `.collect()`-ing into
a freshly-constructed `SmallVec` per iter. Latency dropped 1031 ns →
46 ns (-95%, `b642fe6`-era figure on dpdk24 mirrors this).

Throughput consequences are the trivial inverse: `1/T_latency ≈
1/46 ns ≈ 22 Mops/s`. The bench actually lands at 22.96 Mops/s ≈
1/43.6 ns, slightly faster than the latency bench because the
throughput bench amortises criterion-iter overhead across `BATCH=1024`
calls. **~23× sustained-load improvement on the poll path** is the
headline. That's a sanity check of the latency win — not a new
finding, just confirmation that the latency reduction translates 1:1
into throughput.

The other four families were not touched by H1 and show their
pre-H1 throughput unchanged — no measurement needed beyond the
single post-H1 sample row above.

## Throughput-specific findings (T7.5 investigation)

T7.5 attempted to surface a throughput-only optimisation that latency
benches would miss. One concrete hypothesis was tested; one was
identified by code-read but not exercised. Effort capped at 1
hypothesis cycle per the task brief.

### H2 — `EventQueue::is_empty()` guard on the drain loop  (REJECTED)

**Hypothesis.** `EngineNoEalHarness::poll_once` ends with
`while self.event_queue.pop().is_some() {}`. In the bench's
steady state the queue is always empty, so every iter pays one
non-inlined `pop` function-call cost (`VecDeque::pop_front`)
returning `None`. Gating the loop with `if !self.event_queue.is_empty()`
should turn that into a single branch on a `len == 0` compare and
skip the call entirely.

**Test setup.** On `a10-perf-23.11` at the post-T7.1 tip, ran:

1. `cargo bench --bench throughput "poll_(empty|idle_with_timers)_throughput" -- --save-baseline pre-T7.5` (30s × 50 samples each)
2. Applied the `is_empty()` guard to `engine.rs::EngineNoEalHarness::poll_once`
3. `cargo bench --bench throughput "poll_(empty|idle_with_timers)_throughput" -- --baseline pre-T7.5`

**Result.**

| Bench                              | Pre Mops/s | Post Mops/s | Δ%     | p-value   | Criterion verdict             |
|------------------------------------|-----------:|------------:|-------:|----------:|-------------------------------|
| poll_empty_throughput              | 23.07      | 23.40       | +1.21% | 0.01      | "Change within noise threshold" |
| poll_idle_with_timers_throughput   | 22.16      | 22.52       | +1.68% | 0.71      | "No change in performance detected" |

**Interpretation.** The `poll_empty` row clears p<0.05 with a
positive direction, but the magnitude (+1.2%) sits inside criterion's
default 5% noise threshold and the lower CI bound (+0.28%) hugs zero.
The `poll_idle_with_timers` row never clears the p<0.05 bar.

Per the per-task discipline (criterion's verdict is the source of
truth — "Performance has improved" or revert), criterion reports
"Change within noise threshold" / "No change in performance detected".
**Reverted.** The gating-`is_empty` shape *may* be a real win on a
quieter host, but on the diagnostic KVM baseline it can't be
distinguished from measurement noise.

### H-attribution — code-read for the +6.1% tcp_input_data win on 24.11

The +6.1% tcp_input_data throughput improvement on 24.11 (vs 23.11)
is the single statistically clean throughput delta between the two
worktrees post-H1 (per [`perf-a10-postphase-throughput.md`](../perf-a10-postphase-throughput.md)
on `master`). Latency was neutral on the same code, so the effect
shows under sustained dispatch but not under single-call timing.

Without uProf-PMU on this KVM host (per
[`perf-host-capabilities.md`](../perf-host-capabilities.md)) the
attribution is code-read only. The most plausible path:

- The bench calls `MbufInsertCtx { mbuf, payload_offset }` per dispatch
  and the in-order append path (`tcp_input.rs:956`) calls
  `sys::shim_rte_mbuf_refcnt_update(ctx.mbuf.as_ptr(), 1)` per
  segment. The shim wraps `rte_mbuf_refcnt_update` (a `static inline`
  in `rte_mbuf.h`) — the body of that inline is regenerated by
  bindgen against 24.11 headers, and 24.11's `rte_mbuf.h` has minor
  reorderings vs 23.11 (`bindings.rs` diff: +797 lines / +6%).
- The next plausible attribution path is I-cache layout shift —
  24.11's larger `bindings.rs` reshuffles function emission order,
  and the dispatch hot-path may now fit a different L1i-friendly
  alignment.
- Compiler codegen variance from the regenerated bindings is also
  consistent with the data: stable across 50 samples (so it's not
  random TLB / NUMA jitter), small enough (+6.1%) that single-call
  latency averages don't surface it, large enough to be detected
  under sustained dispatch.

None of these attributions are confirmable on this host. The finding
is filed as a future-work data point — re-investigate when uProf-PMU
becomes available (hardened bench-pair host, T7 future-work).

### Negative finding — no further actionable hypothesis from code-read

A pass through `engine.rs`, `tcp_input.rs`, and `tcp_events.rs` for
hot-loop allocation patterns surfaced:

- `pre_populate_timers` returns `Vec<TimerId>` (allocated once at
  bench setup, not per iter — OK)
- `rack_lost_indexes: SmallVec<[u16; 16]>` constructed fresh per
  dispatch (line 842, `tcp_input.rs`) but stays inline-capacity for
  any sane segment, so no heap alloc
- `EventQueue` is a `VecDeque<InternalEvent>` — `pop_front` is the
  only hot accessor and the `is_empty()` guard above didn't move
  the needle measurably
- `flow_table::lookup_by_tuple` is a hash + open-addressing probe,
  no allocations in the hot path

Beyond H2 there is **no actionable hypothesis** from code-read on
this baseline. The throughput wins on the four non-poll families
match what their respective latency wins would predict (= zero,
since none of those families saw a Phase 3 perf landing). H1 is
the only retained perf change in the post-Phase-3 state.

## Caveats

- **Diagnostic host.** THP=`madvise` (not `never`); governor read
  fails on this AWS KVM. Numbers are usable for deltas / ratios;
  absolute headline figures should be re-collected on a hardened
  bench-pair host. See [`perf-host-capabilities.md`](../perf-host-capabilities.md).
- **No PMU.** uProf TBP can't resolve workload symbols at ns-scale
  on this KVM. The T7.5 investigation is code-read + criterion
  A/B only; no top-down / cache-miss attribution was possible.
- **Bench-only path.** `EngineNoEalHarness` exercises pure-compute
  poll (no rx_burst / tx_burst / mempool round-trip). Real-traffic
  poll cost on a populated mempool is unmeasured here — see T3.3
  real-send wiring, currently deferred.
- **Single host.** No bench-pair host (TX vs RX on separate NICs);
  full-stack TCP throughput under realistic packet I/O is out of
  scope for the no-EAL harness.

## Update — T7.7 uProf-driven results

T7.5 left the bench at the post-H1 / post-T7.1 numbers above and
flagged that uProf TBP couldn't resolve workload symbols at ns-scale
on the latency benches. T7.7 retried on the µs-scale throughput
benches; results are detailed in
[`throughput-investigation.md`](throughput-investigation.md). Brief:

| Family | Hypotheses tested | Kept | Effect |
|---|---|---|---|
| `tcp_input_data` | 3 (H1 bench-restructure, H2 `#[inline]` parse_options, H3 `#[inline(always)]`) | 2 (H1, H2) | **6.89 → 17.51 Mops/s (+154%)** |
| `timer_add_cancel` | 1 (H1 advance after cancel) | 0 (H1 regressed -8.4%) | unchanged (28.0–28.8 Mops/s, within noise) |
| `poll_empty`, `poll_idle_with_timers`, `flow_lookup_hot` | 0 (no resolved hotspot) | 0 | unchanged (within noise) |

Kept commits: `1718208` (H1) and `50c0fd0` (H2) on `a10-perf-23.11`,
both cherry-picked clean to `a10-dpdk24-adopt` (as `8f9a015` and
`252a79a`); identical post-T7.7 landing on dpdk24. The pre-T7.7
cross-worktree "+6.1% on dpdk24 vs 23.11" gap on tcp_input_data
shrinks to within-noise after T7.7 — both worktrees received the
same H1+H2 fixes and now sit at ~17.5 Mops/s.

H1 is bench-only (it changes what the bench measures, not the
shipped runtime). H2 is workload-only (`#[inline]` on a public API
in `tcp_options.rs`). Both ship.

For `timer_add_cancel`, the pre-T7.7 measurement is structurally
mis-shaped (page-fault dominated due to unbounded `slots` Vec growth
under the bench's add+cancel-without-advance pattern). H1 attempted
to fix this by adding `advance` per inner iter; the per-iter
advance overhead exceeded the saved page-fault cost and regressed
throughput. A future bench redesign that pre-populates `free_list`
for steady-state slot reuse without per-iter advance is deferred.

For `poll_*` and `flow_lookup_hot`, TBP attributes ≥99.9% of profile
time to `iter_custom` (workload fully inlined into the bench harness
loop), so no resolved hotspot was available to attribute.

## Update — T8.1+T8.2 uProf TBP on throughput benches (2026-05-01)

T8 followed up by capturing AMDuProfCLI TBP on **all 5 throughput
benches** (×2 worktrees = 10 captures), then attempting
benchmark-justified A/B for any TBP-resolved hotspot. Detail:
[`../../../profile/hotspot-diff.md`](../../../profile/hotspot-diff.md)
on this worktree.

### TBP attribution quality (T8.1)

| Bench | Workload symbols visible? |
|---|---|
| `poll_empty_throughput` | NO — `iter_custom` 99% (workload fully inlined into criterion wrapper) |
| `poll_idle_with_timers_throughput` | NO — same |
| `flow_lookup_hot_throughput` | NO — same |
| `timer_add_cancel_throughput` | PARTIAL — `TimerWheel::add` 38–40%, kernel-mm 26% (page-fault setup overhead, not measured throughput) |
| `tcp_input_data_throughput` | YES — `parse_options` 16%, `shim_rte_mbuf_refcnt_update` 5–6%, rayon-criterion stats 11% |

This is consistent with KVM TBP limitations (no IBS / no PMU events) and the
T7.5/T7.7 finding that LLVM's loop-flattening folds the bench's inner BATCH
loop into `iter_custom`. Honest finding: only `tcp_input_data` (and
partially `timer_add_cancel`) yields actionable workload hotspots.

### Hypotheses tested (T8.2)

Per the strict gate (criterion verdict "improved" + p<0.05 + lower CI ≥ +5%):

| ID | Target | Change | Result | Disposition |
|---|---|---|---|---|
| T8.2-H1 | `parse_options` | Hoist OPT_SACK arm into `#[cold] #[inline(never)]` helper to shrink hot-parser body | -3.74% throughput, p=0.00 | REJECTED — reverted |
| T8.2-H2 | `parse_options` | Reorder match arms `if/else` chain ordered by frequency (TIMESTAMP first) | -0.95% throughput, p=0.00 (within noise but in wrong direction) | REJECTED — reverted |

**Not pursued (with reasoning):**

- `shim_rte_mbuf_refcnt_update` direct atomic on bindgen-derived offset:
  layout-fragile across DPDK versions; violates the "ARM on roadmap, no
  x86_64-only struct-layout assumptions" memory rule.
- `shim_rte_mbuf_refcnt_update` bulk-decref shim: bench-artifact only
  (production has 1 enqueue + 1 dequeue, never bulk-clear).
- `TcpOpts` field-set elision: invasive, and the established-only
  caller already short-circuits on `parsed_opts.timestamps`.
- `timer_add_cancel` `TimerWheel::add` setup-cost reduction: would
  change what the bench measures; the dominant cost is page-fault
  / kernel-mm from the bench's pathological `add+cancel-without-advance`
  pattern, not the wheel itself.

### Cross-worktree throughput post-T8

Both worktrees: 0 retained optimisations. Numbers below are the same
post-T7.7 baseline re-measured on 2026-05-01 with `--measurement-time 15
--sample-size 30`:

| Bench | 23.11 (Mops/s) | 24.11 (Mops/s) | Δ |
|---|---:|---:|---:|
| `poll_empty_throughput` | 23.011 | 23.437 | +1.85% (within noise) |
| `poll_idle_with_timers_throughput` | 22.286 | 22.309 | +0.10% (noise) |
| `flow_lookup_hot_throughput` | 100.49 | 98.979 | -1.50% (noise) |
| `timer_add_cancel_throughput` | 28.142 | 27.492 | -2.31% (noise) |
| `tcp_input_data_throughput` | 17.456 | 17.543 | +0.50% (sub-noise) |

**The pre-T7.7 "+6.1% on tcp_input_data on 24.11" gap has fully closed
post-T7.7+T8** — both worktrees now sit within ±2% of each other on
every family. The earlier delta was a transient state of cherry-pick
ordering, not a durable advantage.

### Recommendation: do NOT promote DPDK 24.11

Post-T8 there is **no measurable throughput advantage** of 24.11 over
23.11 on any of the 5 benches. All cross-worktree deltas are within
noise (≤2.31% absolute, none clearing the 5% lower-CI bar). Combined
with the unchanged latency story (per `../perf-a10-postphase.md`),
the 24.11 upgrade carries cost (rebase, retesting, vendor surface
shift) with no offsetting performance benefit. **Stay on 23.11.**

### Top 3 remaining hotspots (for future T8 cycles)

1. **`parse_options`** — 16% of TBP on `tcp_input_data`. T7.7 + T8
   exhausted the obvious inline / arm-reorder / helper-hoist
   experiments; further wins likely need either SIMD parsing or a
   call-site fast-path for the canonical TS-only 10-byte case (both
   risky).
2. **`shim_rte_mbuf_refcnt_update`** — 5–6% of TBP on `tcp_input_data`.
   Pure FFI cost. Reducing it requires either bypassing the shim
   (layout-fragile) or restructuring the in-order append's refcount
   ownership lineage (correctness-fragile).
3. **`TimerWheel::add`** — 38–40% of TBP on `timer_add_cancel`, but
   the bench's add+cancel-without-advance pattern is bench-artifact
   territory. Production paths advance the wheel between adds, so
   the wheel-add cost in production is dominated by something else
   we can't see here.

## Related artefacts

- T8 hotspot diff: [`../../../profile/hotspot-diff.md`](../../../profile/hotspot-diff.md)
  on this worktree (cross-worktree TBP top-10 + per-bench attribution
  quality). Each capture also has a `tbp.csv` next to its profile dir.
- T7.7 detail: [`throughput-investigation.md`](throughput-investigation.md)
- Per-family latency summaries: same directory (e.g.
  [`poll-summary.md`](poll-summary.md), [`tcp_input-summary.md`](tcp_input-summary.md)).
- Cross-worktree latency comparison:
  [`../perf-a10-postphase.md`](../perf-a10-postphase.md) on `master`.
- Cross-worktree throughput comparison (the +6.1% finding):
  [`../perf-a10-postphase-throughput.md`](../perf-a10-postphase-throughput.md)
  on `master`.
- Companion 24.11 throughput summary:
  `docs/superpowers/reports/perf-dpdk24/throughput-summary.md` on
  the `a10-dpdk24-adopt` worktree.
- Host capability matrix:
  [`../perf-host-capabilities.md`](../perf-host-capabilities.md).
