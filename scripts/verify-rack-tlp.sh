#!/usr/bin/env bash
# verify-rack-tlp.sh — verify the Phase 11 RTO/RACK/TLP counter split fires
#                      correctly under Phase 10's high-loss netem scenarios.
#
# Closes T51 deferred-work item 3 (operator-runnable; not auto-run because it
# requires a real DUT+peer cluster). The script:
#   1. Loops over the three high-loss scenarios.
#   2. Applies netem on the peer (egress only — sufficient to drive RTO/RACK
#      because peer→DUT loss makes DUT's outgoing data go unacknowledged).
#   3. Runs bench-rtt against the peer's echo-server with --counters-csv,
#      sweeping a single 128 B payload at 200k iters (matches Phase 10's
#      SCENARIO_ITERS map for the slower high-loss cells).
#   4. Parses the per-scenario counters CSV (name,pre,post,delta) and asserts
#      the expected counter is non-zero.
#   5. Reports PASS/FAIL with per-scenario counter deltas.
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
#   ITERS                    bench-rtt --iterations (default: 200000;
#                            matches Phase 10 SCENARIO_ITERS for high-loss)
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
WARMUP="${WARMUP:-1000}"
PAYLOAD_BYTES="${PAYLOAD_BYTES:-128}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-/tmp/verify-rack-tlp}"
BENCH_RTT_BIN="${BENCH_RTT_BIN:-$WORKDIR/target/release/bench-rtt}"
PRECONDITION_MODE="${PRECONDITION_MODE:-lenient}"

SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30
          -o ProxyCommand=none -o ConnectTimeout=10 -i "$SSH_KEY")

# Per-scenario netem spec — copied verbatim from scripts/bench-nightly.sh
# step [8/12] NETEM_SPECS map (Phase 10 task 10.2 cells).
declare -A SPECS=(
  [high_loss_3pct]="loss 3% delay 5ms"
  [high_loss_5pct]="loss 5% 25%"
  [symmetric_3pct]="loss 3%"
)

# Per-scenario assertion: which counter MUST have delta > 0 after the run.
# Rationale per the design block in T51 deferred-work item 3:
#
#   high_loss_3pct (3% loss + 5 ms delay)
#     Recoverable losses use RACK fast-retransmit (~1 RTT after the first
#     ACK gap). Long-tail loss clusters that exhaust the SACK reorder
#     window push past the 200 ms RTO floor. Both must fire.
#
#   high_loss_5pct (5% loss, 25% correlation)
#     Correlated bursts are biased toward back-to-back drops; a single
#     burst can take down >cwnd packets, leaving no incoming ACKs to drive
#     RACK's reordering window. RTO is the only available recovery — must
#     fire.
#
#   symmetric_3pct (3% loss, no delay)
#     RACK fires on every recoverable loss. Tail-loss probes (TLP) fire
#     after the PTO when the queue is drained but unacked data remains.
#     Both must fire.
#
declare -A REQUIRED_NONZERO=(
  [high_loss_3pct]="tcp.tx_retrans_rto tcp.tx_retrans_rack"
  [high_loss_5pct]="tcp.tx_retrans_rto"
  [symmetric_3pct]="tcp.tx_retrans_rack tcp.tx_retrans_tlp"
)

# Order matters for the summary table.
SCENARIOS=(high_loss_3pct high_loss_5pct symmetric_3pct)

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
    # Word-split the space-separated REQUIRED_NONZERO entry into an array.
    # The values are static counter names (no whitespace, no globs) baked
    # into the table above, so `read -a` is safe and avoids SC2206.
    read -r -a expected_nonzero <<<"${REQUIRED_NONZERO[$scenario]}"
    counters_csv="$ARTIFACTS_DIR/${scenario}-counters.csv"
    rtt_csv="$ARTIFACTS_DIR/${scenario}-rtt.csv"

    log "=== $scenario === (spec='$spec'; expect>0: ${expected_nonzero[*]})"

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
    log "  bench-rtt: iters=$ITERS warmup=$WARMUP payload=$PAYLOAD_BYTES"
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
        --iterations        "$ITERS" \
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

    # Per-scenario assertion.
    failed_assertions=()
    for c in "${expected_nonzero[@]}"; do
        d="$(read_delta "$counters_csv" "$c")"
        d="${d:-0}"
        if [ "$d" -le 0 ] 2>/dev/null; then
            failed_assertions+=("$c=$d")
        fi
    done

    if [ ${#failed_assertions[@]} -eq 0 ]; then
        RESULT_LINES[$scenario]="PASS rto=$rto_d rack=$rack_d tlp=$tlp_d agg=$agg_d"
        log "  PASS"
    else
        RESULT_LINES[$scenario]="FAIL want>0 got: ${failed_assertions[*]} | rto=$rto_d rack=$rack_d tlp=$tlp_d agg=$agg_d"
        log "  FAIL: ${failed_assertions[*]}"
        overall_rc=1
    fi
done

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------
echo
echo "=============================================================="
echo "verify-rack-tlp summary"
echo "=============================================================="
printf '%-20s %-15s %-30s %s\n' "scenario" "spec" "expected>0" "result"
echo "--------------------------------------------------------------"
for scenario in "${SCENARIOS[@]}"; do
    printf '%-20s %-15s %-30s %s\n' \
        "$scenario" \
        "${SPECS[$scenario]}" \
        "${REQUIRED_NONZERO[$scenario]}" \
        "${RESULT_LINES[$scenario]:-(not run)}"
done
echo "=============================================================="
echo "artifacts: $ARTIFACTS_DIR"
echo "log:       $LOG_FILE"
echo "=============================================================="

if [ $overall_rc -eq 0 ]; then
    log "ALL PASS"
else
    log "OVERALL FAIL (rc=$overall_rc)"
fi
exit $overall_rc
