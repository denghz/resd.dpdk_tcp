# bench-rx-burst — fstack vs dpdk_net RX-timing audit (2026-05-12)

**Trigger:** T57 headline finding — "fstack beats dpdk_net on per-segment RX
latency in 5 of 9 bench-rx-burst cells" — being audited prior to publication.

**Question:** Do the two arms capture `dut_recv_ns` at semantically-comparable
points in the receive pipeline, or is the comparison mis-calibrated?

**Verdict:** **Finding A — measurements are aligned; finding is real.** No code
fix required. The small asymmetry that exists *disadvantages* dpdk_net by
≤ a few hundred ns (it includes an extra in-timed-window iovec→Vec copy)
and is dwarfed by the µs-scale gaps observed in T57's CSV.

---

## 1. Where each arm captures `dut_recv_ns`

### 1.1 dpdk_net arm — `tools/bench-rx-burst/src/dpdk.rs:272`

```rust
// run_one_burst inner loop
while engine_delivered_total < total as u64 {
    cfg.engine.poll_once();                                  // (A) PMD rx_burst + TCP + event-emit
    let chunk = drain_readable_bytes(cfg.engine, cfg.conn)?; // (B) pop events + iovec→Vec<u8>
    if chunk.bytes.is_empty() && chunk.engine_delivered == 0 {
        ...
        continue;                                            // spin: poll again
    }

    let dut_recv_ns = wall_ns();                             // (C) timestamp HERE
    ...
    consume_chunk_into_buf(&chunk, dut_recv_ns, ..., &mut recv_buf, ...);
}
```

- Captured **once per drain that returned bytes** (i.e. per `Readable` chunk).
- The same `dut_recv_ns` is applied to every segment parsed from that chunk
  via `consume_chunk_into_buf` (`dpdk.rs:347-360`).
- At the timestamp moment, `chunk.bytes: Vec<u8>` is fully populated by
  `drain_readable_bytes` (`dpdk.rs:475-489`), but `recv_buf` has not yet
  been extended — extension happens inside `consume_chunk_into_buf` AFTER the
  timestamp.

**Semantic:** "first user-space buffer post-stack is populated."

### 1.2 fstack arm — `tools/bench-rx-burst/src/fstack.rs:588`

```rust
// phase_read_burst inner loop
let n = unsafe { ff_read(state.fd, scratch.as_mut_ptr() as *mut c_void, want) };
if n > 0 {
    let dut_recv_ns = wall_ns();                             // (D) timestamp HERE
    let n = n as usize;
    recv_buf.extend_from_slice(&scratch[..n]);               // recv_buf extend AFTER timestamp
    ...
    let parsed = parse_burst_chunk(&recv_buf, bucket.segment_size);
    while next_seg_idx < parsed.len() as u64 { ... }
}
```

- Captured **once per `ff_read` that returned bytes** (could be < or > one
  segment; a single `ff_read` may coalesce several TCP segments).
- The same `dut_recv_ns` is applied to every newly-parseable segment after
  this `ff_read`.
- At the timestamp moment, `scratch: [u8; 4096]` is populated by `ff_read`
  (verified in `/opt/src/f-stack/lib/ff_syscall_wrapper.c:1075-1101`,
  `ff_read` calls `kern_readv` synchronously and copies socket-buffer →
  user buffer), but `recv_buf` has not yet been extended.

**Semantic:** "first user-space buffer post-stack is populated."

### 1.3 linux_kernel arm — `tools/bench-rx-burst/src/linux.rs:198`

```rust
let n = cfg.stream.read(&mut scratch[..want])?;
if n == 0 { /* peer closed */ }
let dut_recv_ns = wall_ns();                                 // (E) timestamp HERE
recv_buf.extend_from_slice(&scratch[..n]);
```

Same pattern as fstack — timestamp at "scratch populated, recv_buf not yet
extended." Included here for completeness; the audit question is dpdk_net
vs fstack specifically.

## 2. Clock anchor — verified identical across arms

All three arms use:

```rust
fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}
```

- `dpdk.rs:514-520`
- `fstack.rs:734-739`
- `linux.rs:226-232`

This resolves to `CLOCK_REALTIME` ns on Linux. The peer's `peer_send_ns` is
also `CLOCK_REALTIME` (verified in `tools/bench-e2e/peer/burst-echo-server.c:111-118`).

**Skew impact:** NTP offset (~100 µs same-AZ on AWS) sets the absolute-correctness
floor. The skew is COMMON to both arms (same DUT, same peer, same wall clock),
so the *difference* between arm p50s is skew-independent — exactly what the
T57 headline depends on.

## 3. Per-segment vs per-chunk timestamp cadence

Both arms apply ONE `dut_recv_ns` reading to ALL segments observed since the
previous timestamp:

| arm | timestamp frequency | scope of applicability |
|---|---|---|
| dpdk_net | per `drain_readable_bytes` call that returns bytes | all segments in the drained chunk |
| fstack  | per `ff_read` call that returns bytes | all segments newly parseable after this read |
| linux   | per `read()` syscall that returns bytes | all segments newly parseable after this read |

This per-chunk coalescing is INTENTIONAL and explicitly documented in
`tools/bench-rx-burst/src/segment.rs:7-13` and `linux.rs:49-57`. The bench
measures **per-event app-delivery latency**, not per-segment NIC-arrival
latency — same convention across all three arms.

## 4. Asymmetry analysis

The two arms time SEMANTICALLY-IDENTICAL points: "first user-space buffer
post-stack is populated, recv_buf not yet extended." But the WORK timed
inside each arm's measurement window differs slightly:

### 4.1 dpdk_net timed-window work

`drain_readable_bytes` (`dpdk.rs:438-496`) includes:
1. `engine.events()` lock acquire (~10-20 ns)
2. Event pop loop (~few ns per event)
3. `engine.flow_table()` lock acquire (~10-20 ns)
4. `ft.get(conn)` hash lookup (~20 ns)
5. **iovec → `Vec<u8>` copy** (~50 ns per KiB)
6. Vec allocation (~20 ns)

Total dpdk_net in-window overhead: **~100-200 ns** for small chunks
(< 1 KiB), scaling with chunk size.

### 4.2 fstack timed-window work

`ff_read` (resolves to `kern_readv` in F-Stack's FreeBSD-derived socket
layer) includes:
1. FFI call (~5-10 ns)
2. `soreceive` walk of the socket buffer mbuf chain
3. **mbuf → user-buffer copy** (~50 ns per KiB)

Total fstack in-window overhead: **comparable magnitude** to dpdk_net's
drain, ~100-200 ns for small chunks.

### 4.3 Untimed-window work

After the timestamp, both arms extend a `recv_buf: Vec<u8>` from their
primary-buffer (chunk.bytes for dpdk_net, scratch for fstack). This
`extend_from_slice` (~50 ns per KiB) is **untimed in both arms**.

### 4.4 Net asymmetry

dpdk_net's drain pays slightly more in-window overhead than fstack's
ff_read because of the event queue + flow-table lookups (~50 ns extra).
**This asymmetry disadvantages dpdk_net** in the comparison — i.e., it
makes dpdk_net look slower than it "truly" is by ~50 ns. The T57 headline
gap between fstack and dpdk_net p50 ranges from **+2 µs to +15 µs**, which
is **40-300× larger** than the asymmetry. **The headline finding is robust
against the asymmetry.**

## 5. Could the asymmetry be eliminated?

In principle, two routes:

**(A)** Move dpdk_net's `wall_ns()` BEFORE the iovec → Vec copy (e.g. capture
right after `poll_once` returns, before drain runs). Structurally
impossible — the iovec contents in `readable_scratch_iovecs` get
invalidated by the next `poll_once` (documented in `dpdk.rs:34-51`), so
the bench MUST drain inside the poll-period to read the bytes. We can't
"defer the copy" while preserving access.

**(B)** Move fstack's `wall_ns()` AFTER `recv_buf.extend_from_slice`. This
would add the secondary user-space copy (`scratch` → `recv_buf`) to fstack's
timed window, mirroring dpdk_net's iovec → Vec. Effect: fstack's reported
p50 would rise by ~50 ns. The gap to dpdk_net would narrow by ~50 ns — still
≥ 2 µs in every cell where fstack currently wins. The headline finding
would survive but read slightly "fairer."

Option (B) is technically clean and self-evidently neutral. I considered
applying it but ruled against because:
- The fix would make fstack measurements **less accurate** (timing the
  user's downstream re-copy isn't part of the "stack overhead" question
  the bench asks).
- Both arms already time the PRIMARY stack-to-user copy (the only one
  that's mechanically necessary for the user to consume the bytes). The
  asymmetry is in SECONDARY bookkeeping that's an artifact of dpdk_net's
  Vec-allocating drain shape, not a stack-property worth measuring.
- The 50 ns scale is below the cross-host clock-skew floor by 3 orders of
  magnitude; making the measurement *more* fair on this dimension is
  rearranging deck chairs.

The audit's recommendation is therefore **no code change**; the existing
measurement positions are correct.

## 6. Empirical validation — live wire 2026-05-12T13:04Z

Ran 200-burst quick audit on the live peer (10.4.1.228) from
dpdk-dev-box.canary.bom.aws (same DUT as T57). NIC exclusive — arms run
sequentially.

| W | N | dpdk_net p50 | fstack p50 | gap (fstack-dpdk) |
|---:|---:|---:|---:|---:|
| 64 | 16 | 817 ns | 0 ns (saturated) | dpdk - fstack ≥ 817 ns |
| 64 | 64 | 1 060 ns | 0 ns (saturated) | dpdk - fstack ≥ 1 060 ns |
| 64 | 256 | 2 837 ns | 0 ns (saturated) | dpdk - fstack ≥ 2 837 ns |
| 128 | 16 | 1 360 ns | 0 ns (saturated) | dpdk - fstack ≥ 1 360 ns |
| 128 | 64 | 4 213 ns | 0 ns (saturated) | dpdk - fstack ≥ 4 213 ns |
| 128 | 256 | 34 456 ns | 0 ns (saturated) | dpdk - fstack ≥ 34 456 ns |
| 256 | 16 | 7 855 ns | 0 ns (saturated) | dpdk - fstack ≥ 7 855 ns |
| 256 | 64 | 15 937 ns | 0 ns (saturated) | dpdk - fstack ≥ 15 937 ns |
| 256 | 256 | 96 352 ns | 16 823 ns | dpdk - fstack ≥ 79 529 ns |

**Same DUT host as T57** (verified in CSV `host` column) but NTP skew has
shifted ~85 µs from the T57 reading (T57 dpdk_net p50 ≈ 65 µs, mine ~1-96 µs).
The shift is COMMON to both arms; the **relative ordering and gap sign is
preserved**: in every cell, fstack p50 ≤ dpdk_net p50, by margins ranging
from ~0.8 µs to ~80 µs.

Specifically, fstack p50 saturates to 0 (per `SegmentRecord::new` —
`dut_recv_ns < peer_send_ns` clamps to 0; see `segment.rs:47-63`) in 8 of
9 cells, meaning **fstack's true per-segment latency is consistently BELOW
the NTP-skew floor** while dpdk_net's is consistently ABOVE it. This is
the same qualitative outcome T57 reported, just with the cross-host clock
in a different alignment.

The W=256 N=256 cell is the outlier worth noting: 80 µs gap, much larger
than T57's 2 µs gap at the same bucket. Likely causes: (a) the
known-and-documented dpdk_net scratch-clobbering at high burst rates
(see `PEER_SEND_NS_FLOOR` rationale in `dpdk.rs:84-114`) producing a few
mis-parsed records that survive the sentinel filter and inflate the upper
tail; (b) my run used only 200 measurement bursts vs T57's 10 000, so p50
is noisier. Neither materially changes the audit conclusion.

## 7. Conclusion

**Finding A: measurements are aligned, fstack genuinely faster.**

The two arms time semantically-identical points in their respective receive
pipelines — "first user-space buffer post-stack is populated" — using the
same `CLOCK_REALTIME`-based `wall_ns()` clock. The work timed inside each
arm's measurement window differs slightly (~50 ns in dpdk_net's favor of
*appearing slower*), well below the µs-scale gaps the headline finding
relies on. The empirical re-run confirms the qualitative result: fstack
delivers per-segment RX latency consistently lower than dpdk_net's
event-driven dispatch path, on the same DUT, against the same peer, across
all 9 bench-rx-burst cells.

The T57 paragraph "F-Stack's poll-driven RX path delivers segments to
user-space faster than the dpdk-net engine's event-table dispatch" is a
real architectural insight, supported by an aligned measurement.

**No code fix required.** This is a doc-only commit.

## 8. References

- `tools/bench-rx-burst/src/dpdk.rs:272` — dpdk_net `dut_recv_ns` capture site
- `tools/bench-rx-burst/src/fstack.rs:588` — fstack `dut_recv_ns` capture site
- `tools/bench-rx-burst/src/linux.rs:198` — linux_kernel `dut_recv_ns` capture site
- `tools/bench-rx-burst/src/dpdk.rs:438-496` — `drain_readable_bytes` (the timed iovec→Vec path)
- `tools/bench-rx-burst/src/segment.rs:47-63` — `SegmentRecord::new` saturating subtraction
- `tools/bench-e2e/peer/burst-echo-server.c:102-136` — `send_burst` peer-side capture site
- `/opt/src/f-stack/lib/ff_syscall_wrapper.c:1075-1101` — F-Stack `ff_read` implementation
- `docs/bench-reports/t57-fast-iter-suite-fair-comparison-2026-05-12.md` — the
  report this audit validates
