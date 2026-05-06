#!/usr/bin/env bash
# scripts/check-workspace-features.sh
#
# Pattern P1 / pressure-test plan T0 — workspace-feature unification gate.
#
# Cargo's resolver v2 unifies features ACROSS THE WORKSPACE: if any member
# (even a test-only or bench-only tool) declares a non-optional dependency
# on `dpdk-net-core` with a test/bench feature enabled, that feature is
# silently turned ON in every other consumer's resolution — including the
# public production library `crates/dpdk-net`.
#
# This script catches that class of leak before it ships. It runs
# `cargo metadata --workspace` and asserts that the resolved feature set
# of `dpdk-net-core` contains NONE of the forbidden test/bench features
# below. A violation means some workspace member (or transitive dep) is
# pulling a forbidden feature non-optionally.
#
#   FORBIDDEN production features on dpdk-net-core:
#     - pressure-test       (A11.0 pressure-testing harness — Stage 1 only)
#     - test-server         (test-only A7 FSM)
#     - test-inject         (synthetic RX-frame injection for fuzz/scapy)
#     - fault-injector      (post-PMD-RX middleware for fault testing)
#     - obs-none            (zero-observability A/B baseline)
#     - bench-internals     (TimerWheel/EngineNoEalHarness exposure for microbench)
#     - bench-alloc-audit   (counting GlobalAlloc wrapper — alloc-audit regression)
#
# Allowlist (legitimate consumers of those features; not "production binaries"):
#     tests/ffi-test/                       # FFI smoke tests
#     tools/bench-*                         # bench tools
#     tools/layer-h-correctness/            # correctness gate (test-server)
#     tools/scapy-fuzz-runner/              # scapy fuzz harness (test-inject)
#     tools/tcpreq-runner/                  # tcpreq gate
#     tools/packetdrill-shim-runner/        # packetdrill gate
#     crates/dpdk-net-core/fuzz/            # cargo-fuzz target (excluded from workspace)
#
# These crates may legitimately enable forbidden features in their OWN
# Cargo.toml — but they MUST do so via an OPTIONAL dependency (`optional = true`)
# behind their own opt-in feature flag, OR via [dev-dependencies], so that
# Cargo's workspace-feature unifier does not pull the feature into the
# production library's resolution.
#
# # Expected state at this script's introduction
#
# This script is RED on master at introduction time, by design. The known
# offender is `tools/scapy-fuzz-runner/Cargo.toml` line 7-8:
#
#     dpdk-net-core = { path = "...", features = ["test-inject"] }
#
# (non-optional, no feature gate). Task A7 of the cross-phase-fixes plan
# fixes this by switching to `optional = true` and gating behind the runner's
# own `--features inject` flag. Once A7 lands, this gate flips to GREEN.
#
# # Operation
#
# Runs offline (no network). Requires `cargo` and `jq` (or falls back to
# `python3` for JSON parsing). Honours an explicit timeout on cargo metadata
# per project memory `feedback_test_timeouts.md`.
#
# Exit codes:
#   0 — no leak detected (workspace clean)
#   1 — leak detected; offending feature(s) printed with remediation hint
#   2 — tool error (cargo or jq/python3 missing, metadata failed, etc.)

set -euo pipefail

readonly SCRIPT_NAME="check-workspace-features.sh"
readonly CARGO_TIMEOUT=60
readonly TARGET_CRATE="dpdk-net-core"

# The forbidden features on TARGET_CRATE that must never appear in a
# workspace-unified production resolution.
readonly FORBIDDEN_FEATURES=(
    "pressure-test"
    "test-server"
    "test-inject"
    "fault-injector"
    "obs-none"
    "bench-internals"
    "bench-alloc-audit"
)

die() { echo "ERROR [$SCRIPT_NAME]: $*" >&2; exit 2; }
fail() { echo "FAIL [$SCRIPT_NAME]: $*" >&2; exit 1; }

# --- Self-test (--self-test) -----------------------------------------------
# Verifies the parser/violation logic on synthetic JSON without invoking
# cargo. Exits 0 on pass, 1 on logic regression. Run by CI before the real
# gate, so a parser-bug can't silently turn the gate green.
if [[ "${1:-}" == "--self-test" ]]; then
    echo "=== $SCRIPT_NAME --self-test ==="
    fake_clean=$(cat <<'JSON'
{"resolve":{"nodes":[
  {"id":"path+file:///x/crates/dpdk-net-core#0.1.0","features":["default","obs-poll-saturation","hw-verify-llq"]}
]}}
JSON
    )
    fake_dirty=$(cat <<'JSON'
{"resolve":{"nodes":[
  {"id":"path+file:///x/crates/dpdk-net-core#0.1.0","features":["default","test-inject","bench-internals"]}
]}}
JSON
    )
    parse_features() {
        local target="$1" json="$2"
        if command -v jq >/dev/null 2>&1; then
            printf '%s' "$json" | jq -r --arg crate "$target" '
                .resolve.nodes[] | select(.id | test("/" + $crate + "#")) | .features[]
            '
        else
            printf '%s' "$json" | python3 -c "
import json,sys
d=json.load(sys.stdin); c='$target'
for n in d['resolve']['nodes']:
    if ('/'+c+'#') in n['id']:
        for f in n['features']: print(f)
"
        fi
    }
    clean=$(parse_features "$TARGET_CRATE" "$fake_clean")
    dirty=$(parse_features "$TARGET_CRATE" "$fake_dirty")
    # Clean must contain none of the forbidden set.
    for f in "${FORBIDDEN_FEATURES[@]}"; do
        if grep -Fxq -- "$f" <<<"$clean"; then
            echo "self-test FAIL: forbidden feature '$f' falsely matched in clean fixture" >&2
            exit 1
        fi
    done
    # Dirty must trip on at least 'test-inject' AND 'bench-internals'.
    for f in test-inject bench-internals; do
        if ! grep -Fxq -- "$f" <<<"$dirty"; then
            echo "self-test FAIL: forbidden feature '$f' missed in dirty fixture" >&2
            exit 1
        fi
    done
    echo "self-test OK: parser detects clean-vs-dirty correctly"
    exit 0
fi

cd "$(dirname "$0")/.."

# Tool checks.
command -v cargo >/dev/null 2>&1 || die "cargo not on PATH"
HAVE_JQ=0
if command -v jq >/dev/null 2>&1; then
    HAVE_JQ=1
elif ! command -v python3 >/dev/null 2>&1; then
    die "neither jq nor python3 available for JSON parsing"
fi

echo "=== $SCRIPT_NAME: workspace-feature unification gate ==="
echo "Target crate:       $TARGET_CRATE"
echo "Forbidden features: ${FORBIDDEN_FEATURES[*]}"
echo "Mode:               cargo metadata --offline (workspace-wide)"

# --- Step 1: cargo metadata, offline. ---------------------------------------
# `--offline` ensures CI works without network as long as Cargo.lock exists.
# `cargo metadata` covers the entire workspace by default when invoked from
# the workspace root; no --workspace flag is required (or supported on stable).
# Per feedback_test_timeouts.md: explicit timeout on every cargo invocation.
HOST_TRIPLE=$(rustc -vV 2>/dev/null | awk '/^host:/ {print $2}')
[[ -n "$HOST_TRIPLE" ]] || die "rustc not on PATH or no host triple available"

metadata_json=$(timeout "${CARGO_TIMEOUT}" cargo metadata \
    --format-version 1 \
    --offline \
    --filter-platform "$HOST_TRIPLE" \
    2>/dev/null) \
    || die "cargo metadata failed (offline). Ensure Cargo.lock is committed and dependencies are vendored or fetched."

# --- Step 2: extract resolved feature set for $TARGET_CRATE. ---------------
if [[ "$HAVE_JQ" -eq 1 ]]; then
    resolved=$(printf '%s' "$metadata_json" | jq -r --arg crate "$TARGET_CRATE" '
        .resolve.nodes[]
        | select(.id | test("/" + $crate + "#"))
        | .features[]
    ' 2>/dev/null || true)
else
    resolved=$(printf '%s' "$metadata_json" | python3 -c "
import json, sys
data = json.load(sys.stdin)
crate = '$TARGET_CRATE'
for node in data.get('resolve', {}).get('nodes', []):
    if ('/' + crate + '#') in node.get('id', ''):
        for f in node.get('features', []):
            print(f)
" 2>/dev/null || true)
fi

if [[ -z "$resolved" ]]; then
    die "could not extract resolved feature set for $TARGET_CRATE — is it still a workspace member?"
fi

echo ""
echo "Resolved features on $TARGET_CRATE (workspace unification):"
printf '  - %s\n' $resolved

# --- Step 3: assert no forbidden feature is in the resolved set. -----------
violations=()
for forbidden in "${FORBIDDEN_FEATURES[@]}"; do
    if grep -Fxq -- "$forbidden" <<<"$resolved"; then
        violations+=("$forbidden")
    fi
done

if [[ ${#violations[@]} -gt 0 ]]; then
    echo ""
    echo "================================================================"
    echo "WORKSPACE-FEATURE LEAK DETECTED"
    echo "================================================================"
    echo "The following test/bench-only feature(s) are unified into the"
    echo "production resolution of '$TARGET_CRATE':"
    for v in "${violations[@]}"; do
        echo "  * $v"
    done
    echo ""
    echo "Root cause class (Pattern P1): a workspace member declares a"
    echo "non-optional dependency on '$TARGET_CRATE' with one of these"
    echo "features in its [dependencies] table. Cargo's resolver v2"
    echo "unifies features ACROSS THE WORKSPACE, so the feature is also"
    echo "ON in production binaries (crates/dpdk-net)."
    echo ""
    pattern="${violations[0]}"
    for v in "${violations[@]:1}"; do pattern="${pattern}|${v}"; done
    echo "Locate the offender:"
    echo "  grep -rn 'dpdk-net-core' tools/ tests/ crates/ --include=Cargo.toml \\"
    echo "    | grep -E 'features.*(${pattern})'"
    echo ""
    echo "Fix pattern:"
    echo "  In the offending Cargo.toml, change"
    echo "      dpdk-net-core = { path = \"...\", features = [\"<forbidden>\"] }"
    echo "  to"
    echo "      dpdk-net-core = { path = \"...\", optional = true }"
    echo "  and gate the runner's behaviour behind its own opt-in feature"
    echo "  that activates 'dpdk-net-core/<forbidden>' transitively."
    echo ""
    echo "Known offender at master HEAD (until task A7 lands):"
    echo "  tools/scapy-fuzz-runner/Cargo.toml line 8 — 'test-inject'"
    fail "${#violations[@]} forbidden feature(s) leaked into production resolution"
fi

echo ""
echo "OK: no forbidden features present in the production resolution of $TARGET_CRATE."
exit 0
