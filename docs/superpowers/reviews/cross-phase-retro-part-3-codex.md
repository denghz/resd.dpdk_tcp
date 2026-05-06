# Part 3 Cross-Phase Retro Review (Codex)
**Reviewer:** codex:codex-rescue
**Reviewed at:** 2026-05-05
**Part:** 3 — Loss recovery + observability (RACK/RTO/TLP/retransmit + event log + RTT histogram)
**Phases:** A5, A5.5, A5.6

## Verdict
Mechanical review found four BUG-class findings across three mechanical defect classes: retransmit fire handlers treat a void retransmit primitive as success-signaling (TLP and RACK), the event-queue C ABI default is documented but not applied, and invalid RTO bound ordering can reach a runtime panic path. The A5.6 RTT histogram runtime ladder is internally consistent, but its design/test expected-bucket row still disagrees with the shipped edge set. I did not find a new PR #9-style final-release mbuf leak in the retransmit rollback path.

## Architectural drift
- **FYI — A5.6 has no standalone phase tag, and the histogram code is A6-labeled at HEAD.** This matches the task brief rather than a new code defect: the design doc says the work was absorbed into A6, and the code comments now name A6.

`docs/superpowers/specs/2026-04-19-stage1-phase-a5-6-rtt-histogram-design.md:1-3`
```text
# Phase A5.6 — Per-connection RTT histogram (Design Spec)
**Status:** ABSORBED INTO A6.
```

`crates/dpdk-net-core/src/engine.rs:23-25`
```rust
/// A6 (spec §3.8.2): default RTT histogram bucket edges, µs.
/// Applied when `EngineConfig::rtt_histogram_bucket_edges_us` is all zero.
pub const DEFAULT_RTT_HISTOGRAM_EDGES_US: [u32; 15] = [
```

## Cross-phase invariant violations
- **BUG — `on_tlp_fire` records a TLP probe even when `retransmit()` queued nothing.** The TLP fire path calls a void retransmit primitive, then increments `tcp.tx_tlp`, records the recent probe, consumes the TLP budget, and can emit `TcpRetrans`/`TcpLossDetected`. But `retransmit_inner` has normal early-return paths for stale entries and ENOMEM, and `tcp.tx_retrans` only increments after the frame is successfully built/chained/pushed. Under header-mbuf exhaustion, stale index drift, or a chain failure, counters and probe state say a probe fired while no retransmit frame was queued.

`crates/dpdk-net-core/src/engine.rs:3206-3208`
```rust
let probe_idx = retrans_len - 1;
self.retransmit(handle, probe_idx);
crate::counters::inc(&self.counters.tcp.tx_tlp);
```

`crates/dpdk-net-core/src/engine.rs:3221-3225`
```rust
if let Some((probe_seq, probe_len)) = probe_info {
    let now_ns = crate::clock::now_ns();
    let mut ft = self.flow_table.borrow_mut();
```

`crates/dpdk-net-core/src/engine.rs:5879-5881`
```rust
let Some(conn) = ft.get(conn_handle) else {
    return;
};
```

`crates/dpdk-net-core/src/engine.rs:5933-5935`
```rust
let hdr_mbuf = unsafe { sys::shim_rte_pktmbuf_alloc(self.tx_hdr_mempool.as_ptr()) };
if hdr_mbuf.is_null() {
    inc(&self.counters.eth.tx_drop_nomem);
```

`crates/dpdk-net-core/src/engine.rs:6221-6223`
```rust
entry.xmit_count = entry.xmit_count.saturating_add(1);
entry.xmit_ts_ns = crate::clock::now_ns();
entry.lost = false;
```

- **BUG — RACK loss accounting has the same void-retransmit success assumption.** The A5 counter contract says `tcp.tx_rack_loss` means the RACK detect-lost rule fired and a retransmit was queued. HEAD increments it immediately after the same void `self.retransmit(...)` call, so ENOMEM/stale-entry paths can produce `tx_rack_loss` and per-packet `TcpRetrans` events without `tx_retrans` or an actual queued frame.

`crates/dpdk-net-core/src/engine.rs:4286-4288`
```rust
for i in &outcome.rack_lost_indexes {
    self.retransmit(handle, *i as usize);
    crate::counters::inc(&self.counters.tcp.tx_rack_loss);
```

`crates/dpdk-net-core/src/engine.rs:4294-4299`
```rust
let (seq, rtx_count) = {
    let ft = self.flow_table.borrow();
    ft.get(handle)
```

## Tech debt accumulated
- **SMELL — the retransmit primitive still has no success/failure result despite multiple callers needing one.** A5 originally made `retransmit()` own mbuf allocation, chaining, queue push, entry-state update, and `tx_retrans`. A5.5 then layered TLP probe budget and RACK/RTO forensic events around it, but the function signature stayed `()`. The mechanical debt is now observable as the two BUGs above; the repair should make success explicit, for example `enum RetransmitOutcome { Queued { seq, xmit_count }, NoSuchEntry, NoBackingMbuf, NoMem }`.

`crates/dpdk-net-core/src/engine.rs:5824-5826`
```rust
pub(crate) fn retransmit(&self, conn_handle: ConnHandle, entry_index: usize) {
    self.retransmit_inner(conn_handle, entry_index)
}
```

`crates/dpdk-net-core/src/engine.rs:6161-6163`
```rust
let _ = did_adj;
unsafe { sys::shim_rte_pktmbuf_free(hdr_mbuf) };
unsafe { sys::shim_rte_mbuf_refcnt_update(data_mbuf_ptr, -1) };
```

## Test-pyramid concerns
- **SMELL — the A5.6 histogram bucket-edge test knowingly diverges from the design document at `30000us`.** The runtime ladder is consistent with the default edges and inclusive `<=` rule: `30000 > 25000` and `30000 <= 50000`, so bucket 12 is correct for the code. The design test plan still says bucket 11, and the unit test resolves the conflict by declaring the code the source of truth instead of fixing the design doc.

`crates/dpdk-net-core/src/rtt_histogram.rs:28-31`
```rust
pub fn select_bucket(rtt_us: u32, edges: &[u32; 15]) -> usize {
    for i in 0..15 {
        if rtt_us <= edges[i] {
```

`crates/dpdk-net-core/src/rtt_histogram.rs:81-83`
```rust
// treating the code's algorithm result against the §3.2 edges as
// the source of truth for this assertion.
assert_eq!(select_bucket(30000, &edges), 12);
```

`docs/superpowers/specs/2026-04-19-stage1-phase-a5-6-rtt-histogram-design.md:193-195`
```text
- **Bucket selection**: given the default edges, RTT values `{ 10, 50, 75, 150, 1000, 2000, 30000, 600000 }` µs land in buckets `{ 0, 0, 1, 2, 6, 7, 11, 15 }` respectively.
```

- **SMELL — the ignored synthetic-peer TAP scenarios are exactly where TLP/RACK failure accounting would be caught.** The existing scenario comments assert positive-path counters, but the ignored harness does not force retransmit ENOMEM or stale-entry failures around `on_tlp_fire` / RACK fire. That leaves the void-return accounting bug invisible unless a targeted unit/harness test injects a header-mempool allocation failure during `retransmit_inner`.

`crates/dpdk-net-core/tests/tcp_rack_rto_retrans_tap.rs:50-58`
```rust
#[ignore = "requires synthetic-peer TAP harness with selective drop"]
fn tlp_fires_on_tail_loss_and_probes_last_segment() {
    // Expected:
```

## Observability gaps
- **BUG — A5.5 event-queue default `4096` is documented but not applied at the C ABI entry point.** `dpdk_net_engine_create` rejects `event_queue_soft_cap < 64` before any default substitution, so a zero-initialized C config that relies on documented defaults fails with `NULL` instead of getting the 4096-event queue. This is a mechanical defaulting bug in the A5.5 observability surface, separate from the event queue's internal drop-oldest logic.

`crates/dpdk-net/src/lib.rs:149-151`
```rust
if cfg.event_queue_soft_cap < 64 {
    return ptr::null_mut();
}
```

`include/dpdk_net.h:105-107`
```c
 * A5.5 event-queue overflow guard (§3.2 / §5.1). Default 4096;
 * must be >= 64. Queue drops oldest on overflow.
 */
```

`crates/dpdk-net-core/src/engine.rs:1465-1467`
```rust
flow_table: RefCell::new(FlowTable::new(cfg.max_connections)),
events: RefCell::new(EventQueue::with_cap(cfg.event_queue_soft_cap as usize)),
iss_gen: IssGen::new(),
```

## Memory-ordering / ARM-portability concerns
- **FYI — I did not find a new atomic ordering defect in the A5.5 event log or A5.6 histogram path.** `EventQueue` mutates its `VecDeque` behind the engine's `RefCell`, then uses relaxed atomics only for observability counters. The histogram intentionally has no atomics because it is per-connection state under the single-lcore engine model.

`crates/dpdk-net-core/src/tcp_events.rs:168-177`
```rust
if self.q.len() >= self.soft_cap {
    let _ = self.q.pop_front();
    counters.obs.events_dropped.fetch_add(1, Ordering::Relaxed);
```

`crates/dpdk-net-core/src/rtt_histogram.rs:1-4`
```rust
//! Per-connection RTT histogram (spec §3.8). 16 × u32 buckets, exactly
//! 64 B / one cacheline via `repr(C, align(64))`. Update cost: 15-
//! comparison ladder + one `wrapping_add` on cache-resident state.
```

## C-ABI / FFI
- **BUG — invalid RTO bounds are public config but only debug-asserted inside `RttEstimator`.** `EngineConfig` exposes `tcp_min_rto_us`, `tcp_initial_rto_us`, and `tcp_max_rto_us`; active and passive connection construction passes them directly into `RttEstimator::new`. The estimator only has `debug_assert!` ordering checks, so release builds accept `min > max`; the next RTT sample reaches `u32::clamp(min, max)`, which panics when `min > max`, and pre-sample timers can also exceed the configured cap. This should be rejected at `Engine::new` / C ABI creation time just like histogram edges.

`crates/dpdk-net-core/src/engine.rs:5169-5171`
```rust
self.cfg.tcp_min_rto_us,
self.cfg.tcp_initial_rto_us,
self.cfg.tcp_max_rto_us,
```

`crates/dpdk-net-core/src/tcp_rtt.rs:26-28`
```rust
pub fn new(min_rto_us: u32, initial_rto_us: u32, max_rto_us: u32) -> Self {
    debug_assert!(min_rto_us <= initial_rto_us);
    debug_assert!(initial_rto_us <= max_rto_us);
```

`crates/dpdk-net-core/src/tcp_rtt.rs:51-53`
```rust
let srtt = self.srtt_us.unwrap();
let rto = srtt.saturating_add(self.rttvar_us.saturating_mul(4));
self.rto_us = rto.clamp(self.min_rto_us, self.max_rto_us);
```

## Hidden coupling
- **FYI — PR #9's final-release mbuf leak pattern was not reproduced in the retransmit rollback I inspected.** The scary-looking `shim_rte_mbuf_refcnt_update(data_mbuf_ptr, -1)` in `retransmit_inner` is a rollback of the immediately preceding `+1` before the chain takes ownership, not the final release of an owning handle. The final-release paths that mattered for PR #9 use `shim_rte_pktmbuf_free_seg`, and the TX retransmit drift regression documents the rollback pair as the intended invariant.

`crates/dpdk-net-core/src/engine.rs:6144-6146`
```rust
};
unsafe { sys::shim_rte_mbuf_refcnt_update(data_mbuf_ptr, 1) };
let rc = unsafe { sys::shim_rte_pktmbuf_chain(hdr_mbuf, data_mbuf_ptr) };
```

`crates/dpdk-net-core/src/engine.rs:6161-6163`
```rust
let _ = did_adj;
unsafe { sys::shim_rte_pktmbuf_free(hdr_mbuf) };
unsafe { sys::shim_rte_mbuf_refcnt_update(data_mbuf_ptr, -1) };
```

`crates/dpdk-net-core/src/mempool.rs:273-275`
```rust
// A10 Stage B fix: use rte_pktmbuf_free_seg, NOT
// rte_mbuf_refcnt_update(-1). The refcnt_update primitive only
// decrements the atomic; it does NOT return the mbuf to its
```

## Documentation drift
- **SMELL — `tcp_retrans.rs` still says `hdrs_len` is set to 0 after the first retransmit, but HEAD intentionally leaves it unchanged.** The engine comment now says `hdrs_len` remains a forensic record and live prefix detection uses `data_len - entry_len`. The stale field doc is small but misleading in exactly the mbuf-shape area reviewers look at for retransmit leaks and duplicate-header bugs.

`crates/dpdk-net-core/src/tcp_retrans.rs:36-40`
```rust
/// primitive strips these bytes via `rte_pktmbuf_adj` before chaining
/// a fresh header mbuf, so the on-wire retrans frame is well-formed
/// (single L2+L3+TCP header) and `data_len == len` thereafter. Once
```

`crates/dpdk-net-core/src/engine.rs:6212-6216`
```rust
// (`entry.hdrs_len` is the construction-time prefix; the
// live-prefix detection in Phase 4 reads `data_len - entry_len`
// each time, so we don't need to mutate `hdrs_len` after a
```

## FYI / informational
- **FYI — sequence-number wrap handling in the scoped retrans/RACK helpers uses modular helpers where I checked it.** `tcp_retrans.rs` uses `seq_le` / `seq_lt` around `wrapping_add` endpoints, and `tcp_rack.rs` does the same for end-sequence comparisons.

`crates/dpdk-net-core/src/tcp_retrans.rs:129-132`
```rust
while let Some(front) = self.entries.front() {
    let end_seq = front.seq.wrapping_add(front.len as u32);
    if seq_le(end_seq, snd_una) {
```

`crates/dpdk-net-core/src/tcp_rack.rs:138-140`
```rust
let end_seq = e.seq.wrapping_add(e.len as u32);
if crate::tcp_seq::seq_le(end_seq, snd_una) {
    // Already cum-ACKed; prune_below will drop it shortly.
```

## Verification trace
- Git/tag commands run:
  - `git status --short`
  - `git tag --list 'phase-a*-complete'`
  - `git log --oneline phase-a4-complete..phase-a5-complete -- crates/dpdk-net-core/src/tcp_rack.rs crates/dpdk-net-core/src/tcp_rto.rs crates/dpdk-net-core/src/tcp_tlp.rs crates/dpdk-net-core/src/tcp_retrans.rs crates/dpdk-net-core/src/tcp_events.rs crates/dpdk-net-core/src/rtt_histogram.rs crates/dpdk-net-core/src/tcp_rtt.rs crates/dpdk-net-core/src/engine.rs`
  - `git log --oneline phase-a5-complete..phase-a5-5-complete -- ...same scoped paths...`
  - `git log --oneline phase-a5-5-complete..phase-a6-complete -- crates/dpdk-net-core/src/rtt_histogram.rs`
  - `git log --oneline --all --grep='PR #9\\|cliff\\|free_seg\\|refcnt'`
- Grep/search commands run:
  - `rg --files crates/dpdk-net-core/src docs/superpowers/reviews docs/superpowers/specs`
  - `rg -n "on_tlp_fire|retransmit\\(|events_dropped|Ordering|Atomic|timer_wheel\\.(add|cancel)|rte_pktmbuf_free_seg|rte_mbuf_refcnt_update|seq_lt|seq_le|wrapping|saturating|bucket|histogram|borrow_mut|Box::from_raw|unsafe|Err\\(|\\?" ...`
  - `rg -n "fn on_.*fire|on_tlp_fire|tlp|rack|rto|TimerKind|TimerWheel|timer_wheel|rtt_histogram|events_dropped|EventLog|soft" crates/dpdk-net-core/src/engine.rs`
  - `rg -n "rtt_histogram|RttHistogram|histogram|rtt_est\\.sample|rtt_samples|maybe_seed_srtt|sample\\(" ...`
  - `rg -n "free_seg|refcnt_update\\(.*-1|PR #9|cliff|mbuf_refcnt_drop|shim_rte_pktmbuf_free_seg|rte_pktmbuf_free_seg|rte_mbuf_refcnt_update" ...`
  - `rg -n "tcp_min_rto_us|tcp_initial_rto_us|tcp_max_rto_us|Invalid.*RTO|validate.*rto|rto_us|EngineConfig" ...`
  - Prior-review duplicate checks with `rg -n "TLP|tlp|RACK|rack|RTO|rto|retransmit|tx_tlp|tx_rack_loss|tx_rto|tx_retrans|events_dropped|EventQueue|histogram|rtt_histogram|rte_pktmbuf_free_seg|refcnt_update|timer|stale|counter|TcpRetrans|TcpLossDetected" ...`
- Files read at HEAD with line ranges:
  - `crates/dpdk-net-core/src/engine.rs`: `16-52`, `2860-3265`, `4190-4475`, `5164-5174`, `5460-5685`, `5800-6236`, `6805-6815`
  - `crates/dpdk-net-core/src/tcp_tlp.rs`: `1-190`
  - `crates/dpdk-net-core/src/tcp_rack.rs`: `1-230`, `126-146`
  - `crates/dpdk-net-core/src/tcp_retrans.rs`: `1-230`
  - `crates/dpdk-net-core/src/tcp_events.rs`: `1-280`
  - `crates/dpdk-net-core/src/rtt_histogram.rs`: `1-180`
  - `crates/dpdk-net-core/src/tcp_rtt.rs`: `1-140`
  - `crates/dpdk-net-core/src/tcp_input.rs`: `900-970`, `1015-1095`
  - `crates/dpdk-net-core/src/tcp_conn.rs`: `360-410`, `560-625`, `635-672`
  - `crates/dpdk-net-core/src/tcp_timer_wheel.rs`: `1-220`, `218-360`
  - `crates/dpdk-net-core/src/mempool.rs`: `260-286`
  - `crates/dpdk-net-core/src/tcp_reassembly.rs`: `293-314`
  - `crates/dpdk-net-core/tests/tcp_rack_rto_retrans_tap.rs`: `1-90`
  - `crates/dpdk-net-core/tests/tx_mempool_no_leak_under_retrans.rs`: `1-80`
  - `crates/dpdk-net/src/lib.rs`: `135-275`, `899-936`, `1568-1695`
  - `crates/dpdk-net/src/api.rs`: `1-46`
  - `include/dpdk_net.h`: `1-30`, `70-110`
  - `docs/superpowers/specs/2026-04-19-stage1-phase-a5-6-rtt-histogram-design.md`: `1-220`
  - `docs/superpowers/specs/2026-04-18-stage1-phase-a5-5-event-log-forensics-design.md`: `477-484`
  - `docs/superpowers/specs/2026-04-18-stage1-phase-a5-rack-rto-retransmit-iss-design.md`: `235-265`
- Prior-review files spot-checked for duplicate suppression:
  - `docs/superpowers/reviews/phase-a5-mtcp-compare.md`
  - `docs/superpowers/reviews/phase-a5-rfc-compliance.md`
  - `docs/superpowers/reviews/phase-a5-5-mtcp-compare.md`
  - `docs/superpowers/reviews/phase-a5-5-rfc-compliance.md`
  - `docs/superpowers/reviews/cross-phase-retro-part-1-claude.md`
  - `docs/superpowers/reviews/cross-phase-retro-part-1-codex.md`
  - `docs/superpowers/reviews/cross-phase-retro-part-1-synthesis.md`
  - `docs/superpowers/reviews/cross-phase-retro-part-2-claude.md`
  - `docs/superpowers/reviews/cross-phase-retro-part-2-codex.md`
  - `docs/superpowers/reviews/cross-phase-retro-part-2-synthesis.md`
  - I also read `docs/superpowers/reviews/cross-phase-retro-part-3-claude.md` to avoid stepping on the parallel architecture review where possible; the required skip list did not include it.
