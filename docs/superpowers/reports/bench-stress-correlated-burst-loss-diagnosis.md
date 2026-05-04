# bench-stress correlated_burst_loss_1pct — diagnosis (issue #1)

**Symptom (observed across 5 bench-pair runs):** `Error: scenario correlated_burst_loss_1pct counter check failed: tcp.tx_retrans: expected delta > 0, got 0` — even after relaxing the original `tx_rto > 0` to `tx_retrans > 0`, both remain 0 throughout the scenario.

## Root-cause hypothesis (workload-side, not a stack bug)

bench-stress uses a **request-response RTT workload** (same shape as bench-e2e, 128 B request → 128 B response). The asymmetry of where retransmits happen in this workload:

1. **DUT TX (request side)** — DUT sends a 128 B request. Fits in one segment. Peer's TCP stack ACKs immediately. The peer-side ACK arrives BEFORE echo-server even reads the request. **DUT side rarely needs to retransmit** because the request is small + ACKed almost instantly.

2. **DUT RX (response side)** — peer sends 128 B echo response. If netem drops this packet, **the peer's TCP stack handles the retransmit**, not the DUT.

3. **netem applied to peer's `ens6`** (per the script's `tc qdisc` orchestration) drops packets bidirectionally. Both directions of drops surface as **peer-side retransmits**, not DUT-side.

So `tcp.tx_retrans` (a DUT-side counter) stays at 0 because retransmits ARE happening — just on the other end of the connection that the assertion isn't observing.

## What the assertion SHOULD check (recommended fix)

A correct loss-recovery exercise check on the DUT for this workload shape would observe:

- `rx_out_of_order` — DUT's reassembly buffer sees gaps when peer's response stream drops segments
- `rx_drop_csum_bad` — if netem corrupts (it doesn't here, only drops/reorders/duplicates)
- `tcp_dup_acks_sent` — DUT sends duplicate ACKs when it gets out-of-order RX
- `time_to_full_recovery_ns` — measure from first hole to filled

OR: change the workload to a **bulk-send** shape (DUT writes N MB to peer; if netem drops outbound, DUT retransmits) so `tx_retrans` legitimately fires.

## Why this isn't a ship-gate

(Per codex:rescue's review): the netem scenario currently proves only that the peer's TCP stack handles loss correctly in the face of netem, which is uninteresting (Linux kernel's loss recovery is well-tested). The DUT-side proof we want — "dpdk_net engine recovers correctly under loss" — needs:

1. A bulk-send workload OR
2. Asserting on RX-side reorder/drop counters

Either is a moderate refactor of `bench-stress` scenarios. Filed for future Layer-H work (`docs/superpowers/specs/2026-04-29-stage1-phase-a10-5-layer-h-correctness.md` already plans this for the netem-correctness phase).

## Decision for now

- **Keep the assertion relaxed** to `tx_retrans > 0` (commit `5142f71`)
- **Accept the persistent failure** as a known workload-mismatch issue
- **Don't change the script's tolerance** — bench-nightly already skips this scenario gracefully without aborting
- **Plan A10.5 Layer-H phase** to land workload-correct scenarios + assertions

## Filed under

Issue #1 of the perf-a10 follow-on. **Status: deferred to A10.5** with diagnosis documented. Not blocking.
