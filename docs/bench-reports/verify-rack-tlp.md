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

1. Loops over the calibrated scenario set (6 cells; see table below).
2. Applies the per-scenario netem setup on the peer. Most scenarios use
   classic peer-egress netem (`tc qdisc add dev <PEER_NIC> root netem
   ...`) — peer→DUT loss makes the DUT's outgoing data go unacknowledged,
   which is what triggers RTO + TLP from the DUT's transmit-side recovery
   state machines. The `rack_reorder_4k` scenario uses a different setup
   class (peer-INGRESS reorder via `ifb` + `tcp_sack=1` flipped on) —
   that is the only configuration that fires RACK; see the §"Per-scenario
   assertions" header below for the codex B3 repair rationale.
3. Runs `bench-rtt` against the peer's echo-server with the new
   `--counters-csv` flag, sweeping a single payload (size varies per
   scenario — most run at 128 B, `rack_reorder_4k` overrides to 4096 B
   for multi-segment writes; see `SCENARIO_PAYLOAD_BYTES` in the script).
4. Parses the per-scenario counters CSV (columns: `name,pre,post,delta`)
   and asserts both the ALL-of (`REQUIRED_NONZERO`) and the ANY-of
   (`REQUIRED_NONZERO_ANY`) constraints.
5. Reports `PASS` / `FAIL` per scenario plus an overall verdict.

## Per-scenario assertions (assertion-set v3, 2026-05-13)

Codex's 2026-05-13 adversarial review (BLOCKER B3) flagged the prior v2
assertion-set as a vacuous pass for the RACK claim: the low-loss ANY-of
`rack | tlp` always passed via TLP because RACK never fired on the
fast-iter AWS ENA setup, so "RACK validated" was unsupported. Root
cause: (a) the peer AMI sets `net.ipv4.tcp_sack=0` for HFT latency
tuning, so the peer never emits SACK blocks; (b) bench-rtt at 128 B
payload sends one segment per RPC iter, so even with SACK enabled the
DUT never has a "later in-flight segment" to satisfy RFC 8985 §6.2's
detect-lost rule. The v3 assertion-set repairs the gap with a dedicated
`rack_reorder_4k` scenario (ifb ingress reorder on the peer + tcp_sack
temporarily flipped on + 4 KB multi-segment payload), and demotes the
low-loss ANY-of to TLP-only (RACK is no longer claimed in those rows).

| Scenario             | netem spec          | iters | ALL must be > 0                              | ANY must be > 0      |
|----------------------|---------------------|------:|----------------------------------------------|----------------------|
| `low_loss_05pct`     | `loss 0.5%`         |  100k | `tcp.tx_retrans`                             | `tcp.tx_retrans_tlp` |
| `low_loss_1pct`      | `loss 1%`           |  100k | `tcp.tx_retrans`                             | `tcp.tx_retrans_tlp` |
| `high_loss_3pct`     | `loss 3% delay 5ms` |   20k | `tcp.tx_retrans_rto` `tcp.tx_retrans_tlp`    | —                    |
| `symmetric_3pct`     | `loss 3%`           |   20k | `tcp.tx_retrans_rto` `tcp.tx_retrans_tlp`    | —                    |
| `high_loss_5pct`     | `loss 5% 25%`       |   15k | `tcp.tx_retrans_rto`                         | —                    |
| `rack_reorder_4k`    | (ifb-ingress reorder, see below) | 3k | `tcp.tx_retrans_rack`            | —                    |

The first five scenarios apply netem on the peer's NIC root qdisc
(egress direction — peer→DUT). They exercise RTO and TLP under real
packet loss. `rack_reorder_4k` is a different setup class: it loads
`ifb`, redirects peer ingress through `ifb0`, applies a reorder spec
there (default `delay 5ms reorder 50% gap 3`), and runs bench-rtt at
4 KB payload (multi-segment, ~3 segments at 1448-B MSS). Peer's
`tcp_sack` is flipped to 1 for the duration of the scenario and
restored to the saved value at teardown. The combination produces a
sustained out-of-order DUT→peer arrival stream, peer SACK blocks back
to the DUT, and the DUT's RACK rule fires reliably (2026-05-13 three
back-to-back runs: 1965 / 1876 / 1802 RACK retrans events per 3 k-iter
run, vs. 0 RACK and zero other retrans counts under any peer-egress
loss spec).

The iter counts are tuned for **fast-iter wallclock** (~15 min for the
full 6-scenario suite on AWS c6a fast-iter hardware). High-loss cells
use deliberately small iter counts because the assertion is `> 0`:
once 1k+ recovery events fire, the assertion is saturated, and each
additional lost iter adds ~200 ms (one RTO floor) to scenario
wallclock. Low-loss cells use 100 k iters because the recovery
events are rare enough that smaller iter counts risk a false-negative
ANY-of failure (T55's `low_loss_1pct_corr` precursor at 200 k iters
under the now-retired `loss 1% 25%` spec produced exactly 1 TLP event
across the run, and the 2026-05-12 T56 v4 fast-iter run produced 0
across the same iter count — that flake is what motivated dropping
the 25% correlation; uniform `loss 1%` now saturates the ANY-of
assertion with thousands of recovery events). `rack_reorder_4k` uses
3 k iters because each iter takes ~5 ms wall (limited by the netem
delay), so the scenario completes in ~30 s with ~1800-2000 RACK
retrans events — well above the >0 assertion floor.

Operators wanting **nightly-grade statistical depth** on a physical-lab
DUT can override every scenario via the `FORCE_ITERS` env var
(`FORCE_ITERS=1000000 ./scripts/verify-rack-tlp.sh`). The fallback
`ITERS` env var is kept for scenarios not present in the
`SCENARIO_ITERS` map; it does NOT globally override.

Theoretical rationale (per RFC 8985 §6 and the historical
`bench-stress::scenarios.rs:67-83` design block preserved in commit
`fa25bfd`):

- **`low_loss_05pct`** — 0.5% random loss with no induced delay. ACK
  density is high enough that the RPC tail-loss probability is
  saturated; TLP fires on lost-tail iters. RTO does not fire at this
  loss level on a 100 k-iter run. RACK is no longer claimed here
  (see header) — the dedicated `rack_reorder_4k` scenario carries
  that assertion.
- **`low_loss_1pct`** — 1% random loss with no correlation (uniform
  per-packet drop). A single lost peer→DUT ACK or response is
  recovered by TLP (RPC tail). Aggregate must be non-zero; ANY-of
  (`tlp`) confirms the low-loss path took TLP recovery rather than a
  fallback RTO. The earlier `loss 1% 25%` (correlated) spec was
  retired 2026-05-12 because the burst-cluster drop algorithm
  sometimes left 200 k iters with 0 recovery events (T56 v4 fast-iter
  run); the uniform spec yields thousands of recoveries across the
  same iter count.
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
- **`rack_reorder_4k`** — peer ingress reorder via `ifb` redirect
  (default spec `delay 5ms reorder 50% gap 3`), peer `tcp_sack`
  temporarily flipped to 1 for the run, 4 KB multi-segment payload.
  This is the only setup on the fast-iter peer that fires RACK,
  because:
  1. Peer must emit SACK blocks for the out-of-order DUT segments —
     requires `tcp_sack=1` (peer AMI defaults to 0 for HFT latency
     tuning; see `/etc/sysctl.d/99-hft-latency.conf` on the fast-iter
     peer image).
  2. DUT must have multiple in-flight segments per RPC iter — the
     default 128 B payload sends one segment per iter, leaving RACK
     with no "later in-flight segment" to compare against (RFC 8985
     §6.2). 4 KB = ~3 segments at MSS 1448.
  3. Reorder direction must be DUT→peer (peer-INGRESS), not
     peer-egress — peer-egress reorder only shuffles ACKs and the
     response data stream, never causing the peer to receive DUT
     data out of order (which is what produces SACK misorder).
  ALL-of pins {RACK}. Empirical (2026-05-13 three back-to-back runs):
  ~1800–2000 RACK retrans events per 3 k-iter run.

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

Default per-scenario wallclock on the AWS c6a fast-iter DUT:

| scenario             | iters | expected wallclock |
|----------------------|------:|--------------------|
| `low_loss_05pct`     |  100k | ~3-4 min  |
| `low_loss_1pct`      |  100k | ~3-4 min  |
| `high_loss_3pct`     |   20k | ~1-2 min  |
| `symmetric_3pct`     |   20k | ~1-2 min  |
| `high_loss_5pct`     |   15k | ~1 min    |
| `rack_reorder_4k`    |    3k | ~30 s     |
| **Total**            |       | **≤15 min** |

Low-loss wallclock is dominated by the bench-rtt's request/response
cycle (~75 µs/iter base RTT); high-loss wallclock is dominated by the
200 ms RTO floor (each lost iter ≈ +200 ms). On a physical-lab DUT
with line-rate links, each row is roughly 2× faster.

To override iter counts for plumbing checks (everything at e.g. 1000
iters): set `FORCE_ITERS=1000`. The `ITERS` env var is only the
fallback for scenarios missing from `SCENARIO_ITERS`; it does NOT
globally override.

## Interpreting failures

The script prints a per-scenario summary table:

```
scenario             spec                 all>0                            any>0                         result
low_loss_05pct       loss 0.5%            tcp.tx_retrans                   tcp.tx_retrans_tlp            PASS rto=0 rack=0 tlp=27 agg=27
low_loss_1pct        loss 1%              tcp.tx_retrans                   tcp.tx_retrans_tlp            PASS rto=0 rack=0 tlp=131 agg=131
high_loss_3pct       loss 3% delay 5ms    tcp.tx_retrans_rto tcp.tx_retrans_tlp (none)                   PASS rto=8119 rack=0 tlp=1299 agg=9418
symmetric_3pct       loss 3%              tcp.tx_retrans_rto tcp.tx_retrans_tlp (none)                   PASS rto=121 rack=0 tlp=24 agg=145
high_loss_5pct       loss 5% 25%          tcp.tx_retrans_rto                (none)                       FAIL ALL-of want>0 got: tcp.tx_retrans_rto=0 | rto=0 rack=0 tlp=0 agg=0
rack_reorder_4k      ifb-ingress reorder  tcp.tx_retrans_rack               (none)                       PASS rto=3 rack=1965 tlp=9 agg=1977
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
- **ANY-of all-zero in `low_loss_05pct` / `low_loss_1pct`**
  (tlp=0 and agg > 0) — the low-loss scenario fell through to RTO
  recovery instead of TLP. Inspect `tcp.tx_tlp` (probe-fire counter)
  to distinguish "TLP armed but didn't bump retrans" from "TLP
  never armed at all".
- **ALL-of `tcp.tx_retrans_rack=0` in `rack_reorder_4k`** — RACK
  didn't fire even under the dedicated reorder scenario. Check:
  1. Did `setup_ifb_reorder` complete? Inspect the log for "saving
     peer tcp_sack baseline" + "loading ifb on peer". If
     `setup_ifb_reorder` failed silently the scenario marks
     `FAIL apply-ifb-reorder` instead.
  2. Is the peer's `tcp_sack` actually 1 during the run? The
     teardown restores the baseline; to inspect mid-run, set
     `RACK_REORDER_SPEC=delay 1s` and ssh in mid-bench to check
     `cat /proc/sys/net/ipv4/tcp_sack`.
  3. Is the DUT actually seeing SACK blocks? Check
     `tcp.rx_sack_blocks` in the counters CSV — if it's zero, peer
     isn't emitting SACK (either tcp_sack didn't flip, or the conn
     was negotiated before the flip). A non-zero rx_sack_blocks
     with tx_retrans_rack=0 would point at the
     `crates/dpdk-net-core/src/tcp_rack.rs` detect-lost rule
     (specifically the `entry_xmit_ts_ns < self.xmit_ts_ns` check
     and the `reo_wnd_us` window).
  4. Is `RACK_REORDER_PAYLOAD_BYTES` actually > 1 MSS at runtime?
     The MSS negotiated for the conn is in
     `crates/dpdk-net-core/src/tcp_options.rs`; on AWS ENA the
     baseline is 1448. 4096 > 1448 → ~3 segments. If a future
     change drops the default below MSS the scenario would no
     longer have multi-segment writes and RACK could never fire.
- **`bench-rtt exit=2`** with no counters CSV — bench-rtt failed
  precondition strict mode. Re-run with `PRECONDITION_MODE=lenient`.
- **`apply-netem` failure** — peer's `tc qdisc add` returned non-zero;
  inspect the log file (`/tmp/verify-rack-tlp/verify-rack-tlp.log` by
  default) for the remote stderr. The script's pre-flight clears any
  stale qdisc, so EEXIST should not be the cause; more likely peer's
  ssh user lost `sudo` rights or the data NIC name (`PEER_NIC`,
  defaults to `ens5` — fast-iter peer AMI) is wrong.
- **`apply-ifb-reorder` failure** — peer's `modprobe ifb` or
  `tc qdisc add dev ifb0 root netem ...` returned non-zero. Inspect
  the log for the failed step. Common causes: peer kernel
  module-loading disabled (rare on standard Ubuntu AWS AMIs), or a
  prior interrupted run left ifb in an unclean state (the script's
  setup_ifb_reorder is idempotent — it explicitly `tc qdisc del`s
  before `add`s — so this is mostly a defensive-programming note).
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
