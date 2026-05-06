#!/usr/bin/env bash
# A9 Scapy adversarial corpus replay. Regenerates the pcap corpus via
# Scapy, then feeds it through scapy-fuzz-runner via the test-inject hook.
#
# Requires Python3 + Scapy on the agent (see ci-install-deps.sh).
set -euo pipefail
cd "$(dirname "$0")/.."

bash scripts/scapy-corpus.sh
# A7 / Pattern P1: scapy-fuzz-runner gates its `dpdk-net-core` dep behind
# its own `test-inject` feature so workspace builds don't unify the
# forbidden `dpdk-net-core/test-inject` feature into production binaries.
# `--features test-inject` is required to actually build the binary.
cargo run --release -p scapy-fuzz-runner --features test-inject -- --corpus tools/scapy-corpus/out/

echo "=== ci-scapy-replay: PASS ==="
