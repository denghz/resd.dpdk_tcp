#!/usr/bin/env bash
# peer-ifb-setup.sh — set up an IFB redirect on the peer's data NIC so
# ingress traffic (DUT→peer direction) can be shaped via netem.
#
# Usage on the peer host (run via SSH from the bench orchestrator):
#   sudo ./peer-ifb-setup.sh up   ens6 ifb0 "loss 1% delay 5ms"
#   sudo ./peer-ifb-setup.sh down ens6 ifb0
#
# up:    creates ifb0, redirects ens6 ingress to ifb0, applies netem on ifb0.
# down:  removes ingress qdisc + ifb0 device.
#
# Idempotent on `down`: tolerates already-removed state.
# Not idempotent on `up`: a second `up` without `down` leaves a stale qdisc.
set -euo pipefail
mode="${1:?up|down}"
iface="${2:?iface (e.g. ens6)}"
ifb="${3:?ifb dev name (e.g. ifb0)}"
spec="${4:-}"

case "$mode" in
  up)
    if [ -z "$spec" ]; then
      echo "peer-ifb-setup: 'up' mode requires a netem spec as 4th arg" >&2
      exit 1
    fi
    modprobe ifb numifbs=2
    ip link add "$ifb" type ifb 2>/dev/null || true
    ip link set "$ifb" up
    tc qdisc add dev "$iface" handle ffff: ingress
    tc filter add dev "$iface" parent ffff: protocol ip u32 \
        match u32 0 0 action mirred egress redirect dev "$ifb"
    # shellcheck disable=SC2086 # spec is a netem arg list, not a single token
    tc qdisc add dev "$ifb" root netem $spec
    echo "peer-ifb-setup: up — $iface ingress -> $ifb netem ($spec)"
    ;;
  down)
    tc qdisc del dev "$ifb" root 2>/dev/null || true
    tc qdisc del dev "$iface" ingress 2>/dev/null || true
    ip link set "$ifb" down 2>/dev/null || true
    ip link delete "$ifb" type ifb 2>/dev/null || true
    echo "peer-ifb-setup: down — cleaned up $iface ingress + $ifb"
    ;;
  *)
    echo "peer-ifb-setup: unknown mode '$mode'; expected up|down" >&2
    exit 1
    ;;
esac
