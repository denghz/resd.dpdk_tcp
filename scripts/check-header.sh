#!/usr/bin/env bash
# Fails if the committed include/resd_net.h differs from what cbindgen produces.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -p resd-net --quiet
if ! git diff --quiet include/resd_net.h; then
    echo "ERROR: include/resd_net.h differs from cbindgen output. Run 'cargo build -p resd-net' and commit." >&2
    git --no-pager diff include/resd_net.h >&2
    exit 1
fi
