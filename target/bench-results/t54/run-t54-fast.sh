#!/usr/bin/env bash
# T54 fast-iter orchestrator — four passes with reduced warmup/duration.
# fstack_maxtp now measures wire rate via echo counting (commit 0a523cb).
set -euo pipefail

T54=/home/ubuntu/resd.dpdk_tcp-a10-perf/target/bench-results/t54
BIN=/home/ubuntu/resd.dpdk_tcp-a10-perf/target/release/bench-vs-mtcp
PEER=10.4.1.29
LOCAL_IP=10.4.1.141
GATEWAY_IP=10.4.1.1
LCORE=2
EAL_ARGS="-l 2-3 -n 4 --in-memory --huge-unlink -a 0000:00:06.0,large_llq_hdr=1,miss_txc_to=3"

stamp() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }
log() { echo "[$(stamp)] $*" | tee -a "$T54/fast-orchestrator.log"; }

log "T54 fast-iter start (commit=$(git -C /home/ubuntu/resd.dpdk_tcp-a10-perf rev-parse --short HEAD))"

clean_dpdk() {
  rm -rf /run/dpdk/rte/ 2>/dev/null || true
  mkdir -p /run/dpdk/rte/
}

# Pass 1: dpdk_net burst fast-iter (5 warmup + 100 measurement bursts).
log "Pass 1: dpdk burst"
clean_dpdk
"$BIN" \
  --peer-ip "$PEER" --local-ip "$LOCAL_IP" --gateway-ip "$GATEWAY_IP" \
  --lcore "$LCORE" --eal-args "$EAL_ARGS" \
  --workload burst --stacks dpdk \
  --warmup 5 --bursts-per-bucket 100 \
  --precondition-mode lenient \
  --output-csv "$T54/fast-burst-dpdk.csv" \
  >"$T54/fast-burst-dpdk.stdout" 2>"$T54/fast-burst-dpdk.stderr"
log "Pass 1 done — $(wc -l < "$T54/fast-burst-dpdk.csv") CSV lines"

# Pass 2: fstack burst fast-iter.
log "Pass 2: fstack burst"
clean_dpdk
"$BIN" \
  --peer-ip "$PEER" \
  --workload burst --stacks fstack \
  --fstack-peer-port 10001 --fstack-conf /etc/f-stack.conf \
  --warmup 5 --bursts-per-bucket 100 \
  --precondition-mode lenient \
  --output-csv "$T54/fast-burst-fstack.csv" \
  >"$T54/fast-burst-fstack.stdout" 2>"$T54/fast-burst-fstack.stderr"
log "Pass 2 done — $(wc -l < "$T54/fast-burst-fstack.csv") CSV lines"

# Pass 3: dpdk_net + linux maxtp fast-iter.
log "Pass 3: dpdk+linux maxtp"
clean_dpdk
"$BIN" \
  --peer-ip "$PEER" --local-ip "$LOCAL_IP" --gateway-ip "$GATEWAY_IP" \
  --lcore "$LCORE" --eal-args "$EAL_ARGS" \
  --workload maxtp --stacks dpdk,linux \
  --maxtp-warmup-secs 2 --maxtp-duration-secs 10 \
  --precondition-mode lenient \
  --output-csv "$T54/fast-maxtp-dpdk-linux.csv" \
  >"$T54/fast-maxtp-dpdk-linux.stdout" 2>"$T54/fast-maxtp-dpdk-linux.stderr"
log "Pass 3 done — $(wc -l < "$T54/fast-maxtp-dpdk-linux.csv") CSV lines"

# Pass 4: fstack maxtp fast-iter (wire-rate via echo counting).
log "Pass 4: fstack maxtp"
clean_dpdk
"$BIN" \
  --peer-ip "$PEER" \
  --workload maxtp --stacks fstack \
  --fstack-peer-port 10001 --fstack-conf /etc/f-stack.conf \
  --maxtp-warmup-secs 2 --maxtp-duration-secs 10 \
  --precondition-mode lenient \
  --output-csv "$T54/fast-maxtp-fstack.csv" \
  >"$T54/fast-maxtp-fstack.stdout" 2>"$T54/fast-maxtp-fstack.stderr"
log "Pass 4 done — $(wc -l < "$T54/fast-maxtp-fstack.csv") CSV lines"

log "ALL T54 FAST-ITER PASSES COMPLETE"
