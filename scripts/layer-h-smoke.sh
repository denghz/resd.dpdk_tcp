#!/usr/bin/env bash
# scripts/layer-h-smoke.sh — single-invocation per-merge smoke runner.
#
# Runs the 5-scenario --smoke subset (one representative per netem
# dimension) against an existing or freshly-provisioned bench-pair
# fleet. Time budget ≈ 3 minutes (5 × 30 s + setup).
#
# Entry points:
#   ./scripts/layer-h-smoke.sh                  # full smoke run
#   ./scripts/layer-h-smoke.sh --dry-run        # prereq check + plan only
#   ./scripts/layer-h-smoke.sh --help
#
# Mirrors bench-nightly.sh's prereq + provisioning + SCP + EC2-IC pattern;
# kept separate so a layer-h failure doesn't blank a perf re-run and vice
# versa. Reuses the resd-aws-infra bench-pair stack (DUT + peer).
#
# Prerequisite env vars:
#   OUT_DIR          (optional) output dir; default target/layer-h-smoke/<ts>/
#   MY_CIDR          (optional) operator /32 for SSH allow-list; default
#                    auto-discovered via ifconfig.me
#   GATEWAY_IP       (optional) override default-gateway derivation
#   EAL_ARGS         (optional) override EAL boilerplate
#   OPERATOR_PUBKEY  (optional) path to ed25519 pubkey; default
#                    ~/.ssh/id_ed25519.pub
#   SKIP_TEARDOWN    (optional) set to 1 to skip the trap EXIT teardown;
#                    useful for debugging failed runs
#   RESD_INFRA_DIR   (optional) sister-repo path; default
#                    $HOME/resd.aws-infra-setup
set -euo pipefail

# ---------------------------------------------------------------------------
# Arg parsing — dry-run + help only.
# ---------------------------------------------------------------------------
DRY_RUN=0
while (($#)); do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1 (try --help)" >&2; exit 2 ;;
  esac
done

OUT_DIR="${OUT_DIR:-target/layer-h-smoke/$(date -u +%Y-%m-%dT%H-%M-%SZ)}"
mkdir -p "$OUT_DIR"

log() { echo "[layer-h-smoke] $*" >&2; }

# ---------------------------------------------------------------------------
# [1/8] Prereq check — fail fast if anything required is missing.
# ---------------------------------------------------------------------------
log "[1/8] prereq check"

REQUIRED_BINS=(resd-aws-infra cargo jq ssh scp curl aws)
missing=0
for bin in "${REQUIRED_BINS[@]}"; do
  if ! command -v "$bin" >/dev/null 2>&1; then
    log "  MISSING: $bin"
    missing=$((missing + 1))
  fi
done
if ((missing > 0)); then
  log "prereq check failed — $missing binary/binaries missing"
  log "resd-aws-infra: install sister project from resd.aws-infra-setup"
  exit 2
fi

if ! aws sts get-caller-identity >/dev/null 2>&1; then
  log "AWS credentials not configured (aws sts get-caller-identity failed)"
  exit 2
fi

log "  all prereqs present"

if ((DRY_RUN)); then
  log "--dry-run: prereq check OK; would now build, provision bench-pair, run --smoke"
  log "--dry-run: output dir would be $OUT_DIR"
  rmdir "$OUT_DIR" 2>/dev/null || true
  exit 0
fi

# ---------------------------------------------------------------------------
# [2/8] Build peer C binaries (echo-server only — layer-h doesn't use
# linux-tcp-sink). Built BEFORE provisioning so a local build failure
# costs $0 AWS spend.
# ---------------------------------------------------------------------------
log "[2/8] building peer C binaries"
make -C tools/bench-e2e/peer echo-server

# ---------------------------------------------------------------------------
# [3/8] cargo build --release --workspace.
# Build BEFORE provisioning so a local build failure costs $0 AWS spend.
# ---------------------------------------------------------------------------
log "[3/8] cargo build --release --workspace"
timeout 600s cargo build --release --workspace

# ---------------------------------------------------------------------------
# [4/8] Provision bench-pair fleet via resd-aws-infra.
# ---------------------------------------------------------------------------
log "[4/8] provisioning bench-pair fleet via resd-aws-infra"

OPERATOR_CIDR="${MY_CIDR:-$(curl -fsS https://ifconfig.me)/32}"
log "  operator-ssh-cidr=$OPERATOR_CIDR"

RESD_INFRA_DIR="${RESD_INFRA_DIR:-$HOME/resd.aws-infra-setup}"

AMI_ID_ARG=()
if [ -n "${AMI_ID:-}" ]; then
  AMI_ID_ARG=(--ami-id "$AMI_ID")
  log "  ami-id=$AMI_ID (from env)"
elif [ -f "$RESD_INFRA_DIR/cdk.json" ]; then
  CDK_AMI="$(jq -r '.context."default-ami-id" // empty' "$RESD_INFRA_DIR/cdk.json")"
  if [ -n "$CDK_AMI" ] && [ "$CDK_AMI" != "null" ]; then
    AMI_ID_ARG=(--ami-id "$CDK_AMI")
    log "  ami-id=$CDK_AMI (from $RESD_INFRA_DIR/cdk.json)"
  fi
fi

CLI_STDOUT="$(
  cd "$RESD_INFRA_DIR" && \
  resd-aws-infra setup bench-pair \
      --operator-ssh-cidr "$OPERATOR_CIDR" "${AMI_ID_ARG[@]}" --json
)"

STACK_JSON="$(echo "$CLI_STDOUT" | sed -n '/^{/,$p')"

teardown_fleet() {
  if [ "${SKIP_TEARDOWN:-0}" != 1 ]; then
    ( cd "$RESD_INFRA_DIR" && resd-aws-infra teardown bench-pair --wait ) || true
  else
    log "SKIP_TEARDOWN=1; leaving stack up"
  fi
}
trap teardown_fleet EXIT

DUT_SSH="$(jq -r '.DutSshEndpoint // empty' <<<"$STACK_JSON")"
PEER_SSH="$(jq -r '.PeerSshEndpoint // empty' <<<"$STACK_JSON")"
DUT_INSTANCE_ID="$(jq -r '.DutInstanceId // empty' <<<"$STACK_JSON")"
PEER_INSTANCE_ID="$(jq -r '.PeerInstanceId // empty' <<<"$STACK_JSON")"
DUT_IP="$(jq -r '.DutDataEniIp // empty' <<<"$STACK_JSON")"
PEER_IP="$(jq -r '.PeerDataEniIp // empty' <<<"$STACK_JSON")"
AMI_ID="$(jq -r '.AmiId // empty' <<<"$STACK_JSON")"

for var in DUT_SSH PEER_SSH DUT_INSTANCE_ID PEER_INSTANCE_ID DUT_IP PEER_IP AMI_ID; do
  val="${!var}"
  if [ -z "$val" ]; then
    log "resd-aws-infra setup bench-pair missing output '$var'"
    log "CLI output tail:"
    echo "$CLI_STDOUT" | tail -25 | sed 's/^/  /' >&2
    exit 3
  fi
done

log "  DUT ssh=$DUT_SSH data-ip=$DUT_IP"
log "  peer ssh=$PEER_SSH data-ip=$PEER_IP"
log "  AMI=$AMI_ID"

GATEWAY_IP="${GATEWAY_IP:-$(awk -F. '{printf "%s.%s.%s.1", $1,$2,$3}' <<<"$DUT_IP")}"
log "  gateway-ip=$GATEWAY_IP"

SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30)
SCP_OPTS=(-o StrictHostKeyChecking=accept-new)

OPERATOR_PUBKEY="${OPERATOR_PUBKEY:-$HOME/.ssh/id_ed25519.pub}"
if [ ! -f "$OPERATOR_PUBKEY" ]; then
  log "OPERATOR_PUBKEY=$OPERATOR_PUBKEY not found; generate with 'ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519'"
  exit 5
fi

push_operator_pubkey() {
  local instance_id="$1"
  aws ec2-instance-connect send-ssh-public-key \
    --instance-id "$instance_id" \
    --instance-os-user ubuntu \
    --ssh-public-key "file://$OPERATOR_PUBKEY" \
    --output text --query 'Success' >/dev/null
}

refresh_ec2_ic_grants() {
  push_operator_pubkey "$DUT_INSTANCE_ID"
  push_operator_pubkey "$PEER_INSTANCE_ID"
}

log "  pushing operator pubkey via EC2 Instance Connect (60 s grant)"
refresh_ec2_ic_grants

# wait_for_ssh — three consecutive successful BatchMode probes before
# we consider sshd ready (single success can race a transient sshd
# restart during cloud-init finalisation).
wait_for_ssh() {
  local host="$1"
  local instance_id="$2"
  local attempt=0
  local consecutive=0
  local required_consecutive=3
  local max_attempts=30  # 30 * 5 s = 150 s ceiling
  while (( attempt < max_attempts )); do
    push_operator_pubkey "$instance_id"
    if ssh "${SSH_OPTS[@]}" -o BatchMode=yes -o ConnectTimeout=5 \
         "ubuntu@${host}" exit 2>/dev/null; then
      consecutive=$((consecutive + 1))
      if (( consecutive >= required_consecutive )); then
        log "  sshd ready on $host (after ${consecutive} consecutive ok probes, attempt $((attempt+1)))"
        return 0
      fi
    else
      consecutive=0
    fi
    attempt=$((attempt + 1))
    sleep 5
  done
  log "  sshd NEVER came up on $host after ${max_attempts} probes"
  return 1
}

# ---------------------------------------------------------------------------
# [5/8] Wait for sshd on both hosts.
# ---------------------------------------------------------------------------
log "[5/8] waiting for sshd on both hosts"
wait_for_ssh "$DUT_SSH" "$DUT_INSTANCE_ID"
wait_for_ssh "$PEER_SSH" "$PEER_INSTANCE_ID"

EAL_ARGS="${EAL_ARGS:--l 2-3 -n 4 --in-memory --huge-unlink -a 0000:00:06.0,large_llq_hdr=1,miss_txc_to=3}"

# ---------------------------------------------------------------------------
# [6/8] Deploy binaries + bring up peer data NIC + start echo-server.
# ---------------------------------------------------------------------------
log "[6/8] deploying binaries to DUT + peer; starting peer echo-server"

DUT_BINS=(target/release/layer-h-correctness)
PEER_BINS=(tools/bench-e2e/peer/echo-server)
SHARED_SCRIPTS=(scripts/check-bench-preconditions.sh)

for bin in "${DUT_BINS[@]}" "${PEER_BINS[@]}" "${SHARED_SCRIPTS[@]}"; do
  if [ ! -f "$bin" ]; then
    log "missing after build: $bin"
    exit 4
  fi
done

refresh_ec2_ic_grants
log "  -> DUT ($DUT_SSH)"
scp "${SCP_OPTS[@]}" \
    "${DUT_BINS[@]}" "${SHARED_SCRIPTS[@]}" \
    "ubuntu@${DUT_SSH}:/tmp/"
ssh "${SSH_OPTS[@]}" "ubuntu@$DUT_SSH" \
    "chmod +x /tmp/check-bench-preconditions.sh /tmp/layer-h-correctness"

refresh_ec2_ic_grants
log "  -> peer ($PEER_SSH)"
scp "${SCP_OPTS[@]}" \
    "${PEER_BINS[@]}" "${SHARED_SCRIPTS[@]}" \
    "ubuntu@${PEER_SSH}:/tmp/"
ssh "${SSH_OPTS[@]}" "ubuntu@$PEER_SSH" \
    "chmod +x /tmp/check-bench-preconditions.sh"

# Peer-side data NIC bring-up + iptables open.
refresh_ec2_ic_grants
ssh "${SSH_OPTS[@]}" "ubuntu@$PEER_SSH" \
    "sudo bash -s -- $PEER_IP" <<'REMOTE_EOF'
set -euo pipefail
PEER_IP="$1"
PCI="$(/usr/local/bin/dpdk-devbind.py --status-dev net | awk '/drv=vfio-pci/ {print $1; exit}')"
if [ -n "$PCI" ]; then
  echo "peer-prep: unbinding $PCI from vfio-pci, rebinding to ena"
  /usr/local/bin/dpdk-devbind.py --bind ena "$PCI"
  sleep 2
fi
MGMT_IF="$(ip route show default | awk '/default/ {print $5; exit}')"
IFACE="$(ip -o link show | awk -F': ' '{print $2}' | grep -vE "^(lo|docker|${MGMT_IF})$" | head -1)"
if [ -z "$IFACE" ]; then
  echo "peer-prep: no data NIC found after rebind" >&2
  exit 1
fi
echo "peer-prep: bringing up $IFACE with $PEER_IP/24"
ip link set "$IFACE" up
ip addr flush dev "$IFACE" || true
ip addr add "$PEER_IP"/24 dev "$IFACE"
iptables -I INPUT -i "$IFACE" -j ACCEPT 2>/dev/null || true
echo "peer-prep: $IFACE ready"
REMOTE_EOF

refresh_ec2_ic_grants
ssh "${SSH_OPTS[@]}" "ubuntu@$PEER_SSH" \
    "nohup /tmp/echo-server 10001 >/tmp/echo-server.log 2>&1 </dev/null &"

# Give the peer service a moment to bind its listen socket.
sleep 1

# ---------------------------------------------------------------------------
# [7/8] Run layer-h-correctness --smoke on the DUT.
# ---------------------------------------------------------------------------
log "[7/8] running layer-h-correctness --smoke"
refresh_ec2_ic_grants

# Defensive netem cleanup — if a previous run crashed mid-scenario, the
# peer may still have a netem qdisc installed; the layer-h-correctness
# binary's per-scenario apply step would EEXIST and fail.
ssh "${SSH_OPTS[@]}" "ubuntu@$PEER_SSH" \
    "sudo tc qdisc del dev ens6 root || true" \
    || log "  pre-run netem cleanup ssh failed (peer unreachable?); continuing"

REPORT_REMOTE="/tmp/layer-h-smoke-report.md"
BUNDLE_REMOTE="/tmp/layer-h-bundles"
ssh "${SSH_OPTS[@]}" "ubuntu@$DUT_SSH" \
    "rm -rf $BUNDLE_REMOTE && mkdir -p $BUNDLE_REMOTE"

set +e
ssh "${SSH_OPTS[@]}" "ubuntu@$DUT_SSH" \
    "sudo /tmp/layer-h-correctness \
        --peer-ssh ubuntu@$PEER_SSH \
        --peer-iface ens6 \
        --peer-ip $PEER_IP \
        --local-ip $DUT_IP \
        --gateway-ip $GATEWAY_IP \
        --eal-args $(printf '%q' "$EAL_ARGS") \
        --lcore 2 \
        --smoke \
        --report-md $REPORT_REMOTE \
        --bundle-dir $BUNDLE_REMOTE \
        --force"
RC=$?
set -e

# ---------------------------------------------------------------------------
# [8/8] Pull report + bundles regardless of RC.
# ---------------------------------------------------------------------------
log "[8/8] pulling artefacts"
refresh_ec2_ic_grants
scp "${SCP_OPTS[@]}" "ubuntu@$DUT_SSH:$REPORT_REMOTE" "$OUT_DIR/layer-h-smoke.md" \
    || log "  scp report failed (binary may have exited before write)"
scp -r "${SCP_OPTS[@]}" "ubuntu@$DUT_SSH:$BUNDLE_REMOTE" "$OUT_DIR/bundles" \
    || log "  scp bundles failed (binary may have exited before write)"

log "  done — RC=$RC; report at $OUT_DIR/layer-h-smoke.md"
exit "$RC"
