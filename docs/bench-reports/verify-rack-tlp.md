# verify-rack-tlp.sh — Phase 11 RTO/RACK/TLP counter-split verification

`scripts/verify-rack-tlp.sh` is the operator-runnable harness that closes
T51 deferred-work item 3: it confirms that the per-trigger retransmit
counters introduced in Phase 11 (`tcp.tx_retrans_rto` /
`tcp.tx_retrans_rack` / `tcp.tx_retrans_tlp`) actually fire under a
calibrated set of netem scenarios.

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

1. Loops over the calibrated scenario set (5 cells; see table below).
2. Applies `netem` on the peer (egress only — biases peer→DUT loss; the
   DUT's outgoing data goes unacknowledged, which is what triggers RTO,
   RACK, and TLP from the DUT's transmit-side recovery state machines).
3. Runs `bench-rtt` against the peer's echo-server with the new
   `--counters-csv` flag, sweeping a single 128 B payload at the
   per-scenario iter count (low-loss cells need more iters to
   accumulate statistical lossy events; see `SCENARIO_ITERS` in the
   script).
4. Parses the per-scenario counters CSV (columns: `name,pre,post,delta`)
   and asserts both the ALL-of (`REQUIRED_NONZERO`) and the ANY-of
   (`REQUIRED_NONZERO_ANY`) constraints.
5. Reports `PASS` / `FAIL` per scenario plus an overall verdict.

## Per-scenario assertions (assertion-set v2, 2026-05-12)

The 2026-05-11 live verification showed that at ≥3% loss, RACK never
fires on the dpdk_net stack — the ACK stream becomes too sparse for
RACK's reorder window to catch losses before the 200 ms RTO timer
expires, so RTO absorbs the recovery (with TLP filling the RPC-tail
cases). RACK fast-retransmit therefore needs a *low-loss* scenario
with dense ACKs to exercise. The calibrated assertion map below
matches that empirical reality.

| Scenario             | netem spec          | iters | ALL must be > 0                              | ANY must be > 0                              |
|----------------------|---------------------|------:|----------------------------------------------|----------------------------------------------|
| `low_loss_05pct`     | `loss 0.5%`         |  500k | `tcp.tx_retrans`                             | `tcp.tx_retrans_rack` `tcp.tx_retrans_tlp`   |
| `low_loss_1pct_corr` | `loss 1% 25%`       |  200k | `tcp.tx_retrans`                             | `tcp.tx_retrans_rack` `tcp.tx_retrans_tlp`   |
| `high_loss_3pct`     | `loss 3% delay 5ms` |  200k | `tcp.tx_retrans_rto` `tcp.tx_retrans_tlp`    | —                                            |
| `symmetric_3pct`     | `loss 3%`           |  200k | `tcp.tx_retrans_rto` `tcp.tx_retrans_tlp`    | —                                            |
| `high_loss_5pct`     | `loss 5% 25%`       |  100k | `tcp.tx_retrans_rto`                         | —                                            |

Theoretical rationale (per RFC 8985 §6 and the historical
`bench-stress::scenarios.rs:67-83` design block preserved in commit
`fa25bfd`):

- **`low_loss_05pct`** — 0.5% random loss with no induced delay. ACK
  density is high enough for RACK to detect every recoverable loss
  within its reorder window. The bench-rtt RPC pattern naturally
  drains the send queue after every request, so tail-loss probes
  fire on lost-tail iters. Aggregate must be non-zero; the recovery
  trigger is either RACK or TLP (depends on whether the lost packet
  was a tail or a mid-window segment) — ANY-of pins this. RTO does
  not fire at this loss level on a 500 k-iter run.
- **`low_loss_1pct_corr`** — 1% loss with 25% correlation (clustered
  bursts). Per the `fa25bfd` historical observation, the loss-recovery
  path is dominated by RACK/TLP at this loss level: bursts are short
  enough that TLPs recover them before the RTO timer expires.
  Aggregate must be non-zero; ANY-of (`rack | tlp`) confirms the
  low-loss path took recovery rather than a fallback RTO.
- **`high_loss_3pct`** — 3% loss with 5 ms induced delay. Empirically
  (2026-05-11): RTO 86%, RACK 0%, TLP 14%. The induced delay plus
  high loss drops too many ACKs back-to-back for RACK to fire. ALL-of
  pins both RTO (the dominant trigger) and TLP (the RPC-tail trigger).
- **`symmetric_3pct`** — 3% random loss, no induced delay. Empirically
  (2026-05-11): RTO 83%, RACK 0%, TLP 17%. Same shape as
  `high_loss_3pct` minus the delay — RACK still cannot keep up. ALL-of
  pins {RTO, TLP}.
- **`high_loss_5pct`** — 5% loss with 25% correlation. Correlated
  bursts at 5% take down `>cwnd` packets, leaving no incoming ACKs to
  drive RACK or seed the TLP PTO. RTO is the only available recovery.
  ALL-of pins {RTO}.

The aggregate `tcp.tx_retrans` is also reported in the summary so a
reviewer can spot "no retransmits at all" (unlikely under any of the
configured scenarios but diagnostic when chasing a plumbing regression).

### Assertion semantics

- **`REQUIRED_NONZERO[scenario]`** — every counter in this list MUST
  have `delta > 0` (ALL-of). Used for deterministic triggers like RTO
  under ≥3% loss.
- **`REQUIRED_NONZERO_ANY[scenario]`** — at least ONE counter in this
  list MUST have `delta > 0` (ANY-of). Used when the trigger varies
  with packet timing (e.g. low-loss RACK vs TLP). Omit (or empty) →
  no ANY-of check.

A scenario can fail either or both. The summary line records each
failure shape so the operator can distinguish "no recovery fired at
all" (ALL fails) from "low-loss scenario fell through to RTO" (ANY
fails).

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

Default per-scenario wallclock varies with iter count: low-loss cells
run 500 k iters (~30-50 s each), high-loss cells run 100-200 k iters
(~10-20 s each) plus engine boot/teardown. Override via the env
fallbacks (`ITERS=...`, `WARMUP=...`, `PAYLOAD_BYTES=...`) to shrink
the cycle for plumbing checks; `ITERS` only applies to scenarios
without an entry in the script's `SCENARIO_ITERS` map.

## Interpreting failures

The script prints a per-scenario summary table:

```
scenario             spec                 all>0                            any>0                         result
low_loss_05pct       loss 0.5%            tcp.tx_retrans                   tcp.tx_retrans_rack tcp.tx_retrans_tlp PASS rto=0 rack=18 tlp=27 agg=45
low_loss_1pct_corr   loss 1% 25%          tcp.tx_retrans                   tcp.tx_retrans_rack tcp.tx_retrans_tlp PASS rto=0 rack=84 tlp=131 agg=215
high_loss_3pct       loss 3% delay 5ms    tcp.tx_retrans_rto tcp.tx_retrans_tlp (none)                            PASS rto=8119 rack=0 tlp=1299 agg=9418
symmetric_3pct       loss 3%              tcp.tx_retrans_rto tcp.tx_retrans_tlp (none)                            PASS rto=121 rack=0 tlp=24 agg=145
high_loss_5pct       loss 5% 25%          tcp.tx_retrans_rto                (none)                            FAIL ALL-of want>0 got: tcp.tx_retrans_rto=0 | rto=0 rack=0 tlp=0 agg=0
```

Common FAIL modes and what they mean:

- **ALL-of `tcp.tx_retrans_rto=0` in `high_loss_5pct`** — either
  RACK or TLP absorbed every burst before the RTO timer fired
  (unexpected at 5%/25% — investigate whether the peer-side netem
  spec actually delivered the requested correlation; `tc -s qdisc
  show dev <PEER_NIC>` on the peer post-run shows the drop counters),
  or the counter wiring regressed (look at
  `crates/dpdk-net-core/src/counters.rs`'s `inc_tx_retrans_rto` call
  sites — was a path that should bump it switched to a non-bumping
  helper?).
- **ALL-of `tcp.tx_retrans_tlp=0` in `high_loss_3pct` / `symmetric_3pct`**
  — TLP isn't firing at all even though the RPC workload drains the
  send queue every iter. Check whether `tcp.tx_tlp` (probe-fire
  counter) is also zero (would point at the TLP arm/fire plumbing
  in `on_tlp_fire`) and whether `tcp.tx_tlp_spurious` is populated
  (would suggest TLP fires but the retransmit counter increments via
  a different path).
- **ANY-of all-zero in `low_loss_05pct` / `low_loss_1pct_corr`**
  (rack=0 AND tlp=0, but agg > 0) — the low-loss scenario fell
  through to RTO recovery instead of RACK/TLP. RACK's reorder
  window may have widened (check `tcp.rack_reo_wnd_override_active`)
  or the engine took an RTO before RACK could detect (the min RTO
  floor may have collapsed). Inspect `tcp.tx_rack_loss` for RACK
  detect-but-no-retrans paths.
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
