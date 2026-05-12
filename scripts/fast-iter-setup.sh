#!/usr/bin/env bash
# fast-iter-setup.sh — provision a single peer instance for fast bench-iter cycles.
#
# Use the CURRENT host as the DUT (DPDK NIC bind, etc. are operator-managed).
# Spin up ONE peer EC2 instance from the bench-pair AMI running:
#   - echo-server         on :10001   (bench-rtt + bench-tx-burst + bench-tx-maxtp dpdk/fstack arms)
#   - linux-tcp-sink      on :10002   (bench-tx-maxtp linux arm)
#   - burst-echo-server   on :10003   (bench-rx-burst)
#
# Round-trip: ~1-2 min provision, ~30 s teardown — vs. ~6.5h for the full
# scripts/bench-nightly.sh fleet+matrix run.
#
# Usage:
#   ./scripts/fast-iter-setup.sh up                  # provision + start servers; writes .fast-iter.env
#   ./scripts/fast-iter-setup.sh up --with-fstack    # ... plus rebuild bench-{rtt,tx-burst,tx-maxtp,rx-burst}
#                                                    #     with --features fstack and emit a per-DUT
#                                                    #     $HOME/.fast-iter-fstack.conf so `--stack fstack`
#                                                    #     works out of the box. Mirrors bench-nightly step [3/12].
#   ./scripts/fast-iter-setup.sh fstack-conf         # regenerate $HOME/.fast-iter-fstack.conf only (no peer touch)
#   ./scripts/fast-iter-setup.sh down                # tear down peer + clear state
#   ./scripts/fast-iter-setup.sh info                # print peer state + reachability
#   ./scripts/fast-iter-setup.sh sh                  # ssh into the peer (debug)
#
# After `up`:
#   source ./.fast-iter.env
#   sudo ./target/release/bench-rtt --stack dpdk_net --peer-ip "$PEER_IP" \
#       --peer-port 10001 --output-csv /tmp/quick-rtt.csv ...
#   ./scripts/fast-iter-setup.sh down
#
# After `up --with-fstack` (additional flow — the bench-rtt fstack arm needs
# the auto-generated DUT-specific f-stack.conf, NOT /etc/f-stack.conf which
# may point to a stale PCI address):
#   source ./.fast-iter.env
#   sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
#       ./target/release/bench-rtt --stack fstack \
#           --peer-ip "$PEER_IP" --peer-port "$PEER_ECHO_PORT" \
#           --fstack-conf "$FSTACK_CONF" \
#           --payload-bytes-sweep 128 --iterations 1000 --warmup 100 \
#           --output-csv /tmp/fstack-smoke.csv --precondition-mode lenient
#
# State files:
#   $HOME/.bench-fast-iter.state.json   peer instance id + ip + port map
#   $HOME/.fast-iter-fstack.conf        auto-generated F-Stack conf (DUT-NIC + IP detected)
#   ./.fast-iter.env                    sourceable env (PEER_IP, PEER_SSH, *_PORT, FSTACK_CONF)
# Override via $BENCH_FAST_ITER_STATE / $BENCH_FAST_ITER_ENV / $FAST_ITER_FSTACK_CONF.
#
# Env overrides (export before calling `up`):
#   PEER_AMI            AMI ID                   (default: ami-0e483926d07d19647 — bench-pair 1.0.15)
#   PEER_INSTANCE_TYPE  EC2 instance type        (default: c6a.xlarge)
#   PEER_SUBNET         VPC subnet ID            (default: subnet-05d4a1cf65e5df23c)
#   PEER_SG             security group ID        (default: sg-093d563579a51ca88)
#   AWS_PROFILE         AWS credentials profile  (default: resd-infra-operator)
#   AWS_REGION          AWS region               (default: ap-south-1)
#   SSH_KEY             local ssh privkey path   (default: $HOME/.ssh/id_ed25519)
#   FF_PATH             F-Stack install root     (default: /opt/f-stack — checked for libfstack.a)
#
# Why direct `aws ec2 run-instances` and not `resd-aws-infra setup bench-pair`?
#   `bench-pair` provisions the FULL fleet (DUT + peer) — that's the nightly
#   path. Fast-iter wants ONE peer; the current host is the DUT. Mirrors the
#   provisioning shape used by scripts/bench-quick.sh.
set -euo pipefail

mode="${1:-info}"
shift || true

# Parse optional flags after the subcommand. Only --with-fstack is wired
# today; further flags fall through to a friendly error.
WITH_FSTACK=0
for arg in "$@"; do
    case "$arg" in
        --with-fstack) WITH_FSTACK=1 ;;
        *) printf '[fast-iter] ERROR: unknown flag `%s` — expected --with-fstack\n' "$arg" >&2; exit 2 ;;
    esac
done

# ---------------------------------------------------------------------------
# Configuration (env-overridable).
# ---------------------------------------------------------------------------
PEER_AMI="${PEER_AMI:-ami-0e483926d07d19647}"
PEER_SUBNET="${PEER_SUBNET:-subnet-05d4a1cf65e5df23c}"
PEER_SG="${PEER_SG:-sg-093d563579a51ca88}"
PEER_INSTANCE_TYPE="${PEER_INSTANCE_TYPE:-c6a.xlarge}"
ECHO_PORT=10001
SINK_PORT=10002
BURST_PORT=10003

AWS_PROFILE="${AWS_PROFILE:-resd-infra-operator}"
AWS_REGION="${AWS_REGION:-ap-south-1}"
export AWS_PROFILE AWS_REGION

SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -o ProxyCommand=none -i "$SSH_KEY")

STATE_FILE="${BENCH_FAST_ITER_STATE:-$HOME/.bench-fast-iter.state.json}"
WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${BENCH_FAST_ITER_ENV:-$WORKDIR/.fast-iter.env}"
FSTACK_CONF_PATH="${FAST_ITER_FSTACK_CONF:-$HOME/.fast-iter-fstack.conf}"
FSTACK_LIB="${FF_PATH:-/opt/f-stack}/lib/libfstack.a"

# ---------------------------------------------------------------------------
# Helpers.
# ---------------------------------------------------------------------------
log() { printf '[fast-iter %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() { printf '[fast-iter] ERROR: %s\n' "$*" >&2; exit 1; }

# Push the local SSH pubkey into EC2 Instance Connect's 60 s grant window.
push_ic_pubkey() {
    local instance_id="$1"
    aws ec2-instance-connect send-ssh-public-key \
        --instance-id "$instance_id" \
        --instance-os-user ubuntu \
        --ssh-public-key "file://${SSH_KEY}.pub" \
        --output text --query 'Success' >/dev/null
}

require_state() {
    [ -f "$STATE_FILE" ] || die "no state at $STATE_FILE — run '$0 up' first"
    PEER_INSTANCE_ID="$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['instance_id'])")"
    PEER_IP="$(python3 -c "import json; print(json.load(open('$STATE_FILE'))['peer_ip'])")"
}

write_env_file() {
    local instance_id="$1"
    local peer_ip="$2"
    local with_fstack="${3:-0}"
    {
        printf '# fast-iter env — generated by scripts/fast-iter-setup.sh on %s\n' "$(date -u +%FT%TZ)"
        printf '# Source this file (`source ./.fast-iter.env`) before invoking bench tools.\n'
        printf 'export PEER_IP="%s"\n' "$peer_ip"
        printf 'export PEER_SSH="ubuntu@%s"\n' "$peer_ip"
        printf 'export PEER_INSTANCE_ID="%s"\n' "$instance_id"
        printf 'export PEER_ECHO_PORT="%s"\n' "$ECHO_PORT"
        printf 'export PEER_SINK_PORT="%s"\n' "$SINK_PORT"
        printf 'export PEER_BURST_PORT="%s"\n' "$BURST_PORT"
        if [ "$with_fstack" = "1" ]; then
            printf '# F-Stack arm: per-DUT conf auto-generated by `up --with-fstack`.\n'
            printf '# bench-{rtt,tx-burst,tx-maxtp,rx-burst} accept --fstack-conf "$FSTACK_CONF".\n'
            printf 'export FSTACK_CONF="%s"\n' "$FSTACK_CONF_PATH"
        fi
    } >"$ENV_FILE"
}

# Auto-detect the DUT's data NIC PCI address (the row bound to vfio-pci in
# `dpdk-devbind.py --status`). Bench tools only ever drive the vfio-bound NIC;
# the kernel-bound ENA is reserved for SSH. Returns "" if none is bound.
detect_dut_pci() {
    /usr/local/bin/dpdk-devbind.py --status-dev net 2>/dev/null \
        | awk '/drv=vfio-pci/ {print $1; exit}'
}

# Auto-detect the DUT's data NIC IP. EC2 ENIs carry a `device-number` per
# interface; the primary (kernel/SSH) is 0, the data NIC is 1+. We map MAC ->
# device-number via IMDS and pick the first device-number≠0 ENI's local IPv4.
# Falls back to the bench-quick.sh / nightly default 10.4.1.141 if IMDS
# probing fails (e.g. running on a non-EC2 dev host).
detect_dut_ip() {
    local imds_token mac dev_no ip
    imds_token="$(curl -fsS -X PUT "http://169.254.169.254/latest/api/token" \
        -H "X-aws-ec2-metadata-token-ttl-seconds: 60" 2>/dev/null || true)"
    local curl_hdr=()
    [ -n "$imds_token" ] && curl_hdr=(-H "X-aws-ec2-metadata-token: $imds_token")

    local macs
    macs="$(curl -fsS "${curl_hdr[@]}" \
        http://169.254.169.254/latest/meta-data/network/interfaces/macs/ 2>/dev/null || true)"
    if [ -z "$macs" ]; then
        echo "10.4.1.141"
        return
    fi
    while IFS= read -r mac; do
        mac="${mac%/}"
        [ -n "$mac" ] || continue
        dev_no="$(curl -fsS "${curl_hdr[@]}" \
            "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}/device-number" 2>/dev/null || true)"
        if [ -n "$dev_no" ] && [ "$dev_no" != "0" ]; then
            ip="$(curl -fsS "${curl_hdr[@]}" \
                "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}/local-ipv4s" 2>/dev/null \
                | head -n1 | tr -d '[:space:]')"
            if [ -n "$ip" ]; then
                echo "$ip"
                return
            fi
        fi
    done <<<"$macs"
    # No device-number>=1 ENI found — fall back to bench-quick default.
    echo "10.4.1.141"
}

# Compute the default IPv4 gateway for the data NIC's /24. Bench-nightly's
# DUT setup uses `.1` per subnet convention; we apply the same heuristic here
# because the data NIC sits on a separate routing table from the SSH ENI so
# `ip route` on the host doesn't show its default gateway.
infer_gateway() {
    local ip="$1"
    awk -F. '{printf "%s.%s.%s.1", $1, $2, $3}' <<<"$ip"
}

# Write the per-DUT F-Stack conf at $FSTACK_CONF_PATH. Mirrors the [dpdk] /
# [port0] / [freebsd.*] sections that bench-nightly step [6/12] writes onto
# the DUT (see bench-nightly.sh lines 463-510), except:
#   - `allow=` carries this DUT's auto-detected PCI address (which may
#     differ from /etc/f-stack.conf's stale 0000:00:06.0 on older Nitro
#     instance generations);
#   - lcore_mask=4 (lcore 2) matches bench-rtt's `--lcore 2` default
#     (vs the nightly's `lcore_mask=1` / lcore 0 which only suits the
#     `--lcore 0` invocations on the matrix).
write_fstack_conf() {
    local pci="$1"
    local ip="$2"
    local gateway="$3"
    local broadcast
    broadcast="$(awk -F. '{printf "%s.%s.%s.255", $1, $2, $3}' <<<"$ip")"
    cat >"$FSTACK_CONF_PATH" <<EOF
# fast-iter F-Stack conf — generated by scripts/fast-iter-setup.sh on $(date -u +%FT%TZ)
# Auto-detected: PCI=$pci, IP=$ip, gateway=$gateway
# Bench tools point at this file via --fstack-conf "\$FSTACK_CONF".
[dpdk]
# lcore_mask=4 = bit 2 = lcore 2 (matches bench-{rtt,tx-burst,tx-maxtp,rx-burst} --lcore 2 default).
lcore_mask=4
channel=4
promiscuous=1
numa_on=1
tx_csum_offoad_skip=0
tso=0
vlan_strip=1
port_list=0
nb_vdev=0
nb_bond=0
# PCI device: data NIC bound to vfio-pci (auto-detected on this DUT).
allow=$pci

[port0]
addr=$ip
netmask=255.255.255.0
broadcast=$broadcast
gateway=$gateway

[freebsd.boot]
hz=100
fd_reserve=1024
kern.ipc.maxsockets=262144
net.inet.tcp.syncache.hashsize=4096
net.inet.tcp.syncache.bucketlimit=100
net.inet.tcp.tcbhashsize=65536
kern.ncallout=262144
kern.features.inet6=1

[freebsd.sysctl]
kern.ipc.somaxconn=32768
kern.ipc.maxsockbuf=16777216
net.inet.tcp.sendspace=16384
net.inet.tcp.recvspace=8192
net.inet.tcp.cc.algorithm=cubic
net.inet.tcp.sendbuf_max=16777216
net.inet.tcp.recvbuf_max=16777216
net.inet.tcp.sendbuf_auto=1
net.inet.tcp.recvbuf_auto=1
EOF
}

# Top-level F-Stack setup driver. Invoked when `up --with-fstack` is given,
# or directly via `fstack-conf` subcommand to refresh the conf in place
# (e.g. after a NIC rebind on the DUT). Idempotent: overwrites
# $FSTACK_CONF_PATH on every call.
prepare_fstack() {
    [ -f "$FSTACK_LIB" ] || die "F-Stack lib not found at $FSTACK_LIB (set FF_PATH=/path/to/f-stack root)"

    local pci ip gateway
    pci="$(detect_dut_pci)"
    if [ -z "$pci" ]; then
        die "no NIC bound to vfio-pci on this DUT — bind one via dpdk-devbind.py before --with-fstack"
    fi
    ip="$(detect_dut_ip)"
    gateway="$(infer_gateway "$ip")"
    log "  fstack: PCI=$pci, DUT_IP=$ip, gateway=$gateway -> $FSTACK_CONF_PATH"
    write_fstack_conf "$pci" "$ip" "$gateway"
    log "  fstack: rebuilding bench-{rtt,tx-burst,tx-maxtp,rx-burst} --features fstack"
    # Mirrors bench-nightly.sh step [3/12]: RUSTFLAGS=-C linker=gcc because
    # rust-lld (Rust 1.95+ default) does not auto-generate the __start /
    # __stop ELF section-set symbols F-Stack's FreeBSD-derived module system
    # depends on.
    ( cd "$WORKDIR" && \
      RUSTFLAGS="${RUSTFLAGS:-} -C linker=gcc" \
        cargo build --release \
            -p bench-tx-burst --features bench-tx-burst/fstack \
            -p bench-tx-maxtp --features bench-tx-maxtp/fstack \
            -p bench-rtt --features bench-rtt/fstack \
            -p bench-rx-burst --features bench-rx-burst/fstack \
    ) || die "fstack rebuild failed (see cargo output above)"
}

# ---------------------------------------------------------------------------
# `up` — build peer binaries, launch one peer EC2, deploy + start servers.
#        Trap aborts mid-flight: terminate the half-provisioned instance so
#        the operator isn't left paying for a peer with no echo-server.
# ---------------------------------------------------------------------------
cmd_up() {
    if [ -f "$STATE_FILE" ]; then
        log "already up: state at $STATE_FILE — run '$0 down' first"
        cat "$STATE_FILE"
        exit 1
    fi

    cd "$WORKDIR"

    # Prereqs.
    local missing=0
    for bin in aws ssh scp python3 make; do
        if ! command -v "$bin" >/dev/null 2>&1; then
            log "MISSING prereq: $bin"
            missing=$((missing + 1))
        fi
    done
    [ "$missing" -eq 0 ] || die "$missing prereq(s) missing"
    [ -f "$SSH_KEY" ] || die "ssh key not found: $SSH_KEY"
    [ -f "${SSH_KEY}.pub" ] || die "ssh pubkey not found: ${SSH_KEY}.pub"
    aws sts get-caller-identity >/dev/null 2>&1 \
        || die "aws creds not configured (aws sts get-caller-identity failed; AWS_PROFILE=$AWS_PROFILE)"

    log "=== fast-iter up ==="
    log "  ami=$PEER_AMI type=$PEER_INSTANCE_TYPE region=$AWS_REGION"

    # [1/4] Build peer C binaries.
    log "[1/4] building peer C binaries (echo-server + burst-echo-server + linux-tcp-sink)"
    make -C tools/bench-e2e/peer echo-server >/dev/null
    make -C tools/bench-e2e/peer burst-echo-server >/dev/null
    make -C tools/bench-vs-linux/peer linux-tcp-sink >/dev/null
    for f in tools/bench-e2e/peer/echo-server \
             tools/bench-e2e/peer/burst-echo-server \
             tools/bench-vs-linux/peer/linux-tcp-sink; do
        [ -f "$f" ] || die "expected build artifact missing: $f"
    done

    # [2/4] Launch peer.
    log "[2/4] launching peer EC2 instance"
    local launch_json instance_id
    launch_json="$(aws ec2 run-instances \
        --image-id "$PEER_AMI" \
        --instance-type "$PEER_INSTANCE_TYPE" \
        --subnet-id "$PEER_SUBNET" \
        --security-group-ids "$PEER_SG" \
        --no-associate-public-ip-address \
        --count 1 \
        --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=fast-iter-peer}]" \
        --output json)"
    instance_id="$(python3 -c "import sys,json; print(json.load(sys.stdin)['Instances'][0]['InstanceId'])" <<<"$launch_json")"
    log "  instance_id=$instance_id"

    # Trap: if anything below this point fails, terminate the instance so the
    # operator doesn't bleed money on a half-provisioned peer.
    cleanup_on_failure() {
        local rc=$?
        if [ $rc -ne 0 ]; then
            log "FAILED rc=$rc — terminating $instance_id to avoid orphan"
            aws ec2 terminate-instances --instance-ids "$instance_id" --output text >/dev/null 2>&1 || true
            rm -f "$STATE_FILE" "$ENV_FILE"
        fi
    }
    trap cleanup_on_failure EXIT

    aws ec2 wait instance-running --instance-ids "$instance_id"
    local peer_ip
    peer_ip="$(aws ec2 describe-instances --instance-ids "$instance_id" \
        --query 'Reservations[0].Instances[0].PrivateIpAddress' --output text)"
    log "  peer_ip=$peer_ip"

    # [3/4] Wait for SSH, prepare data NIC, deploy binaries.
    log "[3/4] waiting for sshd on $peer_ip (push pubkey via EC2 Instance Connect)"
    local retries=0
    until push_ic_pubkey "$instance_id" && \
          ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes "ubuntu@$peer_ip" true 2>/dev/null; do
        retries=$((retries + 1))
        if [ "$retries" -ge 72 ]; then
            die "ssh timeout after 6 min — check SG ingress + EC2 Instance Connect IAM"
        fi
        if [ $((retries % 6)) -eq 0 ]; then
            log "  still waiting ... ($((retries * 5))s elapsed)"
        fi
        sleep 5
    done
    log "  sshd ready"

    # AMI boots the data NIC bound to vfio-pci (DUT-style). For the peer we
    # need it bound to the kernel ena driver so the userspace echo-server /
    # linux-tcp-sink / burst-echo-server can listen on a kernel TCP socket.
    log "  preparing peer data NIC + firewall"
    push_ic_pubkey "$instance_id"
    ssh "${SSH_OPTS[@]}" "ubuntu@$peer_ip" 'sudo bash -s' <<'REMOTE_EOF'
set -eu
PCI=$(/usr/local/bin/dpdk-devbind.py --status-dev net 2>/dev/null | awk '/drv=vfio-pci/ {print $1; exit}' || true)
if [ -n "$PCI" ]; then
    echo "peer-prep: rebinding $PCI vfio-pci -> ena"
    /usr/local/bin/dpdk-devbind.py --bind ena "$PCI"
    sleep 1
else
    echo "peer-prep: no vfio-pci NIC (already ena)"
fi
# Drop firewall on all interfaces so DUT can reach the three echo-server ports.
iptables -I INPUT -j ACCEPT 2>/dev/null || true
echo "peer-prep: done"
REMOTE_EOF

    log "  scp peer binaries"
    push_ic_pubkey "$instance_id"
    scp "${SSH_OPTS[@]}" \
        tools/bench-e2e/peer/echo-server \
        tools/bench-e2e/peer/burst-echo-server \
        tools/bench-vs-linux/peer/linux-tcp-sink \
        "ubuntu@${peer_ip}:/tmp/"

    # [4/4] Start the three peer servers (mirrors bench-nightly step [6/12]).
    log "[4/4] starting peer servers"
    push_ic_pubkey "$instance_id"
    # shellcheck disable=SC2029  # ECHO_PORT/SINK_PORT/BURST_PORT are script-local constants;
    # client-side expansion is intentional so the remote shell sees concrete port literals.
    ssh "${SSH_OPTS[@]}" "ubuntu@$peer_ip" "
        set -eu
        chmod +x /tmp/echo-server /tmp/burst-echo-server /tmp/linux-tcp-sink
        nohup /tmp/echo-server $ECHO_PORT >/tmp/echo-server.log 2>&1 </dev/null &
        nohup /tmp/linux-tcp-sink $SINK_PORT >/tmp/linux-tcp-sink.log 2>&1 </dev/null &
        nohup /tmp/burst-echo-server $BURST_PORT >/tmp/burst-echo-server.log 2>&1 </dev/null &
        sleep 1
        pgrep -a echo-server || echo 'WARN echo-server not running'
        pgrep -a linux-tcp-sink || echo 'WARN linux-tcp-sink not running'
        pgrep -a burst-echo-server || echo 'WARN burst-echo-server not running'
    "

    # Optional: F-Stack arm setup. Done AFTER peer + servers are up so a
    # rebuild failure doesn't strand a half-provisioned peer (the trap still
    # fires until we tear it down at the end). prepare_fstack is fatal on
    # error (die) so the trap will terminate the instance if cargo fails.
    if [ "$WITH_FSTACK" = "1" ]; then
        log "[5/4] fstack: generate conf + rebuild bench tools with --features fstack"
        prepare_fstack
    fi

    # Persist state + env file.
    python3 -c "
import json
json.dump({
    'instance_id': '$instance_id',
    'peer_ip': '$peer_ip',
    'echo_port': $ECHO_PORT,
    'sink_port': $SINK_PORT,
    'burst_port': $BURST_PORT,
    'with_fstack': bool($WITH_FSTACK),
    'created_utc': '$(date -u +%FT%TZ)',
}, open('$STATE_FILE', 'w'), indent=2)
"
    write_env_file "$instance_id" "$peer_ip" "$WITH_FSTACK"

    # Success — drop the failure trap so a clean exit doesn't terminate
    # the peer we just provisioned.
    trap - EXIT

    log "=== fast-iter up complete ==="
    log "  state    -> $STATE_FILE"
    log "  env file -> $ENV_FILE"
    log "  PEER_IP=$peer_ip"
    log "  Ports: echo=$ECHO_PORT sink=$SINK_PORT burst=$BURST_PORT"
    if [ "$WITH_FSTACK" = "1" ]; then
        log "  FSTACK_CONF=$FSTACK_CONF_PATH"
        log "  Next:  source $ENV_FILE && sudo -E ./target/release/bench-rtt --stack fstack \\"
        log "             --peer-ip \"\$PEER_IP\" --peer-port \"\$PEER_ECHO_PORT\" \\"
        log "             --fstack-conf \"\$FSTACK_CONF\" --output-csv /tmp/fstack.csv ..."
    else
        log "  Next:  source $ENV_FILE && sudo ./target/release/bench-rtt --peer-ip \"\$PEER_IP\" ..."
    fi
    log "  Done:  $0 down"
}

# ---------------------------------------------------------------------------
# `fstack-conf` — regenerate $HOME/.fast-iter-fstack.conf in place + rebuild
# the four bench tools with --features fstack. Useful when:
#   - the peer is already up and the operator wants to add fstack support
#     without re-provisioning (skip-peer fast path), or
#   - the operator rebinds the NIC mid-session (PCI address changes), or
#   - the workspace was rebuilt without --features fstack and needs the
#     fstack arm relinked.
# Does NOT touch the peer; if `.fast-iter.env` exists, also injects
# FSTACK_CONF into it so `source .fast-iter.env` picks it up.
# ---------------------------------------------------------------------------
cmd_fstack_conf() {
    log "=== fast-iter fstack-conf ==="
    prepare_fstack
    # If .fast-iter.env already exists (peer is up), ensure FSTACK_CONF is
    # present in it so the operator's `source .fast-iter.env` flow keeps
    # working without re-running `up`.
    if [ -f "$ENV_FILE" ] && ! grep -q '^export FSTACK_CONF=' "$ENV_FILE"; then
        printf '# Appended by `fast-iter-setup.sh fstack-conf` on %s\n' "$(date -u +%FT%TZ)" >>"$ENV_FILE"
        printf 'export FSTACK_CONF="%s"\n' "$FSTACK_CONF_PATH" >>"$ENV_FILE"
        log "  appended FSTACK_CONF=$FSTACK_CONF_PATH to $ENV_FILE"
    fi
    log "  done. Bench tools: --fstack-conf $FSTACK_CONF_PATH"
}

# ---------------------------------------------------------------------------
# `down` — terminate peer + clear state.
# ---------------------------------------------------------------------------
cmd_down() {
    if [ ! -f "$STATE_FILE" ]; then
        log "no state at $STATE_FILE — nothing to tear down"
        rm -f "$ENV_FILE" "$FSTACK_CONF_PATH"
        exit 0
    fi
    require_state
    log "=== fast-iter down ==="
    log "  terminating $PEER_INSTANCE_ID (peer_ip=$PEER_IP)"
    aws ec2 terminate-instances --instance-ids "$PEER_INSTANCE_ID" --output text >/dev/null
    rm -f "$STATE_FILE" "$ENV_FILE" "$FSTACK_CONF_PATH"
    log "  state cleared"
}

# ---------------------------------------------------------------------------
# `info` — print state + reachability check.
# ---------------------------------------------------------------------------
cmd_info() {
    if [ ! -f "$STATE_FILE" ]; then
        log "not provisioned (no state at $STATE_FILE)"
        exit 0
    fi
    require_state
    log "=== fast-iter info ==="
    log "  state file: $STATE_FILE"
    log "  env file:   $ENV_FILE"
    log "  peer_ip:    $PEER_IP"
    log "  instance:   $PEER_INSTANCE_ID"
    log "  ports:      echo=$ECHO_PORT sink=$SINK_PORT burst=$BURST_PORT"
    local state
    state="$(aws ec2 describe-instances --instance-ids "$PEER_INSTANCE_ID" \
        --query 'Reservations[0].Instances[0].State.Name' --output text 2>/dev/null || echo "unknown")"
    log "  ec2 state:  $state"
    if [ "$state" = "running" ]; then
        push_ic_pubkey "$PEER_INSTANCE_ID" 2>/dev/null || log "  WARN ec2-instance-connect push failed"
        if ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes "ubuntu@$PEER_IP" \
                "pgrep -a echo-server >/dev/null && pgrep -a linux-tcp-sink >/dev/null && pgrep -a burst-echo-server >/dev/null" 2>/dev/null; then
            log "  servers:    all three running"
        else
            log "  servers:    NOT all running (or ssh unreachable) — try '$0 sh'"
        fi
    fi
}

# ---------------------------------------------------------------------------
# `sh` — ssh into the peer.
# ---------------------------------------------------------------------------
cmd_sh() {
    require_state
    push_ic_pubkey "$PEER_INSTANCE_ID"
    log "ssh ubuntu@$PEER_IP"
    exec ssh "${SSH_OPTS[@]}" "ubuntu@$PEER_IP"
}

# ---------------------------------------------------------------------------
# Dispatch.
# ---------------------------------------------------------------------------
case "$mode" in
    up)          cmd_up ;;
    down)        cmd_down ;;
    info)        cmd_info ;;
    sh)          cmd_sh ;;
    fstack-conf) cmd_fstack_conf ;;
    -h|--help|help)
        sed -n '2,60p' "$0"
        exit 0
        ;;
    *)
        echo "fast-iter: unknown mode '$mode'; expected up|down|info|sh|fstack-conf" >&2
        exit 2
        ;;
esac
