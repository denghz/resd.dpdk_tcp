# Codex adversarial review — bench-overhaul fast-iter suite (2026-05-13)

Reviewer: Codex (GPT-5.4 via codex:rescue). Read-only static analysis.
Verdict: **NOT publication-ready.** 3 BLOCKERs, 5 IMPORTANTs, 1 MINOR, 2 NON-ISSUES.

## BLOCKERs

### B1 — T57→T58 absolute RTT regression unresolved
T57 (09:37 UTC): dpdk_net ~77-99µs / fstack ~100µs / linux ~104-109µs.
T58 (14:43-16:35 UTC same day): all three ~200-300µs. 2-3× regression.
Codex note: T57's own "What changed since T56" table lists `abd9601` (peer hardening) as ALREADY in the T57 run — so it's NOT a clean between-T57-and-T58 suspect unless that metadata is wrong. `5fecb92` (engine scratch-fix) is dpdk_net-only; T58 regressed all three. **INSUFFICIENT EVIDENCE statically.**
Action: re-run `61c5e00` and HEAD back-to-back on same host/peer; bisect only if it reproduces. Capture raw samples, host qdisc/iptables, ENA queue/coalescing/IRQ, ENA stats.

### B2 — Linux comparison confounded by a different ENI
dpdk_net + fstack use DPDK PCI `0000:28:00.0`. linux uses kernel `ens5` = `0000:27:00.0` via `nsenter`. **Two different ENIs on the same instance** — different queue config, IRQ affinity, coalescing. T57 claims "same peer, same wire" + "pure software-stack overhead" — that's false.
Action: either rebind the SAME NIC between vfio/kernel per-stack, OR publish explicitly as a two-ENI comparison with logged ethtool/IRQ/qdisc/iptables/route/ENA-stats for both NICs.

### B3 — verify-rack-tlp doesn't actually validate RACK
RACK code IS wired (not feature-gated off — Codex confirmed `tcp_conn.rs:287-292,460-462`, `tcp_input.rs:1056-1113`, `engine.rs:4553-4578`, `counters.rs:868-890`). But it never fires on AWS ENA (sparse ACK stream). The ANY-of assertion `rack>0 OR tlp>0` passes on `rack=0, tlp>0`. "RACK validated" is a vacuous pass.
Action: add a deterministic test requiring `tcp.tx_retrans_rack > 0` (e.g. controlled reorder injection that RACK detects before the RTO timer), OR demote the claim to "RTO/TLP counters validated; RACK not demonstrated on this setup."

## IMPORTANTs

### I1 — fstack RTT bimodality real but unexplained
fstack `rtt_ns` is a DUT-side round-trip (`Instant::now()` → `t0.elapsed()` in `fstack.rs:540-545,617-620`) — no cross-host clock skew. The 128B/1024B ~200↔300µs flip (CV ~21%) is real fstack/harness behavior. The suite doesn't pass `--raw-samples-csv` for bench-rtt — p50 hides the distribution.
Action: re-run fstack with raw samples + single-payload + randomized-payload-order modes; report histograms + p50/p99/p999.

### I2 — bench-tx-burst dpdk_net metric is PMD-handoff, not wire-rate
The captured end-time is after `rte_eth_tx_burst` returns + a poll/drain path, NOT after TX-descriptor reclaim, peer receipt, or HW timestamp. The first-segment fallback (`dpdk.rs:322-391`) is weaker still — it captures right after `send_bytes` returns, before the next `poll_once`.
Action: rename `throughput_per_burst_bps` → `pmd_handoff_rate_bps` (or similar); state it's not NIC completion. Wire-rate claims need peer-capture or HW TX timestamps.

### I3 — statistically underpowered
T57 is effectively one publication run for the key RTT table. T58 has only 3 runs (and itself says "NOT YET"). Summaries-only (p50), no raw distributions, no CIs, no p99/p999 in the headline.
Action: paired repeated runs, confidence intervals, raw-sample archives, p99/p999, randomized/counterbalanced stack order.

### I4 — fixed stack order aliases with time-varying EC2/ENA behavior
Suite runs dpdk→linux→fstack in fixed order; T58 shows hours-scale time sensitivity.
Action: randomize stack order per repetition OR round-robin blocks (dpdk/linux/fstack repeated within a short window).

### I5 — "pure stack overhead" overstated
linux uses blocking `TcpStream::write_all/read_exact`; dpdk_net uses `poll_once()` + Readable drain; fstack uses nonblocking `ff_write/ff_read` loops. Stack + harness-API-choice, not isolated TCP delta.
Action: reword to "end-to-end userspace benchmark harness comparison" unless the API model is normalized.

## MINOR

### M1 — stale docs
- `fast-iter-suite.sh:23-30` header still describes linux as local-loopback (current behavior prefers host-netns real peer).
- `linux-nat-investigation-2026-05-12.md:120-135` records a loopback decision the suite no longer follows.
- `verify-rack-tlp.md:38-54` has old iter counts (current in `verify-rack-tlp.sh:273-279`).
Action: update before publication — stale methodology text is review bait.

## NON-ISSUES (investigated, not problems)

- N1: peer still sets TCP_NODELAY; pthread-per-conn amortized for bench-rtt's persistent conn — don't blame thread creation for per-iter RTT without new evidence.
- N2: `5fecb92` keeps per-poll scratch clearing; for 1-segment req/resp it's a tiny dpdk_net-only bookkeeping change — not a plausible sole cause of the 2-3× cross-stack shift.

## Shortest path to publication-ready

1. Reproduce-or-eliminate the T57→T58 regression with back-to-back paired runs (B1).
2. Fix the Linux two-ENI confound (rebind same NIC) OR disclose it as an explicit limitation (B2).
3. Demote-or-repair the RACK validation claim (B3).
4. Rename dpdk TX metrics as PMD-handoff, not wire-completion (I2).
5. Add raw-sample emission + repeated paired runs + CIs + p99/p999 + randomized stack order (I1, I3, I4).
6. Reword "pure stack overhead" → "end-to-end harness comparison" (I5).
7. Refresh stale docs (M1).
