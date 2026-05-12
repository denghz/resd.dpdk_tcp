#!/usr/bin/env bash
# fast-iter-suite.sh — full fast-iter bench suite.
#
# Runs each of {bench-rtt, bench-tx-burst, bench-tx-maxtp, bench-rx-burst}
# against the fast-iter peer for each of {dpdk_net, linux_kernel, fstack}, then
# runs verify-rack-tlp.sh against the netem scenario set, collects all CSVs
# into a timestamped results directory, and prints a single-page summary.
#
# Pre-condition: the peer must be up with all three servers running. Bring it
# up via:
#
#   ./scripts/fast-iter-setup.sh up --with-fstack
#
# That generates ./.fast-iter.env (PEER_IP, PEER_SSH, PEER_*_PORT, FSTACK_CONF)
# which this script sources. The four bench binaries must already be built
# with --features fstack present in their symbol tables — see CLAUDE.md for
# the build incantation.
#
# DPDK NIC exclusivity: only one process can hold the data NIC at a time, so
# every dpdk_net / fstack arm runs sequentially. Inter-arm gaps are kept tight
# (the peer can serve all three stacks back-to-back without resetting).
#
# Usage:
#   ./scripts/fast-iter-suite.sh
#
# Overrides (env):
#   RESULTS_DIR_OVERRIDE   Absolute path to use instead of the default
#                          target/bench-results/fast-iter-<UTC>/. Useful when
#                          re-running into an existing directory.
#   DUT_PCI                Default 0000:28:00.0 (a10 perf host).
#   DUT_LOCAL_IP           Default 10.4.1.141.
#   DUT_GATEWAY            Default 10.4.1.1.
#   DUT_LCORE              Default 2.
#   PEER_NIC               Default ens5 (peer data NIC, passed to verify-rack-tlp).
#   SKIP_VERIFY_RACK_TLP   Set non-empty to skip the netem matrix.
#   VERIFY_RACK_ITERS      Default 50000 (override for verify-rack-tlp's ITERS).
#
# Exit code: 0 if at least one bench arm produced a non-empty CSV per stack +
# tool combination. Non-zero only on catastrophic failure (missing binaries,
# unreachable peer, etc.). Individual bench-arm failures are logged + tallied
# in $RESULTS_DIR/SUMMARY.md, not propagated.

set -euo pipefail

# ---------------------------------------------------------------------------
# Paths + config.
# ---------------------------------------------------------------------------
WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$WORKDIR"

ENV_FILE="$WORKDIR/.fast-iter.env"
if [ ! -f "$ENV_FILE" ]; then
    printf 'fast-iter-suite: %s not found — run ./scripts/fast-iter-setup.sh up --with-fstack first\n' "$ENV_FILE" >&2
    exit 2
fi
# shellcheck disable=SC1090
source "$ENV_FILE"

: "${PEER_IP:?PEER_IP unset (corrupt .fast-iter.env?)}"
: "${PEER_SSH:?PEER_SSH unset}"
: "${PEER_ECHO_PORT:?PEER_ECHO_PORT unset}"
: "${PEER_SINK_PORT:?PEER_SINK_PORT unset}"
: "${PEER_BURST_PORT:?PEER_BURST_PORT unset}"
: "${FSTACK_CONF:?FSTACK_CONF unset — re-run fast-iter-setup.sh up --with-fstack}"

DUT_PCI="${DUT_PCI:-0000:28:00.0}"
DUT_LOCAL_IP="${DUT_LOCAL_IP:-10.4.1.141}"
DUT_GATEWAY="${DUT_GATEWAY:-10.4.1.1}"
DUT_LCORE="${DUT_LCORE:-2}"
PEER_NIC="${PEER_NIC:-ens5}"
EAL_ARGS="-l 2-3 -n 4 --in-memory --huge-unlink -a ${DUT_PCI},large_llq_hdr=1,miss_txc_to=3"

VERIFY_RACK_ITERS="${VERIFY_RACK_ITERS:-50000}"

UTC_TS="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
RESULTS_DIR="${RESULTS_DIR_OVERRIDE:-$WORKDIR/target/bench-results/fast-iter-$UTC_TS}"
mkdir -p "$RESULTS_DIR"

LOG_FILE="$RESULTS_DIR/suite.log"
: >"$LOG_FILE"

# ---------------------------------------------------------------------------
# Binaries.
# ---------------------------------------------------------------------------
BENCH_RTT="$WORKDIR/target/release/bench-rtt"
BENCH_TX_BURST="$WORKDIR/target/release/bench-tx-burst"
BENCH_TX_MAXTP="$WORKDIR/target/release/bench-tx-maxtp"
BENCH_RX_BURST="$WORKDIR/target/release/bench-rx-burst"
VERIFY_RACK_TLP="$WORKDIR/scripts/verify-rack-tlp.sh"

for bin in "$BENCH_RTT" "$BENCH_TX_BURST" "$BENCH_TX_MAXTP" "$BENCH_RX_BURST"; do
    [ -x "$bin" ] || { printf 'fast-iter-suite: missing binary %s\n' "$bin" >&2; exit 2; }
done
[ -x "$VERIFY_RACK_TLP" ] || { printf 'fast-iter-suite: missing %s\n' "$VERIFY_RACK_TLP" >&2; exit 2; }

# Verify fstack symbols are present in all four binaries.
for bin in "$BENCH_RTT" "$BENCH_TX_BURST" "$BENCH_TX_MAXTP" "$BENCH_RX_BURST"; do
    count=$(nm "$bin" 2>/dev/null | grep -c ' T ff_socket' || true)
    if [ "$count" -eq 0 ]; then
        printf 'fast-iter-suite: %s missing fstack symbols — rebuild with --features fstack\n' "$bin" >&2
        exit 2
    fi
done

# ---------------------------------------------------------------------------
# Logging + run helpers.
# ---------------------------------------------------------------------------
declare -a FAILS=()
declare -a OKS=()
declare -i FAIL_COUNT=0
declare -i OK_COUNT=0

# Per-arm hard cap (seconds). Generous enough for the heaviest configured arm
# (bench-tx-maxtp at 12s × 9 buckets = ~108s) but short enough to bail if a
# bench gets stuck on a hung peer echo-server. Override via env if needed.
RUN_ONE_TIMEOUT="${RUN_ONE_TIMEOUT:-300}"

# Peer echo-server worker count is bounded (~10). The dpdk_net and fstack
# arms leave stale ESTABLISHED connections behind because their stacks tear
# down the TX queue without sending TCP FINs to the peer — so each bucket
# pins one worker slot. Restart the peer's echo-server (NOT the sink/burst
# servers) before every dpdk_net/fstack arm to release worker slots.
PEER_RESTART_DELAY="${PEER_RESTART_DELAY:-1}"

log() { printf '[suite %s] %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$LOG_FILE" >&2; }

ts_now() { date -u +%s; }

# Restart the peer's :10001 echo-server. Used between dpdk_net/fstack arms
# to clear leaked TCP connections that pin all echo-server worker threads.
#
# Implementation note: `pkill -f /tmp/echo-server $PEER_ECHO_PORT` would also
# match the remote bash command we're running (its own argv contains the
# pattern), self-killing the shell. We therefore use `pgrep -fx` (exact full
# cmdline match) → explicit `kill`, which only matches the echo-server.
peer_restart_echo_server() {
    log "    peer: restart echo-server :$PEER_ECHO_PORT (clear stale ESTAB)"
    ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$PEER_SSH" "
        pids=\$(pgrep -fx '/tmp/echo-server $PEER_ECHO_PORT' 2>/dev/null || true)
        if [ -n \"\$pids\" ]; then
            kill -KILL \$pids 2>/dev/null || true
            sleep 0.3
        fi
        nohup /tmp/echo-server $PEER_ECHO_PORT >/tmp/echo-server.log 2>&1 </dev/null &
        disown
        sleep $PEER_RESTART_DELAY
        pgrep -fx '/tmp/echo-server $PEER_ECHO_PORT' >/dev/null
    " >>"$LOG_FILE" 2>&1 || log "    WARN: peer echo-server restart failed (see $LOG_FILE)"
}

# run_one <desc> <output-csv> <command...>
#
# Runs the bench command under `timeout $RUN_ONE_TIMEOUT`, appending stdout+stderr
# to the per-arm log file (and the suite log). Outputs OK / FAIL / TIMEOUT
# message. Never aborts the suite.
run_one() {
    local desc="$1" outcsv="$2"
    shift 2
    local arm_log
    arm_log="$RESULTS_DIR/$(basename "$outcsv" .csv).log"

    local started ended elapsed
    started=$(ts_now)
    log ">>> $desc"
    log "    csv:   $outcsv"
    log "    log:   $arm_log"

    {
        printf '=== %s ===\n' "$desc"
        printf 'cmd: %s\n' "$*"
        printf 'started: %s timeout=%ss\n' "$(date -u -Iseconds)" "$RUN_ONE_TIMEOUT"
    } >"$arm_log"

    # `timeout --foreground` lets Ctrl+C reach the bench process; `-k 30`
    # SIGKILLs 30s after SIGTERM if the bench refuses to exit.
    if timeout --foreground -k 30 "$RUN_ONE_TIMEOUT" "$@" >>"$arm_log" 2>&1; then
        ended=$(ts_now)
        elapsed=$((ended - started))
        log "    OK ($elapsed s)"
        OKS+=("$desc")
        OK_COUNT=$((OK_COUNT + 1))
        printf 'OK %s elapsed=%ds\n' "$desc" "$elapsed" >>"$arm_log"
        return 0
    else
        local rc=$?
        ended=$(ts_now)
        elapsed=$((ended - started))
        local tag="FAIL"
        if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
            tag="TIMEOUT"
        fi
        log "    $tag rc=$rc ($elapsed s) — see $arm_log"
        FAILS+=("$desc ($tag rc=$rc, log=$arm_log)")
        FAIL_COUNT=$((FAIL_COUNT + 1))
        printf '%s rc=%d %s elapsed=%ds\n' "$tag" "$rc" "$desc" "$elapsed" >>"$arm_log"
        return 0  # don't abort the suite
    fi
}

# ---------------------------------------------------------------------------
# Pre-flight peer reachability check.
# ---------------------------------------------------------------------------
preflight() {
    log "preflight: peer=$PEER_IP fstack_conf=$FSTACK_CONF"
    if ! ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$PEER_SSH" \
            "pgrep -af '/tmp/echo-server' >/dev/null && pgrep -af '/tmp/linux-tcp-sink' >/dev/null && pgrep -af '/tmp/burst-echo-server' >/dev/null"; then
        log "FATAL: one or more peer servers not running — abort"
        exit 2
    fi
    log "preflight: all 3 peer servers running OK"

    # DPDK NIC must be bound to vfio-pci.
    local drv
    drv=$(readlink "/sys/bus/pci/devices/$DUT_PCI/driver" 2>/dev/null | xargs -r basename || echo "unbound")
    if [ "$drv" != "vfio-pci" ]; then
        log "FATAL: DUT NIC $DUT_PCI bound to '$drv' (need vfio-pci) — abort"
        exit 2
    fi
    log "preflight: DUT $DUT_PCI bound to vfio-pci OK"

    # Reset the peer's :10001 echo-server so we start with no stale ESTAB
    # connections holding worker slots.
    peer_restart_echo_server
}

# ---------------------------------------------------------------------------
# Per-stack invocation helpers.
# ---------------------------------------------------------------------------

# bench-rtt
run_bench_rtt() {
    log "=== bench-rtt — RTT (payload sweep 64,128,256,1024) ==="

    run_one "bench-rtt dpdk_net" \
        "$RESULTS_DIR/bench-rtt-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_RTT" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rtt-dpdk_net.csv" \
                --payload-bytes-sweep 64,128,256,1024 \
                --iterations 10000 --warmup 100
    peer_restart_echo_server

    run_one "bench-rtt linux_kernel" \
        "$RESULTS_DIR/bench-rtt-linux_kernel.csv" \
        "$BENCH_RTT" \
            --stack linux_kernel \
            --peer-ip "$PEER_IP" \
            --peer-port "$PEER_ECHO_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-rtt-linux_kernel.csv" \
            --payload-bytes-sweep 64,128,256,1024 \
            --iterations 10000 --warmup 100

    run_one "bench-rtt fstack" \
        "$RESULTS_DIR/bench-rtt-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_RTT" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rtt-fstack.csv" \
                --payload-bytes-sweep 64,128,256,1024 \
                --iterations 10000 --warmup 100
    peer_restart_echo_server
}

# bench-tx-burst
run_bench_tx_burst() {
    log "=== bench-tx-burst — K x G grid (K={64K,1M}, G={0,10}) ==="

    run_one "bench-tx-burst dpdk_net" \
        "$RESULTS_DIR/bench-tx-burst-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_TX_BURST" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-burst-dpdk_net.csv" \
                --burst-sizes 65536,1048576 \
                --gap-mss 0,10 \
                --bursts-per-bucket 200 --warmup 20
    peer_restart_echo_server

    run_one "bench-tx-burst linux_kernel" \
        "$RESULTS_DIR/bench-tx-burst-linux_kernel.csv" \
        "$BENCH_TX_BURST" \
            --stack linux_kernel \
            --peer-ip "$PEER_IP" \
            --peer-port "$PEER_ECHO_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-tx-burst-linux_kernel.csv" \
            --burst-sizes 65536,1048576 \
            --gap-mss 0,10 \
            --bursts-per-bucket 200 --warmup 20

    run_one "bench-tx-burst fstack" \
        "$RESULTS_DIR/bench-tx-burst-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_TX_BURST" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-burst-fstack.csv" \
                --burst-sizes 65536,1048576 \
                --gap-mss 0,10 \
                --bursts-per-bucket 200 --warmup 20
    peer_restart_echo_server
}

# bench-tx-maxtp
run_bench_tx_maxtp() {
    log "=== bench-tx-maxtp — W x C grid (W={4K,16K,64K}, C={1,4,16}) ==="

    run_one "bench-tx-maxtp dpdk_net" \
        "$RESULTS_DIR/bench-tx-maxtp-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_TX_MAXTP" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-maxtp-dpdk_net.csv" \
                --write-sizes 4096,16384,65536 \
                --conn-counts 1,4,16 \
                --warmup-secs 2 --duration-secs 10
    peer_restart_echo_server

    # linux_kernel arm — note PEER_SINK_PORT (10002), not ECHO_PORT. The
    # `--local-ip` flag is documented as dpdk-only but bench-tx-maxtp's
    # peer-rwnd probe path still parses it as IPv4 for every stack, so we
    # pass DUT_LOCAL_IP here too — it's a no-op for the linux arm itself.
    run_one "bench-tx-maxtp linux_kernel" \
        "$RESULTS_DIR/bench-tx-maxtp-linux_kernel.csv" \
        "$BENCH_TX_MAXTP" \
            --stack linux_kernel \
            --local-ip "$DUT_LOCAL_IP" \
            --peer-ip "$PEER_IP" \
            --peer-port "$PEER_SINK_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-tx-maxtp-linux_kernel.csv" \
            --write-sizes 4096,16384,65536 \
            --conn-counts 1,4,16 \
            --warmup-secs 2 --duration-secs 10

    run_one "bench-tx-maxtp fstack" \
        "$RESULTS_DIR/bench-tx-maxtp-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_TX_MAXTP" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-maxtp-fstack.csv" \
                --write-sizes 4096,16384,65536 \
                --conn-counts 1,4,16 \
                --warmup-secs 2 --duration-secs 10
    peer_restart_echo_server
}

# bench-rx-burst
run_bench_rx_burst() {
    log "=== bench-rx-burst — W x N grid (W={64,128,256}, N={16,64,256}) ==="

    run_one "bench-rx-burst dpdk_net" \
        "$RESULTS_DIR/bench-rx-burst-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_RX_BURST" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-control-port "$PEER_BURST_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rx-burst-dpdk_net.csv" \
                --segment-sizes 64,128,256 \
                --burst-counts 16,64,256 \
                --measure-bursts 200 --warmup-bursts 20

    run_one "bench-rx-burst linux_kernel" \
        "$RESULTS_DIR/bench-rx-burst-linux_kernel.csv" \
        "$BENCH_RX_BURST" \
            --stack linux_kernel \
            --peer-ip "$PEER_IP" \
            --peer-control-port "$PEER_BURST_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-rx-burst-linux_kernel.csv" \
            --segment-sizes 64,128,256 \
            --burst-counts 16,64,256 \
            --measure-bursts 200 --warmup-bursts 20

    run_one "bench-rx-burst fstack" \
        "$RESULTS_DIR/bench-rx-burst-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_RX_BURST" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-control-port "$PEER_BURST_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rx-burst-fstack.csv" \
                --segment-sizes 64,128,256 \
                --burst-counts 16,64,256 \
                --measure-bursts 200 --warmup-bursts 20
}

# verify-rack-tlp
run_verify_rack_tlp() {
    if [ -n "${SKIP_VERIFY_RACK_TLP:-}" ]; then
        log "=== verify-rack-tlp SKIPPED (SKIP_VERIFY_RACK_TLP=$SKIP_VERIFY_RACK_TLP) ==="
        return 0
    fi
    log "=== verify-rack-tlp — netem scenario matrix (ITERS=$VERIFY_RACK_ITERS) ==="

    local artifacts="$RESULTS_DIR/verify-rack-tlp"
    mkdir -p "$artifacts"

    # verify-rack-tlp runs 5 netem scenarios sequentially; allow up to 30 min
    # before SIGKILL. Override via VERIFY_RACK_TLP_TIMEOUT env if needed.
    local prev_timeout="$RUN_ONE_TIMEOUT"
    RUN_ONE_TIMEOUT="${VERIFY_RACK_TLP_TIMEOUT:-1800}"
    run_one "verify-rack-tlp" \
        "$artifacts/verify-rack-tlp.log" \
        env \
            PEER_IP="$PEER_IP" \
            PEER_SSH="$PEER_SSH" \
            PEER_NIC="$PEER_NIC" \
            PEER_ECHO_PORT="$PEER_ECHO_PORT" \
            DUT_IP="$DUT_LOCAL_IP" \
            DUT_GATEWAY="$DUT_GATEWAY" \
            DUT_PCI="$DUT_PCI" \
            DUT_LCORE="$DUT_LCORE" \
            DUT_EAL_ARGS="$EAL_ARGS" \
            ARTIFACTS_DIR="$artifacts" \
            ITERS="$VERIFY_RACK_ITERS" \
            PRECONDITION_MODE=lenient \
            BENCH_RTT_BIN="$BENCH_RTT" \
            "$VERIFY_RACK_TLP"
    RUN_ONE_TIMEOUT="$prev_timeout"
}

# ---------------------------------------------------------------------------
# Summary generation (parse CSVs into SUMMARY.md).
# ---------------------------------------------------------------------------

# CSV schema (spec §14, bench_common::csv_row::CsvRow). The columns we rely on
# here are `test_case`, `feature_set`, `dimensions_json` (JSON-encoded grid
# coords), `metric_name`, `metric_unit`, `metric_value`, `metric_aggregation`.
# One row per (bucket, metric, aggregation) tuple — typically 7 aggregations
# per metric per bucket (p50/p99/p999/mean/stddev/ci95_lower/ci95_upper).
#
# This summarizer pivots into (bucket × metric) tables with p50/p99/mean cols.

summarize_one_csv() {
    local csv="$1"
    if [ ! -s "$csv" ]; then
        printf '_(no data — CSV missing or empty)_\n\n'
        return
    fi
    python3 - "$csv" <<'PY' 2>&1 || true
import csv, json, sys
path = sys.argv[1]
try:
    with open(path) as f:
        rows = list(csv.DictReader(f))
except Exception as e:
    print(f"_(error reading CSV: {e})_")
    sys.exit(0)
if not rows:
    print("_(empty CSV — header only)_")
    sys.exit(0)

# (dim_tuple, metric_name, aggregation) → (value, unit)
data = {}
metrics = []
seen_metrics = set()
buckets = []
seen_buckets = set()
dim_keys_order = []

for r in rows:
    try:
        dims = json.loads(r.get("dimensions_json", "{}") or "{}")
    except Exception:
        dims = {}
    # Drop the `stack` dim (constant per CSV).
    dims.pop("stack", None)
    for k in dims:
        if k not in dim_keys_order:
            dim_keys_order.append(k)
    dim_tup = tuple(str(dims.get(k, "")) for k in dim_keys_order)
    metric = r.get("metric_name", "")
    unit = r.get("metric_unit", "")
    agg = r.get("metric_aggregation", "")
    val = r.get("metric_value", "")
    data[(dim_tup, metric, agg)] = (val, unit)
    if metric and metric not in seen_metrics:
        metrics.append(metric)
        seen_metrics.add(metric)
    if dim_tup not in seen_buckets:
        buckets.append(dim_tup)
        seen_buckets.add(dim_tup)

# Re-normalize bucket tuples to the final dim_keys_order length.
buckets = [b + ("",) * (len(dim_keys_order) - len(b)) for b in buckets]

for metric in metrics:
    # Pick the unit from the first matching row.
    unit = next((u for ((_, m, _), (_, u)) in data.items() if m == metric), "")
    print(f"**metric: `{metric}`** ({unit})")
    print()
    hdr = list(dim_keys_order) + ["p50", "p99", "mean"]
    print("| " + " | ".join(hdr) + " |")
    print("|" + "|".join(["---"] * len(hdr)) + "|")
    for b in buckets:
        row = list(b)
        for agg in ("p50", "p99", "mean"):
            val, _ = data.get((b, metric, agg), ("—", ""))
            row.append(val if val else "—")
        print("| " + " | ".join(row) + " |")
    print()
PY
}

write_summary() {
    local summary="$RESULTS_DIR/SUMMARY.md"
    {
        printf '# fast-iter-suite SUMMARY — %s\n\n' "$UTC_TS"
        printf '**Results directory:** `%s`\n\n' "$RESULTS_DIR"
        printf '**Peer:** `%s` (ens5)  •  **DUT:** `%s` (PCI `%s`, lcore %s)\n\n' \
            "$PEER_IP" "$DUT_LOCAL_IP" "$DUT_PCI" "$DUT_LCORE"
        printf '**Wallclock:** %s — %s\n\n' "$WALLCLOCK_START_HUMAN" "$WALLCLOCK_END_HUMAN"
        printf '**Outcome:** %d OK, %d FAIL\n\n' "$OK_COUNT" "$FAIL_COUNT"

        printf '## bench-rtt — RTT (ns), payload sweep\n\n'
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            summarize_one_csv "$RESULTS_DIR/bench-rtt-$stack.csv"
            printf '\n'
        done

        printf '## bench-tx-burst — burst throughput (bps) + initiation (ns)\n\n'
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            summarize_one_csv "$RESULTS_DIR/bench-tx-burst-$stack.csv"
            printf '\n'
        done

        printf '## bench-tx-maxtp — sustained goodput (bps)\n\n'
        printf '> linux_kernel arm points at peer port %s (linux-tcp-sink); dpdk_net and fstack at %s (echo-server).\n\n' \
            "$PEER_SINK_PORT" "$PEER_ECHO_PORT"
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            summarize_one_csv "$RESULTS_DIR/bench-tx-maxtp-$stack.csv"
            printf '\n'
        done

        printf '## bench-rx-burst — per-segment RX latency (ns)\n\n'
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            summarize_one_csv "$RESULTS_DIR/bench-rx-burst-$stack.csv"
            printf '\n'
        done

        printf '## verify-rack-tlp — netem scenarios\n\n'
        if [ -n "${SKIP_VERIFY_RACK_TLP:-}" ]; then
            printf '_Skipped (SKIP_VERIFY_RACK_TLP=%s)_\n\n' "$SKIP_VERIFY_RACK_TLP"
        elif [ -f "$RESULTS_DIR/verify-rack-tlp/verify-rack-tlp.log" ]; then
            printf '```\n'
            # Pull the summary block from the verify-rack-tlp log.
            sed -n '/verify-rack-tlp summary/,/^======/p' "$RESULTS_DIR/verify-rack-tlp/verify-rack-tlp.log" | head -25
            printf '```\n\n'
        else
            printf '_(no verify-rack-tlp log found)_\n\n'
        fi

        if [ "$FAIL_COUNT" -gt 0 ]; then
            printf '## Failed runs (%d)\n\n' "$FAIL_COUNT"
            for f in "${FAILS[@]}"; do
                printf '- %s\n' "$f"
            done
            printf '\n'
        fi

        printf '## Artifacts\n\n'
        find "$RESULTS_DIR" -maxdepth 2 -type f \( -name '*.csv' -o -name '*.log' \) | sort \
            | sed "s|^$RESULTS_DIR/|- |"
    } >"$summary"
    log "summary written: $summary"
}

# ---------------------------------------------------------------------------
# Top-level orchestration.
# ---------------------------------------------------------------------------

WALLCLOCK_START=$(ts_now)
WALLCLOCK_START_HUMAN="$(date -u -Iseconds)"

on_exit() {
    local rc=$?
    WALLCLOCK_END=$(ts_now)
    WALLCLOCK_END_HUMAN="$(date -u -Iseconds)"
    local elapsed=$((WALLCLOCK_END - WALLCLOCK_START))
    log "=== suite done — elapsed ${elapsed}s, $OK_COUNT OK, $FAIL_COUNT FAIL (rc=$rc) ==="
    write_summary || log "summary generation failed"

    # Final compact stdout summary so the operator gets a single screen.
    echo
    echo "================================================================================"
    echo "fast-iter-suite summary  ($UTC_TS)"
    echo "================================================================================"
    echo "results: $RESULTS_DIR"
    echo "summary: $RESULTS_DIR/SUMMARY.md"
    echo "wallclock: ${elapsed}s"
    echo "outcome: $OK_COUNT OK, $FAIL_COUNT FAIL"
    if [ "$FAIL_COUNT" -gt 0 ]; then
        echo
        echo "failed runs:"
        for f in "${FAILS[@]}"; do echo "  - $f"; done
    fi
    echo "================================================================================"
}
trap on_exit EXIT

log "fast-iter-suite start — results=$RESULTS_DIR"
preflight

# DPDK NIC exclusivity: must be strictly sequential across DPDK/fstack arms,
# but the helpers themselves already serialize correctly because each is a
# `run_one` invocation.
run_bench_rtt
run_bench_tx_burst
run_bench_tx_maxtp
run_bench_rx_burst
run_verify_rack_tlp

exit 0
