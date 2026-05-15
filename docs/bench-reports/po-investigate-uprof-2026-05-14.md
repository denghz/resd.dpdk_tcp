# PO investigate (uProf + perf) — bench-tx-burst & bench-rx-burst hot path (2026-05-14)

## 1. Executive summary

Top latency bottlenecks (data from live dpdk_net runs on this AMI):

1. **DPDK ENA driver dominates TX CPU** — the data-segment TX path spends 33-47 % of bench-tx-burst CPU inside `ena_com_prepare_tx` + `ena_com_create_meta` + `ena_com_write_sq_doorbell`. Of that, **~5 % is pure dead `rte_log` call overhead** on the doorbell-debug message that `ena_trc_dbg` evaluates on every doorbell write even though the level is filtered out (a wasteful runtime call into `rte_log → rte_vlog`).
2. **`tx_tcp_frame` (control / bare-ACK emitter) is fired once per inbound peer ACK** during the burst's RX-drain phase. With G=0 and K=65 KiB this fires ~45 times per burst (one ACK per data segment that arrives back). Each call pays a per-frame `rte_eth_tx_burst(...1)` (single packet, no batching) + 2 `lock xadd` counter increments — **PO8 did NOT batch this path** (PO8 only collapsed atomics inside the per-segment `send_bytes` loop).
3. **`advance_timer_wheel` is 11.7 % of bench-rx-burst self-CPU**, dominated by a `RefCell::borrow_mut` runtime check + an unconditional TSC read + ns conversion every poll. The TSC-tick early-exit happens AFTER the borrow; a TSC-only outer gate would skip the borrow entirely on idle polls.

Top 3 proposed ideas (full prioritized list in §6):

- **PO-Idea-1 (high confidence, latency win)**: silence `ena_trc_dbg` runtime log calls by setting `ena_logtype_com` log level to WARNING via EAL `--log-level lib.eal.ena.com:warning`, OR rebuild librte_net_ena with `ena_trc_dbg` compile-gated to a no-op. **Expected: ~30-80 ns saved per `rte_eth_tx_burst` + per `ena_com_write_sq_doorbell` site, ~1-3 % of bench-tx-burst CPU**. Pure win, zero risk.
- **PO-Idea-2 (medium confidence, latency-only win)**: gate `advance_timer_wheel`'s `RefCell::borrow_mut` behind a TSC tick comparison stored in a `Cell<u64>` outside the wheel (free `now_ns()` ➜ if `(now_ns/TICK_NS) <= last_advanced_tick` early-return). **Expected: ~15-25 ns per idle poll, ~5-8 % of bench-rx-burst CPU**. Risk: subtle (RefCell borrow guard semantics) but mechanical.
- **PO-Idea-3 (high confidence, latency-only win)**: stop calling `rte_eth_tx_done_cleanup` on every empty-ring poll. Gate behind a "we pushed mbufs onto the TX ring within the last N polls OR last M ms" condition. **Expected: ~6 % of bench-rx-burst CPU; 0 ns added to TX latency, removes ~1-3 µs of work from every idle poll iteration that runs between bursts**. Risk: documented mbuf pool depletion path under pathological no-TX bench shapes — see §6 for the safe-guard pattern.

## 2. Environment & methodology

| Attribute | Value |
|-----------|-------|
| Host | AWS dev-box `dpdk-dev-box.canary.bom.aws`, 8 vCPU C-class (KVM) |
| CPU | **Intel Xeon Platinum 8488C** (Family 0x6 Model 0x8f, Sapphire Rapids), 2.4 GHz base, invariant TSC |
| Kernel | 6.8.0-1053-aws |
| DPDK | 23.11 at `/usr/local/lib/x86_64-linux-gnu/dpdk/pmds-24.0/` |
| NIC | ENA (driver `net_ena`, port `0000:28:00.0` bound `vfio-pci`) |
| Branch | `a10-perf-23.11` @ `e1302c2` (post-PO1..PO10 merge) |
| Binary | `target/release/bench-{tx,rx}-burst` built `--features fstack` @ 06:59 UTC, debug_info present |
| Peer | `10.4.1.228` running echo-server (:10001), linux-tcp-sink (:10002), burst-echo-server (:10003) — all up |
| Profilers | AMD uProf 5.2.606 `--config hotspots` + Linux `perf record -F 999 -g --call-graph dwarf` |
| `perf_event_paranoid` | -1 (set this session) |

**Important caveat — Intel, not AMD.** The skill description called for AMD uProf on AMD Zen, but this host is **Intel Xeon**. AMD uProf's hardware PMC features (IBS, AMD-specific events, IPC/CPI metrics) are unavailable here, AND the KVM guest does not expose any hardware perf counters at all (`perf stat -e cycles,instructions` returns `<not supported>`). I fell back to **time-based sampling (TBP / cpu-clock)** only. The reports below have:

- Function-level cycle attribution: YES (via timer-IRQ sampling at 999-1000 Hz)
- IPC / CPI / branch mispred / cache miss rates: **NO — hardware counters not available in this guest**
- Per-instruction precise IBS: NO (Intel)
- Call-graph: YES via DWARF unwind on the Rust binary

Configurations actually run (steady-state samples only, no profile-mode startup):

| Profile | Binary | Config | Wallclock | Samples | Notes |
|---------|--------|--------|-----------|---------|-------|
| TBP-1   | bench-tx-burst | K=65536, G=0, 2 k bursts | 3.6 s | ~1.3 k (uProf, no callstack) | First-pass top-functions |
| Hotspots-1 | bench-tx-burst | K=65536, G=0, 10 k bursts | 10.0 s | ~5 k (uProf, no cs) | Steady-state |
| Hotspots-2 | bench-tx-burst | K=65536, G=0, 10 k bursts | 10.5 s | 1 299 (perf -g dwarf) | Cross-check, call-graph |
| Hotspots-3 | bench-tx-burst | K=65536, G=10 ms, 500 bursts | 11.6 s | uProf | Idle-poll-dominated arm |
| Hotspots-4 | bench-rx-burst | W=256, N=64, 2 k bursts | ~6 s | uProf | RX-path profile |
| Hotspots-5 | bench-rx-burst | W=256, N=64, 2 k bursts | ~7 s | 772 (perf -g dwarf) | Cross-check |

All session dirs and reports live under `/tmp/uprof-bench-{tx,rx}-*/` and `/tmp/perf-bench-{tx,rx}-*.data`.

Observed measurement-window results (single-run, sanity-only; not statistical):

- `bench-tx-burst dpdk_net K=65536 G=0`: `burst_initiation_ns` **p50=4 559**, p99=6 105, p999=15 037, mean=4 636 ns. `pmd_handoff_rate_bps` p50 ≈ 1.10 Gbps. Run took ~3.6 s of which ~0.5 s was EAL init + ARP probe.
- `bench-rx-burst dpdk_net W=256 N=64`: per-segment `latency_ns` p50=125 µs, p99=154 µs, p999=164 µs (cross-host CLOCK_REALTIME so this includes NTP offset; cf. methodology doc).

## 3. bench-tx-burst hotspot table (K=65 KiB, G=0, steady state)

Combined uProf + perf, normalized to bench process CPU (5.32 s of 10.04 s wallclock; lcore 2 pinned, busy-polling):

| # | Function | Module | Self CPU | TOTAL CPU (incl. children) | Notes |
|---|---|---|---|---|---|
| 1 | `dpdk_net_core::engine::Engine::tx_tcp_frame` | bench-tx-burst | 2.44 s (45.9 %) | 2.25 s (uProf children-roll) | Mostly mis-attributed leaf samples from ENA driver beneath; true self-cost ~8-12 % (LOCK ADD on `tx_bytes`/`tx_pkts` counters dominates) |
| 2 | `ena_com_create_meta` | librte_net_ena | 0.96 s (18.0 %) | 0.96 s | Per-pkt LLQ metadata fill (memset of bounce buffer + descriptor layout) |
| 3 | `ena_com_prepare_tx` | librte_net_ena | 0.60 s (11.3 %) | 1.53 s | Per-pkt TX descriptor prep; calls `ena_com_create_meta` |
| 4 | `dpdk_net_core::engine::Engine::poll_once` | bench-tx-burst | 0.21 s (3.9 %) | **4.69 s (88.1 %)** | Outer poll loop owns everything below |
| 5 | `rte_log` + `rte_vlog` | librte_log | 0.29 s (5.5 %) | 0.29 s | **DEAD log calls** triggered by `ena_trc_dbg` in `ena_com_write_sq_doorbell` every TX burst |
| 6 | `eth_ena_xmit_pkts` | librte_net_ena | 0.24 s (4.5 %) | 2.10 s | Per-burst ENA TX driver entry |
| 7 | `__memset_avx512_unaligned_erms` | libc | 0.11 s (2.1 %) | 0.11 s | Page-zero (hugetlb populate, mempool init), startup-leaning |
| 8 | `__GI___libc_read` | libc | 0.09 s (1.7 %) | 0.09 s | startup-only (config / EAL probe) |
| 9 | `dpdk_net_core::engine::Engine::drain_tx_pending_data` | bench-tx-burst | 0.09 s (1.7 %) | 2.10 s | Batches data segments to `rte_eth_tx_burst` |
| 10 | `clear_page_erms` (kernel) | vmlinux | 0.06 s (1.1 %) | 0.06 s | Hugetlb fault, startup |

Top **call-tree** path (perf `--children`, samples that include the poll loop):

```
77.44% main → run_burst_grid_dpdk → poll_once → ...
   ├─ 36.18 % tx_tcp_frame (ACK emit per peer ACK)
   ├─ 32.41 % drain_tx_pending_data
   │     └─ 31.18 % eth_ena_xmit_pkts
   │           ├─ 24.10 % ena_com_prepare_tx
   │           │     └─ 14.93 % ena_com_create_meta   ← leaf cost
   │           └─  4.08 % ena_com_write_sq_doorbell
   │                 └─  4.08 % rte_log
   │                       └─  2.62 % rte_vlog        ← dead-log overhead
   ├─  1.39 % advance_timer_wheel (incl. retransmit_inner)
   └─  0.54 % shim_rte_eth_rx_burst (RX path: peer ACKs)
```

Key engine-side functions ranked by SELF time (perf flat report):

```
34.87 % tx_tcp_frame
15.09 % ena_com_create_meta
 9.62 % ena_com_prepare_tx
 9.31 % __memset_avx512_unaligned_erms (likely page populate)
 5.47 % clear_page_erms (kernel, hugetlb)
 3.23 % eth_ena_xmit_pkts
 3.23 % rte_vlog
 2.62 % poll_once (true self)
 1.54 % rte_log
 1.23 % drain_tx_pending_data (true self)
 1.08 % rte_pktmbuf_init (startup; mempool init)
 1.00 % _raw_spin_unlock_irqrestore (kernel, VFIO IRQ path)
 0.77 % internet_checksum
```

## 4. bench-rx-burst hotspot table (W=256, N=64, steady state)

bench-rx-burst process CPU: 0.96 s of 6 s wallclock (lcore 2 pinned). Samples sparser because most time is spent waiting on peer-sent bytes — i.e. CPU is mostly polling.

| # | Function | Module | Self CPU | TOTAL CPU | Notes |
|---|---|---|---|---|---|
| 1 | `dpdk_net_core::engine::Engine::poll_once` | bench-rx-burst | 0.16 s (16.7 %) | 0.60 s (62.5 %) | Outer hot loop |
| 2 | `dpdk_net_core::engine::Engine::advance_timer_wheel` | bench-rx-burst | 0.14 s (14.6 %) | 0.20 s (20.8 %) | **RefCell borrow + TSC + tick check** — runs per poll iteration |
| 3 | `dpdk_net_core::engine::Engine::tx_tcp_frame` | bench-rx-burst | 0.10 s (10.4 %) | 0.10 s | Bare-ACK emit on every received chunk (mostly mis-attributed leaf again) |
| 4 | `[vdso]!0x00000b03` (`__GI___clock_gettime`) | vDSO | 0.09 s (9.4 %) | 0.09 s | `bench-rx-burst::wall_ns` (`SystemTime::now` for cross-host CLOCK_REALTIME) per chunk drain |
| 5 | `__memcpy_avx512_unaligned_erms` | libc | 0.07 s (7.3 %) | 0.07 s | `extend_from_slice` of conn iovec scratch into `recv_buf` per chunk |
| 6 | `__GI___libc_read` | libc | 0.07 s (7.3 %) | 0.07 s | Mixed: VFIO ioctl + some EAL state probes |
| 7 | `ena_tx_cleanup` | librte_net_ena | 0.03 s (3.1 %) | 0.05 s | Called by `drain_tx_pending_data` even with empty TX ring (mbuf-completion reclaim) |
| 8 | `dpdk_net_core::engine::Engine::drain_tx_pending_data` | bench-rx-burst | 0.03 s (3.1 %) | 0.10 s | Empty-ring path still calls `rte_eth_tx_done_cleanup` |
| 9 | `eth_ena_recv_pkts` | librte_net_ena | 0.02 s (2.1 %) | 0.03 s | DPDK RX driver entry |
| 10 | `__memset_avx512_unaligned_erms` | libc | 0.02 s (2.1 %) | 0.02 s | Startup hugetlb |

perf `--children` view (steady state):

```
80.05 % main → run_one_burst → poll_once → ...
   ├─ 19.04 % tx_tcp_frame   (ACK emit per RX burst)
   ├─ 13.21 % advance_timer_wheel
   │     └─  2.07 % __memmove_avx512_unaligned_erms (cascade re-bucket of arming RTO timers)
   ├─ 11.79 % drain_tx_pending_data
   │     └─  9.72 % rte_eth_tx_done_cleanup
   │           └─  6.87 % ena_tx_cleanup   ← runs on EMPTY-ring polls
   ├─  5.05 % shim_rte_eth_rx_burst
   │     └─  4.53 % eth_ena_recv_pkts
   └─  0.65 % check_and_emit_rx_enomem
```

`wall_ns()` skid: 8.42 % self in the vDSO leaf, called from `bench_rx_burst::dpdk::run_one_burst` per chunk drain. Path is `SystemTime::now → clock_gettime(CLOCK_REALTIME) → vDSO → rdtsc + scale`. This is **bench methodology, not engine** — we need a wall clock to compare against `peer_send_ns` from the peer's `clock_gettime`.

## 5. Per-function bottleneck analysis

### 5.1 `tx_tcp_frame` — single-packet ENA driver + per-call atomics

`crates/dpdk-net-core/src/engine.rs:2732-2810`. Called from emit_ack at engine.rs:5666, persist-probe at 3825, FIN/RST emitters at 5735/5773/5825, SYN at 2962, RST-from-RX-orphan at 7031, listen-probe at 8336, and once-per-loop from `dpdk_net_flush`. **Always invoked with `rte_eth_tx_burst(... pkts.as_mut_ptr(), 1)` — a single packet per call.**

Hot-path issues for the bench:
1. **Bare-ACK emit fires once per inbound peer ACK during the data drain**, because `emit_ack` is unconditionally called from the input path on every received data segment after `outcome.ack_to_send`. Under our bench shape (1 conn, no delayed-ACK aggregator on the DUT side), that's ~45 calls per K=65 KiB burst.
2. Each call: `shim_rte_pktmbuf_alloc(tx_hdr_mempool)` + `shim_rte_pktmbuf_append` + `memcpy(bytes→data)` + `tx_offload_finalize` + `rte_eth_tx_burst(...1)` + `lock add %r14, eth.tx_bytes` + `lock incq eth.tx_pkts`. The two LOCK ADDs at lines 2802-2803 are visible as a `100%-local` skid hotspot in `perf annotate` (function offset `+156`).
3. **PO8 did NOT batch this path.** PO8 batched the per-segment counter atomics inside `send_bytes`'s data loop; the bare-ACK / control-frame TX still pays 2 atomic RMWs per emit.

### 5.2 `ena_com_prepare_tx` + `ena_com_create_meta` + doorbell `rte_log`

Library `librte_net_ena.so.24.0` (DPDK 24-built; built without the `ena_trc_dbg` compile gate, since the runtime check inside `rte_log` is reached).

- `ena_com_create_meta` (15.09 % self in TX bench) is the cost of building the LLQ TX descriptor metadata + `memset`-ing the bounce buffer. This is **per packet** — for one K=65 KiB burst it runs 45 times. The function is in DPDK ENA driver source `lib/log/log.c` and `drivers/net/ena/base/ena_eth_com.h`.
- `ena_com_write_sq_doorbell` (~1 % self, but 4.08 % including `rte_log`/`rte_vlog`) is called once per TX-burst-batch. The dead-log site is at `/opt/src/f-stack/dpdk/drivers/net/ena/base/ena_eth_com.h:160-162`:

  ```c
  ena_trc_dbg(ena_com_io_sq_to_ena_dev(io_sq),
      "Write submission queue doorbell for queue: %d tail: %d\n",
      io_sq->qid, tail);
  ```

  `ena_trc_dbg` expands to `rte_log(RTE_LOG_DEBUG, ena_logtype_com, ...)` at `/opt/src/f-stack/dpdk/drivers/net/ena/base/ena_plat_dpdk.h:118` with **no compile-time gate**. `rte_log` always builds `va_list` + calls `rte_vlog`, which checks `rte_log_can_log(logtype, level)` AFTER paying call-overhead + va-arg setup. Roughly 80-150 ns per filtered-out call.

### 5.3 `advance_timer_wheel` (RX-side)

`crates/dpdk-net-core/src/engine.rs:3426-3428`, calling `tcp_timer_wheel::TimerWheel::advance` at `crates/dpdk-net-core/src/tcp_timer_wheel.rs:150-189`.

- `advance_timer_wheel` is called **unconditionally** from every `poll_once` (both the RX-idle path at engine.rs:3131 and the RX-took-pkts path at 3199). 
- Inside, `let _ = self.fire_timers_at(crate::clock::now_ns())` first takes `RefCell::borrow_mut()` on the timer wheel (the perf-annotate skid hit on `cmpq $0x0,0x278(%rdi)` at engine.rs:c7f16 is the BorrowFlag check at +38 bytes from function entry, which is the 70 % local hotspot of `advance_timer_wheel`'s 90 sampled instructions). 
- THEN it calls `advance(now_ns)`, which computes `now_tick = now_ns / TICK_NS` and early-returns if `now_tick <= self.last_tick`. **The borrow + TSC read are paid on every poll regardless of whether a tick has actually elapsed.**
- The 2.07 % `__memmove_avx512_unaligned_erms` deeper in the call tree is the `std::mem::swap(Vec<u32>, Vec<u32>)` in `cascade()` (`tcp_timer_wheel.rs:238` / `:254`) — fires on level-0 cursor wraparound, ~once per ~2.5 ms.

## 6. Prioritized optimization proposals

Each item is tagged `(latency / throughput / both)`, with a confidence level and a `risk` note. Listed in expected `bench-rtt p50` + `burst_initiation p50` win order with low-risk items at the top.

### PO-Idea-1 — Silence DPDK ENA debug log on the doorbell hot path
- **What**: At engine bring-up (`Engine::new`), call `rte_log_set_level_pattern("lib.eal.ena.com", RTE_LOG_WARNING)` (or `rte_log_set_global_level`). Alternative fix: rebuild `librte_net_ena.so` with `ena_plat_dpdk.h:118` patched to `#define ena_trc_dbg(dev, format, arg...) do { (void)(dev); } while (0)`.
- **File:line**: site is the dead-log call at `/opt/src/f-stack/dpdk/drivers/net/ena/base/ena_eth_com.h:160-162` and `ena_plat_dpdk.h:111-118`. Our fix lives at engine bring-up (`crates/dpdk-net-core/src/engine.rs::engine_init` or `dpdk-net-sys/build.rs` if we go the rebuild route).
- **Effect on burst latency**: every `rte_eth_tx_burst` call hits this. With ~46 TX bursts per K=65 KiB burst (45 data segments × 1-pkt-per-burst calls for the TX path's batched `drain_tx_pending_data` is 1-2; with the per-ACK `tx_tcp_frame(...1)` it's 45+). Each `rte_log` dead call ≈ 80-150 ns. **Estimated p50 burst_initiation reduction: 80-300 ns**; for the full burst (RX-drain phase has 45 ACKs × 1 doorbell each) ~3-6 µs of CPU savings, **~3-5 % of bench-tx-burst CPU**.
- **Risk**: zero functional risk for the runtime log-level approach. Rebuild approach requires reproducible DPDK source patch + AMI bake.
- **Confidence**: **high**. Profile shows 4.08 % self-time in `rte_log+rte_vlog` from `ena_com_write_sq_doorbell` exclusively, on a default-level build.
- **Latency vs throughput tradeoff**: latency-only win; throughput may benefit too. Zero tradeoff.

### PO-Idea-2 — TSC-tick early-exit before `RefCell::borrow_mut` in `advance_timer_wheel`
- **What**: Hoist `now_tick` check out of `TimerWheel::advance` into the engine. Replace `engine.rs:3426-3428` `let _ = self.fire_timers_at(now_ns)` with:
  ```rust
  let now_ns = crate::clock::now_ns();
  let now_tick = now_ns / crate::tcp_timer_wheel::TICK_NS;
  if now_tick <= self.advance_last_tick.get() { return; }
  let _ = self.fire_timers_at_with(now_ns, now_tick);
  ```
  Add `advance_last_tick: Cell<u64>` to `Engine`. Update inside `fire_timers_at` to maintain it after each successful advance.
- **File:line**: `crates/dpdk-net-core/src/engine.rs:3426-3428`. Wheel impl `crates/dpdk-net-core/src/tcp_timer_wheel.rs:150-152` already has the inner early-exit but it's behind the `borrow_mut`.
- **Effect**: skip 1 `RefCell::borrow_mut` (~5 ns) + 1 `now_ns()` re-read (already paid above; now shared) per idle poll. **Estimated: ~10-20 ns per poll iteration**. For bench-rx-burst that's 8-14 % of self-CPU. Doesn't change first-byte burst latency for bench-tx-burst, but reduces poll cost during idle phases — improves bench-rtt p50 by ~30-100 ns (idle gap between iters dominated by poll cost).
- **Risk**: low; the wheel's last_tick is monotone and only advances inside `advance()`. Cell + monotonicity protects against TOCTOU.
- **Confidence**: **medium-high**. The wheel's inner check already exists, this just moves it earlier.
- **Latency vs throughput**: latency-only. No throughput tradeoff.

### PO-Idea-3 — Stop calling `rte_eth_tx_done_cleanup` on every empty-ring poll
- **What**: Gate the empty-ring branch at `engine.rs:3280-3303` so it runs only when (a) the previous poll pushed to the ring, OR (b) at most every M ms (e.g. 1 ms). Add `tx_pending_drained_recently: Cell<bool>` (set when ring drained inline) and `tx_done_cleanup_last_tsc: Cell<u64>`.
  ```rust
  if ring.is_empty() {
      let now = crate::clock::rdtsc();
      if self.tx_pending_drained_recently.replace(false)
         || now.wrapping_sub(self.tx_done_cleanup_last_tsc.get())
            >= self.tsc_hz.get() / 1000   /* ~1 ms */ {
          unsafe { sys::rte_eth_tx_done_cleanup(...); }
          self.tx_done_cleanup_last_tsc.set(now);
      }
      return;
  }
  ```
- **File:line**: `crates/dpdk-net-core/src/engine.rs:3277-3303` (`drain_tx_pending_data` empty-ring branch).
- **Effect**: bench-rx-burst spends **6.87 % of CPU in `ena_tx_cleanup`** called from this empty-ring branch. Each `rte_eth_tx_done_cleanup` iterates the completion ring (~1-3 µs at idle). **Estimated savings: 1-3 µs every poll between bursts** — directly reduces the poll loop's idle cost, which is what runs between RX bursts and during the `poll_gap` phase. For bench-rtt this can shave 0.5-2 µs off each idle window.
- **Risk**: medium. The comment at engine.rs:3281-3293 documents that without this, mbufs in the ENA completion queue stay refcnt=1 and exhaust the pool. The 1 ms timer guards that. Worst-case under deep idle: pool drain is delayed by ≤1 ms — bounded.
- **Confidence**: **high** on perf gain; **medium** on safety — needs a soak test of "10 minutes of idle with one stale TX in completion queue" before merge.
- **Latency vs throughput**: latency-only win. No throughput hurt; under sustained TX the ring is non-empty so this branch never fires anyway.

### PO-Idea-4 — Batch `tx_tcp_frame` ACK emits via the existing `tx_pending_data` ring
- **What**: Route control frames (bare ACK in particular — see `engine.rs::emit_ack` at 5666) through the same `tx_pending_data` batching ring used by `send_bytes` data segments. Today every `tx_tcp_frame` calls `rte_eth_tx_burst(...1)` directly; instead, push the mbuf onto the ring and let the end-of-poll `drain_tx_pending_data` send all queued frames in one `rte_eth_tx_burst(N)` call. Schema: `tx_tcp_frame_queued()` for control frames issued from rx-path callsites, with a fallback to inline send for paths that must serialize immediately (RST, FIN).
- **File:line**: re-route from `engine.rs:5666` (`emit_ack`) into a new helper that uses `tx_pending_data`. The `tx_hdr_mempool` mbufs are smaller than data mbufs so we'd need a second ring or a tagged-mbuf ring.
- **Effect**: collapses ~45 single-packet `rte_eth_tx_burst(1)` calls per K=65 KiB burst into 1-2 batched calls. ENA driver per-packet overhead is fixed (~1.5 µs each), but the driver's batch entry/exit + doorbell amortization saves ~5-10 µs total per burst. **Estimated burst-end-to-end latency reduction: 3-8 µs** (the bench measures `t1` after the final segment's `rte_eth_tx_burst` returns, so the ACK-drain phase of the burst contributes to `t1 - t0`).
- **Risk**: medium-high. Re-ordering control vs data frames could subtly affect ACK timing (peer's view of the ACK clock) and break a downstream RACK / TLP measurement. Needs careful inspection of every `tx_tcp_frame` callsite — fewer than 10 sites.
- **Confidence**: **medium** on impl complexity; **high** on the underlying perf payoff if done correctly.
- **Latency vs throughput**: throughput improves; latency for bare-ACK improves slightly (the peer sees the ACK ~1 poll iter later but the DUT spends less CPU emitting it, freeing the next data segment). **Possible tradeoff**: a single-bare-ACK-after-N-segments scenario sees ACK emission delayed by ≤1 poll iter (~0.5-1 µs). Acceptable for latency-favoring trading workloads where every ACK eventually clears within the same poll loop.

### PO-Idea-5 — Batch `tx_tcp_frame`'s per-call counter atomics
- **What**: Same pattern as PO8 but for the control-frame path. Replace the two `fetch_add(_, Relaxed)` at `engine.rs:2802-2803` with `engine`-level stack-local accumulators flushed at end-of-poll. Since `tx_tcp_frame` already isn't a parallel hot path within one poll (single lcore), accumulate locally and flush at exit.
- **File:line**: `crates/dpdk-net-core/src/engine.rs:2801-2804`.
- **Effect**: each `lock xadd` is ~10-15 ns on Sapphire Rapids; saves ~25-30 ns per `tx_tcp_frame`. At 45 calls/burst that's **~1.1-1.4 µs per burst**.
- **Risk**: low — counters are observability-only, snapshot cadence is documented as ≤1 Hz, so within-poll delay is invisible to consumers. Matches PO8's design.
- **Confidence**: high.
- **Latency vs throughput**: latency-only win.

### PO-Idea-6 — Reduce per-poll bookkeeping for the conn-local scratch clears
- **What**: `poll_once` at `engine.rs:3105-3116` iterates every conn and calls `c.delivered_segments.clear()` + `c.readable_scratch_iovecs.clear()` per poll. With one conn this is two cheap clears; the cost is the `flow_table.borrow_mut()` + the conn handle scratch SmallVec ops. Track a `dirty` bit per conn that gets set when `deliver_readable` populates the scratch, and only clear conns whose dirty bit is set.
- **File:line**: `engine.rs:3105-3116`.
- **Effect**: bench-rx-burst self-time in `poll_once` is 16.7 %; some of that is this prelude. **Estimated savings: ~50-100 ns per poll iteration when no Readable fired the previous poll** (most polls between bursts).
- **Risk**: low; pure scratch-clear is idempotent.
- **Confidence**: medium.
- **Latency vs throughput**: latency-only.

### PO-Idea-7 — Hoist `now_ns` in `poll_once`'s mempool-sampling block
- **What**: `engine.rs:3057-3078` does a TSC `rdtsc` + `rte_get_tsc_hz()` call + comparison every poll, even though the sampling only fires once per second. Cache `tsc_hz` at `Engine::new` (we already have it via `cfg.tsc_hz` or a `Cell`) and just compare TSC; skip the FFI call.
- **File:line**: `engine.rs:3057-3078`.
- **Effect**: `rte_get_tsc_hz()` is a thin wrapper but still a non-inlined FFI call. ~10-15 ns per poll iteration.
- **Risk**: zero.
- **Confidence**: high.
- **Latency vs throughput**: latency-only.

### PO-Idea-8 — Lift `BULK_MAX=32` cap in `send_bytes`
- **What**: At engine.rs:6414, `const BULK_MAX: usize = 32`. For K=65 KiB at MSS=1460, total_segments=45 > BULK_MAX, so the call accepts only 32 × 1460 = 46 720 bytes, the caller (`drive_burst_remainder_to_completion`) loops, and we pay a second `bulk_alloc + drain_tx_pending_data` cycle. Bump to 64 or 96 so K=65 KiB completes in one send_bytes / one drain.
- **File:line**: `engine.rs:6414`. Side-effect: the on-stack array `mbufs: [*mut sys::rte_mbuf; BULK_MAX]` grows to 8 bytes × 96 = 768 B (still well under the 4 KiB stack-safe budget).
- **Effect on burst latency**: removes one full `rte_eth_tx_burst` round-trip's worth of poll overhead from the K=65 KiB burst — **estimated 1-3 µs savings** on the per-burst t1-t0 window. **Does not affect burst_initiation** (which captures after the first send_bytes returns; that returns earlier with the same 32-segment first batch unless we bump higher).
- **Risk**: low. Mempool `rte_mempool_get_bulk` succeeds or fails atomically.
- **Confidence**: high.
- **Latency vs throughput**: latency wins; throughput wins. No tradeoff.

### PO-Idea-9 — Cache `tsc_hz` in `clock::now_ns` to remove the `OnceLock` read
- **What**: `clock::now_ns` at `crates/dpdk-net-core/src/clock.rs:44-50` calls `tsc_epoch()` which dereferences the static `TSC_EPOCH: OnceLock<TscEpoch>`. The `OnceLock::get_or_init` path is hot. Replace with a `tsc_epoch_uninit: AtomicPtr<TscEpoch>` that's set once during `init()` and read with a single `load(Relaxed)` after — no `OnceLock` cell-state-machine.
- **File:line**: `crates/dpdk-net-core/src/clock.rs:31-50`.
- **Effect**: `OnceLock::get` is already fast (atomic load + nullness check) but still 2-3 ns. Saves ~2-3 ns per `now_ns` call; called dozens of times per poll. **Estimated: 30-50 ns per poll iteration.**
- **Risk**: low (init order shift; would need `Engine::new` to set the atomic).
- **Confidence**: medium.
- **Latency vs throughput**: latency-only.

### PO-Idea-10 — Eliminate cross-host `wall_ns()` in `bench-rx-burst` hot path
- **What**: This is a **bench methodology** change, not engine: capture per-segment timestamps via TSC and post-process to wall-clock domain at run end, rather than reading CLOCK_REALTIME per chunk. The vDSO path is 8.42 % of bench-rx-burst CPU.
- **File:line**: `tools/bench-rx-burst/src/dpdk.rs:272` (`let dut_recv_ns = wall_ns();`).
- **Effect**: removes ~80-100 ns from each chunk drain. **Does not change the measured `latency_ns` distribution shape** — only frees CPU on the DUT for tighter polling. May indirectly improve p999 by reducing per-burst CPU jitter.
- **Risk**: tooling-only.
- **Confidence**: high.
- **Latency vs throughput**: tooling improvement, **does not change product latency**. Flagged separately so the report doesn't claim engine latency from a bench-only change.

### PO-Idea-11 — Inline `clock::rdtsc + scale` instead of `clock::now_ns` everywhere internal
- **What**: Where the engine just needs to compare two cycle counts (e.g., timer fire condition), skip the ns conversion and compare TSC ticks directly.
- **File:line**: `engine.rs:3058-3076`, plus all `advance_timer_wheel` callers.
- **Effect**: saves the `mulq + shrd` chain per use. ~5-10 ns per call.
- **Risk**: low if scoped to internal comparisons; the external `rate_us` API still needs ns.
- **Confidence**: medium.
- **Latency vs throughput**: latency-only.

### PO-Idea-12 — Defer `maybe_emit_gratuitous_arp` + `maybe_probe_gateway_mac` checks
- **What**: At `engine.rs:3133-3134` (RX-idle path) and `3201-3202` (RX-took-pkts path), `maybe_emit_gratuitous_arp` and `maybe_probe_gateway_mac` are called every poll. Each contains an internal "is it time?" check (~10 ns). Defer behind the same TSC-tick mechanism as PO-Idea-7.
- **File:line**: `engine.rs:3133-3134`, `3201-3202`.
- **Effect**: ~20-30 ns per poll.
- **Risk**: low.
- **Confidence**: medium.
- **Latency vs throughput**: latency-only.

### PO-Idea-13 — Compile DPDK ENA driver with PMD_TX_LOG / PMD_RX_LOG gated to no-op
- **What**: At DPDK build time (AMI rebake), set `-DRTE_ETHDEV_DEBUG_TX=0` and `-DRTE_ETHDEV_DEBUG_RX=0`, AND patch `drivers/net/ena/base/ena_plat_dpdk.h` so `ena_trc_dbg` is a no-op:
  ```c
  #define ena_trc_dbg(dev, format, arg...) do { (void)(dev); } while (0)
  ```
- **File:line**: AMI-side; tracked by upstream DPDK build flags.
- **Effect**: removes 4-5 % of bench-tx-burst CPU AND ~30-80 ns from every doorbell write. Stronger version of PO-Idea-1 because it's a compile-time no-op vs. runtime-filter dead call.
- **Risk**: low; we lose the ability to enable ENA debug logging without a rebuild.
- **Confidence**: high.
- **Latency vs throughput**: both improve. No tradeoff.

### PO-Idea-14 — Pre-allocate `flow_table` `conn_handles_scratch` to avoid SmallVec spill
- **What**: `poll_once` at `engine.rs:3107` does `handles.clear(); handles.extend(ft.iter_handles())`. With one conn this stays in inline storage. With >N conns the SmallVec spills to heap on every poll. Verify the inline cap matches expected per-engine conn count for trading workloads (1-4 conns is typical).
- **File:line**: `engine.rs:3107`. Allocation declared elsewhere (search `conn_handles_scratch:`).
- **Effect**: protective; current trading workload has 1 conn so this is a no-op today, but a regression-guard for future workloads.
- **Risk**: zero.
- **Confidence**: low (no observable impact at 1 conn).
- **Latency vs throughput**: protective.

### PO-Idea-15 — Reorder `tx_tcp_frame`'s per-call atomic increments to a single 16-byte CAS
- **What**: At `engine.rs:2802-2803`, replace the two separate `fetch_add(Relaxed)` on adjacent `AtomicU64`s (`tx_bytes` then `tx_pkts`) with a single `_mm_storeu_si128`-style 16-byte store after computing both new values. (Note: requires non-atomic store since the lock prefix would be lost — only safe because lcore-2 is the sole writer.)
- **File:line**: `engine.rs:2802-2803`; counters struct layout in `crates/dpdk-net-core/src/counters.rs::EthCounters`.
- **Effect**: 2 × LOCK ADD (24-30 ns total) → 1 × ordinary 16B store (~3 ns). PO5 covers the same idea via batching at exit; this is a single-call alternative.
- **Risk**: medium — relies on single-writer invariant being preserved forever. PO5 (batched at exit) is the safer pattern.
- **Confidence**: low; prefer PO-Idea-5.
- **Latency vs throughput**: latency-only.

## 7. Profiling limitations / what's untrustworthy

1. **No hardware PMU counters.** The KVM guest hides IPC/CPI/cache-miss/branch-mispred from both perf and uProf. All numbers above are *time-based sampling at 999-1000 Hz* via the timer IRQ. Function-level wallclock attribution is fine; per-instruction cycle attribution carries ±1 cycle skid into the wrong instruction. **Specifically: `tx_tcp_frame`'s 100% local-period skid on the `lock add` instruction is misleading** — the real cycles are spent in ENA driver functions beneath, and the perf timer-IRQ samples are misattributed to the parent because ENA driver `.so` has no DWARF debug info and the unwinder stops one frame up. Reconciling: the 34.87% "tx_tcp_frame self" should be read as "samples taken while inside the tx_tcp_frame call subtree that perf couldn't unwind further into the ENA driver". uProf's `TOTAL_CPU_TIME` view confirms `tx_tcp_frame` TOTAL = self for the no-callstack run, which is what we'd see if it really were a leaf — so the children-roll-up still attributes most of that to ena_com_*.

2. **Wallclock includes startup overhead.** EAL init, hugepage population (`clear_page_erms`, `__memset_avx512_unaligned_erms`), mempool init (`rte_pktmbuf_init`), and ARP probe consume the first ~0.5-1 s of each profile. The 10 k-burst hotspot runs (10 s wallclock) dilute startup to ~5-10 %; the 2 k-burst TBP runs (3.6 s) carry ~25 % startup noise. For steady-state numbers, trust the 10 k-burst runs.

3. **DPDK ENA driver attribution.** The `librte_net_ena.so.24.0` install **DOES** carry symbol names but no DWARF debug info, so perf's `--call-graph dwarf` can resolve function names but cannot walk past those functions back into our Rust caller. The "ena_com_create_meta self=15.09%" attribution is correct *as a function-level number*; it just means perf could not unwind further.

4. **vDSO sample at `[vdso]!0x00000b03`**: uProf labels this as a raw offset, perf resolves it as `__GI___clock_gettime (inlined)`. Both refer to the same thing — the vDSO's CLOCK_REALTIME read called from `SystemTime::now`. This appears almost exclusively in bench-rx-burst (`wall_ns()` per chunk).

5. **`__memmove_avx512_unaligned_erms` (2.07 % in bench-rx-burst advance_timer_wheel)**: confirmed by source inspection to be the `std::mem::swap(Vec<u32>, Vec<u32>)` in `tcp_timer_wheel::cascade()`. NOT a buffer copy; the swap moves 24 B of Vec header. The compiler emits a memmove call for the symmetric-swap pattern.

6. **`__GI___libc_read` overhead (1.7 % in bench-tx-burst, 7.3 % in bench-rx-burst)** is a mix of: (a) bench startup `/etc/passwd` / `/proc/cpuinfo` reads, (b) VFIO `ioctl` for IRQ enable/disable, (c) bench-rx-burst's `peer-ssh` placeholder read at startup. NOT a measurement-window hot path.

7. **Did NOT measure**: `bench-tx-burst K=1 MiB` or `K=4 MiB` or larger K — the 65 KiB profile is the narrowest, most latency-sensitive shape, which is what we care about for the burst_initiation_ns metric. Larger K shifts the profile mix toward ENA driver throughput (saturated `rte_eth_tx_burst`).

8. **Did NOT measure**: bench-rtt itself. The RTT benchmark is a different shape (small ping-pong rather than batched K-byte burst). I expect the same `tx_tcp_frame` ACK-emit + `advance_timer_wheel` overheads to dominate, but on the bench-rtt critical path the time is also bounded by RTT (~70 µs same-AZ AWS), so engine CPU contribution is proportionally smaller.

9. **Did NOT modify code**. All findings come from profile-and-analyze; no experimental builds were tested.

10. **Skill mismatch on AMD uProf**. The skill brief assumed AMD Zen. This is Intel Sapphire Rapids on KVM with no PMU access. AMD uProf's IBS / hardware-counter features are unavailable; only the TBP (time-based sampling) facility worked. Linux `perf record` with `cpu-clock` software events worked equivalently and was used as the primary cross-check. uProf reports live at `/tmp/uprof-bench-{tx,rx}-*/`; perf data live at `/tmp/perf-bench-{tx,rx}-*.data`.
