#!/usr/bin/env bash
# verify-rack-tlp.sh — verify the Phase 11 RTO/RACK/TLP counter split fires
#                      correctly under Phase 10's netem scenarios.
#
# Closes T51 deferred-work item 3 (operator-runnable; not auto-run because it
# requires a real DUT+peer cluster). The script:
#   1. Loops over the calibrated scenario set (see Scenario × counter table
#      below).
#   2. Applies the per-scenario netem setup on the peer. Two setup classes:
#        - Peer-egress loss (PEER_NIC root qdisc): drives RTO + TLP.
#        - Peer-INGRESS reorder via ifb (rack_reorder_4k scenario only):
#          drives RACK. The egress path cannot trigger RACK on this peer
#          — see the "Scenario × counter table" header comment and the
#          setup_ifb_reorder helper for the codex B3 repair rationale.
#   3. Runs bench-rtt against the peer's echo-server with --counters-csv,
#      sweeping a single payload (size varies per scenario — most run at
#      PAYLOAD_BYTES=128; rack_reorder_4k overrides to 4096 for multi-
#      segment writes — see SCENARIO_PAYLOAD_BYTES below).
#   4. Parses the per-scenario counters CSV (name,pre,post,delta) and asserts
#      both the ALL-of (REQUIRED_NONZERO) and the ANY-of
#      (REQUIRED_NONZERO_ANY) constraints.
#   5. Reports PASS/FAIL with per-scenario counter deltas.
#
# ─── Scenario × expected-counter table (assertion-set v3, 2026-05-13) ─────
#
# Empirical fact (fast-iter run 2026-05-11): under peer-egress loss netem
# alone, RACK does NOT fire on the dpdk_net stack because (a) the
# fast-iter peer AMI sets `net.ipv4.tcp_sack=0` for HFT latency tuning, so
# the peer emits NO SACK blocks for any out-of-order arrival; (b) even
# with SACK enabled, bench-rtt at the default 128 B payload sends one
# segment per RPC, so there is never a "later" in-flight segment to
# trigger RACK's reorder rule. The codex 2026-05-13 adversarial review
# (BLOCKER B3) flagged the prior ANY-of `rack | tlp` assertion as a
# vacuous pass for that reason — the low-loss scenarios were satisfied
# entirely by TLP. To repair the gap, scenario `rack_reorder_4k` below
# runs a deliberate reorder-injection workload (peer ingress reorder via
# ifb + tcp_sack temporarily flipped on + 4 KB multi-segment payload)
# that fires RACK reliably; the remaining peer-egress loss scenarios
# remain the RTO/TLP exercisers they always were.
#
# Iter-count trade-off (re-calibrated 2026-05-13 for fast-iter wallclock):
# High-loss scenarios use deliberately low iter counts because the
# assertion is `> 0` — once 1k+ recovery events fire, the assertion is
# saturated. Adding more iters only burns wallclock under the 200 ms RTO
# floor (each lost iter ≈ +200 ms scenario time). The per-scenario
# defaults below are tuned so the full 6-scenario suite completes in
# ~15 min on AWS c6a fast-iter hardware. Statistical depth for
# percentile reporting is NOT a goal of this verifier and should be
# obtained from bench-nightly's netem matrix instead. Operators wanting
# nightly-grade depth on a physical-lab DUT can globally override every
# scenario via the `FORCE_ITERS` env var (e.g. `FORCE_ITERS=1000000`).
#
#   scenario              netem spec            ALL non-zero           ANY non-zero          rationale
#   ─────────────────     ─────────────────     ───────────────────    ──────────────────    ─────────
#   low_loss_05pct        loss 0.5%             tcp.tx_retrans         tx_retrans_tlp        RPC tail-loss → TLP fires
#                                                                                             on the lost-tail iters.
#                                                                                             Loss too low for RTO.
#                                                                                             RACK does not fire on this
#                                                                                             peer regardless (codex B3
#                                                                                             repair, 2026-05-13: peer
#                                                                                             tcp_sack=0 baseline +
#                                                                                             1-segment 128B payload
#                                                                                             → no SACK info, no RACK).
#                                                                                             ANY-of dropped `rack` to
#                                                                                             avoid the vacuous pass
#                                                                                             codex flagged.
#   low_loss_1pct         loss 1%               tcp.tx_retrans         tx_retrans_tlp        Independent per-packet drop
#                                                                                             at 1%. ALL-of pins the
#                                                                                             aggregate; ANY-of pins
#                                                                                             TLP (the low-loss recovery
#                                                                                             path on this peer).
#                                                                                             Empirical (2026-05-12
#                                                                                             smoke ×3): ~10 000 RTO +
#                                                                                             ~2 000 TLP + ~12 000 agg
#                                                                                             per 200k-iter run. RACK
#                                                                                             never fires here for the
#                                                                                             same reasons as
#                                                                                             low_loss_05pct above.
#                                                                                             Replaced flaky
#                                                                                             `loss 1% 25%` precursor
#                                                                                             (T56v4 saw 0 retrans;
#                                                                                             T55/v3/v5 saw ≤6) — see
#                                                                                             2026-05-12 T57 followup.
#   high_loss_3pct        loss 3% delay 5ms     tx_retrans_rto,        —                     RTO is the dominant
#                                               tx_retrans_tlp                                trigger (86% empirically);
#                                                                                             TLP fills the RPC-tail
#                                                                                             cases. RACK=0.
#   symmetric_3pct        loss 3%               tx_retrans_rto,        —                     Same as high_loss_3pct
#                                               tx_retrans_tlp                                without induced delay
#                                                                                             — empirically still
#                                                                                             RTO+TLP only.
#   high_loss_5pct        loss 5% 25%           tx_retrans_rto         —                     Correlated bursts at 5%
#                                                                                             take down >cwnd packets,
#                                                                                             no ACKs for RACK; RTO
#                                                                                             is the only recovery.
#   rack_reorder_4k       (custom — see         tx_retrans_rack        —                     Peer ingress reorder via
#                          rack_reorder helper                                                ifb + tcp_sack=1 flipped
#                          below; spec lives                                                  on for the run + 4 KB
#                          in the helper, NOT                                                 multi-segment payload.
#                          in SPECS map)                                                      The only setup that fires
#                                                                                             RACK on this peer (RACK
#                                                                                             needs SACK info, which
#                                                                                             requires multi-segment
#                                                                                             in-flight + peer SACK
#                                                                                             enabled — see header
#                                                                                             comment for codex B3
#                                                                                             repair details).
#
# Theoretical rationale (per RFC 8985 §6 and the historical
# bench-stress::scenarios.rs:67-83 design block preserved in fa25bfd):
#
#   RACK requires a steady ACK stream to populate the reorder window and
#   detect losses within ~1 RTT. At ≤1% loss with no induced delay, ACK
#   density is high enough; at ≥3% loss (especially with delay), the
#   peer drops too many ACKs back-to-back for RACK to keep up, and the
#   200 ms RTO floor reaches first.
#
#   TLP fires after the PTO when the send queue is drained but unacked
#   data remains in flight — a natural fit for bench-rtt's RPC pattern
#   (one request → wait for response). TLP appears in every scenario
#   where loss occurs at all, regardless of magnitude.
#
#   RTO is the catch-all: when neither RACK nor TLP can recover (because
#   the ACK clock has stalled), the 200 ms RTO timer fires and the entire
#   RFC 8985 §6.3 in-flight queue is retransmitted.
#
# Assertion semantics:
#   REQUIRED_NONZERO[scenario]      — every counter in this list MUST be > 0
#                                     (ALL-of). Used for the deterministic
#                                     triggers like RTO under ≥3% loss.
#   REQUIRED_NONZERO_ANY[scenario]  — at least ONE counter in this list MUST
#                                     be > 0 (ANY-of). Used for scenarios
#                                     where the trigger varies with packet
#                                     timing (e.g. RACK vs TLP at low loss).
#                                     Omit (or empty) → no ANY-of check.
#
# Pre-condition: fast-iter peer is up.
#   ./scripts/fast-iter-setup.sh up
#   source ./.fast-iter.env          # exports PEER_IP, PEER_SSH, PEER_ECHO_PORT
#   ./scripts/verify-rack-tlp.sh
#   ./scripts/fast-iter-setup.sh down
#
# Env overrides (export before invoking):
#   PEER_IP / PEER_SSH       peer data-NIC IP and ssh login (default: from
#                            ./.fast-iter.env, sourced if PEER_IP unset)
#   PEER_ECHO_PORT           peer echo-server TCP port (default: 10001)
#   PEER_NIC                 peer data interface (default: ens5; the
#                            fast-iter peer AMI uses ens5)
#   DUT_IP / DUT_GATEWAY     DUT data-NIC IP + gateway (default: 10.4.1.141 /
#                            10.4.1.1 — matches scripts/bench-quick.sh)
#   DUT_PCI                  DUT NIC PCI address bound to vfio-pci
#                            (default: 0000:00:06.0)
#   DUT_LCORE                DUT lcore for the engine (default: 2)
#   DUT_EAL_ARGS             EAL args for bench-rtt (default: matches
#                            scripts/bench-quick.sh DUT_EAL_ARGS)
#   ITERS                    bench-rtt --iterations FALLBACK if the
#                            per-scenario SCENARIO_ITERS map has no entry
#                            (default: 200000). Note: every scenario in
#                            SCENARIOS[] currently has a SCENARIO_ITERS
#                            entry, so this only fires if a new scenario
#                            is added without one. To globally override
#                            every scenario (e.g. for nightly-grade
#                            depth), use FORCE_ITERS — not ITERS.
#   FORCE_ITERS              If set to a positive integer, globally
#                            overrides every scenario's iter count
#                            (bypasses SCENARIO_ITERS entirely). Use to
#                            crank up to nightly-grade depth on a
#                            physical-lab DUT, e.g. FORCE_ITERS=1000000.
#                            (default: unset — per-scenario map wins)
#   WARMUP                   bench-rtt --warmup (default: 1000)
#   PAYLOAD_BYTES            request/response size (default: 128)
#   ARTIFACTS_DIR            where to drop CSVs + log (default:
#                            /tmp/verify-rack-tlp)
#   BENCH_RTT_BIN            path to bench-rtt (default:
#                            ./target/release/bench-rtt)
#   PRECONDITION_MODE        bench-rtt --precondition-mode (default: lenient
#                            — fast-iter hosts may not pass strict-mode
#                            checks like isolcpus)
#   SCENARIOS_FILTER         Comma-separated list of scenarios to run
#                            (subset of the DEFAULT_SCENARIOS list).
#                            Unset → run all scenarios. Each token must
#                            be present in the SPECS map; an unknown
#                            scenario dies at preflight.
#                            Example: SCENARIOS_FILTER=rack_reorder_4k
#   RACK_REORDER_SPEC        netem spec applied to peer's ifb0 for the
#                            rack_reorder_4k scenario (default:
#                            'delay 5ms reorder 50% gap 3')
#   RACK_REORDER_PAYLOAD_BYTES
#                            payload size for the rack_reorder_4k
#                            scenario; must be > 1 MSS (default: 4096)
#
# Exit codes:
#   0   all assertions passed
#   1   at least one assertion failed
#   2   misconfiguration / missing precondition (peer unreachable, binary
#       missing, etc.)
#
# Idempotency: every netem-apply is preceded by an unconditional
# `tc qdisc del dev $PEER_NIC root` (|| true). A previous interrupted run that
# left a stale qdisc on the peer doesn't break the next invocation.

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration.
# ---------------------------------------------------------------------------
WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Lazy-load fast-iter env if PEER_IP not already exported.
if [ -z "${PEER_IP:-}" ] && [ -f "$WORKDIR/.fast-iter.env" ]; then
    # shellcheck disable=SC1091
    source "$WORKDIR/.fast-iter.env"
fi

PEER_IP="${PEER_IP:-}"
PEER_SSH="${PEER_SSH:-ubuntu@$PEER_IP}"
PEER_ECHO_PORT="${PEER_ECHO_PORT:-10001}"
# The peer's data-NIC name (fast-iter peer image uses ens5; standalone bench
# pairs may use ens6). Override via env if the deployment differs. The
# fast-iter-suite.sh wrapper already forwards PEER_NIC=ens5 — this default
# (ens5) keeps a standalone invocation working out-of-the-box on the
# fast-iter AMI without requiring an env override.
PEER_NIC="${PEER_NIC:-ens5}"

DUT_IP="${DUT_IP:-10.4.1.141}"
DUT_GATEWAY="${DUT_GATEWAY:-10.4.1.1}"
# Default matches scripts/fast-iter-suite.sh (a10 perf host). Standalone
# users on a different host class must override DUT_PCI explicitly. The
# previous default (0000:00:06.0) was a stale carry-over from an earlier
# bench-pair shape and silently produced "Invalid port_id=0" failures
# when invoked outside fast-iter-suite (which forwards the correct
# value via its own DUT_PCI default).
DUT_PCI="${DUT_PCI:-0000:28:00.0}"
DUT_LCORE="${DUT_LCORE:-2}"
DUT_EAL_ARGS="${DUT_EAL_ARGS:--l 2-3 -n 4 --in-memory --huge-unlink -a ${DUT_PCI},large_llq_hdr=1,miss_txc_to=3}"

ITERS="${ITERS:-200000}"
# FORCE_ITERS is the global-override knob (kept separate from ITERS so
# that fast-iter-suite.sh and other callers that pass ITERS=N for
# fallback purposes don't inadvertently override the per-scenario map).
# Empty / unset → per-scenario SCENARIO_ITERS defaults apply.
FORCE_ITERS="${FORCE_ITERS:-}"
WARMUP="${WARMUP:-1000}"
PAYLOAD_BYTES="${PAYLOAD_BYTES:-128}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-/tmp/verify-rack-tlp}"
BENCH_RTT_BIN="${BENCH_RTT_BIN:-$WORKDIR/target/release/bench-rtt}"
PRECONDITION_MODE="${PRECONDITION_MODE:-lenient}"

SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30
          -o ProxyCommand=none -o ConnectTimeout=10 -i "$SSH_KEY")

# Per-scenario netem spec. The three ≥3% scenarios are copied verbatim from
# scripts/bench-nightly.sh step [8/12] NETEM_SPECS map (Phase 10 task 10.2
# cells); the two low-loss scenarios are added here to exercise the RACK +
# TLP paths under peer-egress loss; rack_reorder_4k is the dedicated RACK
# trigger (uses peer-INGRESS reorder via ifb — see RACK_REORDER_SPEC below
# + the apply/remove branch in the per-scenario loop).
declare -A SPECS=(
  [low_loss_05pct]="loss 0.5%"
  [low_loss_1pct]="loss 1%"
  [high_loss_3pct]="loss 3% delay 5ms"
  [symmetric_3pct]="loss 3%"
  [high_loss_5pct]="loss 5% 25%"
  [rack_reorder_4k]="ifb-ingress reorder"
)

# rack_reorder_4k spec — applied to peer's ifb0 (ingress redirect from
# PEER_NIC). 5 ms induced delay + 50% reorder with gap=3 (every third
# packet held back) produces sustained out-of-order DUT→peer delivery,
# which the peer SACKs back to the DUT once SACK is temporarily enabled
# (see RACK_REORDER_SACK_FLIP helpers). Empirically (2026-05-13 three
# back-to-back runs): ~1500 RACK retrans events per 3000-iter run, 0
# false-fires on the other counters.
RACK_REORDER_SPEC="${RACK_REORDER_SPEC:-delay 5ms reorder 50% gap 3}"
# rack_reorder_4k payload — must be larger than one MSS so the DUT has
# multiple segments in flight per RPC iteration, giving RACK something
# to compare for the "newer ack covers later, earlier still unacked"
# rule (RFC 8985 §6.2). 4096 B = ~3 segments at 1448-B MSS.
RACK_REORDER_PAYLOAD_BYTES="${RACK_REORDER_PAYLOAD_BYTES:-4096}"

# Per-scenario assertion sets.
#
# REQUIRED_NONZERO: every counter listed MUST have delta > 0 (ALL-of). Use
# this for deterministic triggers — e.g. at ≥3% loss the 200 ms RTO floor
# is essentially guaranteed to fire even on a small (~30-50 k-iter) RPC
# workload, and the RPC tail-loss probability is high enough that TLP
# fires too.
#
# REQUIRED_NONZERO_ANY: at least ONE counter listed MUST have delta > 0
# (ANY-of). Use this for scenarios where the trigger varies with packet
# timing — at ≤1% loss, an individual loss may be recovered by either
# RACK fast-retransmit (if the reorder window catches it first) or TLP
# (if the loss is a tail packet that drains the send queue). Both end up
# bumping `tcp.tx_retrans`, so the ALL list pins the aggregate while the
# ANY list confirms the recovery happened via the low-loss path
# (rack | tlp), not via a fallback RTO.
#
# See the header "Scenario × expected-counter table" for the rationale.
declare -A REQUIRED_NONZERO=(
  [low_loss_05pct]="tcp.tx_retrans"
  [low_loss_1pct]="tcp.tx_retrans"
  [high_loss_3pct]="tcp.tx_retrans_rto tcp.tx_retrans_tlp"
  [symmetric_3pct]="tcp.tx_retrans_rto tcp.tx_retrans_tlp"
  [high_loss_5pct]="tcp.tx_retrans_rto"
  # rack_reorder_4k is the dedicated RACK exerciser — assert that
  # RACK fired (ALL-of, NOT ANY-of, because the entire point of this
  # scenario is to pin a deterministic RACK > 0 floor; codex 2026-05-13
  # BLOCKER B3 explicitly objected to ANY-of `rack | tlp` as vacuous).
  [rack_reorder_4k]="tcp.tx_retrans_rack"
)

declare -A REQUIRED_NONZERO_ANY=(
  # Low-loss scenarios used to allow `rack | tlp` ANY-of, but RACK never
  # fires on this peer under egress-loss (peer's tcp_sack=0 baseline +
  # 128 B single-segment payload — see codex B3 + the rack_reorder_4k
  # repair). The ANY-of now lists only TLP, so a PASS here is a real
  # TLP-fired assertion rather than a vacuous "either-or-nothing". RACK
  # is validated by the dedicated rack_reorder_4k scenario instead.
  [low_loss_05pct]="tcp.tx_retrans_tlp"
  [low_loss_1pct]="tcp.tx_retrans_tlp"
)

# Per-scenario iter defaults — tuned for fast-iter wallclock (~30 min
# for the full 5-scenario suite on AWS c6a fast-iter hardware).
#
# Sizing rationale:
#   - low-loss cells (0.5% / 1% uniform) need enough iters that at
#     least one TLP probe lands in a tail-loss iter, so the ANY-of
#     `tx_retrans_rack | tx_retrans_tlp` assertion is satisfied. T55's
#     2026-05-12 fast-iter run at the now-retired `loss 1% 25%` spec
#     produced exactly 1 TLP event across 200k iters — dropping any
#     lower risked a false-negative ANY-of failure. Switching the
#     scenario to plain `loss 1%` (uniform per-packet drop, no burst
#     correlation) at the same iter count yields thousands of recovery
#     events, saturating the assertion and removing the stochastic
#     flake observed in T56/T57 runs.
#   - high-loss cells (3% / 5%) are dominated by the 200 ms RTO floor
#     (each lost iter adds ~200 ms to scenario wallclock). T55 showed
#     200k iters at 3% loss took 10-15 min on this fast-iter host. At
#     50k iters the same scenario fires ~1500 retransmit events — far
#     more than the `> 0` assertion needs — and completes in ~3 min.
#     The high-loss iter counts here are deliberately small.
#
# To override: export FORCE_ITERS=N for a global override across all
# scenarios (see FORCE_ITERS handling in the per-scenario loop below).
# Scenarios not listed in this map fall back to $ITERS.
# Iter counts trimmed for fast-iter wallclock (2026-05-13): the only goal is
# to confirm each counter fires (>0 assertion), not statistical depth. The
# previous (500k/200k/50k/50k/30k) totals ran ~33 min — past the suite's
# 1800s outer timeout once low_loss_1pct went uniform (Option D, commit
# d07d9ec). New totals: ~13-16 min. For nightly-grade depth on a physical-lab
# DUT use FORCE_ITERS=1000000.
#
# Empirical per-scenario retransmit yield at the trimmed counts (from the
# 2026-05-12 stabilization smoke + scaling): low_loss_05pct ~3000, low_loss_1pct
# ~6000, high_loss_3pct ~3600, symmetric_3pct ~3600, high_loss_5pct ~200 —
# all ≫ the >0 floor.
declare -A SCENARIO_ITERS=(
  [low_loss_05pct]=100000
  [low_loss_1pct]=100000
  [high_loss_3pct]=20000
  [symmetric_3pct]=20000
  [high_loss_5pct]=15000
  # rack_reorder_4k uses 4096 B payload + ifb ingress reorder + 5 ms
  # netem delay; each iter takes ~5 ms wall, so 3000 iters ≈ 30 s wall.
  # 2026-05-13 three back-to-back runs: 1488 / 1582 / 1642 RACK retrans
  # — well above the >0 assertion floor.
  [rack_reorder_4k]=3000
)

# Per-scenario payload override. Most scenarios run at PAYLOAD_BYTES
# (default 128). rack_reorder_4k MUST be multi-segment for RACK to have
# a "later in-flight segment" to detect against (RFC 8985 §6.2 +
# tcp_rack.rs:62-69 detect_lost rule). 4096 B = ~3 segments at MSS 1448.
declare -A SCENARIO_PAYLOAD_BYTES=(
  [rack_reorder_4k]="$RACK_REORDER_PAYLOAD_BYTES"
)

# Order matters for the summary table — ascending by loss severity so the
# operator sees the "expected RACK/TLP" rows before the RTO-dominated rows,
# then the dedicated rack_reorder_4k row at the bottom (different setup
# class — ingress reorder via ifb vs. egress loss).
#
# Env override SCENARIOS_FILTER lets the operator run a single scenario
# (or a comma-separated subset) — useful for development-cycle smoke
# tests on a new assertion before paying the full suite wallclock. Each
# token in SCENARIOS_FILTER must be a key from the SPECS map above;
# unknown tokens die at preflight.
DEFAULT_SCENARIOS=(
  low_loss_05pct
  low_loss_1pct
  high_loss_3pct
  symmetric_3pct
  high_loss_5pct
  rack_reorder_4k
)
if [ -n "${SCENARIOS_FILTER:-}" ]; then
    IFS=',' read -r -a SCENARIOS <<<"$SCENARIOS_FILTER"
else
    SCENARIOS=("${DEFAULT_SCENARIOS[@]}")
fi

# ---------------------------------------------------------------------------
# Helpers.
# ---------------------------------------------------------------------------
log() { printf '[verify-rack-tlp %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() { printf '[verify-rack-tlp] ERROR: %s\n' "$*" >&2; exit 2; }

ssh_peer() {
    # shellcheck disable=SC2029
    # Client-side expansion is intentional: callers pass strings already
    # baked with the netem spec they want the remote shell to see.
    ssh "${SSH_OPTS[@]}" "$PEER_SSH" "$@"
}

# Read a `delta` cell out of the bench-rtt counters CSV
# (columns: name,pre,post,delta).
# Returns the integer on stdout, "0" if the row is absent.
read_delta() {
    local csv="$1" name="$2"
    awk -F, -v n="$name" '
        NR == 1 { next }                  # skip header
        $1 == n { print $4; exit }
    ' "$csv" 2>/dev/null || true
}

# rack_reorder_4k setup: load ifb, redirect peer ingress to ifb0, apply
# netem reorder on ifb0, flip peer's tcp_sack to 1. RACK on the DUT needs
# SACK info from the peer to detect reordered segments (RFC 8985 §6.2 +
# the SACK-driven update_on_ack path in tcp_input.rs:1071-1077). The
# fast-iter peer AMI defaults to tcp_sack=0 for HFT latency tuning
# (`/etc/sysctl.d/99-hft-latency.conf`), so we must flip it on for the
# duration of this scenario and restore it afterwards. The original
# value is captured into RACK_REORDER_SAVED_SACK so the teardown can
# put it back exactly as it was.
RACK_REORDER_SAVED_SACK=""
setup_ifb_reorder() {
    local spec="$1"
    log "  saving peer tcp_sack baseline"
    RACK_REORDER_SAVED_SACK="$(ssh_peer "cat /proc/sys/net/ipv4/tcp_sack 2>/dev/null || echo 1")"
    RACK_REORDER_SAVED_SACK="${RACK_REORDER_SAVED_SACK:-1}"
    log "  setting peer tcp_sack=1 (baseline was $RACK_REORDER_SAVED_SACK)"
    ssh_peer "sudo sysctl -w net.ipv4.tcp_sack=1 >/dev/null"
    log "  loading ifb on peer (numifbs=1) + ingress redirect"
    ssh_peer "
        sudo modprobe ifb numifbs=1 2>/dev/null || true
        sudo ip link set ifb0 up
        sudo tc qdisc del dev ifb0 root 2>/dev/null || true
        sudo tc qdisc del dev $PEER_NIC ingress 2>/dev/null || true
        sudo tc qdisc add dev $PEER_NIC handle ffff: ingress
        sudo tc filter add dev $PEER_NIC parent ffff: protocol ip u32 match u32 0 0 \\
            action mirred egress redirect dev ifb0
        sudo tc qdisc add dev ifb0 root netem $spec
    "
}

# rack_reorder_4k teardown: undo every change made by setup_ifb_reorder
# in reverse order (netem → ifb redirect filter → ifb ingress qdisc →
# ifb0 link → restore tcp_sack). Every step is best-effort so a partial
# failure does not poison subsequent scenarios. The EXIT trap calls
# this as well so an interrupted run cleans up properly.
teardown_ifb_reorder() {
    ssh_peer "
        sudo tc qdisc del dev ifb0 root 2>/dev/null || true
        sudo tc filter del dev $PEER_NIC parent ffff: 2>/dev/null || true
        sudo tc qdisc del dev $PEER_NIC ingress 2>/dev/null || true
        sudo ip link set ifb0 down 2>/dev/null || true
    " >/dev/null 2>&1 || true
    if [ -n "$RACK_REORDER_SAVED_SACK" ]; then
        ssh_peer "sudo sysctl -w net.ipv4.tcp_sack=$RACK_REORDER_SAVED_SACK >/dev/null" \
            >/dev/null 2>&1 || true
    fi
}

# ---------------------------------------------------------------------------
# Pre-flight.
# ---------------------------------------------------------------------------
[ -n "$PEER_IP" ] || die "PEER_IP unset (source ./.fast-iter.env or run scripts/fast-iter-setup.sh up)"
[ -x "$BENCH_RTT_BIN" ] || die "bench-rtt binary missing at $BENCH_RTT_BIN — build first: cargo build --release --bin bench-rtt"

# Validate SCENARIOS — every entry must be a known SPECS key. This
# catches typos in SCENARIOS_FILTER before any teardown-able state is
# touched on the peer.
for s in "${SCENARIOS[@]}"; do
    [ -n "${SPECS[$s]:-}" ] || die "Unknown scenario '$s' (not present in SPECS map). Known: ${!SPECS[*]}"
done

# Probe peer reachability + ssh + echo-server before doing anything destructive.
log "preflight: ssh $PEER_SSH"
if ! ssh_peer "true" 2>/dev/null; then
    die "ssh to $PEER_SSH failed — is fast-iter up?"
fi
log "preflight: echo-server on :$PEER_ECHO_PORT"
if ! ssh_peer "pgrep -a echo-server >/dev/null"; then
    die "echo-server not running on peer — is fast-iter up?"
fi

mkdir -p "$ARTIFACTS_DIR"
LOG_FILE="$ARTIFACTS_DIR/verify-rack-tlp.log"
: >"$LOG_FILE"
log "artifacts: $ARTIFACTS_DIR (log=$LOG_FILE)"

# Defensive netem teardown — clears any stale qdisc from a previous interrupt.
log "preflight: clearing any stale netem on peer"
ssh_peer "sudo tc qdisc del dev $PEER_NIC root 2>/dev/null || true" || true

# Trap teardown so an unexpected exit doesn't leave the peer behind a
# netem qdisc / ifb ingress redirect / tcp_sack override (any of which
# would silently break subsequent bench runs). Both branches are
# best-effort and idempotent so a partial state from a previous
# interrupted run is also cleaned.
cleanup_netem() {
    ssh_peer "sudo tc qdisc del dev $PEER_NIC root 2>/dev/null || true" >/dev/null 2>&1 || true
    teardown_ifb_reorder
}
trap cleanup_netem EXIT

# ---------------------------------------------------------------------------
# Per-scenario verification loop.
# ---------------------------------------------------------------------------
declare -A RESULT_LINES
overall_rc=0

for scenario in "${SCENARIOS[@]}"; do
    spec="${SPECS[$scenario]}"
    # Word-split the space-separated REQUIRED_NONZERO + REQUIRED_NONZERO_ANY
    # entries into arrays. The values are static counter names (no
    # whitespace, no globs) baked into the tables above, so `read -a` is
    # safe and avoids SC2206. Missing ANY entry → empty array → ANY check
    # is skipped (treated as vacuously satisfied).
    read -r -a expected_nonzero <<<"${REQUIRED_NONZERO[$scenario]:-}"
    read -r -a expected_nonzero_any <<<"${REQUIRED_NONZERO_ANY[$scenario]:-}"
    # Precedence: FORCE_ITERS (explicit global override) > per-scenario
    # SCENARIO_ITERS default > $ITERS fallback default. See the
    # FORCE_ITERS init block + the env-overrides docstring above.
    if [ -n "$FORCE_ITERS" ]; then
        scenario_iters="$FORCE_ITERS"
    else
        scenario_iters="${SCENARIO_ITERS[$scenario]:-$ITERS}"
    fi
    # Per-scenario payload override (rack_reorder_4k needs multi-segment
    # writes — see SCENARIO_PAYLOAD_BYTES header comment). All others
    # fall through to the global PAYLOAD_BYTES (default 128).
    scenario_payload="${SCENARIO_PAYLOAD_BYTES[$scenario]:-$PAYLOAD_BYTES}"
    counters_csv="$ARTIFACTS_DIR/${scenario}-counters.csv"
    rtt_csv="$ARTIFACTS_DIR/${scenario}-rtt.csv"

    log "=== $scenario === (spec='$spec'; iters=$scenario_iters; payload=$scenario_payload; all>0: ${expected_nonzero[*]:-(none)}; any>0: ${expected_nonzero_any[*]:-(none)})"

    # Apply the per-scenario netem setup. Two setup classes:
    #   - rack_reorder_4k: peer-INGRESS reorder via ifb redirect, with
    #     tcp_sack temporarily flipped on (see setup_ifb_reorder helper).
    #     This is the scenario that actually fires RACK — the egress-
    #     loss scenarios below cannot, even on a SACK-enabled peer,
    #     because peer-egress-loss only drops ACKs and the response
    #     data path; it never causes the peer to receive DUT data
    #     out of order, so the peer never SACKs misorder, so the DUT's
    #     RACK detect-lost rule never sees a "later sacked, earlier
    #     unacked" condition (RFC 8985 §6.2).
    #   - everything else: classic peer-egress netem on PEER_NIC root.
    if [ "$scenario" = "rack_reorder_4k" ]; then
        log "  applying ifb ingress reorder: $RACK_REORDER_SPEC"
        if ! setup_ifb_reorder "$RACK_REORDER_SPEC"; then
            log "  FAIL: ifb reorder setup failed; skipping $scenario"
            RESULT_LINES[$scenario]="FAIL apply-ifb-reorder"
            overall_rc=1
            teardown_ifb_reorder
            continue
        fi
    else
        log "  applying netem: $spec"
        if ! ssh_peer "sudo tc qdisc add dev $PEER_NIC root netem $spec"; then
            log "  FAIL: tc qdisc add failed; skipping $scenario"
            RESULT_LINES[$scenario]="FAIL apply-netem (could not add qdisc)"
            overall_rc=1
            continue
        fi
    fi

    # Run bench-rtt with the counters sidecar enabled. We don't fail-fast on
    # bench-rtt's exit code — high-loss scenarios may bump failed_iter_count
    # without bailing the run; the counters CSV is the source of truth for
    # the assertion.
    #
    # The `>>"$LOG_FILE" 2>&1` redirect is on the operator-owned outer
    # shell (sudo's child writes to those file descriptors after the
    # parent shell opens them), so the redirect target ownership is
    # irrelevant — `LOG_FILE` is created above as the operator. SC2024
    # is suppressed because the redirect intentionally targets the
    # outer shell's stream and not a sudo-context-only path.
    log "  bench-rtt: iters=$scenario_iters warmup=$WARMUP payload=$scenario_payload"
    bench_rc=0
    # shellcheck disable=SC2024
    sudo "$BENCH_RTT_BIN" \
        --stack             dpdk_net \
        --local-ip          "$DUT_IP" \
        --gateway-ip        "$DUT_GATEWAY" \
        --peer-ip           "$PEER_IP" \
        --peer-port         "$PEER_ECHO_PORT" \
        --eal-args          "$DUT_EAL_ARGS" \
        --lcore             "$DUT_LCORE" \
        --payload-bytes-sweep "$scenario_payload" \
        --iterations        "$scenario_iters" \
        --warmup            "$WARMUP" \
        --tool              verify-rack-tlp \
        --feature-set       "rack-tlp-${scenario}" \
        --precondition-mode "$PRECONDITION_MODE" \
        --output-csv        "$rtt_csv" \
        --counters-csv      "$counters_csv" \
        >>"$LOG_FILE" 2>&1 || bench_rc=$?

    # Remove the per-scenario netem setup before moving on, even if bench
    # failed. Mirror the apply-side branch so the right teardown runs.
    if [ "$scenario" = "rack_reorder_4k" ]; then
        log "  removing ifb ingress reorder"
        teardown_ifb_reorder
    else
        log "  removing netem"
        ssh_peer "sudo tc qdisc del dev $PEER_NIC root" >>"$LOG_FILE" 2>&1 || \
            log "    netem removal failed (peer state inspection: 'sudo tc qdisc show dev $PEER_NIC')"
    fi

    if [ $bench_rc -ne 0 ]; then
        log "  bench-rtt exit=$bench_rc — see $LOG_FILE"
    fi
    if [ ! -s "$counters_csv" ]; then
        log "  FAIL: counters CSV missing or empty ($counters_csv)"
        RESULT_LINES[$scenario]="FAIL counters-csv-missing (bench-rtt exit=$bench_rc)"
        overall_rc=1
        continue
    fi

    # Read the deltas of interest. Always include the aggregate so the
    # summary line documents whether ANY retransmit fired even when the
    # specific assertion fails.
    rto_d="$(read_delta "$counters_csv" tcp.tx_retrans_rto)"
    rack_d="$(read_delta "$counters_csv" tcp.tx_retrans_rack)"
    tlp_d="$(read_delta "$counters_csv" tcp.tx_retrans_tlp)"
    agg_d="$(read_delta "$counters_csv" tcp.tx_retrans)"
    rto_d="${rto_d:-0}"; rack_d="${rack_d:-0}"; tlp_d="${tlp_d:-0}"; agg_d="${agg_d:-0}"

    log "  deltas: rto=$rto_d rack=$rack_d tlp=$tlp_d agg=$agg_d"

    # Per-scenario assertions. The ALL-of check (REQUIRED_NONZERO) and the
    # ANY-of check (REQUIRED_NONZERO_ANY) are independent — a scenario can
    # fail either or both. The summary line records each failure shape so
    # the operator can distinguish "no recovery fired at all" (ALL fails)
    # from "low-loss scenario fell through to RTO" (ANY fails).
    failed_assertions=()
    for c in "${expected_nonzero[@]}"; do
        d="$(read_delta "$counters_csv" "$c")"
        d="${d:-0}"
        if [ "$d" -le 0 ] 2>/dev/null; then
            failed_assertions+=("$c=$d")
        fi
    done

    any_satisfied=1
    any_seen=()
    if [ ${#expected_nonzero_any[@]} -gt 0 ]; then
        any_satisfied=0
        for c in "${expected_nonzero_any[@]}"; do
            d="$(read_delta "$counters_csv" "$c")"
            d="${d:-0}"
            any_seen+=("$c=$d")
            if [ "$d" -gt 0 ] 2>/dev/null; then
                any_satisfied=1
            fi
        done
    fi

    if [ ${#failed_assertions[@]} -eq 0 ] && [ "$any_satisfied" -eq 1 ]; then
        RESULT_LINES[$scenario]="PASS rto=$rto_d rack=$rack_d tlp=$tlp_d agg=$agg_d"
        log "  PASS"
    else
        fail_parts=()
        if [ ${#failed_assertions[@]} -gt 0 ]; then
            fail_parts+=("ALL-of want>0 got: ${failed_assertions[*]}")
        fi
        if [ "$any_satisfied" -eq 0 ]; then
            fail_parts+=("ANY-of all-zero: ${any_seen[*]}")
        fi
        RESULT_LINES[$scenario]="FAIL ${fail_parts[*]} | rto=$rto_d rack=$rack_d tlp=$tlp_d agg=$agg_d"
        log "  FAIL: ${fail_parts[*]}"
        overall_rc=1
    fi
done

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------
echo
echo "================================================================================"
echo "verify-rack-tlp summary"
echo "================================================================================"
printf '%-20s %-20s %-32s %-28s %s\n' "scenario" "spec" "all>0" "any>0" "result"
echo "--------------------------------------------------------------------------------"
for scenario in "${SCENARIOS[@]}"; do
    printf '%-20s %-20s %-32s %-28s %s\n' \
        "$scenario" \
        "${SPECS[$scenario]}" \
        "${REQUIRED_NONZERO[$scenario]:-(none)}" \
        "${REQUIRED_NONZERO_ANY[$scenario]:-(none)}" \
        "${RESULT_LINES[$scenario]:-(not run)}"
done
echo "================================================================================"
echo "artifacts: $ARTIFACTS_DIR"
echo "log:       $LOG_FILE"
echo "================================================================================"

if [ $overall_rc -eq 0 ]; then
    log "ALL PASS"
else
    log "OVERALL FAIL (rc=$overall_rc)"
fi
exit $overall_rc
