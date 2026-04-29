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

## Related artefacts

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
