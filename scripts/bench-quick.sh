#!/usr/bin/env bash
# bench-quick.sh — fast local bench pair for iterative dev testing.
#
# THIS machine acts as the DPDK DUT (data NIC 0000:00:06.0, IP 10.4.1.141).
# A fresh peer EC2 instance runs echo-server in the same subnet.
# Iteration time: ~3-5 min (vs 60+ min for full bench-nightly).
#
# Usage:
#   ./scripts/bench-quick.sh setup               spin up peer + bind DUT NIC
#   ./scripts/bench-quick.sh run [workload] ...   build + run bench
#   ./scripts/bench-quick.sh teardown             terminate peer + rebind DUT NIC
#
# Env overrides:
#   BENCH_ITERATIONS   per-bucket iterations (default: 200)
#   BENCH_WARMUP       warmup iterations     (default: 20)
#   AWS_PROFILE        AWS credential profile (default: resd-infra-operator)

set -euo pipefail

# ── DUT ───────────────────────────────────────────────────────────────────────
DUT_IP="10.4.1.141"
DUT_GATEWAY="10.4.1.1"
DUT_PCI="0000:00:06.0"
DUT_INSTANCE_ID="i-0a6e844d6af751c1f"
DUT_EAL_ARGS="-l 2-3 -n 4 --in-memory --huge-unlink -a ${DUT_PCI},large_llq_hdr=1,miss_txc_to=3"
DUT_LCORE=2
DUT_NIC_MAX_BPS=10000000000   # c6a.2xlarge baseline: 10 Gbps

# ── Peer ──────────────────────────────────────────────────────────────────────
PEER_AMI="ami-0e483926d07d19647"
PEER_SUBNET="subnet-05d4a1cf65e5df23c"
PEER_SG="sg-093d563579a51ca88"
PEER_INSTANCE_TYPE="c6a.xlarge"
PEER_ECHO_PORT=10001

# ── AWS / SSH ─────────────────────────────────────────────────────────────────
AWS_PROFILE="${AWS_PROFILE:-resd-infra-operator}"
AWS_REGION="${AWS_REGION:-ap-south-1}"
SSH_KEY="${HOME}/.ssh/id_ed25519"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -o ProxyCommand=none -i "$SSH_KEY")
STATE_FILE="/tmp/bench-quick-state.json"

# ── Bench params ──────────────────────────────────────────────────────────────
# burst workload
BENCH_BURSTS_PER_BUCKET="${BENCH_BURSTS_PER_BUCKET:-200}"
BENCH_BURST_WARMUP="${BENCH_BURST_WARMUP:-20}"
# maxtp workload
BENCH_MAXTP_WARMUP_SECS="${BENCH_MAXTP_WARMUP_SECS:-3}"
BENCH_MAXTP_DURATION_SECS="${BENCH_MAXTP_DURATION_SECS:-10}"

WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
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
    [ -f "$STATE_FILE" ] || die "No state file — run setup first"
    PEER_IP=$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['peer_ip'])")
    PEER_INSTANCE_ID=$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['instance_id'])")
}

# ── setup ─────────────────────────────────────────────────────────────────────
cmd_setup() {
    log "=== bench-quick setup ==="
    cd "$WORKDIR"

    # [1] echo-server
    log "[1/5] Building echo-server peer binary..."
    make -C tools/bench-e2e/peer echo-server

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
    scp "${SSH_OPTS[@]}" tools/bench-e2e/peer/echo-server "ubuntu@${peer_ip}:/tmp/echo-server"

    push_ic_pubkey "$instance_id"
    ssh "${SSH_OPTS[@]}" "ubuntu@$peer_ip" \
        "chmod +x /tmp/echo-server; nohup /tmp/echo-server ${PEER_ECHO_PORT} >/tmp/echo-server.log 2>&1 </dev/null & sleep 1; pgrep -a echo-server && echo ok"
    log "  echo-server listening on :${PEER_ECHO_PORT}"

    # Save state
    python3 -c "
import json
json.dump({'instance_id': '$instance_id', 'peer_ip': '$peer_ip'}, open('$STATE_FILE', 'w'))
"
    log "State → $STATE_FILE"
    log "=== Setup complete ==="
    log "    DUT:  $DUT_IP  (this machine, data NIC vfio-pci)"
    log "    Peer: $peer_ip (echo-server :${PEER_ECHO_PORT})"
    log "    Run:  ./scripts/bench-quick.sh run [burst|maxtp]"
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
    # workload; one --stack per invocation.
    local bin
    case "$workload" in
        burst) bin=bench-tx-burst ;;
        maxtp) bin=bench-tx-maxtp ;;
        *)
            log "ERROR unknown workload `$workload` (valid: burst, maxtp)"
            return 2
            ;;
    esac

    log "[1/2] Building $bin (incremental)..."
    cargo build --release -p "$bin" 2>&1 \
        | grep -E "^error|Compiling $bin|Finished" | tail -5

    local csv_out="/tmp/bench-quick-${workload}.csv"
    log "[2/2] Running $bin ${workload} → $csv_out"

    local workload_args=()
    case "$workload" in
        burst)
            workload_args+=(--bursts-per-bucket "$BENCH_BURSTS_PER_BUCKET"
                            --warmup            "$BENCH_BURST_WARMUP") ;;
        maxtp)
            workload_args+=(--warmup-secs   "$BENCH_MAXTP_WARMUP_SECS"
                            --duration-secs "$BENCH_MAXTP_DURATION_SECS") ;;
    esac

    sudo "target/release/$bin" \
        --stack               dpdk_net \
        --local-ip            "$DUT_IP" \
        --gateway-ip          "$DUT_GATEWAY" \
        --peer-ip             "$PEER_IP" \
        --peer-port           "$PEER_ECHO_PORT" \
        --eal-args            "$DUT_EAL_ARGS" \
        --lcore               "$DUT_LCORE" \
        "${workload_args[@]}" \
        --nic-max-bps         "$DUT_NIC_MAX_BPS" \
        --tool                "$bin" \
        --feature-set         trading-latency \
        --precondition-mode   lenient \
        --output-csv          "$csv_out" \
        "$@"
    log "CSV written: $csv_out"
}

# ── teardown ──────────────────────────────────────────────────────────────────
cmd_teardown() {
    log "=== bench-quick teardown ==="

    if [ -f "$STATE_FILE" ]; then
        local instance_id
        instance_id=$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['instance_id'])")
        log "Terminating peer $instance_id..."
        aws ec2 terminate-instances --instance-ids "$instance_id" --output text >/dev/null
        rm -f "$STATE_FILE"
        log "Peer terminated, state cleared"
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
  setup               spin up peer EC2 + bind DUT data NIC to vfio-pci
  run [workload] ...  incremental build + run bench-tx-burst (workload=burst)
                      or bench-tx-maxtp (workload=maxtp). Default: burst.
                      Remaining args forwarded to the selected binary.
  teardown            terminate peer instance + rebind DUT NIC to ena

Env overrides (export before calling):
  BENCH_BURSTS_PER_BUCKET=200   burst: bursts per bucket post-warmup
  BENCH_BURST_WARMUP=20         burst: discarded warm-up bursts
  BENCH_MAXTP_WARMUP_SECS=3     maxtp: warmup duration in seconds
  BENCH_MAXTP_DURATION_SECS=10  maxtp: measurement duration in seconds
  AWS_PROFILE=resd-infra-operator

Examples:
  ./scripts/bench-quick.sh setup
  ./scripts/bench-quick.sh run burst
  ./scripts/bench-quick.sh run maxtp
  BENCH_ITERATIONS=1000 ./scripts/bench-quick.sh run burst
  ./scripts/bench-quick.sh teardown
EOF
        exit 1
        ;;
esac
