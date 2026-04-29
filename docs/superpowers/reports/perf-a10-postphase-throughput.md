# A10-perf post-phase — throughput addendum (cross-worktree, post-H1)

Companion to [`perf-a10-postphase.md`](perf-a10-postphase.md) (the
latency-focused cross-worktree summary). This file covers the
**throughput** numbers collected via the T7.1 throughput bench
(`tools/bench-micro/benches/throughput.rs`) on both worktrees:

- Worktree 1 — `a10-perf-23.11` at commit `2387a8a` (T7.1 added)
- Worktree 2 — `a10-dpdk24-adopt` at commit `6c635c7` (T7.1 cherry-picked)

Methodology: 30s `measurement_time` × 50 samples per bench, `BATCH=1024`
ops per criterion iter, identical bench source on both worktrees.
Diagnostic baseline (KVM dev host, THP=`madvise`, no governor read).
Numbers are good for *deltas*; absolute headline figures should be
re-collected on a hardened bench-pair host (T7 future-work — see
T3.3 / Phase 5).

## Cross-worktree comparison table

| Bench                              | 23.11+H1 (Mops/s) | 24.11+H1 (Mops/s) | Δ vs 23.11   | Verdict                           |
|------------------------------------|------------------:|------------------:|-------------:|-----------------------------------|
| poll_empty_throughput              | 22.96             | 22.96             | +0.0%        | tied (within noise)               |
| poll_idle_with_timers_throughput   | 21.65             | 22.18             | +2.4%        | small +ve, within noise           |
| flow_lookup_hot_throughput         | 102.67            | 102.67            | +0.0%        | tied (within noise)               |
| timer_add_cancel_throughput        | 28.01             | 28.01             | +0.0%        | tied (within noise)               |
| **tcp_input_data_throughput**      | **6.89**          | **7.31**          | **+6.1%**    | **statistically real (p<0.05)**  |

Four of five rows are tied within criterion's noise threshold. One
row — `tcp_input_data_throughput` — shows a clean, statistically
significant +6.1% on 24.11.

## Throughput-specific finding: tcp_input_data is 6.1% faster on 24.11

This is the single non-trivial cross-worktree throughput delta.
Latency on the same dispatch path was neutral, so this is a
throughput-only effect — it shows under sustained dispatch but not
under single-call timing.

**Plausible attribution (code-read only — uProf-PMU unavailable on
this KVM host).** The bench calls `MbufInsertCtx { mbuf, payload_offset }`
per dispatch and the in-order append path
(`tcp_input.rs:956`) calls `sys::shim_rte_mbuf_refcnt_update(ctx.mbuf.as_ptr(), 1)`
per segment. The shim wraps `rte_mbuf_refcnt_update` (a `static
inline` in `rte_mbuf.h`); 24.11's `rte_mbuf.h` differs from 23.11's,
and the regenerated `bindings.rs` is +797 lines / +6% larger.
Candidate paths:

1. **Bindgen-regenerated inline body.** 24.11's `rte_mbuf_refcnt_update`
   may differ in instruction count or branch shape under sustained
   refcount touches.
2. **I-cache layout shift.** Larger `bindings.rs` reshuffles function
   emission order; the dispatch hot-path may align differently inside
   L1i.
3. **Compiler codegen variance.** Same LLVM build, but the new
   `bindings.rs` surfaces type / inline / `repr` differences that
   change inlining decisions for the dispatch wrapper.

None are confirmable on this host. The finding is real (criterion
p<0.05, stable across 50 samples) but the attribution is conjecture.

## H1's throughput contribution (sanity check)

H1 (poll harness scratch reuse — landed on both worktrees) is the
only retained perf change in the post-Phase-3 state. It changed
`EngineNoEalHarness::poll_once` only — the four non-poll families
were untouched.

- Pre-H1 latency on `poll_once`: 1031 ns ≈ 0.97 Mops/s ceiling
- Post-H1 latency: ~46 ns ≈ 22 Mops/s
- Bench-measured post-H1 throughput: 22.96 Mops/s

**~23× sustained-load improvement on the poll path** — but this is
trivially predictable from the latency reduction (-95%). It is a
sanity check that the latency win translates 1:1 to throughput, not
a new throughput-specific finding.

## T7.5 throughput-only optimisation pass (outcome: rejected)

T7.5 attempted to surface a throughput-only optimisation that
single-call latency benches would miss. One concrete hypothesis was
tested:

**H2 — `EventQueue::is_empty()` guard on the `pop` drain loop in
`poll_once`.** Replace `while self.event_queue.pop().is_some() {}`
with `if !self.event_queue.is_empty() { while ... }` so the
common bench / steady-state case (queue empty) skips the
non-inlined `pop` function call entirely.

**Result on 23.11:** poll_empty +1.2% (p=0.01); poll_idle_with_timers
+1.7% (p=0.71). Criterion verdict: "Change within noise threshold"
on both. **Reverted.** The directional effect is consistent with
the hypothesis but the magnitude can't beat the diagnostic-host
noise floor. May be worth retrying on a hardened bench-pair host;
left as future-work data.

Detailed methodology + raw numbers in
[`perf-23.11/throughput-summary.md`](perf-23.11/throughput-summary.md)
on the `a10-perf-23.11` worktree.

No further actionable hypothesis surfaced from a code-read pass over
`engine.rs`, `tcp_input.rs`, and `tcp_events.rs`.

## Recommendation

The latency-focused A10-perf cross-worktree review
([`perf-a10-postphase.md`](perf-a10-postphase.md)) recommended
**staying on 23.11** for now, on the basis that 24.11's promotion
question hinges on full-stack measurement that a no-EAL bench
harness can't provide.

That recommendation **stands**. The +6.1% `tcp_input_data` throughput
improvement on 24.11 is a real signal — but:

- Magnitude is small (+6.1% on one of five families)
- Attribution is unconfirmed (code-read only — no PMU on this host)
- Other four families are tied within noise
- Real-traffic dispatch cost (mempool round-trip, rx_burst / tx_burst
  amortisation, NIC-driver hot-path) is not exercised by the
  no-EAL harness; the +6.1% may compound or evaporate under real
  packet I/O

**The +6.1% finding may justify revisiting the 24.11 promotion
question in the future**, but only when:

1. T3.3 lands real-send wiring through the engine, so dispatch is
   exercised against a populated mempool + actual rx_burst frames
2. A bench-pair host (TX vs RX on separate NICs) enables full-stack
   throughput measurement — not just no-EAL micro-benches
3. uProf-PMU (or equivalent top-down attribution) is available so
   the +6.1% can be traced to a specific code path / inline change

For now, the throughput finding is filed as a future-work data
point. The 23.11-stays recommendation from
[`perf-a10-postphase.md`](perf-a10-postphase.md) is unchanged.

## Caveats

- Diagnostic baseline (THP=`madvise`, governor unavailable, no PMU
  resolution at workload symbols). See
  [`perf-host-capabilities.md`](perf-host-capabilities.md).
- Single host — no bench-pair (TX/RX on separate NICs); no realistic
  packet-I/O cost in the loop.
- T3.3 real-send wiring deferred — dispatch is tested against a fake
  mbuf storage cell, not a live mempool round-trip.
- `EngineNoEalHarness` is a pure-compute surrogate, not the real
  `Engine::poll_once`. Real-traffic poll cost on a populated mempool
  is unmeasured here.

## Related artefacts

- Latency-focused cross-worktree summary:
  [`perf-a10-postphase.md`](perf-a10-postphase.md) (this same directory).
- Worktree 1 throughput summary:
  `docs/superpowers/reports/perf-23.11/throughput-summary.md` on
  `a10-perf-23.11`.
- Worktree 2 throughput summary:
  `docs/superpowers/reports/perf-dpdk24/throughput-summary.md` on
  `a10-dpdk24-adopt`.
- Worktree 1 latency summaries:
  `docs/superpowers/reports/perf-23.11/*.md` on `a10-perf-23.11`.
- Worktree 2 latency summaries:
  [`perf-dpdk24/summary.md`](perf-dpdk24/summary.md) and friends.
- Host capability matrix:
  [`perf-host-capabilities.md`](perf-host-capabilities.md).
