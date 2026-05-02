#!/usr/bin/env bash
# scripts/layer-h-nightly.sh — full-matrix runner (4 invocations, merged).
#
# Runs the 14 pure-netem rows + 3 composed-FI rows (one per FI spec) and
# concatenates the four per-invocation Markdown reports into the
# canonical docs/superpowers/reports/layer-h-<date>.md. Time budget
# ≈ 12 min. Triggered nightly + at every stage cut.
#
# Each composed scenario gets its own process invocation because EAL is
# once-per-process and FaultConfig::from_env reads DPDK_NET_FAULT_INJECTOR
# once at engine bring-up. Single-FI-spec invariant is enforced by the
# binary at startup.
#
# Entry points:
#   ./scripts/layer-h-nightly.sh                # full nightly run
#   ./scripts/layer-h-nightly.sh --dry-run      # prereq check + plan only
#   ./scripts/layer-h-nightly.sh --help
#
# Mirrors bench-nightly.sh's prereq + provisioning + SCP + EC2-IC pattern.
# Bring-up blocks (Steps 1-6) are duplicated from layer-h-smoke.sh; a
# future refactor can extract a shared layer-h-fleet-up.sh helper.
#
# Exit codes:
#   0 — all 4 invocations exited 0 (every scenario passed).
#   1 — any invocation exited 1 (any scenario failed).
#   2 — orchestrator-level error (prereq miss, build failure, AWS error).
#
# Prerequisite env vars:
#   OUT_DIR          (optional) output dir; default target/layer-h-nightly/<ts>/
#   MY_CIDR          (optional) operator /32 for SSH allow-list
#   GATEWAY_IP       (optional) override default-gateway derivation
#   EAL_ARGS         (optional) override EAL boilerplate
#   OPERATOR_PUBKEY  (optional) path to ed25519 pubkey
#   SKIP_TEARDOWN    (optional) set to 1 to skip the trap EXIT teardown
#   RESD_INFRA_DIR   (optional) sister-repo path
set -euo pipefail

# ---------------------------------------------------------------------------
# Arg parsing — dry-run + help only.
# ---------------------------------------------------------------------------
DRY_RUN=0
while (($#)); do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,35p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1 (try --help)" >&2; exit 2 ;;
  esac
done

OUT_DIR="${OUT_DIR:-target/layer-h-nightly/$(date -u +%Y-%m-%dT%H-%M-%SZ)}"
REPORT_DATE="$(date -u +%Y-%m-%d)"
CANONICAL_REPORT="docs/superpowers/reports/layer-h-${REPORT_DATE}.md"
mkdir -p "$OUT_DIR" "$(dirname "$CANONICAL_REPORT")"

log() { echo "[layer-h-nightly] $*" >&2; }

# ---------------------------------------------------------------------------
# [1/9] Prereq check — fail fast if anything required is missing.
# ---------------------------------------------------------------------------
log "[1/9] prereq check"

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
  log "--dry-run: prereq check OK; would now build, provision bench-pair, run 4 invocations"
  log "--dry-run: output dir would be $OUT_DIR"
  log "--dry-run: canonical report would be $CANONICAL_REPORT"
  rmdir "$OUT_DIR" 2>/dev/null || true
  exit 0
fi

# ---------------------------------------------------------------------------
# [2/9] Build peer C binaries (echo-server only).
# ---------------------------------------------------------------------------
log "[2/9] building peer C binaries"
make -C tools/bench-e2e/peer echo-server

# ---------------------------------------------------------------------------
# [3/9] cargo build --release --workspace.
# ---------------------------------------------------------------------------
log "[3/9] cargo build --release --workspace"
timeout 600s cargo build --release --workspace

# ---------------------------------------------------------------------------
# [4/9] Provision bench-pair fleet via resd-aws-infra.
# ---------------------------------------------------------------------------
log "[4/9] provisioning bench-pair fleet via resd-aws-infra"

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
# [5/9] Wait for sshd on both hosts.
# ---------------------------------------------------------------------------
log "[5/9] waiting for sshd on both hosts"
wait_for_ssh "$DUT_SSH" "$DUT_INSTANCE_ID"
wait_for_ssh "$PEER_SSH" "$PEER_INSTANCE_ID"

EAL_ARGS="${EAL_ARGS:--l 2-3 -n 4 --in-memory --huge-unlink -a 0000:00:06.0,large_llq_hdr=1,miss_txc_to=3}"

# ---------------------------------------------------------------------------
# [6/9] Deploy binaries + bring up peer data NIC + start echo-server.
# ---------------------------------------------------------------------------
log "[6/9] deploying binaries to DUT + peer; starting peer echo-server"

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
# [7/9] Run the 4 invocations.
# Pure-netem (no --scenarios → all 14 pure-netem rows) + 3 composed
# rows (one per FI spec, --scenarios <name>). Each gets its own
# Markdown report and bundle dir on the DUT, scp'd back into per-label
# subdirs of $OUT_DIR.
#
# RC tracking: WORST_RC absorbs the worst-case rc across all 4 calls.
# 0 = all clean; 1 = any scenario failed; >1 = orchestrator-level error.
# ---------------------------------------------------------------------------
log "[7/9] running 4 invocations"

INVOCATIONS=(
  "pure-netem|"
  "composed-fi-drop|composed_loss_1pct_50ms_fi_drop"
  "composed-fi-dup|composed_loss_1pct_50ms_fi_dup"
  "composed-fi-reord|composed_loss_1pct_50ms_fi_reord"
)

WORST_RC=0

for inv in "${INVOCATIONS[@]}"; do
  label="${inv%%|*}"
  scenarios="${inv##*|}"
  log "  -> invocation: $label (scenarios=${scenarios:-<empty=all-pure-netem>})"

  # Defensive netem cleanup — each invocation starts from a clean qdisc
  # state on the peer so the binary's per-scenario apply step doesn't
  # EEXIST against an orphan from a prior invocation.
  refresh_ec2_ic_grants
  ssh "${SSH_OPTS[@]}" "ubuntu@$PEER_SSH" \
      "sudo tc qdisc del dev ens6 root || true" \
      || log "    pre-invocation netem cleanup ssh failed; continuing"

  REPORT_REMOTE="/tmp/layer-h-nightly-${label}.md"
  BUNDLE_REMOTE="/tmp/layer-h-bundles-${label}"
  ssh "${SSH_OPTS[@]}" "ubuntu@$DUT_SSH" \
      "rm -rf $BUNDLE_REMOTE && mkdir -p $BUNDLE_REMOTE"

  # Build the remote argv. For pure-netem, omit --scenarios so the
  # binary picks the default (all pure-netem rows). For composed
  # invocations, pass --scenarios <single name> so the FI spec is
  # honoured by the matrix selection logic.
  REMOTE_CMD="sudo /tmp/layer-h-correctness \
      --peer-ssh ubuntu@$PEER_SSH \
      --peer-iface ens6 \
      --peer-ip $PEER_IP \
      --local-ip $DUT_IP \
      --gateway-ip $GATEWAY_IP \
      --eal-args $(printf '%q' "$EAL_ARGS") \
      --lcore 2 \
      --report-md $REPORT_REMOTE \
      --bundle-dir $BUNDLE_REMOTE \
      --force"
  if [ -n "$scenarios" ]; then
    REMOTE_CMD+=" --scenarios $scenarios"
  fi

  refresh_ec2_ic_grants
  set +e
  ssh "${SSH_OPTS[@]}" "ubuntu@$DUT_SSH" "$REMOTE_CMD"
  RC=$?
  set -e

  log "    invocation rc=$RC"
  if (( RC > WORST_RC )); then
    WORST_RC=$RC
  fi

  # Pull artefacts regardless of RC — partial output is still useful.
  refresh_ec2_ic_grants
  scp "${SCP_OPTS[@]}" \
      "ubuntu@$DUT_SSH:$REPORT_REMOTE" "$OUT_DIR/layer-h-${label}.md" \
      || log "    scp report-${label} failed (binary may have exited before write)"
  scp -r "${SCP_OPTS[@]}" \
      "ubuntu@$DUT_SSH:$BUNDLE_REMOTE" "$OUT_DIR/bundles-${label}" \
      || log "    scp bundles-${label} failed (no failed-scenario bundles?)"
done

# ---------------------------------------------------------------------------
# [8/9] Merge the four per-invocation reports into the canonical date-stamped
# report. Operator commits this file separately to master as a per-stage-cut
# artefact.
# ---------------------------------------------------------------------------
log "[8/9] merging into $CANONICAL_REPORT"
{
  echo "# Layer H Correctness Report — ${REPORT_DATE}"
  echo
  echo "Full matrix run, 4 invocations merged."
  echo
  for label in pure-netem composed-fi-drop composed-fi-dup composed-fi-reord; do
    echo "---"
    echo
    echo "## Invocation: $label"
    echo
    if [ -f "$OUT_DIR/layer-h-${label}.md" ]; then
      cat "$OUT_DIR/layer-h-${label}.md"
    else
      echo "_(report missing — invocation crashed)_"
    fi
    echo
  done
} > "$CANONICAL_REPORT"

# ---------------------------------------------------------------------------
# [9/9] Done.
# Exit code: WORST_RC across all 4 invocations.
#   0 — all 4 invocations exited 0 (every scenario passed).
#   1 — any invocation exited 1 (any scenario failed).
#   >1 — orchestrator-level error from the binary itself.
# ---------------------------------------------------------------------------
log "[9/9] done — canonical report at $CANONICAL_REPORT"
log "       per-invocation reports + bundles in $OUT_DIR"
log "       worst rc across invocations: $WORST_RC"
exit "$WORST_RC"
