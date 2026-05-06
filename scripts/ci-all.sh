#!/usr/bin/env bash
# Single-node CI aggregator for Jenkins nodes that run everything serially
# (or for developers reproducing CI locally). Runs all 14 per-merge stages
# in the order defined by the flowchart in scripts/README.md
# (cheapest-fails-first). Exits non-zero on first failure.
#
# Agent requirements:
#   - Passwordless sudo (needed by ci-install-deps.sh for `apt-get install`
#     and by the two TAP stages (hardening-cpp-sanitizers, hardening-no-alloc)
#     which invoke DPDK under `sudo -E` to create TAP devices).
#   - CAP_NET_ADMIN (implied by root/sudo) for TAP creation.
#   - TAP stages serialize on the host: if multiple ci-all.sh runs share an
#     agent, wrap the pipeline in `lock('dpdk-tap')` or pin to a dedicated
#     agent label with disableConcurrentBuilds — TAP names are host-global.
#
# Stages 1 (ci-install-deps) is idempotent — re-running on a warm agent is a
# no-op on apt / rustup / cargo-fuzz / pip. Developers reproducing CI
# locally can export CI_ALL_SKIP_INSTALL=1 to skip it.
#
# The sudo pre-check fires before any stage runs so an agent missing
# passwordless sudo fails fast with a clear diagnostic instead of mid-pipeline.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! sudo -n true 2>/dev/null; then
    echo "ERROR: ci-all.sh requires passwordless sudo on this agent." >&2
    echo "       Needed by ci-install-deps.sh (apt-get) and the two TAP stages" >&2
    echo "       (hardening-cpp-sanitizers, hardening-no-alloc) which invoke" >&2
    echo "       DPDK under 'sudo -E' to create TAP devices." >&2
    echo "       Configure NOPASSWD sudo for the Jenkins user or schedule this" >&2
    echo "       pipeline on a privileged agent label." >&2
    exit 1
fi

TOTAL=16
step() {
    echo ""
    echo "=== ci-all: stage $1/${TOTAL} — $2 ==="
}

if [[ "${CI_ALL_SKIP_INSTALL:-0}" == "1" ]]; then
    step 1 "ci-install-deps (skipped via CI_ALL_SKIP_INSTALL=1)"
else
    step 1 "ci-install-deps"
    bash scripts/ci-install-deps.sh
fi

# Stage 2: cheap mechanical workspace-feature unification gate (Pattern P1).
# Runs `cargo metadata --offline` only — no compile, no link. Placed early so
# a feature-leak failure surfaces in <2 s instead of after the long stages.
# See scripts/check-workspace-features.sh banner for the leak class.
step  2 "check-workspace-features";  bash scripts/check-workspace-features.sh

# Stage 3: cheap mechanical C-ABI dead-field audit (Pattern P5). Pure bash
# + ripgrep, sub-second. Placed adjacent to the workspace-feature gate so
# any mechanical-class regression fails fast before the long stages run.
# See scripts/check-cabi-fields.sh banner.
step  3 "check-cabi-fields";         bash scripts/check-cabi-fields.sh

step  4 "check-header";              bash scripts/check-header.sh
step  5 "ci-fault-injector-compile"; bash scripts/ci-fault-injector-compile.sh
step  6 "hardening-panic-firewall";  bash scripts/hardening-panic-firewall.sh
step  7 "ci-unit-tests";             bash scripts/ci-unit-tests.sh
step  8 "ci-feature-matrix";         bash scripts/ci-feature-matrix.sh
step  9 "hardening-miri";            bash scripts/hardening-miri.sh
step 10 "ci-counter-coverage";       bash scripts/ci-counter-coverage.sh
step 11 "ci-tcpreq-gate";            bash scripts/ci-tcpreq-gate.sh
step 12 "fuzz-smoke";                TIME="${TIME:-30}" bash scripts/fuzz-smoke.sh
step 13 "ci-scapy-replay";           bash scripts/ci-scapy-replay.sh
step 14 "ci-packetdrill-corpus";     bash scripts/ci-packetdrill-corpus.sh

# TAP stages: require sudo + DPDK_NET_TEST_TAP=1. Placed last so that a
# fleet of non-privileged agents can still run the 14 non-TAP stages above.
step 15 "hardening-cpp-sanitizers (sudo+TAP)"
sudo -E DPDK_NET_TEST_TAP=1 bash scripts/hardening-cpp-sanitizers.sh

step 16 "hardening-no-alloc (sudo+TAP)"
sudo -E DPDK_NET_TEST_TAP=1 bash scripts/hardening-no-alloc.sh

echo ""
echo "=== ci-all: ALL PASSED ==="
