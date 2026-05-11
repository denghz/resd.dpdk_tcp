# verify-rack-tlp.sh — Phase 11 RTO/RACK/TLP counter-split verification

`scripts/verify-rack-tlp.sh` is the operator-runnable harness that closes
T51 deferred-work item 3: it confirms that the per-trigger retransmit
counters introduced in Phase 11 (`tcp.tx_retrans_rto` /
`tcp.tx_retrans_rack` / `tcp.tx_retrans_tlp`) actually fire under the
high-loss netem scenarios added in Phase 10.

It does NOT auto-run as part of any nightly. The script needs a real
DUT+peer cluster (DPDK-bound NIC on the DUT, kernel-bound NIC on the
peer with the echo-server up), so it sits at the operator level, runs on
demand, and reports PASS/FAIL.

## What it verifies

Phase 11 split the aggregate `tcp.tx_retrans` counter into three
trigger-specific sub-counters that each also bump the aggregate. The
split is wired in `crates/dpdk-net-core/src/counters.rs`; what it does
NOT prove on its own is that real packet loss actually drives all three
trigger paths during a real run.

This script does:

1. Loops over the three high-loss scenarios.
2. Applies `netem` on the peer (egress only — biases peer→DUT loss; the
   DUT's outgoing data goes unacknowledged, which is what triggers RTO,
   RACK, and TLP from the DUT's transmit-side recovery state machines).
3. Runs `bench-rtt` against the peer's echo-server with the new
   `--counters-csv` flag, sweeping a single 128 B payload at 200 000
   iterations (matches Phase 10's `SCENARIO_ITERS` map for the slower
   high-loss cells in `scripts/bench-nightly.sh`).
4. Parses the per-scenario counters CSV (columns: `name,pre,post,delta`)
   and asserts the expected counter has a non-zero delta.
5. Reports `PASS` / `FAIL` per scenario plus an overall verdict.

## Per-scenario assertions

| Scenario          | netem spec        | Required `delta > 0`                          |
|-------------------|-------------------|-----------------------------------------------|
| `high_loss_3pct`  | `loss 3% delay 5ms` | `tcp.tx_retrans_rto`, `tcp.tx_retrans_rack` |
| `high_loss_5pct`  | `loss 5% 25%`     | `tcp.tx_retrans_rto`                          |
| `symmetric_3pct`  | `loss 3%`         | `tcp.tx_retrans_rack`, `tcp.tx_retrans_tlp`   |

Rationale per the design block in T51 deferred-work item 3:

- **`high_loss_3pct`** mixes 3 % loss with 5 ms delay. Most losses are
  recoverable via RACK fast-retransmit (one RTT after the first ACK
  gap). Long-tail loss clusters that exhaust the SACK reorder window
  push past the 200 ms RTO floor — both must fire.
- **`high_loss_5pct`** is correlated bursts (25 %), which cluster
  back-to-back drops. A single burst can take down `>cwnd` packets,
  leaving no incoming ACKs to drive RACK's reordering window. RTO is
  the only available recovery — it must fire.
- **`symmetric_3pct`** is plain 3 % loss. RACK fires on every
  recoverable loss; tail-loss probes (TLP) fire after the PTO when the
  send queue is drained but unacked data remains in flight. Both must
  fire.

The aggregate `tcp.tx_retrans` is also reported in the summary so a
reviewer can spot "no retransmits at all" (unlikely under 3 %+ loss but
diagnostic when chasing a plumbing regression).

## Counter-snapshot mechanism

The script leverages a new `--counters-csv <PATH>` flag added to
`bench-rtt`. When set, `bench-rtt` snapshots every name in
`dpdk_net_core::counters::ALL_COUNTER_NAMES`:

- **Pre**: immediately after the engine boots, before the first
  connection opens.
- **Post**: after every payload bucket's measurement loop completes,
  before any post-run A-HW Task 18 assertion.

Both snapshots use `dpdk_net_core::counters::lookup_counter`, the same
master resolver consumed by `tools/layer-h-correctness/src/counters_snapshot.rs`,
so any counter visible to layer-h is also visible to this verifier. The
sidecar CSV columns are `name,pre,post,delta` (one row per counter,
order matches `ALL_COUNTER_NAMES`).

The flag is dpdk_net-only — `linux_kernel` and `fstack` paths do not
run the dpdk-net-core engine and silently emit no counter snapshot.

## How to run

Pre-condition: a fast-iter peer is up.

```bash
./scripts/fast-iter-setup.sh up           # provision peer (~1-2 min)
source ./.fast-iter.env                   # exports PEER_IP / PEER_SSH / PEER_ECHO_PORT
./scripts/verify-rack-tlp.sh
./scripts/fast-iter-setup.sh down         # tear down (~30 s)
```

Default per-scenario wallclock is roughly the bench-rtt 200 k-iter run
under each netem profile (~10-20 s each plus engine boot/teardown).
Override via `ITERS=...`, `WARMUP=...`, `PAYLOAD_BYTES=...` to shrink
the cycle for plumbing checks.

## Interpreting failures

The script prints a per-scenario summary table:

```
scenario             spec            expected>0                     result
high_loss_3pct       loss 3% delay 5ms tcp.tx_retrans_rto tcp.tx_retrans_rack PASS rto=42 rack=189 tlp=0 agg=231
high_loss_5pct       loss 5% 25%     tcp.tx_retrans_rto             FAIL want>0 got: tcp.tx_retrans_rto=0 | rto=0 rack=812 tlp=23 agg=835
symmetric_3pct       loss 3%          tcp.tx_retrans_rack tcp.tx_retrans_tlp PASS rto=0 rack=420 tlp=12 agg=432
```

Common FAIL modes and what they mean:

- **`rto=0` in `high_loss_5pct`** — either RACK absorbed every burst
  before the RTO timer fired (RACK's reorder window may be too lax;
  check `tcp.rack_reo_wnd_override_active`), or the counter wiring
  regressed (look at `crates/dpdk-net-core/src/counters.rs`'s
  `inc_tx_retrans_rto` call sites — was a path that should bump it
  switched to a non-bumping helper?).
- **`rack=0` and `tlp=0` in `symmetric_3pct`** — RACK or TLP isn't
  firing at all, or all losses are converting to RTO (200 ms+ tail).
  Check whether `tcp.tx_rack_loss` is also zero (would point at RACK
  loss-detection plumbing) and whether `tcp.tx_tlp_spurious` is
  populated (would suggest TLP fires but the counter increments via a
  different path).
- **`bench-rtt exit=2`** with no counters CSV — bench-rtt failed
  precondition strict mode. Re-run with `PRECONDITION_MODE=lenient`.
- **`apply-netem` failure** — peer's `tc qdisc add` returned non-zero;
  inspect the log file (`/tmp/verify-rack-tlp/verify-rack-tlp.log` by
  default) for the remote stderr. The script's pre-flight clears any
  stale qdisc, so EEXIST should not be the cause; more likely peer's
  ssh user lost `sudo` rights or the data NIC name (`PEER_NIC`,
  defaults to `ens6`) is wrong.
- **`counters CSV missing`** — bench-rtt aborted before reaching the
  workload (e.g. EAL init error, missing peer route). The summary log
  file has the bench-rtt stderr.

## Re-running after a code change

After modifying anything in `crates/dpdk-net-core/src/tcp/` that
touches retransmit logic:

```bash
cargo build --release --bin bench-rtt
./scripts/verify-rack-tlp.sh
```

Any FAIL row is a plumbing regression that needs root-cause analysis
before merge. The artifacts directory (`/tmp/verify-rack-tlp` by
default) keeps the per-scenario `*-counters.csv` and `*-rtt.csv` so a
follow-up triage can diff against a known-good run captured earlier.
