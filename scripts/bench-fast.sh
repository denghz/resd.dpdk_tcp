#!/usr/bin/env bash
# bench-fast.sh — 3-arm dev-verification wrapper around the fast-iter peer.
#
# Quicker than `fast-iter-suite.sh` (which runs ~7 min/run across all
# stacks) but covers the three latency-relevant arms — bench-rtt,
# bench-tx-burst, bench-rx-burst — at small loop counts. Total wallclock
# target: 3-5 min.
#
# Only the dpdk_net arm is driven; the comparison stack is held fixed.
# Sources the persistent peer described in
# `/home/ubuntu/resd.dpdk_tcp-a10-perf/.fast-iter.env` (provisioned via
# `scripts/fast-iter-setup.sh up`).
#
# DUT NIC PCI: `0000:28:00.0` (a10-perf c7i-flex.2xlarge — NOT the AMI-bake
# host's 0000:00:06.0).
#
# Outputs:
#   /tmp/bench-fast/rtt.csv
#   /tmp/bench-fast/tx-burst.csv
#   /tmp/bench-fast/rx-burst.csv
#
# Each arm prints a one-line summary after the CSV is written:
#   bench-rtt       mean +/- p99 per payload
#   bench-tx-burst  burst_initiation_ns mean + p999 per K x G cell
#   bench-rx-burst  latency_ns mean per W x N cell
#
# Exit 0 on success, non-zero if any arm fails.

set -euo pipefail

WORKDIR="/home/ubuntu/resd.dpdk_tcp-a10-perf"
FAST_ITER_ENV="$WORKDIR/.fast-iter.env"
OUT_DIR="/tmp/bench-fast"

# ── DUT ───────────────────────────────────────────────────────────────────────
DUT_IP="10.4.1.141"
DUT_GATEWAY="10.4.1.1"
DUT_PCI="0000:28:00.0"
DUT_LCORE="2"
DUT_EAL_ARGS="-l 2-3 -n 4 --in-memory --huge-unlink -a ${DUT_PCI},large_llq_hdr=1,miss_txc_to=3"

# Lenient precondition mode: dev host has some known/permanent gaps
# (governor reads unreadable, RSS-on deferred) — match the modes that
# fast-iter-suite.sh uses for the same reason.
PRECONDITION_MODE="lenient"

# ── Helpers ───────────────────────────────────────────────────────────────────
log() { printf '[bench-fast %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() { printf '[bench-fast] ERROR: %s\n' "$*" >&2; exit 1; }

# Sweep zombie bench processes + clear stale hugepages. CRITICAL: the
# `pkill -f` pattern matches `/target/release/bench-` (with the full
# path prefix) so that we never kill the wrapping shell or sibling tooling
# whose process name happens to contain `bench-`.
reset_dpdk_state() {
    log "reset_dpdk_state: sweep zombie /target/release/bench-* + clear stale hugepages"
    sudo pkill -9 -f '/target/release/bench-' 2>/dev/null || true
    sleep 2  # let kernel release DMA mappings / IOMMU
    sudo rm -f /dev/hugepages/* 2>/dev/null || true
    sleep 1
}

# Source the persistent peer env. Fail loudly if missing — bench-fast.sh
# is a dev-loop tool that assumes a fast-iter peer is already up.
load_peer() {
    [ -f "$FAST_ITER_ENV" ] \
        || die "Missing $FAST_ITER_ENV — provision a fast-iter peer first (./scripts/fast-iter-setup.sh up)"
    # shellcheck disable=SC1090
    source "$FAST_ITER_ENV"
    : "${PEER_IP:?PEER_IP missing in $FAST_ITER_ENV}"
    : "${PEER_ECHO_PORT:?PEER_ECHO_PORT missing in $FAST_ITER_ENV}"
    : "${PEER_BURST_PORT:?PEER_BURST_PORT missing in $FAST_ITER_ENV}"
    log "peer: $PEER_IP  echo=:$PEER_ECHO_PORT  burst=:$PEER_BURST_PORT"
}

# Verify the required bench binaries exist; surface a build hint if not.
require_bins() {
    local missing=()
    for bin in bench-rtt bench-tx-burst bench-rx-burst; do
        [ -x "$WORKDIR/target/release/$bin" ] || missing+=("$bin")
    done
    if [ "${#missing[@]}" -gt 0 ]; then
        die "missing bench binaries: ${missing[*]} — run \`cargo build --release --features fstack\` from $WORKDIR"
    fi
}

# ── Summary printers ──────────────────────────────────────────────────────────
# Each printer takes a CSV path and emits one line of headline metrics
# parsed via python3 (csv + json stdlib only — no extra deps). Failure
# to parse is non-fatal so the run isn't lost.

summarise_rtt() {
    local csv="$1"
    python3 - "$csv" <<'PY' || log "  (summary parse failed — see $csv)"
import csv, json, sys, collections
csv_path = sys.argv[1]
# bucket -> agg -> value
buckets = collections.defaultdict(dict)
with open(csv_path, newline="") as fh:
    for row in csv.DictReader(fh):
        if row.get("metric_name") != "rtt_ns":
            continue
        dim = json.loads(row["dimensions_json"])
        payload = dim.get("payload_bytes")
        agg = row["metric_aggregation"]
        try:
            buckets[payload][agg] = float(row["metric_value"])
        except ValueError:
            continue
parts = []
for payload in sorted(buckets, key=lambda p: int(p) if p is not None else 0):
    b = buckets[payload]
    mean = b.get("mean")
    p99 = b.get("p99")
    if mean is None or p99 is None:
        continue
    parts.append(f"P{payload}: mean={mean/1e3:.1f}us p99={p99/1e3:.1f}us")
if parts:
    print("  RTT  " + "  |  ".join(parts))
else:
    print("  RTT  (no rtt_ns rows parsed)")
PY
}

summarise_tx_burst() {
    local csv="$1"
    python3 - "$csv" <<'PY' || log "  (summary parse failed — see $csv)"
import csv, json, sys, collections
csv_path = sys.argv[1]
# (K,G) -> agg -> value
cells = collections.defaultdict(dict)
with open(csv_path, newline="") as fh:
    for row in csv.DictReader(fh):
        if row.get("metric_name") != "burst_initiation_ns":
            continue
        dim = json.loads(row["dimensions_json"])
        k = dim.get("K_bytes")
        g = dim.get("G_ms")
        agg = row["metric_aggregation"]
        try:
            cells[(k, g)][agg] = float(row["metric_value"])
        except ValueError:
            continue
parts = []
for (k, g) in sorted(cells, key=lambda kg: (int(kg[0] or 0), float(kg[1] or 0.0))):
    c = cells[(k, g)]
    mean = c.get("mean")
    p999 = c.get("p999")
    if mean is None or p999 is None:
        continue
    parts.append(f"K={k} G={g}: mean={mean:.0f}ns p999={p999:.0f}ns")
if parts:
    print("  TX-BURST  burst_initiation_ns: " + "  |  ".join(parts))
else:
    print("  TX-BURST  (no burst_initiation_ns rows parsed)")
PY
}

summarise_rx_burst() {
    local csv="$1"
    python3 - "$csv" <<'PY' || log "  (summary parse failed — see $csv)"
import csv, json, sys, collections
csv_path = sys.argv[1]
# (W,N) -> agg -> value
cells = collections.defaultdict(dict)
with open(csv_path, newline="") as fh:
    for row in csv.DictReader(fh):
        if row.get("metric_name") != "latency_ns":
            continue
        dim = json.loads(row["dimensions_json"])
        w = dim.get("segment_size_bytes")
        n = dim.get("burst_count")
        agg = row["metric_aggregation"]
        try:
            cells[(w, n)][agg] = float(row["metric_value"])
        except ValueError:
            continue
parts = []
for (w, n) in sorted(cells, key=lambda wn: (int(wn[0] or 0), int(wn[1] or 0))):
    mean = cells[(w, n)].get("mean")
    if mean is None:
        continue
    parts.append(f"W={w} N={n}: mean={mean/1e3:.1f}us")
if parts:
    print("  RX-BURST  latency_ns: " + "  |  ".join(parts))
else:
    print("  RX-BURST  (no latency_ns rows parsed)")
PY
}

# ── Arm runners ───────────────────────────────────────────────────────────────
run_rtt() {
    local csv="$OUT_DIR/rtt.csv"
    log "[1/3] bench-rtt → $csv"
    reset_dpdk_state
    sudo "$WORKDIR/target/release/bench-rtt" \
        --stack               dpdk_net \
        --local-ip            "$DUT_IP" \
        --gateway-ip          "$DUT_GATEWAY" \
        --peer-ip             "$PEER_IP" \
        --peer-port           "$PEER_ECHO_PORT" \
        --eal-args            "$DUT_EAL_ARGS" \
        --lcore               "$DUT_LCORE" \
        --iterations          5000 \
        --warmup              100 \
        --payload-bytes-sweep "64,128,256,1024" \
        --connections         1 \
        --tool                bench-rtt \
        --feature-set         trading-latency \
        --precondition-mode   "$PRECONDITION_MODE" \
        --output-csv          "$csv"
    summarise_rtt "$csv"
}

run_tx_burst() {
    local csv="$OUT_DIR/tx-burst.csv"
    log "[2/3] bench-tx-burst → $csv"
    reset_dpdk_state
    sudo "$WORKDIR/target/release/bench-tx-burst" \
        --stack               dpdk_net \
        --local-ip            "$DUT_IP" \
        --gateway-ip          "$DUT_GATEWAY" \
        --peer-ip             "$PEER_IP" \
        --peer-port           "$PEER_ECHO_PORT" \
        --eal-args            "$DUT_EAL_ARGS" \
        --lcore               "$DUT_LCORE" \
        --bursts-per-bucket   2000 \
        --warmup              100 \
        --burst-sizes         "65536,1048576" \
        --gap-mss             "0,10" \
        --tool                bench-tx-burst \
        --feature-set         trading-latency \
        --precondition-mode   "$PRECONDITION_MODE" \
        --output-csv          "$csv"
    summarise_tx_burst "$csv"
}

run_rx_burst() {
    local csv="$OUT_DIR/rx-burst.csv"
    log "[3/3] bench-rx-burst → $csv"
    reset_dpdk_state
    sudo "$WORKDIR/target/release/bench-rx-burst" \
        --stack               dpdk_net \
        --local-ip            "$DUT_IP" \
        --gateway-ip          "$DUT_GATEWAY" \
        --peer-ip             "$PEER_IP" \
        --peer-control-port   "$PEER_BURST_PORT" \
        --eal-args            "$DUT_EAL_ARGS" \
        --lcore               "$DUT_LCORE" \
        --measure-bursts      500 \
        --warmup-bursts       100 \
        --segment-sizes       "64,128,256" \
        --burst-counts        "16,64,256" \
        --tool                bench-rx-burst \
        --feature-set         trading-latency \
        --precondition-mode   "$PRECONDITION_MODE" \
        --output-csv          "$csv"
    summarise_rx_burst "$csv"
}

# ── Main ──────────────────────────────────────────────────────────────────────
main() {
    log "=== bench-fast: 3-arm dev verification (dpdk_net) ==="
    mkdir -p "$OUT_DIR"
    require_bins
    load_peer
    # Up-front state reset so the first arm starts from a clean slate.
    reset_dpdk_state
    local t0
    t0="$(date +%s)"

    run_rtt
    run_tx_burst
    run_rx_burst

    local t1
    t1="$(date +%s)"
    log "=== bench-fast complete in $((t1 - t0))s — CSVs under $OUT_DIR/ ==="
}

main "$@"
