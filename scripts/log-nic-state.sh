#!/usr/bin/env bash
# log-nic-state.sh — capture per-NIC state for the two-ENI comparison.
#
# Background. fast-iter-suite.sh runs three TCP stacks against the same peer
# but each stack drives a DIFFERENT physical NIC on the DUT:
#   - dpdk_net / fstack: DPDK NIC at PCI 0000:28:00.0, bound to vfio-pci
#                        (driven by DPDK from lcore 2).
#   - linux_kernel:      kernel NIC at PCI 0000:27:00.0 (ens5, 10.4.1.139),
#                        bound to the in-tree `ena` driver and reached via
#                        `sudo nsenter -t 1 -n` to escape the dev-host's
#                        REDSOCKS proxy. See linux-nat-investigation-2026-
#                        05-12.md.
#
# Both NICs are AWS ENA on the same subnet to the same peer (10.4.1.228);
# they differ in queue/coalescing/IRQ defaults. To make the two-ENI shape
# reviewable rather than hidden, this script dumps state for BOTH DUT NICs
# AND the peer's `ens5` to a structured text file (one ASCII section per
# NIC, per probe), easy to diff across runs.
#
# Output format: a single file with `=== <section> ===` headers. Designed
# for grep-friendly diffing; not parsed by anything (the SUMMARY.md
# generator only LINKS to it).
#
# Usage:
#   log-nic-state.sh <output-file>
#
# Env (all optional, defaults match fast-iter-suite.sh):
#   DUT_KERNEL_NIC          Default ens5 (host-netns kernel NIC).
#   DUT_KERNEL_NIC_IP       Default 10.4.1.139 (host-netns kernel NIC IP — for log header only).
#   DUT_DPDK_PCI            Default 0000:28:00.0 (DPDK NIC PCI ID).
#   PEER_SSH                Required for peer capture; if unset, peer
#                           section is skipped (logged as `(skipped — PEER_SSH unset)`).
#   PEER_NIC                Default ens5 (peer kernel NIC name).
#
# Exit code: 0 always — this is observational. Probe failures are logged
# inline in the output file with `(probe failed — ...)` markers. The
# script never aborts the caller (fast-iter-suite.sh).

set -uo pipefail
# NOTE: `set -e` is NOT used. Individual `ethtool` / `cat` probes may fail
# (e.g., `ethtool -S ens5` on older driver versions) without invalidating
# the rest of the capture. Each probe handles its own failure path inline.

if [ $# -lt 1 ]; then
    printf 'log-nic-state.sh: missing <output-file>\n  usage: %s <output-file>\n' "$0" >&2
    exit 2
fi
OUT="$1"
mkdir -p "$(dirname "$OUT")"

DUT_KERNEL_NIC="${DUT_KERNEL_NIC:-ens5}"
DUT_KERNEL_NIC_IP="${DUT_KERNEL_NIC_IP:-10.4.1.139}"
DUT_DPDK_PCI="${DUT_DPDK_PCI:-0000:28:00.0}"
PEER_NIC="${PEER_NIC:-ens5}"
PEER_SSH_VAL="${PEER_SSH:-}"

# Header + run metadata.
{
    printf '# nic-state — captured %s\n' "$(date -u -Iseconds)"
    printf '# DUT kernel NIC: %s (%s)\n' "$DUT_KERNEL_NIC" "$DUT_KERNEL_NIC_IP"
    printf '# DUT DPDK NIC:   %s (vfio-pci)\n' "$DUT_DPDK_PCI"
    printf '# Peer NIC:       %s (via %s)\n' "$PEER_NIC" "${PEER_SSH_VAL:-<unset>}"
    printf '#\n'
    printf '# Two-ENI comparison disclosure — see\n'
    printf '#   docs/bench-reports/t57-fast-iter-suite-fair-comparison-2026-05-12.md\n'
    printf '#   "Methodology — two-ENI comparison" section.\n'
    printf '#\n'
} >"$OUT"

# Helper. Run a probe command, capture stdout+stderr, prefix with a
# `=== <label> ===` header. On non-zero exit, emit `(probe failed rc=N)`
# but do NOT abort.
emit() {
    local label="$1"
    shift
    {
        printf '\n=== %s ===\n' "$label"
        if "$@" 2>&1; then
            :
        else
            local rc=$?
            printf '(probe failed rc=%d cmd=%q)\n' "$rc" "$*"
        fi
    } >>"$OUT"
}

# Wrap a command in `sudo nsenter -t 1 -n` to run in the host netns
# (where ens5 lives — the dev-host container only sees vethpxtn0).
nsenter_emit() {
    local label="$1"
    shift
    emit "$label" sudo -n nsenter -t 1 -n "$@"
}

# ---------------------------------------------------------------------------
# DUT kernel NIC (ens5) — full ethtool + IRQ + qdisc + iptables + route.
# ---------------------------------------------------------------------------
{
    printf '\n############################################################\n'
    printf '# DUT kernel NIC: %s (host netns)\n' "$DUT_KERNEL_NIC"
    printf '############################################################\n'
} >>"$OUT"

nsenter_emit "ip -s link show $DUT_KERNEL_NIC" ip -s link show "$DUT_KERNEL_NIC"
nsenter_emit "ethtool $DUT_KERNEL_NIC" ethtool "$DUT_KERNEL_NIC"
nsenter_emit "ethtool -c $DUT_KERNEL_NIC (coalescing)" ethtool -c "$DUT_KERNEL_NIC"
nsenter_emit "ethtool -k $DUT_KERNEL_NIC (offloads)" ethtool -k "$DUT_KERNEL_NIC"
nsenter_emit "ethtool -l $DUT_KERNEL_NIC (channels)" ethtool -l "$DUT_KERNEL_NIC"
nsenter_emit "ethtool -S $DUT_KERNEL_NIC (xstats)" ethtool -S "$DUT_KERNEL_NIC"
nsenter_emit "/proc/interrupts | grep $DUT_KERNEL_NIC" \
    bash -c "grep '$DUT_KERNEL_NIC' /proc/interrupts || true"
nsenter_emit "tc qdisc show dev $DUT_KERNEL_NIC" tc qdisc show dev "$DUT_KERNEL_NIC"
nsenter_emit "iptables -L -v -n (head 30)" \
    bash -c "iptables -L -v -n 2>/dev/null | head -30 || printf '(iptables unavailable)\n'"
nsenter_emit "ip route" ip route

# ---------------------------------------------------------------------------
# DUT DPDK NIC — bound to vfio-pci. `ethtool` does not work; capture
# what's available from sysfs + lspci instead.
# ---------------------------------------------------------------------------
{
    printf '\n############################################################\n'
    printf '# DUT DPDK NIC: %s (vfio-pci — sysfs+lspci only)\n' "$DUT_DPDK_PCI"
    printf '############################################################\n'
} >>"$OUT"

emit "lspci -k -s $DUT_DPDK_PCI -vv" lspci -k -s "$DUT_DPDK_PCI" -vv
emit "sysfs vendor/device/subsystem (ENA = 1d0f:ec20)" \
    bash -c "for f in vendor device subsystem_vendor subsystem_device; do
        v=\$(cat /sys/bus/pci/devices/$DUT_DPDK_PCI/\$f 2>/dev/null || echo unreadable)
        printf '%-20s = %s\n' \"\$f\" \"\$v\"
    done"
emit "sysfs current driver" \
    bash -c "readlink /sys/bus/pci/devices/$DUT_DPDK_PCI/driver 2>/dev/null \
        | awk -F/ '{print \$NF}' || printf 'unbound\n'"
emit "sysfs numa_node" \
    bash -c "cat /sys/bus/pci/devices/$DUT_DPDK_PCI/numa_node 2>/dev/null || true"
emit "sysfs iommu_group" \
    bash -c "readlink /sys/bus/pci/devices/$DUT_DPDK_PCI/iommu_group 2>/dev/null \
        | awk -F/ '{print \$NF}' || true"

# ---------------------------------------------------------------------------
# Peer NIC (ens5) — via SSH. Only run if PEER_SSH is set.
# ---------------------------------------------------------------------------
{
    printf '\n############################################################\n'
    printf '# Peer NIC: %s (via %s)\n' "$PEER_NIC" "${PEER_SSH_VAL:-<unset>}"
    printf '############################################################\n'
} >>"$OUT"

if [ -z "$PEER_SSH_VAL" ]; then
    printf '\n=== peer ===\n(skipped — PEER_SSH unset)\n' >>"$OUT"
else
    # Single SSH invocation so we don't pay handshake cost per probe.
    # Each probe is wrapped in its own `=== ... ===` header by the remote
    # shell. `2>&1` so stderr ends up in the file too. Final `|| true` so
    # a transient SSH failure doesn't abort `set -uo pipefail`.
    ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
        "$PEER_SSH_VAL" \
        "NIC='$PEER_NIC'
         emit() {
             printf '\n=== peer: %s ===\n' \"\$1\"; shift
             \"\$@\" 2>&1 || printf '(peer probe failed rc=%d)\n' \"\$?\"
         }
         emit \"ip -s link show \$NIC\"        ip -s link show \$NIC
         emit \"ethtool \$NIC\"                ethtool \$NIC
         emit \"ethtool -c \$NIC (coalescing)\" ethtool -c \$NIC
         emit \"ethtool -k \$NIC (offloads)\"   ethtool -k \$NIC
         emit \"ethtool -l \$NIC (channels)\"   ethtool -l \$NIC
         emit \"ethtool -S \$NIC (xstats)\"     ethtool -S \$NIC
         emit \"/proc/interrupts | grep \$NIC\" bash -c \"grep \$NIC /proc/interrupts || true\"
         emit \"tc qdisc show dev \$NIC\"       tc qdisc show dev \$NIC
         emit \"iptables -L -v -n (head 30)\"   bash -c 'sudo -n iptables -L -v -n 2>/dev/null | head -30 || printf \"(iptables unavailable / no sudo)\n\"'
         emit \"ip route\"                       ip route" \
        >>"$OUT" 2>&1 \
        || printf '\n=== peer ===\n(SSH to %s failed — peer state not captured)\n' \
            "$PEER_SSH_VAL" >>"$OUT"
fi

# Footer marker so `tail` confirms completion.
printf '\n# nic-state capture complete %s\n' "$(date -u -Iseconds)" >>"$OUT"

exit 0
