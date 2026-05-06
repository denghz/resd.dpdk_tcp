#!/usr/bin/env bash
# Workspace-wide unit + doc tests. Covers every member crate except the
# fuzz sub-project (which lives in crates/dpdk-net-core/fuzz and is
# excluded from the root workspace).
#
# Per-package loop for the unit-test pass (not `cargo test --workspace`):
# a workspace-wide invocation would activate `test-inject` (via
# scapy-fuzz-runner), `test-server` (via tcpreq-runner), and
# `bench-alloc-audit` (via bench-rx-zero-copy) all at once on dpdk-net-core
# through Cargo feature unification. The test-inject feature gates in a
# different `Engine::inject_rx_frame` variant that does not bump
# `eth.rx_pkts`, so the counter-coverage test `cover_eth_rx_pkts` fails
# under a unified build. Running each crate in its own invocation keeps
# each crate's feature set scoped to what its Cargo.toml actually
# requests — matching the plan's stated intent of "default features work
# across every workspace crate".
#
# Doc tests use a single workspace-wide `cargo test --workspace --doc`
# call because (a) workspace-doc-test silently skips crates without a lib
# target (ffi-test / bench-rx-zero-copy / scapy-fuzz-runner have no lib),
# and (b) doc tests in this repo don't touch the feature-gated
# inject_rx_frame paths that create the unification problem for the
# regular test pass.
#
# The feature-matrix stage (ci-feature-matrix.sh) covers the feature-space
# sweep for dpdk-net-core, so this stage's job is "the default build of
# each crate passes its tests".
#
# Placed before ci-feature-matrix.sh in the CI order so regressions that
# break the default-feature build of any workspace crate surface in
# minutes, before the tens of minutes the 8-build matrix takes.
set -euo pipefail
cd "$(dirname "$0")/.."

# Each entry is "<pkg>[ <extra cargo args>...]". The extra args are passed
# verbatim to `cargo test`. Crates whose forbidden-feature dep is now
# optional (A7 / Pattern P1) need `--features <name>` here so their tests
# (or test-binaries' compilation) actually exercise the feature surface.
PKGS=(
    "dpdk-net-sys"
    "dpdk-net-core"
    "dpdk-net"
    "ffi-test"
    "bench-rx-zero-copy --features bench-alloc-audit"
    "packetdrill-shim-runner"
    "scapy-fuzz-runner --features test-inject"
    "tcpreq-runner"
)

for entry in "${PKGS[@]}"; do
    # shellcheck disable=SC2086 # word-split intended for per-pkg extra args
    set -- $entry
    pkg="$1"; shift
    echo "=== ci-unit-tests: cargo test -p ${pkg} $* ==="
    cargo test -p "${pkg}" "$@"
done

echo "=== ci-unit-tests: cargo test --workspace --exclude dpdk-net-core-fuzz --doc ==="
cargo test --workspace --exclude dpdk-net-core-fuzz --doc

echo "=== ci-unit-tests: PASS ==="
