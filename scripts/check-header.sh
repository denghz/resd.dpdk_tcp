#!/usr/bin/env bash
# Fails if the committed include/dpdk_net.h differs from what cbindgen produces.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -p dpdk-net --quiet
if ! git diff --quiet include/dpdk_net.h; then
    echo "ERROR: include/dpdk_net.h differs from cbindgen output. Run 'cargo build -p dpdk-net' and commit." >&2
    git --no-pager diff include/dpdk_net.h >&2
    exit 1
fi
