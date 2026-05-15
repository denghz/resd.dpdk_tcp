#!/usr/bin/env bash
# bench-quick.sh — fast local bench pair for iterative dev testing.
#
# THIS machine acts as the DPDK DUT (data NIC bound to vfio-pci).
# Default DUT_PCI is 0000:28:00.0 (a10-perf c7i-flex.2xlarge); override
# via env for other hosts (e.g. the AMI-bake c6a.2xlarge uses 0000:00:06.0).
# Iteration time: ~3-5 min (vs 60+ min for full bench-nightly).
#
# Peer source: if `$WORKDIR/.fast-iter.env` is present (provisioned via
# `scripts/fast-iter-setup.sh up`), it is reused as the peer — no AWS
# credentials needed and `setup` just binds the local NIC + hugepages.
# Otherwise `setup` launches a fresh c6a.xlarge peer via the AWS CLI.
#
# Usage:
#   ./scripts/bench-quick.sh setup
#   ./scripts/bench-quick.sh run [burst|maxtp|rtt|rx-burst] [extra-args...]
#   ./scripts/bench-quick.sh teardown
#
# Workloads (all --stack dpdk_net; see fast-iter-suite.sh for cross-stack):
#   burst     bench-tx-burst    K × G one-shot burst grid     (echo-server :10001)
#   maxtp     bench-tx-maxtp    W × C sustained-rate grid     (echo-server :10001)
#   rtt       bench-rtt         req/resp p50/p99 payload sweep (echo-server :10001)
#   rx-burst  bench-rx-burst    RX-side W × N segment-absorb   (burst-echo-server :10003)
#
# Env overrides:
#   DUT_IP                     DUT data-NIC IPv4              (default: 10.4.1.141)
#   DUT_GATEWAY                DUT data-NIC gateway IPv4      (default: 10.4.1.1)
#   DUT_PCI                    DUT data-NIC PCI BDF           (default: 0000:28:00.0)
#   DUT_LCORE                  Worker lcore                   (default: 2)
#   DUT_NIC_MAX_BPS            NIC saturation guard (bps)     (default: 10 Gbps)
#   AWS_PROFILE                AWS credential profile (default: resd-infra-operator,
#                              only used when bench-quick launches its own peer)
#   BENCH_BURSTS_PER_BUCKET    burst:    bursts per bucket post-warmup (default: 200)
#   BENCH_BURST_WARMUP         burst:    discarded warm-up bursts      (default: 20)
#   BENCH_MAXTP_WARMUP_SECS    maxtp:    warmup duration s             (default: 3)
#   BENCH_MAXTP_DURATION_SECS  maxtp:    measurement duration s        (default: 10)
#   BENCH_RTT_ITERATIONS       rtt:      iterations per payload        (default: 20000)
#   BENCH_RTT_WARMUP           rtt:      warmup iterations             (default: 100)
#   BENCH_RTT_PAYLOADS         rtt:      payload-bytes sweep CSV       (default: 64,128,256,1024)
#   BENCH_RX_MEASURE_BURSTS    rx-burst: bursts per (W,N) bucket       (default: 1000)
#   BENCH_RX_WARMUP_BURSTS     rx-burst: discarded warm-up bursts      (default: 100)
#   BENCH_RX_SEGMENT_SIZES     rx-burst: W (segment-size) CSV          (default: 64,128,256)
#   BENCH_RX_BURST_COUNTS      rx-burst: N (burst-count) CSV           (default: 16,64,256,4096,10240)
#   BENCH_PRECONDITION_MODE    shared:   strict|lenient                (default: strict)

set -euo pipefail

# ── DUT ───────────────────────────────────────────────────────────────────────
# All DUT params env-overridable (matches fast-iter-suite.sh conventions).
# Defaults are the a10-perf c7i-flex.2xlarge host; override on other DUTs.
DUT_IP="${DUT_IP:-10.4.1.141}"
DUT_GATEWAY="${DUT_GATEWAY:-10.4.1.1}"
DUT_PCI="${DUT_PCI:-0000:28:00.0}"
DUT_LCORE="${DUT_LCORE:-2}"
DUT_EAL_ARGS="-l 2-3 -n 4 --in-memory --huge-unlink -a ${DUT_PCI},large_llq_hdr=1,miss_txc_to=3"
DUT_NIC_MAX_BPS="${DUT_NIC_MAX_BPS:-10000000000}"   # 10 Gbps baseline

# ── Peer ──────────────────────────────────────────────────────────────────────
PEER_AMI="ami-0e483926d07d19647"
PEER_SUBNET="subnet-05d4a1cf65e5df23c"
PEER_SG="sg-093d563579a51ca88"
PEER_INSTANCE_TYPE="c6a.xlarge"
# Ports default to bench-pair convention but are env-overridable so an
# already-sourced .fast-iter.env (which exports the same names) wins.
PEER_ECHO_PORT="${PEER_ECHO_PORT:-10001}"
PEER_BURST_PORT="${PEER_BURST_PORT:-10003}"

# ── AWS / SSH ─────────────────────────────────────────────────────────────────
AWS_PROFILE="${AWS_PROFILE:-resd-infra-operator}"
AWS_REGION="${AWS_REGION:-ap-south-1}"
SSH_KEY="${HOME}/.ssh/id_ed25519"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -o ProxyCommand=none -i "$SSH_KEY")
STATE_FILE="/tmp/bench-quick-state.json"
WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FAST_ITER_ENV="$WORKDIR/.fast-iter.env"

# ── Bench params ──────────────────────────────────────────────────────────────
# burst workload
BENCH_BURSTS_PER_BUCKET="${BENCH_BURSTS_PER_BUCKET:-200}"
BENCH_BURST_WARMUP="${BENCH_BURST_WARMUP:-20}"
# maxtp workload
BENCH_MAXTP_WARMUP_SECS="${BENCH_MAXTP_WARMUP_SECS:-3}"
BENCH_MAXTP_DURATION_SECS="${BENCH_MAXTP_DURATION_SECS:-10}"
# rtt workload — default 20k iters gives ~200 samples in the p99 tail
# (binary default is 100k; 2k was too few for any p99 credibility).
BENCH_RTT_ITERATIONS="${BENCH_RTT_ITERATIONS:-20000}"
BENCH_RTT_WARMUP="${BENCH_RTT_WARMUP:-100}"
BENCH_RTT_PAYLOADS="${BENCH_RTT_PAYLOADS:-64,128,256,1024}"
# rx-burst workload — 1000 bursts × 16-256 segments ≈ 16k-256k samples
# per bucket; matches the per-cell budget bench-nightly uses under netem.
BENCH_RX_MEASURE_BURSTS="${BENCH_RX_MEASURE_BURSTS:-1000}"
BENCH_RX_WARMUP_BURSTS="${BENCH_RX_WARMUP_BURSTS:-100}"
BENCH_RX_SEGMENT_SIZES="${BENCH_RX_SEGMENT_SIZES:-64,128,256}"
BENCH_RX_BURST_COUNTS="${BENCH_RX_BURST_COUNTS:-16,64,256,4096,10240}"

# Precondition gate (shared across all workloads). Strict by default to
# match nightly behaviour — flips precondition violations from warnings
# to aborts so a drifted DUT can't silently taint a perf comparison.
# Drop to lenient if the dev host has known/permanent precondition gaps.
BENCH_PRECONDITION_MODE="${BENCH_PRECONDITION_MODE:-strict}"

export AWS_PROFILE AWS_REGION

# ── Helpers ───────────────────────────────────────────────────────────────────
log() { printf '[bench-quick %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() { printf '[bench-quick] ERROR: %s\n' "$*" >&2; exit 1; }

push_ic_pubkey() {
    local instance_id="$1"
    aws ec2-instance-connect send-ssh-public-key \
        --instance-id "$instance_id" \
        --instance-os-user ubuntu \
        --ssh-public-key "file://${SSH_KEY}.pub" \
        --output text --query 'Success' >/dev/null
}

load_state() {
    # Prefer an existing fast-iter peer when `$WORKDIR/.fast-iter.env` is
    # present (provisioned via `scripts/fast-iter-setup.sh up`). This lets
    # bench-quick.sh skip the AWS launch entirely on hosts where a peer
    # has already been brought up for fast-iter — no AWS creds required.
    # Falls back to the bench-quick-private state file from `cmd_setup`.
    if [ -f "$FAST_ITER_ENV" ]; then
        # shellcheck disable=SC1090
        source "$FAST_ITER_ENV"
        : "${PEER_IP:?PEER_IP missing in $FAST_ITER_ENV}"
        : "${PEER_INSTANCE_ID:?PEER_INSTANCE_ID missing in $FAST_ITER_ENV}"
        : "${PEER_ECHO_PORT:?PEER_ECHO_PORT missing in $FAST_ITER_ENV}"
        : "${PEER_BURST_PORT:?PEER_BURST_PORT missing in $FAST_ITER_ENV}"
        PEER_SOURCE="fast-iter"
        return 0
    fi
    [ -f "$STATE_FILE" ] \
        || die "No peer state — run \`./scripts/bench-quick.sh setup\` or provision a fast-iter peer via \`./scripts/fast-iter-setup.sh up\`"
    PEER_IP=$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['peer_ip'])")
    PEER_INSTANCE_ID=$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['instance_id'])")
    PEER_SOURCE="bench-quick"
}

# Sweep zombie bench processes + clear stale hugepages left by a prior
# SIGKILL'd EAL init. Cribbed from fast-iter-suite.sh — without this,
# a previous run that Ctrl-C'd mid-init can leave `/dev/hugepages/rtemap_*`
# wedged and the next EAL init either fails or silently maps tainted state.
# Idempotent + safe to call repeatedly. Never aborts the script.
reset_dpdk_state() {
    log "  reset_dpdk_state: sweep zombie bench-* + clear stale hugepages"
    sudo pkill -9 -f '/target/release/bench-(rtt|tx-burst|tx-maxtp|rx-burst)' 2>/dev/null || true
    sleep 2  # let kernel release DMA mappings / IOMMU
    sudo rm -f /dev/hugepages/* 2>/dev/null || true
}

# ── setup ─────────────────────────────────────────────────────────────────────
cmd_setup() {
    log "=== bench-quick setup ==="
    cd "$WORKDIR"

    # If a fast-iter peer is already provisioned, reuse it: skip the AWS
    # launch path entirely and just do the local DUT prep. Saves ~2 min
    # per setup and avoids needing AWS credentials when a peer is already
    # running. The fast-iter peer must already have echo-server + burst-
    # echo-server listening (fast-iter-setup.sh up does that).
    local reuse_fast_iter=0
    if [ -f "$FAST_ITER_ENV" ]; then
        reuse_fast_iter=1
        log "  Detected $FAST_ITER_ENV — reusing existing fast-iter peer (skipping AWS launch)"
    fi

    # [1] peer C binaries (skipped when reusing fast-iter peer: fast-iter-
    # setup.sh already built + deployed them onto the live peer).
    if [ "$reuse_fast_iter" -eq 0 ]; then
        log "[1/5] Building peer binaries (echo-server + burst-echo-server)..."
        make -C tools/bench-e2e/peer echo-server burst-echo-server
    else
        log "[1/5] Skipping peer-binary build (fast-iter peer already provisioned)"
    fi

    # [2] hugepages
    log "[2/5] Checking hugepages..."
    local hp_current hp_needed=1024
    hp_current=$(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages)
    if [ "$hp_current" -lt "$hp_needed" ]; then
        log "  Allocating hugepages: $hp_current → $hp_needed"
        echo "$hp_needed" | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages >/dev/null
    else
        log "  Hugepages OK: $hp_current × 2MB"
    fi

    # [3] DUT NIC → vfio-pci
    log "[3/5] Binding DUT data NIC $DUT_PCI to vfio-pci..."
    local current_drv
    current_drv=$(readlink /sys/bus/pci/devices/${DUT_PCI}/driver 2>/dev/null | xargs -r basename || echo "unbound")
    if [ "$current_drv" = "vfio-pci" ]; then
        log "  Already vfio-pci"
    else
        log "  Was $current_drv — binding to vfio-pci"
        sudo /usr/local/bin/dpdk-devbind.py --bind vfio-pci "$DUT_PCI"
        log "  Bound"
    fi

    if [ "$reuse_fast_iter" -eq 1 ]; then
        # shellcheck disable=SC1090
        source "$FAST_ITER_ENV"
        log "=== Setup complete (fast-iter peer reused) ==="
        log "    DUT:  $DUT_IP  ($DUT_PCI, vfio-pci)"
        log "    Peer: $PEER_IP (echo-server :${PEER_ECHO_PORT}, burst-echo-server :${PEER_BURST_PORT})"
        log "    Note: \`bench-quick.sh teardown\` will NOT terminate the fast-iter peer."
        log "          Use \`./scripts/fast-iter-setup.sh down\` to terminate it."
        log "    Run:  ./scripts/bench-quick.sh run [burst|maxtp|rtt|rx-burst]"
        return 0
    fi

    # [4] launch peer
    log "[4/5] Launching peer EC2 instance ($PEER_INSTANCE_TYPE)..."
    local launch_json instance_id
    launch_json=$(aws ec2 run-instances \
        --image-id   "$PEER_AMI" \
        --instance-type "$PEER_INSTANCE_TYPE" \
        --subnet-id  "$PEER_SUBNET" \
        --security-group-ids "$PEER_SG" \
        --no-associate-public-ip-address \
        --count 1 \
        --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=bench-quick-peer}]" \
        --output json)
    instance_id=$(python3 -c "import sys,json; print(json.load(sys.stdin)['Instances'][0]['InstanceId'])" <<<"$launch_json")
    log "  $instance_id — waiting for running..."
    aws ec2 wait instance-running --instance-ids "$instance_id"

    local peer_ip
    peer_ip=$(aws ec2 describe-instances --instance-ids "$instance_id" \
        --query 'Reservations[0].Instances[0].PrivateIpAddress' --output text)
    log "  Peer IP: $peer_ip"

    # [5] SSH + peer-prep + echo-server
    log "[5/5] Waiting for SSH on peer $peer_ip..."
    local retries=0
    until push_ic_pubkey "$instance_id" && \
          ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes "ubuntu@$peer_ip" true 2>/dev/null; do
        retries=$((retries + 1))
        [ "$retries" -ge 72 ] && die "SSH timeout after 6 min — check security group / EC2 IC IAM perms"
        [ $((retries % 6)) -eq 0 ] && log "  Still waiting... ($((retries * 5))s elapsed)"
        sleep 5
    done
    log "  SSH ready — running peer-prep..."

    # Rebind any vfio-pci NIC left by AMI boot; open firewall.
    push_ic_pubkey "$instance_id"
    ssh "${SSH_OPTS[@]}" "ubuntu@$peer_ip" 'sudo bash -s' <<'REMOTE_EOF'
set -eu
PCI=$(/usr/local/bin/dpdk-devbind.py --status-dev net 2>/dev/null | awk '/drv=vfio-pci/ {print $1; exit}' || true)
if [ -n "$PCI" ]; then
    echo "peer-prep: rebinding $PCI vfio-pci → ena"
    /usr/local/bin/dpdk-devbind.py --bind ena "$PCI"
    sleep 1
else
    echo "peer-prep: no vfio-pci NIC (already ena)"
fi
iptables -I INPUT -j ACCEPT 2>/dev/null || true
echo "peer-prep: done"
REMOTE_EOF

    push_ic_pubkey "$instance_id"
    scp "${SSH_OPTS[@]}" \
        tools/bench-e2e/peer/echo-server \
        tools/bench-e2e/peer/burst-echo-server \
        "ubuntu@${peer_ip}:/tmp/"

    push_ic_pubkey "$instance_id"
    ssh "${SSH_OPTS[@]}" "ubuntu@$peer_ip" \
        "chmod +x /tmp/echo-server /tmp/burst-echo-server; \
         nohup /tmp/echo-server ${PEER_ECHO_PORT} >/tmp/echo-server.log 2>&1 </dev/null & \
         nohup /tmp/burst-echo-server ${PEER_BURST_PORT} >/tmp/burst-echo-server.log 2>&1 </dev/null & \
         sleep 1; pgrep -a echo-server && pgrep -a burst-echo-server && echo ok"
    log "  echo-server listening on :${PEER_ECHO_PORT}"
    log "  burst-echo-server listening on :${PEER_BURST_PORT}"

    # Save state
    python3 -c "
import json
json.dump({'instance_id': '$instance_id', 'peer_ip': '$peer_ip'}, open('$STATE_FILE', 'w'))
"
    log "State → $STATE_FILE"
    log "=== Setup complete ==="
    log "    DUT:  $DUT_IP  (this machine, data NIC vfio-pci)"
    log "    Peer: $peer_ip (echo-server :${PEER_ECHO_PORT}, burst-echo-server :${PEER_BURST_PORT})"
    log "    Run:  ./scripts/bench-quick.sh run [burst|maxtp|rtt|rx-burst]"
}

# ── run ───────────────────────────────────────────────────────────────────────
cmd_run() {
    local workload="${1:-burst}"; [ $# -gt 0 ] && shift || true
    load_state
    cd "$WORKDIR"

    log "=== bench-quick run: workload=$workload peer=$PEER_IP ==="

    # Phase 5 of the 2026-05-09 bench-suite overhaul split the legacy
    # bench-vs-mtcp into bench-tx-burst (one-shot K x G grid) and
    # bench-tx-maxtp (sustained-rate W x C grid). One binary per
    # workload; one --stack per invocation. `rtt` + `rx-burst` added
    # later so a single dev loop covers TX-burst / TX-maxtp / RTT / RX
    # without dropping to fast-iter-suite (~35 min).
    local bin
    case "$workload" in
        burst)    bin=bench-tx-burst ;;
        maxtp)    bin=bench-tx-maxtp ;;
        rtt)      bin=bench-rtt ;;
        rx-burst) bin=bench-rx-burst ;;
        *)
            log "ERROR unknown workload \`$workload\` (valid: burst, maxtp, rtt, rx-burst)"
            return 2
            ;;
    esac

    log "[1/3] Building $bin (incremental)..."
    cargo build --release -p "$bin" 2>&1 \
        | grep -E "^error|Compiling $bin|Finished" | tail -5

    log "[2/3] Resetting DPDK state (zombie sweep + hugepage clear)..."
    reset_dpdk_state

    # Tail-credibility warning: if iteration count is below the floor for
    # p99 stability, print a banner so the operator doesn't read p99/p999
    # CSV columns as meaningful. ~10k samples gets ~100 samples in the
    # p99 tail — workable. Below that, p99 is mostly noise.
    case "$workload" in
        rtt)
            if [ "$BENCH_RTT_ITERATIONS" -lt 10000 ]; then
                log "  WARN BENCH_RTT_ITERATIONS=$BENCH_RTT_ITERATIONS < 10000 — p99/p999 columns will be noisy; only trust p50/mean"
            fi
            ;;
        rx-burst)
            if [ "$BENCH_RX_MEASURE_BURSTS" -lt 500 ]; then
                log "  WARN BENCH_RX_MEASURE_BURSTS=$BENCH_RX_MEASURE_BURSTS < 500 — p99/p999 will be noisy; only trust p50/mean"
            fi
            ;;
    esac

    local csv_out="/tmp/bench-quick-${workload}.csv"
    log "[3/3] Running $bin ${workload} → $csv_out"

    # Per-workload args: peer-port vs peer-control-port and the
    # nic-max-bps saturation guard differ across bins. Common
    # stack-level args (--stack, --local-ip, --eal-args, …) stay
    # shared in the invocation below.
    local workload_args=()
    case "$workload" in
        burst)
            workload_args+=(--peer-port         "$PEER_ECHO_PORT"
                            --nic-max-bps       "$DUT_NIC_MAX_BPS"
                            --bursts-per-bucket "$BENCH_BURSTS_PER_BUCKET"
                            --warmup            "$BENCH_BURST_WARMUP") ;;
        maxtp)
            workload_args+=(--peer-port         "$PEER_ECHO_PORT"
                            --nic-max-bps       "$DUT_NIC_MAX_BPS"
                            --warmup-secs       "$BENCH_MAXTP_WARMUP_SECS"
                            --duration-secs     "$BENCH_MAXTP_DURATION_SECS") ;;
        rtt)
            workload_args+=(--peer-port         "$PEER_ECHO_PORT"
                            --connections       1
                            --payload-bytes-sweep "$BENCH_RTT_PAYLOADS"
                            --iterations        "$BENCH_RTT_ITERATIONS"
                            --warmup            "$BENCH_RTT_WARMUP") ;;
        rx-burst)
            workload_args+=(--peer-control-port "$PEER_BURST_PORT"
                            --segment-sizes     "$BENCH_RX_SEGMENT_SIZES"
                            --burst-counts      "$BENCH_RX_BURST_COUNTS"
                            --warmup-bursts     "$BENCH_RX_WARMUP_BURSTS"
                            --measure-bursts    "$BENCH_RX_MEASURE_BURSTS") ;;
    esac

    sudo "target/release/$bin" \
        --stack               dpdk_net \
        --local-ip            "$DUT_IP" \
        --gateway-ip          "$DUT_GATEWAY" \
        --peer-ip             "$PEER_IP" \
        --eal-args            "$DUT_EAL_ARGS" \
        --lcore               "$DUT_LCORE" \
        "${workload_args[@]}" \
        --tool                "$bin" \
        --feature-set         trading-latency \
        --precondition-mode   "$BENCH_PRECONDITION_MODE" \
        --output-csv          "$csv_out" \
        "$@"
    log "CSV written: $csv_out"
}

# ── teardown ──────────────────────────────────────────────────────────────────
cmd_teardown() {
    log "=== bench-quick teardown ==="

    # Only terminate a peer that bench-quick.sh itself launched (i.e. the
    # private state file is present). Fast-iter peers are owned by
    # fast-iter-setup.sh and must be torn down through it — terminating one
    # here would invalidate someone else's iteration loop.
    if [ -f "$STATE_FILE" ]; then
        local instance_id
        instance_id=$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['instance_id'])")
        log "Terminating peer $instance_id..."
        aws ec2 terminate-instances --instance-ids "$instance_id" --output text >/dev/null
        rm -f "$STATE_FILE"
        log "Peer terminated, state cleared"
    elif [ -f "$FAST_ITER_ENV" ]; then
        log "Fast-iter peer detected ($FAST_ITER_ENV) — leaving it running"
        log "  Use \`./scripts/fast-iter-setup.sh down\` to terminate the fast-iter peer"
    else
        log "No state file — nothing to terminate"
    fi

    log "Rebinding data NIC $DUT_PCI → ena..."
    local current_drv
    current_drv=$(readlink /sys/bus/pci/devices/${DUT_PCI}/driver 2>/dev/null | xargs -r basename || echo "unbound")
    if [ "$current_drv" = "vfio-pci" ]; then
        sudo /usr/local/bin/dpdk-devbind.py --bind ena "$DUT_PCI"
        log "  Rebound to ena"
    else
        log "  Already $current_drv — nothing to do"
    fi

    log "=== Teardown complete ==="
}

# ── main ──────────────────────────────────────────────────────────────────────
case "${1:-help}" in
    setup)    cmd_setup ;;
    run)      shift; cmd_run "$@" ;;
    teardown) cmd_teardown ;;
    *)
        cat >&2 <<'EOF'
Usage: bench-quick.sh <command> [args]

Commands:
  setup               bind DUT data NIC to vfio-pci + ensure peer is ready.
                      If `.fast-iter.env` is present, that peer is reused
                      (no AWS launch needed); otherwise a fresh c6a.xlarge
                      peer is launched via the AWS CLI.
  run [workload] ...  incremental build + run the selected workload (default: burst).
                      Workloads (all --stack dpdk_net):
                        burst     bench-tx-burst    K × G one-shot burst grid
                        maxtp     bench-tx-maxtp    W × C sustained-rate grid
                        rtt       bench-rtt         req/resp p50/p99 payload sweep
                        rx-burst  bench-rx-burst    RX-side W × N segment-absorb
                      Remaining args forwarded to the selected binary.
  teardown            terminate the bench-quick-launched peer + rebind DUT
                      NIC to ena. Leaves fast-iter peers alone (use
                      `./scripts/fast-iter-setup.sh down` for those).

Env overrides (export before calling):
  DUT_IP=10.4.1.141                DUT data-NIC IPv4
  DUT_GATEWAY=10.4.1.1             DUT data-NIC gateway IPv4
  DUT_PCI=0000:28:00.0             DUT data-NIC PCI BDF (a10-perf default)
  DUT_LCORE=2                      Worker lcore
  DUT_NIC_MAX_BPS=10000000000      NIC saturation guard, bps (10 Gbps default)
  AWS_PROFILE=resd-infra-operator  Only used when bench-quick launches its own peer
  BENCH_BURSTS_PER_BUCKET=200      burst:    bursts per bucket post-warmup
  BENCH_BURST_WARMUP=20            burst:    discarded warm-up bursts
  BENCH_MAXTP_WARMUP_SECS=3        maxtp:    warmup duration in seconds
  BENCH_MAXTP_DURATION_SECS=10     maxtp:    measurement duration in seconds
  BENCH_RTT_ITERATIONS=20000       rtt:      iterations per payload
  BENCH_RTT_WARMUP=100             rtt:      warmup iterations
  BENCH_RTT_PAYLOADS=64,128,256,1024  rtt:   payload-bytes sweep CSV
  BENCH_RX_MEASURE_BURSTS=1000     rx-burst: bursts per (W,N) bucket
  BENCH_RX_WARMUP_BURSTS=100       rx-burst: discarded warm-up bursts
  BENCH_RX_SEGMENT_SIZES=64,128,256  rx-burst: W (segment-size) CSV
  BENCH_RX_BURST_COUNTS=16,64,256,4096,10240  rx-burst: N (burst-count) CSV
  BENCH_PRECONDITION_MODE=strict   shared:   strict|lenient (drop to lenient
                                             only on hosts with known precondition gaps)

Examples:
  ./scripts/bench-quick.sh setup
  ./scripts/bench-quick.sh run burst
  ./scripts/bench-quick.sh run maxtp
  ./scripts/bench-quick.sh run rtt
  ./scripts/bench-quick.sh run rx-burst
  BENCH_RTT_PAYLOADS=64,1500 ./scripts/bench-quick.sh run rtt
  ./scripts/bench-quick.sh teardown
EOF
        exit 1
        ;;
esac
