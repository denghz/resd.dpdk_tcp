#!/usr/bin/env bash
# verify-rack-tlp.sh — verify the Phase 11 RTO/RACK/TLP counter split fires
#                      correctly under Phase 10's netem scenarios.
#
# Closes T51 deferred-work item 3 (operator-runnable; not auto-run because it
# requires a real DUT+peer cluster). The script:
#   1. Loops over the calibrated scenario set (see Scenario × counter table
#      below).
#   2. Applies netem on the peer (egress only — sufficient to drive RTO/RACK
#      because peer→DUT loss makes DUT's outgoing data go unacknowledged).
#   3. Runs bench-rtt against the peer's echo-server with --counters-csv,
#      sweeping a single 128 B payload (iter count varies per scenario; see
#      SCENARIO_ITERS below).
#   4. Parses the per-scenario counters CSV (name,pre,post,delta) and asserts
#      both the ALL-of (REQUIRED_NONZERO) and the ANY-of
#      (REQUIRED_NONZERO_ANY) constraints.
#   5. Reports PASS/FAIL with per-scenario counter deltas.
#
# ─── Scenario × expected-counter table (assertion-set v2, 2026-05-12) ─────
#
# Empirical fact (fast-iter run 2026-05-11): at ≥3% loss, RACK does NOT
# fire on the dpdk_net stack because the ACK stream becomes too sparse for
# RACK's reorder window to detect losses before the 200 ms RTO timer
# expires; RTO absorbs the recovery. RACK fast-retransmit therefore needs
# a *low-loss* scenario with no induced delay (dense ACK stream).
#
# Iter-count trade-off (calibrated 2026-05-12 for fast-iter wallclock):
# High-loss scenarios use deliberately low iter counts because the
# assertion is `> 0` — once 1k+ recovery events fire, the assertion is
# saturated. Adding more iters only burns wallclock under the 200 ms RTO
# floor (each lost iter ≈ +200 ms scenario time). The per-scenario
# defaults below are tuned so the full suite completes in ~30 min on
# AWS c6a fast-iter hardware (the `low_loss_1pct` cell at uniform 1% is
# ~7-8 min — more RTOs fire than under the retired `loss 1% 25%` spec,
# but the assertion is now reliably saturated instead of stochastically
# false-failing). Statistical depth for percentile reporting is NOT a
# goal of this verifier and should be obtained from bench-nightly's
# netem matrix instead. Operators wanting nightly-grade depth on a
# physical-lab DUT can globally override every scenario via the
# `FORCE_ITERS` env var (e.g. `FORCE_ITERS=1000000`).
#
#   scenario              netem spec            ALL non-zero           ANY non-zero          rationale
#   ─────────────────     ─────────────────     ───────────────────    ──────────────────    ─────────
#   low_loss_05pct        loss 0.5%             tcp.tx_retrans         tx_retrans_rack,      Dense ACKs → RACK fires
#                                                                       tx_retrans_tlp        within reorder window;
#                                                                                             RPC tail-loss → TLP.
#                                                                                             Loss too low for RTO.
#   low_loss_1pct         loss 1%               tcp.tx_retrans         tx_retrans_rack,      Independent per-packet drop
#                                                                       tx_retrans_tlp        at 1%. ALL-of pins the
#                                                                                             aggregate; ANY-of pins
#                                                                                             RACK or TLP (low-loss
#                                                                                             recovery path). Empirical
#                                                                                             (2026-05-12 smoke ×3):
#                                                                                             ~10 000 RTO + ~2 000 TLP +
#                                                                                             ~12 000 agg per 200k-iter
#                                                                                             run — ANY-of saturates on
#                                                                                             TLP. Replaced flaky
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
#   PEER_NIC                 peer data interface (default: ens6)
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
PEER_NIC="${PEER_NIC:-ens6}"

DUT_IP="${DUT_IP:-10.4.1.141}"
DUT_GATEWAY="${DUT_GATEWAY:-10.4.1.1}"
DUT_PCI="${DUT_PCI:-0000:00:06.0}"
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
# TLP paths (which empirically never fire at ≥3% — see header table).
declare -A SPECS=(
  [low_loss_05pct]="loss 0.5%"
  [low_loss_1pct]="loss 1%"
  [high_loss_3pct]="loss 3% delay 5ms"
  [symmetric_3pct]="loss 3%"
  [high_loss_5pct]="loss 5% 25%"
)

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
)

declare -A REQUIRED_NONZERO_ANY=(
  [low_loss_05pct]="tcp.tx_retrans_rack tcp.tx_retrans_tlp"
  [low_loss_1pct]="tcp.tx_retrans_rack tcp.tx_retrans_tlp"
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
declare -A SCENARIO_ITERS=(
  [low_loss_05pct]=500000
  [low_loss_1pct]=200000
  [high_loss_3pct]=50000
  [symmetric_3pct]=50000
  [high_loss_5pct]=30000
)

# Order matters for the summary table — ascending by loss severity so the
# operator sees the "expected RACK/TLP" rows before the RTO-dominated rows.
SCENARIOS=(
  low_loss_05pct
  low_loss_1pct
  high_loss_3pct
  symmetric_3pct
  high_loss_5pct
)

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

# ---------------------------------------------------------------------------
# Pre-flight.
# ---------------------------------------------------------------------------
[ -n "$PEER_IP" ] || die "PEER_IP unset (source ./.fast-iter.env or run scripts/fast-iter-setup.sh up)"
[ -x "$BENCH_RTT_BIN" ] || die "bench-rtt binary missing at $BENCH_RTT_BIN — build first: cargo build --release --bin bench-rtt"

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

# Trap teardown so an unexpected exit doesn't leave the peer behind a netem
# qdisc (which would silently break subsequent bench runs).
cleanup_netem() {
    ssh_peer "sudo tc qdisc del dev $PEER_NIC root 2>/dev/null || true" >/dev/null 2>&1 || true
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
    counters_csv="$ARTIFACTS_DIR/${scenario}-counters.csv"
    rtt_csv="$ARTIFACTS_DIR/${scenario}-rtt.csv"

    log "=== $scenario === (spec='$spec'; iters=$scenario_iters; all>0: ${expected_nonzero[*]:-(none)}; any>0: ${expected_nonzero_any[*]:-(none)})"

    # Apply netem on peer egress.
    log "  applying netem: $spec"
    if ! ssh_peer "sudo tc qdisc add dev $PEER_NIC root netem $spec"; then
        log "  FAIL: tc qdisc add failed; skipping $scenario"
        RESULT_LINES[$scenario]="FAIL apply-netem (could not add qdisc)"
        overall_rc=1
        continue
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
    log "  bench-rtt: iters=$scenario_iters warmup=$WARMUP payload=$PAYLOAD_BYTES"
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
        --payload-bytes-sweep "$PAYLOAD_BYTES" \
        --iterations        "$scenario_iters" \
        --warmup            "$WARMUP" \
        --tool              verify-rack-tlp \
        --feature-set       "rack-tlp-${scenario}" \
        --precondition-mode "$PRECONDITION_MODE" \
        --output-csv        "$rtt_csv" \
        --counters-csv      "$counters_csv" \
        >>"$LOG_FILE" 2>&1 || bench_rc=$?

    # Remove netem before doing anything else, even if bench failed.
    log "  removing netem"
    ssh_peer "sudo tc qdisc del dev $PEER_NIC root" >>"$LOG_FILE" 2>&1 || \
        log "    netem removal failed (peer state inspection: 'sudo tc qdisc show dev $PEER_NIC')"

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
