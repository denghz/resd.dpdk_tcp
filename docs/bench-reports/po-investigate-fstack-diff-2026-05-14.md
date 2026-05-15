# PO investigate — fstack vs dpdk-net deep diff (RX burst + TX latency)

Date: 2026-05-14
Scope: read-only source comparison, F-Stack `a8b3a9ad…` vs our `a10-perf-23.11` head `e1302c2`.
Primary metric: **rx burst latency + tx (per-write / per-segment) latency**. Throughput is explicitly
not a target — wherever a known fstack technique boosts throughput at the cost of latency, it is
recommended AGAINST.

---

## 1. Executive summary

- **Why we already win on RTT / burst-init (today)**: our `send_bytes` returns BEFORE
  `rte_eth_tx_burst` runs (mbufs are pushed to an internal `tx_pending_data` ring; the burst-syscall
  itself fires at end-of-poll), so `t_first_wire` in bench-tx-burst captures the build-and-enqueue
  cost only. fstack's `ff_write` walks BSD socket buffers, calls `tcp_output` → `send_single_packet`
  → `send_burst`, the last of which only fires `rte_eth_tx_burst` once 32 packets have queued OR a
  100 µs drain timer ticks (`BURST_TX_DRAIN_US` in `lib/ff_config.h:56`). The 100 µs drain is the
  root cause of the bimodal fstack RTT (already documented in
  `docs/bench-reports/fstack-bimodality-investigation-2026-05-13.md`); we work around it by setting
  `pkt_tx_delay=0` in the auto-generated fstack conf.
- **Where fstack does something better that we should adopt** (top 3):
  1. RX-burst **mbuf data-cache prefetch with `PREFETCH_OFFSET=3`** (`lib/ff_memory.h:54`,
     `lib/ff_dpdk_if.c:2392-2408`). We do zero `rte_prefetch0` on the burst dispatch loop
     (`crates/dpdk-net-core/src/engine.rs:3118-3219`).  Expected win: 50-150 ns per RX segment.
  2. **Header-prediction TCP input fast-path** (`freebsd/netinet/tcp_input.c:1773-1941`). fstack's
     pure-ACK and in-order-data branches skip option/SACK/state-machine work entirely and call
     `tcp_output` inline (`:1869`, `:1938`). Our `handle_established`
     (`crates/dpdk-net-core/src/tcp_input.rs:727-1004`) always re-parses options, runs PAWS, runs
     SACK decode, etc. Expected win: 40-120 ns per ACK-bearing segment, biggest win on bench-rtt.
  3. **mbuf bulk alloc** — fstack uses `m_gethdr` (single-mbuf alloc) for control frames; we still
     pay 1 FFI hop per `tx_frame`/`tx_tcp_frame`/`tx_data_frame` control-frame call
     (`engine.rs:2662`, `:2743`, `:2827`). We have bulk-alloc in `send_bytes` (PO9, `:6437`), but
     not on the ACK emit path (`emit_ack` calls `tx_tcp_frame` which calls
     `shim_rte_pktmbuf_alloc`). Expected win: 30-50 ns per ACK.
- **Where fstack is worse and we should NOT copy it**:
  - 100 µs TX drain timer (causes bimodal latency).
  - Per-packet `INP_HASH_LOCK` taken inside `in_pcblookup_*`.
  - Two-stage "header mbuf + chained data mbuf" pattern with `m_copydata` from sockbuf (vs our
    single-mbuf direct write via PO10).
- **Top 3 latency wins to propose**: PO-Idea-1 (RX prefetch), PO-Idea-2 (header-prediction
  fast-path), PO-Idea-3 (ACK alloc batching).

---

## 2. Already-done PO list (do not re-propose)

(from `git log --oneline -40`; PO1..PO10 are wholly engine-side perf work)

- **PO1** `446547d` — `internet_checksum` 4-byte/iter fold.
- **PO2** `4a00d13` — `parse_options` Linux-canonical TS-only fast path.
- **PO3** `7b3328e` — `FlowTable::insert` O(1) via free-slot LIFO stack.
- **PO4** `5a3bfba` — `build_segment_offload` writes pseudo-only TCP cksum, skips full payload fold.
- **PO5** `6ff3754` — `parse_options` Linux-canonical TS+SACK fast path (24/32/40-byte buffers).
- **PO6** `80b66c9` — extend offload skip to `build_retrans_header` + ACK emitter.
- **PO7** `0436aef` — hoist `clock::now_ns()` out of `send_bytes` per-segment loop.
- **PO8** `bca569f` — batch per-segment counter atomics in `send_bytes`.
- **PO9** `b4aec6a` — bulk mbuf alloc in `send_bytes` via `rte_pktmbuf_alloc_bulk`.
- **PO10** `411b70e` + `09525f0` — eliminate `tx_frame_scratch`; `build_segment` writes directly
  into the mbuf data area; runtime overflow + size-drift guards.

Prior perf wins (older T-series): T7.7 H1 (hoist conn construct), T9 H5 (gate `reorder.drain` on
`is_empty`), T9 H7 (TS-only fast-path).

---

## 3. Comparison sub-system by sub-system

For each area: cite fstack file:line and our file:line, describe divergence, mark verdict.

### 3.1 RX burst poll loop

- **fstack** (`lib/ff_dpdk_if.c:2385-2409`): `rte_eth_rx_burst(MAX_PKT_BURST=32)`; first 3 mbufs
  prefetched via `rte_prefetch0(rte_pktmbuf_mtod(...))`; then loop "prefetch i+3, process i" until
  the tail (`PREFETCH_OFFSET=3` from `lib/ff_memory.h:54`). Per-mbuf calls
  `process_packets(... count=1 ...)` which does `rte_pktmbuf_mtod` again to get the data pointer.
- **ours** (`crates/dpdk-net-core/src/engine.rs:3118-3190`): `rte_eth_rx_burst(BURST=32)`; **zero
  prefetch**; `for &m in &mbufs[..n]` calls `dispatch_one_rx_mbuf` (`:3185`), which calls
  `dispatch_one_real_mbuf` (`:4488`), which dereferences `rte_pktmbuf_mtod` via the
  `mbuf_data_slice` helper. The first cache-miss on each mbuf's data area is paid inline in the
  decode path.
- **Verdict — fstack is better.** A `rte_prefetch0` issued 3 mbufs ahead of decode hides ~70-100
  cycles of L3-miss latency per mbuf when the NIC has just DMA'd the burst into memory cold of L1.
  At 32 in-burst this is a 4-iter pipeline win. RTT (1-pkt) bursts see no win; bench-rx-burst
  (N=16/32 segments) sees a measurable improvement.
- One subtlety: ENA's "LRO-like" coalescing on receive does pre-touch the mbuf, but only the
  metadata, not the user data. Prefetch of the actual payload addr is still warranted.

### 3.2 TX hot path — segment build

- **fstack** (`freebsd/netinet/tcp_output.c:1050-1098`):
  - `m_gethdr(M_NOWAIT, MT_DATA)` per output segment (single mbuf for the header).
  - `m_copydata(mb, moff, len, mtod(m, caddr_t) + hdrlen)` copies bytes from the socket buffer
    into the head mbuf when payload fits inline (`MHLEN - hdrlen - max_linkhdr`).
  - Otherwise `tcp_m_copym` returns a **chained** payload mbuf; the head mbuf carries headers only.
  - Header fields (IP/TCP) written into the mbuf via `tcpip_fillheaders` then per-field assign.
  - Checksum partially offloaded to NIC via `csum_flags`.
- **ours** (`crates/dpdk-net-core/src/engine.rs:6437-6788`, `tcp_output.rs:217-321`):
  - PO9 bulk-alloc of mbufs up front (`shim_rte_pktmbuf_alloc_bulk` for `total_segments` mbufs).
  - PO10 in-place header+payload write into the mbuf's data area via `build_segment` /
    `build_segment_offload`. **Single mbuf per segment** (no chain) for normal sends.
  - Cksum: PO4 `build_segment_offload` writes pseudo-only and lets the NIC fold the rest.
- **Verdict — we are better.** Bulk alloc + single-mbuf + in-place write is unambiguously lower
  latency. fstack's two-stage chain costs extra mempool gets and the `m_copydata` walk through
  the BSD sockbuf chain. Don't regress.
- **Caveat**: fstack's "chain header to data" pattern is precisely how zero-copy works (the data
  mbuf is the same one queued in the sockbuf). We don't do that for `send_bytes` — we copy from
  the caller `&[u8]` directly into the mbuf data area. That's a copy hit but a small one (memcpy
  is ~5 GB/s on Zen) and it avoids any cross-mbuf bookkeeping; on retransmit
  (`tcp_output.rs:125-202` `build_retrans_header`) we already use the chain pattern (header
  mbuf + retained data mbuf), so the retrans path doesn't double-copy.

### 3.3 TX burst submission (rte_eth_tx_burst batching)

- **fstack** (`lib/ff_dpdk_if.c:2037-2099`):
  - `send_single_packet` queues onto `qconf->tx_mbufs[port].m_table[MAX_PKT_BURST]`.
  - `send_burst` (= one `rte_eth_tx_burst`) fires when queue is full (`len == MAX_PKT_BURST=32`)
    OR the main-loop drain timer expires every `pkt_tx_delay` ≤ 100 µs (`:2295-2367`).
  - **Latency tax**: a single ACK in response to a small inbound segment waits up to 100 µs to
    leave the wire. RTT round-trip bimodality is the symptom (see
    `docs/bench-reports/fstack-bimodality-investigation-2026-05-13.md`); the workaround we use is
    `pkt_tx_delay=0` so fstack drains on every main-loop iter, which then runs the drain at
    NIC-poll rate.
- **ours** (`engine.rs:3266-3382`): `drain_tx_pending_data` runs at the end of EVERY
  `poll_once` iteration. There is no time-based drain timer — we drain as fast as the caller
  polls. `tx_pending_data` ring capacity = `tx_ring_size = 512` (`:1070`), well over fstack's
  MAX_PKT_BURST. The send_bytes hot path pushes into the ring (`:6664-6688`) and lets the next
  poll do the burst. On `pushed_ok == false` (ring full), we drain inline before retrying
  (`:6685`), so no segment is ever stranded.
- **Verdict — we are better, by a wide margin, on latency.** Specifically:
  - In bench-tx-burst, `t_first_wire` captures TSC right after `send_bytes` RETURNS, but BEFORE
    the actual `rte_eth_tx_burst` runs (the drain happens inside the caller's next `poll_once`).
    This puts the burst-build cost on `t_first_wire − t0` and the burst-submission cost on
    `t1 − t_first_wire`, which is exactly the right thing to optimize for "time to ring".
  - In bench-rtt's single-packet case, our drain fires on the NEXT `poll_once` after the ACK
    response arrives — so the ACK leaves the wire ~1 poll iter (~250 ns) after the inbound. No
    100 µs penalty.
- **Red flag**: Do NOT add a fstack-style time-based drain. Even with `pkt_tx_delay=1µs` it would
  add a TSC read per main-loop iter; with `pkt_tx_delay=0` (the workaround we use on fstack) the
  drain runs on every main-loop iter — which is what we already do.

### 3.4 mbuf lifecycle (alloc, refill, free)

- **fstack** (`lib/ff_dpdk_if.c:344-396`): mempool size = `nb_ports*(max_portid+1)*2*nb_lcores*32
  + nb_lcores*MEMPOOL_CACHE_SIZE` + RX-queue-cushion. Cache size = `MEMPOOL_CACHE_SIZE=256`
  (`lib/ff_memory.h:34`). Per-segment alloc uses `rte_pktmbuf_alloc(mp)` single-mbuf
  (`:2118`, `:2242`). No bulk alloc on the BSD-output path.
- **ours** (`crates/dpdk-net-core/src/mempool.rs:80-108`, `engine.rs:1717-1776`):
  - RX mempool: cache 256, sized by formula (per-conn × max_conns + 4×rx_ring floor).
  - **TX hdr mempool: cache 64** (`engine.rs:1728`). Used by `tx_frame` / `tx_tcp_frame` —
    THESE are the ACK / SYN / RST / FIN paths.
  - **TX data mempool: cache 128** (`engine.rs:1772`). Used by `send_bytes` for data segments.
- **Verdict — ambiguous → potentially better with a tweak.** A small per-lcore mempool cache
  shrinks the average alloc fast-path: when the cache hits, alloc is a load from L1 with no
  spinlock; when it misses, the cache-refill (`get_bulk` of `cache_size` mbufs from the
  underlying pool) costs ~600-900 ns. Our 64-entry tx_hdr cache will miss every 64 ACKs, so an
  ACK-storm pays ~10 ns avg/ACK in refill overhead. fstack's 256 is "more headroom"; either
  bumping our tx_hdr cache to 256 (cheap memory-wise) or moving to bulk-alloc-on-emit for ACKs
  (proposed in PO-Idea-3) reduces the refill rate without changing tail behaviour.

### 3.5 ACK generation (delayed vs immediate)

- **fstack** (`freebsd/netinet/tcp_input.c:517-529`, `:1934-1937`, `:3120-3128`,
  `tcp_timer.h:122`):
  - `DELAY_ACK(tp, tlen)` returns true when no DELACK timer is currently ticking, no zero-window
    was sent, and `tlen <= t_maxseg`. When true, sets `TF_DELACK` and `tcp_timer_activate(...
    TT_DELACK, tcp_delacktime)` with `tcp_delacktime = TCPTV_DELACK = hz/25` → **40 ms**.
  - On every other segment within the window, the ACK piggybacks on outbound data.
- **ours** (`engine.rs:5592-5697`): `emit_ack` fires immediately on every accepted RX data
  segment from `dispatch` (`:5314-5325`). The config has `EngineConfig::tcp_delayed_ack: bool`
  (`:998`) but it's defaulted to `false` (`:1095`) and we don't honor it on the emit path.
- **Verdict — we are better for latency-sensitive workloads.** Delayed ACK is a throughput
  optimization (fewer header bytes on the wire) at a 40 ms tail-latency cost. For trading
  request-response RTT it is unambiguously bad. The trading-latency-defaults user note
  ("prefer latency-favoring defaults over RFC recommendations") rules in our favor.
- Note: we do piggyback inside `send_bytes` (the data segment carries `TCP_ACK` always —
  `engine.rs:6497`), so when there IS data to send the ACK comes free anyway.

### 3.6 Socket / send-buffer copy-out

- **fstack** (`tcp_output.c:1066-1097`):
  - `sbsndptr_noadv(&so->so_snd, off, &moff)` finds the right starting mbuf inside the BSD
    socket buffer mbuf chain.
  - For payloads ≤ `MHLEN - hdrlen - max_linkhdr` (≈ 100 bytes), `m_copydata` copies into the
    head mbuf inline.
  - Otherwise `tcp_m_copym` returns a chained payload mbuf (m_dup_pkthdr-ish copy of the sockbuf
    mbuf with appropriate refcount), and the head mbuf carries headers only.
- **bench-tx-burst/src/fstack.rs:752-793** (the `pump_one_burst` arm): in our bench harness we
  call `ff_write(fd, payload, remaining.len())` directly — fstack's user→kernel boundary then
  invokes `sbappendstream` and friends inside the BSD network stack to land bytes in `so_snd`.
  This adds the cost of: (a) sockbuf mbuf alloc, (b) the user→kernel copyin from `payload` into
  the sockbuf mbuf, (c) `tcp_output` build pulling FROM the sockbuf mbuf back into the head mbuf
  via `m_copydata`. That's TWO copies for ≤ MHLEN payloads, one copy + one chain for larger.
- **ours** (`engine.rs:6254-6849`, `bench-tx-burst/src/dpdk.rs:153-518`):
  - The bench calls `engine.send_bytes(conn, &payload[sent..])` directly — `payload` is the
    application's buffer.
  - `send_bytes` does **one** copy: `copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8,
    bytes.len())` where `dst` is the mbuf's data area (PO10, see comment at `:6582-6588`).
- **Verdict — we are clearly better.** Single-copy direct-to-mbuf vs fstack's two-stage
  sockbuf→head-mbuf path. The per-byte gap is roughly MSS × memcpy bandwidth ≈ 1.5 KB at 5 GB/s
  ≈ 300 ns saved per MSS segment vs fstack.

### 3.7 Connection lookup on RX

- **fstack** (`freebsd/netinet/in_pcb.c:155`, `tcp_input.c:875,884,891`): `in_pcblookup_mbuf` →
  `in_pcblookup_hash_locked` reads an mbuf-stashed PCB hash hint (if present) or walks the BSD
  PCB hash chain. Holds `INP_HASH_RLOCK` for the duration. Per-segment cost: hash compute + chain
  walk + RWlock acquire+release. The lock is contended in multi-thread BSD, single-thread
  irrelevant.
- **ours** (`crates/dpdk-net-core/src/flow_table.rs:172-195`,
  `engine.rs:4818-4828`): `hash_bucket_for_lookup` uses either the NIC-provided RSS hash (when
  `hw-offload-rss-hash` active) or `siphash_4tuple` (std `RandomState` SipHash-1-3, lazy
  static seed). Lookup is a Rust `HashMap<FourTuple, u32>::get` — load factor + double-probe
  + key compare.
- **Verdict — ambiguous.** Both are O(1) on average; std HashMap on Zen runs SipHash-1-3 at
  ~3.5 cyc/byte = ~85 cyc for the 12-byte tuple = ~30 ns; fstack's PCB hash + RWlock is similar
  on single-threaded fast path. The Rust HashMap also has a load-factor-driven probe sequence
  which can spike the tail; under typical bench loads (1-2 conns) it's a direct hit.
- **Potential win**: a fixed-bucket open-addressed table keyed off RSS hash low bits would avoid
  the SipHash compute and the HashMap probe sequence, plus saving the RefCell `.borrow()`
  enforcement. See PO-Idea-7.

### 3.8 TCP options parsing (TS / SACK)

- **fstack** (`freebsd/netinet/tcp_input.c:680-697` + `tcp_subr.c:tcp_dooptions`): walks
  `[opt_type, opt_len, ...]` TLVs in a switch loop, decodes MSS / WS / SACK / TS / TFO / signature.
  No length-prefix fast-path.
- **ours** (`crates/dpdk-net-core/src/tcp_options.rs:292-440`): **three fast-paths** matched on
  total options length:
  - `len==12 && opts[0..4]==[NOP,NOP,8,10]` (PO2 canonical TS-only ACK).
  - `len==12 && opts[0..2]==[8,10] && opts[10..12]==[NOP,NOP]` (T9 H7 TS-first variant).
  - `len in {24,32,40} && NOP,NOP,TS,10, ...,NOP,NOP,SACK,...` (PO5 TS+SACK ACK shapes).
  - Generic TLV-walk fallback otherwise.
- **Verdict — we are better.** Tight straight-line decode beats the TLV switch on the every-ACK
  hot path. Linux peer + dpdk_net peer both emit the canonical TS shape, so the fast-paths
  cover ≥ 99 % of ACK-bearing segments.

### 3.9 Software vs HW timestamping (TX/RX)

- **fstack** (BSD-shaped API): does not expose `rte_mbuf::tx_timestamp` to user code. RX-side
  `tcp_input` reads `m->m_pkthdr.rcv_tstmp` if the NIC stamped it (`tcp_input.c` various sites
  guarded by `M_TSTMP`).
- **ours** (`engine.rs:2397-2433`, `:4521`, `dispatch_one_real_mbuf` `:4488-4531`): `hw_rx_ts_ns`
  is read once per RX mbuf at the dispatch boundary; returns 0 on ENA (no TX-TS dynfield, no
  RX-TS dynfield either per spec §10.5). `bench-tx-burst/src/dpdk.rs:80-93` documents the
  `TxTsMode::TscFallback` mode used on ENA — `t_first_wire` is TSC at `send_bytes` return.
- **Verdict — ambiguous, hardware-limited.** On c6a/ENA neither side has HW TX-TS; both fall
  back to TSC. On mlx5/ice both could read the dynfield. Not a latency lever today.

### 3.10 Cache prefetching

- **fstack** (`lib/ff_dpdk_if.c:2392-2408`): explicit `rte_prefetch0` on mbuf payload addr, with
  `PREFETCH_OFFSET=3`.
- **ours**: zero `rte_prefetch0` calls anywhere in production (`/wc_verify.rs` and
  `/counters.rs` references are about prefetchable BAR, unrelated). The dispatch loop
  `for &m in &mbufs[..n] { ... dispatch_one_rx_mbuf(m); ... }` (`engine.rs:3171-3190`) has no
  prefetch.
- **Verdict — fstack is better.** This is the largest single source-level gap. See PO-Idea-1.

### 3.11 Branch / inlining

- **fstack**: most hot helpers are `static inline`. The `process_packets` body is `static inline`
  (`:1641`). The TCP-input header-prediction branch (`tcp_input.c:1779-1942`) is one big function
  body that LLVM can keep hot.
- **ours**: `dispatch_one_rx_mbuf` has `#[inline]` (`engine.rs:4487`). `parse_options` has
  `#[inline]`. `build_segment` / `build_segment_offload` are not `#[inline]` but are pub fns
  inlined by LLVM's normal heuristics. `handle_established` is not `#[inline]` (it's 277 lines).
- **Verdict — ambiguous; minor.** Force-inlining mega-functions can flip cache behaviour either
  way. Not a top-priority lever.

---

## 4. Prioritized optimization proposals

Ranking: latency impact first, risk second.

### PO-Idea-1 — RX-burst `rte_prefetch0` with offset = 3

- **What/where**:
  Add a prefetch pass to `Engine::poll_once`'s RX burst dispatch at `engine.rs:3171` BEFORE the
  per-mbuf decode loop. Mirror fstack's pattern:
  - Prefetch `rte_pktmbuf_mtod(mbufs[0..min(n,3)])` first.
  - In the body of the dispatch loop, prefetch `mbufs[i+3]` (if valid) right before decoding
    `mbufs[i]`.
  - Use a stable DPDK prefetch primitive — `rte_prefetch0` is available as a `shim_*` wrap or
    via inline `core::arch::x86_64::_mm_prefetch(ptr as *const i8, _MM_HINT_T0)`.
- **Expected effect**: 50-150 ns saved per RX mbuf decode on AWS C6a (Zen 4, large LLC, but the
  NIC's just-DMA'd data is cold in L1/L2). At a 32-burst this is 1.6-4.8 µs/burst saved on the
  RX path. Direct impact on bench-rx-burst p50/p99, indirect on bench-rtt (one-segment bursts
  don't benefit — the offset-3 prefetch only kicks in at index ≥ 3).
- **Risk**: very low. Prefetch is a hint; mispredicting the addr (e.g. NULL slot in `mbufs[i]`)
  is a no-op. Stable across DPDK versions.
- **Latency vs throughput**: latency win; throughput unchanged.
- **Confidence**: high. This is the single most-cited DPDK perf lever and we are leaving it
  on the table.

### PO-Idea-2 — Header-prediction fast-path in `handle_established`

- **What/where**:
  Add a top-of-function fast-path branch in `tcp_input.rs:727`'s `handle_established` (mirror
  fstack `tcp_input.c:1773-1941`). The shape:
  ```
  if seg.flags == TCP_ACK
     && parsed_opts == TS-only (we have this via parse_options fast-paths)
     && (no SACK reorder, no ws update, no rcv_zero_window, no PAWS-flag)
     && seg.payload.empty()                      // pure ACK
     && seg.seq == conn.rcv_nxt
     && seg.ack ∈ (conn.snd_una, conn.snd_nxt]   // new-data cum-ACK
  {
     // Fast path: snd_una update, RTT sample via TS, ACK-pruning of snd_retrans,
     // RTO timer reschedule. No reorder, no SACK board, no urgent.
     return Outcome { ... };
  }
  ```
  Companion: a similar fast-path for in-order data-bearing ACK (`seg.payload.len() > 0`,
  `seg.seq == conn.rcv_nxt`, payload fits in recv window).
- **Expected effect**: 40-120 ns per ACK on bench-rtt's request-response path; pure-ACKs in
  steady state see the biggest win since they bypass ~30 lines of branchy work
  (`tcp_input.rs:798-1004`).
- **Risk**: medium. Splitting the input handler into a fast/slow path duplicates state-update
  ordering — easy to drop a counter bump or a `send_refused_pending` clear. Requires careful
  audit. Mitigate by deriving the fast-path body from a generated trace (LLM-assisted) and
  asserting bit-equivalence in counter snapshot vs slow path via the
  `tests/counter-coverage.rs` harness.
- **Latency vs throughput**: latency win; throughput unchanged (work is the same, just one
  branch instead of N).
- **Confidence**: medium — high impact but high engineering surface. Recommended only if PO-Idea-1
  + PO-Idea-3 don't close the gap to the desired latency floor.

### PO-Idea-3 — Bulk-alloc the ACK emit path

- **What/where**:
  `Engine::emit_ack` at `engine.rs:5592` currently calls `tx_tcp_frame` (`:2732`) which calls
  `shim_rte_pktmbuf_alloc(self.tx_hdr_mempool.as_ptr())` per emit. Replace with:
  - A per-Engine 16-slot small ringbuffer of pre-allocated hdr mbufs, refilled in bulk via
    `shim_rte_pktmbuf_alloc_bulk(tx_hdr_mempool, mbufs, 16)` when the ring drops below the
    refill watermark.
  - `emit_ack` pops one mbuf from the ring; on miss, falls back to the existing single-alloc.
  - Refill runs at end-of-poll alongside `drain_tx_pending_data`.
- **Expected effect**: 30-50 ns saved per ACK (one FFI hop + mempool cache lookup → one array
  index). Bench-rtt's ack-per-request workload sees this every iteration.
- **Risk**: low. Refcount semantics are unchanged: pre-allocated mbufs are passed to
  `rte_eth_tx_burst` with refcount 1 same as today; the driver consumes them on completion.
  Edge case: engine shutdown must drain the pre-alloc ring back to the mempool — same pattern
  as `tx_pending_data` drain at `Engine::drop`.
- **Latency vs throughput**: latency win; no throughput change.
- **Confidence**: high.

### PO-Idea-4 — Hoist the RefCell borrow in the RX dispatch hot path

- **What/where**:
  `Engine::tcp_input` (`engine.rs:4741-4828`) takes `flow_table.borrow()` (`:4824-4828`) for the
  initial lookup, then the dispatch path takes a SECOND `flow_table.borrow_mut()` to apply the
  outcome (`:5510`, `:5292`, `:5510`, `:5556`, and dozens more — `grep -c flow_table.borrow
  engine.rs = 94`). Each `RefCell::borrow()` does an atomic-like inc/dec on a borrow counter.
  - Refactor: take the `borrow_mut()` once at the top of `tcp_input`, thread an `&mut FlowTable`
    + `handle` through `dispatch`, `handle_established`, `emit_ack`, and post-dispatch outcome
    application. Drop the borrow precisely once at the bottom.
- **Expected effect**: 10-30 ns per RX segment (Zen's RefCell borrow is ~3-5 cycles each, ~25
  borrow ops per packet = ~75 cycles = ~25 ns at 3 GHz).
- **Risk**: medium-high. The current code repeatedly drops + re-borrows specifically to avoid
  nested mut+timer_wheel borrows that would `panic!` at runtime. Refactor needs to split
  flow_table mutations from timer_wheel mutations into two passes. The existing 4-phase ordering
  notes (e.g. `:5217-5224`) document the constraint. Aliased borrows across `transition_conn`,
  `emit_ack`, `arm_tlp_pto` are the gotchas.
- **Latency vs throughput**: latency win; throughput unchanged.
- **Confidence**: medium — high impact, real engineering cost. Suggest pilot on `handle_established`
  + `emit_ack` boundary only.

### PO-Idea-5 — Bump `tx_hdr_mempool` cache_size 64 → 256 + `tx_data_mempool` 128 → 256

- **What/where**: `engine.rs:1728` and `:1772`. Match fstack's `MEMPOOL_CACHE_SIZE=256`.
- **Expected effect**: 5-20 ns/seg of reduced cache-refill rate on a saturated burst. Below
  Idea-3's impact (bulk-alloc is structurally better), but composes with it.
- **Risk**: minimal. The cache is per-lcore; bigger cache uses more memory (256 mbufs × ~2 KB =
  ~500 KB per pool, ~1 MB extra total) but stays within L2 footprint on Zen.
- **Latency vs throughput**: minor latency win; minor throughput win.
- **Confidence**: high.

### PO-Idea-6 — `#[inline]` on `build_segment`, `build_segment_offload`, `tx_offload_finalize`

- **What/where**:
  - `tcp_output.rs:43` (`build_segment`)
  - `:77` (`build_segment_offload`)
  - `:452` (`tx_offload_finalize`)
  - Also `emit_ack` itself at `engine.rs:5592` (large function but only called from one site;
    `#[inline]` is safe).
- **Expected effect**: 5-15 ns/segment by eliminating the call-frame setup and letting LLVM
  inline the literal-known header offset arithmetic.
- **Risk**: code-size growth in the engine module by ~5-10 KB; could push some cold paths out
  of icache. Mitigate with `#[inline]` over `#[inline(always)]` so LLVM still uses its size
  heuristic on cold callsites.
- **Latency vs throughput**: minor latency win; throughput unchanged.
- **Confidence**: medium — LLVM may already inline these; PGO-style profiling would clarify.

### PO-Idea-7 — Replace `HashMap<FourTuple, u32>` with a fixed-bucket open-addressed table

- **What/where**: `crates/dpdk-net-core/src/flow_table.rs:102-217`. Today: `HashMap<FourTuple,
  u32>`. Proposed: a fixed-power-of-two bucket array indexed by (low N bits of) the NIC's RSS
  hash (or `siphash_4tuple` fallback), with linear probing inside the bucket.
- **Expected effect**: 15-40 ns per RX segment when RSS hash offload is active — direct array
  index, no SipHash compute, no Rust HashMap probe ceremony. With RSS-active path (default in
  production), this also eliminates the `siphash_4tuple` ~30 ns cost.
- **Risk**: medium. Resize behaviour, deletion (tombstones), and the `flow_table()`
  RefMut-escape-hatch API change. The existing tests rely on `lookup_by_tuple` and
  `iter_handles`. Doable but a non-trivial diff.
- **Latency vs throughput**: latency win; small throughput win too.
- **Confidence**: medium. The TODO at `flow_table.rs:191` already acknowledges this is a
  Stage-2 work item.

### PO-Idea-8 — Eliminate `SmallVec` indirection in `dispatch_one_rx_mbuf`

- **What/where**: `engine.rs:4452-4478`. Even on the fault-injector-off feature config
  (production), the function builds a `smallvec![mbuf]` of size 1 and iterates it. LLVM SHOULD
  elide this, but at -O3 with current Rust release the SmallVec construction still touches the
  3-pointer SmallVec header.
  - Replace with a direct call: under `#[cfg(not(feature = "fault-injector"))]`,
    `dispatch_one_real_mbuf(mbuf)` and discard the SmallVec entirely.
- **Expected effect**: 1-5 ns per RX mbuf.
- **Risk**: minimal — the feature-on path stays unchanged.
- **Latency vs throughput**: tiny latency win; nothing else.
- **Confidence**: high (correctness is trivial), medium on impact (LLVM may already collapse it).

### PO-Idea-9 — Per-conn cached `SegmentTx` template

- **What/where**: `engine.rs:6488-6501` (in `send_bytes`) and `:5635-5648` (in `emit_ack`).
  Each segment build re-assembles 11 fields (`src_mac`, `dst_mac`, src/dst ip/port, etc.) into
  a fresh `SegmentTx` struct on the stack. The MAC pair, IPs, and ports don't change for the
  life of the connection — only seq/ack/flags/window/options/payload do.
  - Stash a pre-built `[u8; 34]` template (`ETH_HDR_LEN + IPV4_HDR_MIN`) in `TcpConn` at
    handshake time. Patch in the per-segment fields directly into the mbuf data area, skipping
    `SegmentTx` aggregation entirely.
- **Expected effect**: 10-30 ns per segment on the build path.
- **Risk**: medium. Departs from the clean "pass a SegmentTx, build_segment writes" boundary.
  Re-orders the tx_offload_finalize / cksum write expectations. Easy to get the seq/ack
  endianness wrong on the patch site.
- **Latency vs throughput**: latency win; throughput unchanged.
- **Confidence**: medium.

### PO-Idea-10 — Coalesce `now_ns()` reads in `poll_once`

- **What/where**: `engine.rs:3039-3220` (`poll_once`). The function reads TSC at the top
  (`rdtsc` for mempool-sample-tsc), then `advance_timer_wheel` reads `crate::clock::now_ns()`
  again (`:3427`), then per-conn `emit_ack` reads `now_us` again (`:5623`), then
  `check_and_emit_rx_enomem` reads now_ns again (`:3257`). Each `clock::now_ns()` is rdtsc +
  scale-multiply — ~30 cycles each.
  - Compute `now_ns` once at the top, thread it through.
- **Expected effect**: 5-30 ns per poll iteration (depending on how many TSC reads we collapse).
- **Risk**: low. `now_ns` drift across the body of one poll iteration is sub-µs and not
  observable on any RTO/TLP/wheel math.
- **Latency vs throughput**: latency win; throughput unchanged.
- **Confidence**: medium.

### PO-Idea-11 — Compile-time-known mbuf data offset (skip `rte_pktmbuf_mtod` shim)

- **What/where**: every RX-decode site uses `crate::mbuf_data_slice` which calls
  `shim_rte_pktmbuf_data` (FFI). On DPDK 23.11 with stable mbuf layout, the data offset is
  `(buf_addr + data_off)` — both fields are in the mbuf header at known offsets. Inline a
  `#[inline] unsafe fn` that reads those fields directly, skipping the C shim FFI hop.
- **Expected effect**: 2-5 ns per RX mbuf.
- **Risk**: low if stable on DPDK 23.11 / fast follow on a future bump. Already documented in
  `dpdk_consts.rs`.
- **Latency vs throughput**: tiny latency win; nothing else.
- **Confidence**: medium.

### PO-Idea-12 — RX-burst inline-batch ACK emit

- **What/where**: under bench-rtt's ping-pong shape, a single RX burst can contain N TCP
  segments from the peer; today we emit one ACK per segment serially (`emit_ack` per match arm).
  - Add a per-poll-iter "merged ACK" mode: defer ACK emission until end-of-burst, then emit ONE
    cumulative ACK reflecting the latest `rcv_nxt`. Today's `last_advertised_wnd` /
    `last_sack_trigger` bookkeeping (`engine.rs:5683-5689`) already exists; extend it to coalesce
    pure-ACK emits within one poll.
- **Expected effect**: only helps when ≥ 2 in-order data segments arrive per RX burst (N=2
  bench-rx-burst case onwards). bench-rtt (always N=1) sees nothing. bench-rx-burst at N=16:
  ~15 ACK emits collapsed into 1 → ~600 ns saved per burst.
- **Risk**: medium-low. Coalesced ACK is RFC 9293 compliant (cumulative ACK semantics permit it
  trivially); the corner case is the "ACK on every other segment" SHOULD from §3.8.6, which is a
  bandwidth-not-latency hint and we're already deviating from with delayed-ACK off. Throughput
  side: fewer ACKs means slightly less wire traffic, no penalty.
- **Latency vs throughput**: latency win for multi-segment bursts; tiny throughput win.
- **Confidence**: medium.

### PO-Idea-13 — Use `_mm_pause` between `poll_once` calls in bench arms

- **What/where**: bench-tx-burst `dpdk.rs:540-547` `poll_gap` loop. On the burst-driver side,
  tight-spinning `poll_once + drain_and_accumulate_readable` chains can hammer the cache. A
  `core::arch::x86_64::_mm_pause()` (or `std::hint::spin_loop()`) between iters when no work
  was done lets SMT siblings (if any) make progress and reduces frontend pressure.
- **Expected effect**: marginal latency improvement (5-15 ns) on the burst-drain phase; mostly
  a power-saving / fairness improvement. Indirectly improves p99/p999 by reducing branch-mispred
  rates in the dispatch loop.
- **Risk**: minimal — `spin_loop()` is portable.
- **Latency vs throughput**: neither — it's a bench-side cleanup.
- **Confidence**: low impact, low risk.

### PO-Idea-14 — Reduce inline-write of zeros on small payloads

- **What/where**: `tcp_output.rs:248-282` writes `0x0000` to `id`, `flags+offset`, `checksum`,
  then later overwrites the cksum field. For small (control / ACK) frames, this is small but
  adds up.
  - Combine into one 8-byte `u64` store covering `id || flags || ttl || proto || cksum_zero`.
- **Expected effect**: 2-5 ns per segment build.
- **Risk**: low (endianness is well-defined; tests already check wire bytes).
- **Latency vs throughput**: tiny latency win.
- **Confidence**: low.

### PO-Idea-15 — Cache `rte_get_tsc_hz()` to avoid the FFI hop in poll_once

- **What/where**: `engine.rs:3060` calls `sys::rte_get_tsc_hz()` once per poll iteration
  (slow-path mempool sample gating). The value is constant after EAL init.
  - Cache on `Engine` at construction.
- **Expected effect**: 1-3 ns per poll iteration.
- **Risk**: trivial.
- **Latency vs throughput**: tiny latency win.
- **Confidence**: high.

---

## 5. Red flags / things NOT to do

These are fstack techniques that would HURT our latency if copied:

- **DO NOT add a `BURST_TX_DRAIN_US` time-based drain.** fstack's 100 µs drain is the proven
  root cause of its bimodal RTT (see fstack-bimodality-investigation report). Our end-of-poll
  drain is strictly better for latency. If someone proposes "batch the drain to amortize
  syscalls", flag it.
- **DO NOT enable delayed-ACK (`tcp_delayed_ack=true` in `EngineConfig`) by default.** It
  trades a 40 ms tail for ~50 % fewer ACKs on the wire; we don't care about wire economy at
  trading scale, we care about p99/p999. Keep the field for spec parity but don't honor it
  in `emit_ack` until a use case appears.
- **DO NOT adopt fstack's "chain header mbuf to data mbuf" pattern on the steady-state
  `send_bytes` path.** Our PO10 single-mbuf direct-write is faster. The chain pattern is
  appropriate only on retransmit (where we already use it — `build_retrans_header`).
- **DO NOT switch RX-burst batch size higher than 32.** fstack's `MAX_PKT_BURST=32` is the
  DPDK convention; higher (64, 128) reduces per-burst syscalls but pushes more L1 pressure
  during the dispatch loop and increases head-of-line latency for the last packet in a burst.
  32 is the right number; we already use it (`engine.rs:3118`).
- **DO NOT enable BSD-style PCB hash hint stashing on the RX mbuf.** fstack does
  `in_pcblookup_mbuf` which reads a precomputed hash from the mbuf header; on our side, the
  NIC already provides the RSS hash via `nic_rss_hash` (`engine.rs:4511`) — we use it. No
  middleware lookup-hint needed.
- **DO NOT copy fstack's `M_NOWAIT` + `mempool_get` retry loop.** fstack falls back to a
  contention-friendly path under mempool pressure (`m_gethdr` returns NULL → caller drops). Our
  `tx_drop_nomem` counter + edge-triggered `ENOMEM` event (Site 3, `engine.rs:3241`) is the
  right shape — backpressure surfaces, no retry storm.
- **DO NOT add `INP_HASH_LOCK`-style RWlocks on the flow table.** We're single-thread on the
  data path; locks add ~5-10 ns/segment for zero benefit. The `RefCell` borrow check is already
  the minimum machinery.

---

## 6. Outside the scope of "match fstack"

These are independent wins that do NOT come from the fstack diff but emerged while reading our
own code. Listed here for completeness, not part of the prioritized list above:

- The `for h in handles.drain(..)` cleanup at the top of `poll_once` (`engine.rs:3107-3115`)
  iterates EVERY active connection's `delivered_segments` and `readable_scratch_iovecs` to
  `.clear()` them. On a many-conn workload this is O(N_conns) per poll. Could be amortized
  by clearing on next-use rather than next-poll.
- `obs-poll-saturation` (`engine.rs:3158-3163`) bumps a counter when `n == BURST` on every
  poll. The counter is default-on per spec §9.1.1; a feature-off build trims one branch per
  poll. Not a latency lever in production, but a tiny win for bench-only configs.
- `engine.rs:6435-6470` allocates a stack array of `[*mut sys::rte_mbuf; BULK_MAX=32]` in
  `send_bytes` on every call. That's 256 B written zero on every invocation; the bulk-alloc
  shim then overwrites whatever portion it uses. Using `MaybeUninit::<*mut _; 32>::uninit()`
  saves the zero-fill — measurable when `send_bytes` is called many times for small payloads.

---

## 7. Confidence + verification notes

- All file:line citations above are against fstack tree `a8b3a9ad…` (read-only at
  `/opt/src/f-stack`) and our worktree HEAD `e1302c2` at `/home/ubuntu/resd.dpdk_tcp-a10-perf/`.
  Re-grep before landing any patch — line numbers shift.
- Throughout, I bound the per-µs estimates by what's measurable in the bench-tx-burst
  burst-initiation pipeline (we already win there by 4-60× vs fstack, so additional latency
  improvements compound on top of that lead).
- No tests were run. Every claim about "what fstack does" is from source reading. Every claim
  about "what we do" is from source reading + the existing bench reports listed in §2.
- Recommendation for sequencing: PO-Idea-1 (prefetch) and PO-Idea-3 (ACK bulk-alloc) first —
  both small, both high confidence. Land them separately so the bench-pair before/after
  attribution is clean. PO-Idea-2 (header-prediction fast-path) is the biggest single win but
  also the biggest engineering surface; it would be the next major PO cycle.
