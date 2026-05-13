#!/usr/bin/env bash
# fast-iter-suite.sh — full fast-iter bench suite.
#
# Runs each of {bench-rtt, bench-tx-burst, bench-tx-maxtp, bench-rx-burst}
# against the fast-iter peer for each of {dpdk_net, linux_kernel, fstack}, then
# runs verify-rack-tlp.sh against the netem scenario set, collects all CSVs
# into a timestamped results directory, and prints a single-page summary.
#
# Pre-condition: the peer must be up with all three servers running. Bring it
# up via:
#
#   ./scripts/fast-iter-setup.sh up --with-fstack
#
# That generates ./.fast-iter.env (PEER_IP, PEER_SSH, PEER_*_PORT, FSTACK_CONF)
# which this script sources. The four bench binaries must already be built
# with --features fstack present in their symbol tables — see CLAUDE.md for
# the build incantation.
#
# DPDK NIC exclusivity: only one process can hold the data NIC at a time, so
# every dpdk_net / fstack arm runs sequentially. Inter-arm gaps are kept tight
# (the peer can serve all three stacks back-to-back without resetting).
#
# linux_kernel arms run against a local 127.0.0.1 echo / burst-echo server
# spawned on the DUT, NOT against the remote peer. See
# docs/bench-reports/linux-nat-investigation-2026-05-12.md — the dev-host
# container has a transparent SOCKS5 proxy (REDSOCKS) intercepting all
# outbound TCP, which inflates per-iter RTT from ~75 µs to ~250 ms and times
# out the 10 k-iter bench arms. Local-loopback kernel TCP is the meaningful
# baseline anyway (the linux-vs-dpdk delta is socket-call overhead, not wire
# trip), so this avoids the proxy + measures the right thing.
#
# Usage:
#   ./scripts/fast-iter-suite.sh [--seed N] [--dry-run]
#
# Flags:
#   --seed N        Seed for per-tool stack-order randomization (default: current
#                   epoch). Stack order within each tool is shuffled so the
#                   third-run stack is not systematically disadvantaged by AWS
#                   ENA bandwidth-allowance drain over the suite wallclock (see
#                   codex IMPORTANT I4, 2026-05-13). The seed is logged into
#                   $RESULTS_DIR/metadata.json so a run can be replayed.
#   --dry-run       Print the planned per-tool stack order matrix and exit
#                   without running any bench. Useful for verifying the
#                   randomization works without burning ~35 min of wallclock.
#
# Overrides (env):
#   RESULTS_DIR_OVERRIDE       Absolute path to use instead of the default
#                              target/bench-results/fast-iter-<UTC>/. Useful when
#                              re-running into an existing directory.
#   DUT_PCI                    Default 0000:28:00.0 (a10 perf host).
#   DUT_LOCAL_IP               Default 10.4.1.141.
#   DUT_GATEWAY                Default 10.4.1.1.
#   DUT_LCORE                  Default 2.
#   PEER_NIC                   Default ens5 (peer data NIC, passed to verify-rack-tlp).
#   SKIP_VERIFY_RACK_TLP       Set non-empty to skip the netem matrix.
#   VERIFY_RACK_ITERS          Default 50000 (override for verify-rack-tlp's ITERS).
#   LOCAL_LINUX_ECHO_PORT      Default 10002 (loopback port for linux_kernel echo;
#                              MUST be 10002 because bench-tx-maxtp's linux arm
#                              hard-asserts peer_port=10002, see tools/bench-tx-maxtp/
#                              src/linux.rs::assert_peer_is_sink).
#   LOCAL_LINUX_BURST_PORT     Default 19003 (loopback port for linux_kernel burst).
#   LINUX_KERNEL_PEER_IP       Default 127.0.0.1 (override to force the kernel arm
#                              to talk to the remote peer instead — only useful on
#                              hosts WITHOUT the REDSOCKS proxy in the OUTPUT chain).
#
# Exit code: 0 if at least one bench arm produced a non-empty CSV per stack +
# tool combination. Non-zero only on catastrophic failure (missing binaries,
# unreachable peer, etc.). Individual bench-arm failures are logged + tallied
# in $RESULTS_DIR/SUMMARY.md, not propagated.

set -euo pipefail

# ---------------------------------------------------------------------------
# Paths + config.
# ---------------------------------------------------------------------------
WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$WORKDIR"

# --self-test: run the python summarizer pivot against a synthetic CSV that
# replicates the T57 follow-up #3 shape (mixed-dim rows where `bucket_invalid`
# only appears on a subset). Asserts every cell with a valid CSV value renders
# in the SUMMARY table (i.e. the dim_keys_order grow-mid-stream pivot bug
# does NOT regress). Exits 0 on pass, 1 on regression. No peer / NIC required.
if [ "${1:-}" = "--self-test" ]; then
    python3 - "$WORKDIR/scripts/fast-iter-suite.sh" <<'PY'
import csv, json, os, re, subprocess, sys, tempfile

script_path = sys.argv[1]
src = open(script_path).read()
# Extract the python block between `python3 - "$csv" <<'PY' 2>&1 || true`
# and the trailing `PY` (the heredoc body) so we can exec the summarizer
# in-process on a fixture CSV. The ` 2>&1 || true` suffix is what
# distinguishes the actual heredoc opener from the documentation marker
# above (which appears inside a comment).
m = re.search(r"python3 - \"\$csv\" <<'PY' 2>&1 \|\| true\n(.*?)\nPY\n", src, re.DOTALL)
if not m:
    print("self-test: cannot locate summarize_one_csv python block", file=sys.stderr)
    sys.exit(1)
summarizer_src = m.group(1)

# Build synthetic CSV matching the T57 follow-up #3 shape (fstack maxtp):
# bucket_invalid surfaces only on the W=65536 C=1/C=4 rows, growing
# dim_keys_order mid-iteration. Pre-fix: first 6 rows render as `—`. Post-fix:
# all rows render their CSV values.
cols = [
    "dimensions_json", "metric_name", "metric_unit",
    "metric_value", "metric_aggregation",
]
fixture = [
    ({"C": 1,  "W_bytes": 4096,  "tx_ts_mode": "tsc_fallback", "workload": "maxtp"},
        "sustained_goodput_bps", "mean", "2004778818.3479238"),
    ({"C": 4,  "W_bytes": 4096,  "tx_ts_mode": "tsc_fallback", "workload": "maxtp"},
        "sustained_goodput_bps", "mean", "3010245220.249125"),
    ({"C": 16, "W_bytes": 4096,  "tx_ts_mode": "tsc_fallback", "workload": "maxtp"},
        "sustained_goodput_bps", "mean", "2627077244.311063"),
    ({"C": 1,  "W_bytes": 16384, "tx_ts_mode": "tsc_fallback", "workload": "maxtp"},
        "sustained_goodput_bps", "mean", "2717615814.7228184"),
    ({"C": 4,  "W_bytes": 16384, "tx_ts_mode": "tsc_fallback", "workload": "maxtp"},
        "sustained_goodput_bps", "mean", "2716357792.638736"),
    ({"C": 16, "W_bytes": 16384, "tx_ts_mode": "tsc_fallback", "workload": "maxtp"},
        "sustained_goodput_bps", "mean", "1712376277.6851037"),
    ({"C": 1,  "W_bytes": 65536, "tx_ts_mode": "tsc_fallback", "workload": "maxtp",
      "bucket_invalid": "connect timeout"},
        "sustained_goodput_bps", "mean", "0.0"),
    ({"C": 4,  "W_bytes": 65536, "tx_ts_mode": "tsc_fallback", "workload": "maxtp",
      "bucket_invalid": "connect timeout"},
        "sustained_goodput_bps", "mean", "0.0"),
    ({"C": 16, "W_bytes": 65536, "tx_ts_mode": "tsc_fallback", "workload": "maxtp"},
        "sustained_goodput_bps", "mean", "0.0"),
]
fd, csv_path = tempfile.mkstemp(prefix="fast-iter-selftest-", suffix=".csv")
try:
    with os.fdopen(fd, "w") as f:
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        for dims, metric, agg, val in fixture:
            w.writerow({
                "dimensions_json": json.dumps(dims),
                "metric_name": metric,
                "metric_unit": "bits_per_sec",
                "metric_value": val,
                "metric_aggregation": agg,
            })
    # Run the extracted summarizer in a subprocess so its module-level
    # sys.argv handling matches the production path.
    r = subprocess.run(
        ["python3", "-c", summarizer_src, csv_path],
        capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        print(f"self-test: summarizer crashed rc={r.returncode}\nstderr:\n{r.stderr}", file=sys.stderr)
        sys.exit(1)
    out = r.stdout
finally:
    os.unlink(csv_path)

# Each expected value MUST appear in the rendered table — pre-fix the first
# six rows lost their values entirely.
expected_values = [
    "2004778818.3479238",
    "3010245220.249125",
    "2627077244.311063",
    "2717615814.7228184",
    "2716357792.638736",
    "1712376277.6851037",
]
missing = [v for v in expected_values if v not in out]
if missing:
    print("self-test FAIL: summarizer dropped values for mixed-dim rows.", file=sys.stderr)
    print(f"  missing values: {missing}", file=sys.stderr)
    print("  full output:\n" + out, file=sys.stderr)
    sys.exit(1)

# And the row that lacks bucket_invalid AFTER the dim grew (the
# `C=16, W_bytes=65536` row) must still render its 0.0 value, not be lost.
# Look for the literal markdown row with the C=16 W=65536 cell.
final_row_re = re.compile(r"\|\s*16\s*\|\s*65536\s*\|.*\|\s*0\.0\s*\|")
if not final_row_re.search(out):
    print("self-test FAIL: C=16 W=65536 row missing or value lost.", file=sys.stderr)
    print("  full output:\n" + out, file=sys.stderr)
    sys.exit(1)

print("fast-iter-suite --self-test: OK (pivot handles mixed dim shapes)")
PY
    exit 0
fi

# ---------------------------------------------------------------------------
# CLI flag parsing — --seed + --dry-run.
#
# codex IMPORTANT I4 (2026-05-13): the per-tool stack order is randomized so a
# single suite invocation does not systematically disadvantage the third-run
# stack via AWS ENA bandwidth-allowance drain over the ~35-min wallclock. The
# `--seed N` flag pins the RNG so a regression can be replayed exactly; the
# default is the current epoch (logged into $RESULTS_DIR/metadata.json so any
# run can be replayed).
# ---------------------------------------------------------------------------
SEED=""
DRY_RUN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --seed)
            if [ $# -lt 2 ]; then
                printf 'fast-iter-suite: --seed requires a value\n' >&2
                exit 2
            fi
            SEED="$2"
            shift 2
            ;;
        --seed=*)
            SEED="${1#--seed=}"
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --help|-h)
            sed -n '2,68p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            printf 'fast-iter-suite: unknown flag %s (try --help)\n' "$1" >&2
            exit 2
            ;;
    esac
done
if [ -z "$SEED" ]; then
    SEED=$(date -u +%s)
fi
# Validate seed is a non-negative integer (bash arithmetic later uses it).
case "$SEED" in
    ''|*[!0-9]*)
        printf 'fast-iter-suite: --seed must be a non-negative integer, got %q\n' "$SEED" >&2
        exit 2
        ;;
esac

ENV_FILE="$WORKDIR/.fast-iter.env"
if [ ! -f "$ENV_FILE" ]; then
    printf 'fast-iter-suite: %s not found — run ./scripts/fast-iter-setup.sh up --with-fstack first\n' "$ENV_FILE" >&2
    exit 2
fi
# shellcheck disable=SC1090
source "$ENV_FILE"

: "${PEER_IP:?PEER_IP unset (corrupt .fast-iter.env?)}"
: "${PEER_SSH:?PEER_SSH unset}"
: "${PEER_ECHO_PORT:?PEER_ECHO_PORT unset}"
: "${PEER_SINK_PORT:?PEER_SINK_PORT unset}"
: "${PEER_BURST_PORT:?PEER_BURST_PORT unset}"
: "${FSTACK_CONF:?FSTACK_CONF unset — re-run fast-iter-setup.sh up --with-fstack}"

DUT_PCI="${DUT_PCI:-0000:28:00.0}"
DUT_LOCAL_IP="${DUT_LOCAL_IP:-10.4.1.141}"
DUT_GATEWAY="${DUT_GATEWAY:-10.4.1.1}"
DUT_LCORE="${DUT_LCORE:-2}"
PEER_NIC="${PEER_NIC:-ens5}"
# DUT kernel NIC name + IP — used by the linux_kernel arm via nsenter
# (when LINUX_KERNEL_VIA_NSENTER=1) AND captured by log-nic-state.sh into
# nic-state.txt. This is the SEPARATE physical ENI from $DUT_PCI; see
# T57 "Methodology — two-ENI comparison" section.
DUT_KERNEL_NIC="${DUT_KERNEL_NIC:-ens5}"
DUT_KERNEL_NIC_IP="${DUT_KERNEL_NIC_IP:-10.4.1.139}"
EAL_ARGS="-l 2-3 -n 4 --in-memory --huge-unlink -a ${DUT_PCI},large_llq_hdr=1,miss_txc_to=3"

VERIFY_RACK_ITERS="${VERIFY_RACK_ITERS:-50000}"

# linux_kernel arm routing — see header + docs/bench-reports/linux-nat-investigation-2026-05-12.md.
#
# Two modes:
# - "nsenter" (DEFAULT when available): wrap linux_kernel invocations with
#   `sudo nsenter -t 1 -n` to escape the proxied netns. linux TCP then
#   exits the host's kernel-bound NIC (ens5 / 10.4.1.139) and reaches the
#   REAL peer at $PEER_IP:10001/10002/10003 — fair comparison with
#   dpdk_net (vfio-pci) and fstack (DPDK on the same NIC).
# - "loopback" (fallback): spawn local echo-server + burst-echo-server on
#   127.0.0.1 and point linux at them. Used only when `sudo nsenter -t 1
#   -n true` fails (e.g., on a host that doesn't have nsenter or denies
#   passwordless sudo).
#
# Override via LINUX_KERNEL_VIA_NSENTER=1|0|auto (default auto).
LINUX_KERNEL_VIA_NSENTER="${LINUX_KERNEL_VIA_NSENTER:-auto}"

# Local-loopback fallback config (only used if VIA_NSENTER resolves to 0).
# LOCAL_LINUX_ECHO_PORT defaults to 10002 because bench-tx-maxtp's linux
# arm hard-asserts `peer_port == 10002` (assert_peer_is_sink in
# tools/bench-tx-maxtp/src/linux.rs).
LOCAL_LINUX_PEER_IP="${LINUX_KERNEL_PEER_IP:-127.0.0.1}"
LOCAL_LINUX_ECHO_PORT="${LOCAL_LINUX_ECHO_PORT:-10002}"
LOCAL_LINUX_BURST_PORT="${LOCAL_LINUX_BURST_PORT:-19003}"
LOCAL_ECHO_SERVER_BIN="$WORKDIR/tools/bench-e2e/peer/echo-server"
LOCAL_BURST_ECHO_SERVER_BIN="$WORKDIR/tools/bench-e2e/peer/burst-echo-server"
LOCAL_ECHO_SERVER_PID=""
LOCAL_BURST_ECHO_SERVER_PID=""

UTC_TS="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
RESULTS_DIR="${RESULTS_DIR_OVERRIDE:-$WORKDIR/target/bench-results/fast-iter-$UTC_TS}"
mkdir -p "$RESULTS_DIR"

LOG_FILE="$RESULTS_DIR/suite.log"
: >"$LOG_FILE"

# ---------------------------------------------------------------------------
# Binaries.
# ---------------------------------------------------------------------------
BENCH_RTT="$WORKDIR/target/release/bench-rtt"
BENCH_TX_BURST="$WORKDIR/target/release/bench-tx-burst"
BENCH_TX_MAXTP="$WORKDIR/target/release/bench-tx-maxtp"
BENCH_RX_BURST="$WORKDIR/target/release/bench-rx-burst"
VERIFY_RACK_TLP="$WORKDIR/scripts/verify-rack-tlp.sh"

# --dry-run skips binary existence checks: dry-run only prints the planned
# stack order matrix and exits, so the bench binaries are never invoked.
# This lets the order-randomization logic be smoke-tested on a host that
# hasn't built the suite (e.g. a fresh checkout or an agent worktree).
if [ "$DRY_RUN" != "1" ]; then
    for bin in "$BENCH_RTT" "$BENCH_TX_BURST" "$BENCH_TX_MAXTP" "$BENCH_RX_BURST"; do
        [ -x "$bin" ] || { printf 'fast-iter-suite: missing binary %s\n' "$bin" >&2; exit 2; }
    done
    [ -x "$VERIFY_RACK_TLP" ] || { printf 'fast-iter-suite: missing %s\n' "$VERIFY_RACK_TLP" >&2; exit 2; }

    # Peer C binaries — only needed in loopback fallback mode. Auto-detected
    # below; we still require the binaries on disk in case the operator
    # explicitly sets LINUX_KERNEL_VIA_NSENTER=0.
    [ -x "$LOCAL_ECHO_SERVER_BIN" ] || {
        printf 'fast-iter-suite: missing %s — run `make -C tools/bench-e2e/peer` to build\n' \
            "$LOCAL_ECHO_SERVER_BIN" >&2
        exit 2
    }
    [ -x "$LOCAL_BURST_ECHO_SERVER_BIN" ] || {
        printf 'fast-iter-suite: missing %s — run `make -C tools/bench-e2e/peer` to build\n' \
            "$LOCAL_BURST_ECHO_SERVER_BIN" >&2
        exit 2
    }
fi

# Resolve LINUX_KERNEL_VIA_NSENTER=auto by probing sudo nsenter.
if [ "$LINUX_KERNEL_VIA_NSENTER" = "auto" ]; then
    if sudo -n nsenter -t 1 -n true >/dev/null 2>&1; then
        LINUX_KERNEL_VIA_NSENTER=1
    else
        LINUX_KERNEL_VIA_NSENTER=0
    fi
fi

# Bind the linux_kernel-arm command-prefix array + peer ip/ports.
if [ "$LINUX_KERNEL_VIA_NSENTER" = "1" ]; then
    LINUX_NETNS_WRAPPER=(sudo nsenter -t 1 -n)
    LINUX_PEER_IP="${PEER_IP:-}"
    LINUX_ECHO_PORT="${PEER_ECHO_PORT:-10001}"
    LINUX_SINK_PORT="${PEER_SINK_PORT:-10002}"
    LINUX_BURST_PORT="${PEER_BURST_PORT:-10003}"
else
    LINUX_NETNS_WRAPPER=()
    LINUX_PEER_IP="$LOCAL_LINUX_PEER_IP"
    LINUX_ECHO_PORT="$LOCAL_LINUX_ECHO_PORT"
    LINUX_SINK_PORT="$LOCAL_LINUX_ECHO_PORT"  # local echo-server serves sink semantics too
    LINUX_BURST_PORT="$LOCAL_LINUX_BURST_PORT"
fi

# Verify fstack symbols are present in all four binaries.
# --dry-run skips this check too — dry-run never invokes the binaries.
if [ "$DRY_RUN" != "1" ]; then
    for bin in "$BENCH_RTT" "$BENCH_TX_BURST" "$BENCH_TX_MAXTP" "$BENCH_RX_BURST"; do
        count=$(nm "$bin" 2>/dev/null | grep -c ' T ff_socket' || true)
        if [ "$count" -eq 0 ]; then
            printf 'fast-iter-suite: %s missing fstack symbols — rebuild with --features fstack\n' "$bin" >&2
            exit 2
        fi
    done
fi

# ---------------------------------------------------------------------------
# Logging + run helpers.
# ---------------------------------------------------------------------------
declare -a FAILS=()
declare -a OKS=()
declare -i FAIL_COUNT=0
declare -i OK_COUNT=0

# Per-arm hard cap (seconds). Generous enough for the heaviest configured arm
# (bench-tx-maxtp at 12s × 9 buckets = ~108s) but short enough to bail if a
# bench gets stuck on a hung peer echo-server. Override via env if needed.
RUN_ONE_TIMEOUT="${RUN_ONE_TIMEOUT:-300}"

# Peer echo-server worker count is bounded (~10). The dpdk_net and fstack
# arms leave stale ESTABLISHED connections behind because their stacks tear
# down the TX queue without sending TCP FINs to the peer — so each bucket
# pins one worker slot. Restart the peer's echo-server (NOT the sink/burst
# servers) before every dpdk_net/fstack arm to release worker slots.
PEER_RESTART_DELAY="${PEER_RESTART_DELAY:-1}"

log() { printf '[suite %s] %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$LOG_FILE" >&2; }

ts_now() { date -u +%s; }

# ---------------------------------------------------------------------------
# Per-tool stack-order randomization (codex IMPORTANT I4, 2026-05-13).
#
# Why: scripts/fast-iter-suite.sh used to run dpdk_net → linux_kernel → fstack
# in that fixed order for every arm of every tool. AWS ENA has bandwidth-
# allowance / burst-credit accounting that drains over the ~35-min suite
# wallclock, so the third-run stack is systematically disadvantaged. Cross-
# stack comparisons therefore carried a built-in order bias. T58 variance
# runs also showed environmental drift over a single suite run (T57 → T58
# 2-3× regression on the same code). Result: we now randomize the order
# per-tool, derive the RNG state from a top-level $SEED, and log the
# resulting matrix into $RESULTS_DIR/metadata.json so any run can be
# replayed deterministically.
#
# Shuffle implementation: Fisher-Yates over a copy of $STACKS, seeded by
# bash's $RANDOM. We re-seed RANDOM with a per-tool offset of the master
# seed so a single $SEED reproduces all four tools' orderings independently.
# Bash's $RANDOM is a 15-bit LCG (`__random_seed` in lib/sh/random.c), which
# is more than enough entropy for a 3-element shuffle (3! = 6 permutations).
# ---------------------------------------------------------------------------
STACKS=(dpdk_net linux_kernel fstack)

# Tool index map — index into the master seed so each tool gets a
# deterministic but distinct per-tool seed offset. Declared as a parallel
# pair of arrays (assoc arrays exist but indices stay readable this way).
TOOLS=(bench-rtt bench-tx-burst bench-tx-maxtp bench-rx-burst)

# fisher_yates_shuffle <seed> <stack...> — prints the shuffled stack list
# on stdout, one per line, deterministically. The seed is fed into $RANDOM
# so the same seed always emits the same permutation.
fisher_yates_shuffle() {
    local seed="$1"
    shift
    local -a arr=("$@")
    local n=${#arr[@]}
    local i j tmp
    # Seed bash's PRNG. RANDOM is treated as a sink for the integer.
    RANDOM=$seed
    # Standard Fisher-Yates: walk from end, swap with a random index in
    # [0..i]. The trailing modulus collapses RANDOM's 15-bit range to
    # [0..i] uniformly enough for n=3 (rejection sampling is overkill).
    for (( i = n - 1; i > 0; i-- )); do
        j=$(( RANDOM % (i + 1) ))
        if [ "$j" != "$i" ]; then
            tmp="${arr[i]}"
            arr[i]="${arr[j]}"
            arr[j]="$tmp"
        fi
    done
    local s
    for s in "${arr[@]}"; do
        printf '%s\n' "$s"
    done
}

# Pre-compute the per-tool stack order. Each tool gets a seed of
# `SEED + tool_index` so a single $SEED reproduces the full matrix.
declare -a ORDER_BENCH_RTT=()
declare -a ORDER_BENCH_TX_BURST=()
declare -a ORDER_BENCH_TX_MAXTP=()
declare -a ORDER_BENCH_RX_BURST=()

compute_stack_orders() {
    local tool_idx tool tool_seed stack
    # shellcheck disable=SC2034
    for tool_idx in "${!TOOLS[@]}"; do
        tool="${TOOLS[$tool_idx]}"
        tool_seed=$(( SEED + tool_idx ))
        local -a shuffled=()
        # Capture shuffled list into an array via mapfile.
        mapfile -t shuffled < <(fisher_yates_shuffle "$tool_seed" "${STACKS[@]}")
        case "$tool" in
            bench-rtt)       ORDER_BENCH_RTT=("${shuffled[@]}") ;;
            bench-tx-burst)  ORDER_BENCH_TX_BURST=("${shuffled[@]}") ;;
            bench-tx-maxtp)  ORDER_BENCH_TX_MAXTP=("${shuffled[@]}") ;;
            bench-rx-burst)  ORDER_BENCH_RX_BURST=("${shuffled[@]}") ;;
        esac
    done
}

# tool_stack_order <tool-name> — print the per-tool resolved order, one
# stack per line. Wraps the four ORDER_* arrays so call sites stay terse.
tool_stack_order() {
    case "$1" in
        bench-rtt)       printf '%s\n' "${ORDER_BENCH_RTT[@]}" ;;
        bench-tx-burst)  printf '%s\n' "${ORDER_BENCH_TX_BURST[@]}" ;;
        bench-tx-maxtp)  printf '%s\n' "${ORDER_BENCH_TX_MAXTP[@]}" ;;
        bench-rx-burst)  printf '%s\n' "${ORDER_BENCH_RX_BURST[@]}" ;;
        *) printf 'tool_stack_order: unknown tool %s\n' "$1" >&2; return 1 ;;
    esac
}

# write_metadata_json: emit $RESULTS_DIR/metadata.json with the seed +
# per-tool stack order. Hand-written JSON (no jq dependency): the schema
# is small and stable.
write_metadata_json() {
    local out="$RESULTS_DIR/metadata.json"
    {
        printf '{\n'
        printf '  "seed": %s,\n' "$SEED"
        printf '  "utc_ts": "%s",\n' "$UTC_TS"
        printf '  "dry_run": %s,\n' "$([ "$DRY_RUN" = 1 ] && printf 'true' || printf 'false')"
        printf '  "stack_orders": {\n'
        local last=$((${#TOOLS[@]} - 1))
        local i tool stack first
        for i in "${!TOOLS[@]}"; do
            tool="${TOOLS[$i]}"
            printf '    "%s": [' "$tool"
            first=1
            while IFS= read -r stack; do
                if [ "$first" = 1 ]; then first=0; else printf ', '; fi
                printf '"%s"' "$stack"
            done < <(tool_stack_order "$tool")
            if [ "$i" -lt "$last" ]; then
                printf '],\n'
            else
                printf ']\n'
            fi
        done
        printf '  }\n'
        printf '}\n'
    } >"$out"
}

# Reset DPDK / F-Stack state on the DUT. Used after every DPDK/fstack arm
# (success or failure) and especially after TIMEOUT/FAIL paths where the
# bench process may have been SIGKILL'd while still holding hugepage maps,
# vfio-pci DMA mappings, or running F-Stack callout threads.
#
# Failure mode this guards against (T55 follow-up #6, 2026-05-12 06:10
# fast-iter run): after `bench-rx-burst fstack` timed out at 331 s and was
# SIGKILL'd by the outer `timeout`, every subsequent dpdk_net engine init
# failed with `Invalid port_id=0 / Engine::new failed: PortInfo(0, -19)`
# — DPDK enumerated zero ports because vfio-pci was in a half-released
# state and `/dev/hugepages/` retained 23 stale `rtemap_*` files from the
# killed process.
#
# Sequence:
#   1. SIGKILL any leftover bench-* processes (covers the timeout path
#      where the outer `timeout` killed only the foreground command and
#      the EAL worker thread / F-Stack callout thread kept spinning).
#   2. Sleep 2 s for the kernel to release DMA mappings + IOMMU bindings.
#   3. Clear `/dev/hugepages/*` — `--huge-unlink` is supposed to remove
#      these at startup, but a killed process can leave them behind.
#   4. Log `dpdk-devbind.py --status` so the suite log captures the post-
#      reset vfio-pci binding state (purely diagnostic; non-fatal).
#
# Idempotent + safe to call repeatedly. Never aborts the suite.
reset_dpdk_state() {
    log "    reset_dpdk_state: sweep zombie bench-* + clear stale hugepages"
    sudo pkill -9 -f '/target/release/bench-(rtt|tx-burst|tx-maxtp|rx-burst)' 2>/dev/null || true
    # Brief settle window for kernel to fully release DMA mappings / IOMMU.
    sleep 2
    sudo rm -f /dev/hugepages/* 2>/dev/null || true
    sudo /usr/local/bin/dpdk-devbind.py --status 2>&1 \
        | grep -A 1 'DPDK-compatible' >>"$LOG_FILE" 2>&1 || true
}

# Restart the peer's :10001 echo-server. Used between dpdk_net/fstack arms
# to clear leaked TCP connections that pin all echo-server worker threads.
#
# Implementation note: `pkill -f /tmp/echo-server $PEER_ECHO_PORT` would also
# match the remote bash command we're running (its own argv contains the
# pattern), self-killing the shell. We therefore use `pgrep -fx` (exact full
# cmdline match) → explicit `kill`, which only matches the echo-server.
peer_restart_echo_server() {
    log "    peer: restart echo-server :$PEER_ECHO_PORT (clear stale ESTAB)"
    ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$PEER_SSH" "
        pids=\$(pgrep -fx '/tmp/echo-server $PEER_ECHO_PORT' 2>/dev/null || true)
        if [ -n \"\$pids\" ]; then
            kill -KILL \$pids 2>/dev/null || true
            sleep 0.3
        fi
        nohup /tmp/echo-server $PEER_ECHO_PORT >/tmp/echo-server.log 2>&1 </dev/null &
        disown
        sleep $PEER_RESTART_DELAY
        pgrep -fx '/tmp/echo-server $PEER_ECHO_PORT' >/dev/null
    " >>"$LOG_FILE" 2>&1 || log "    WARN: peer echo-server restart failed (see $LOG_FILE)"
}

# Restart the peer's burst-echo-server on :$PEER_BURST_PORT — mirror of
# peer_restart_echo_server but for the rx-burst tests. A wedged
# burst-echo-server (stuck in write() to a SIGKILLed DUT) is the
# canonical cause of bench-rx-burst per-bucket stalls.
peer_restart_burst_echo_server() {
    log "    peer: restart burst-echo-server :$PEER_BURST_PORT (clear stale ESTAB)"
    ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$PEER_SSH" "
        pids=\$(pgrep -fx '/tmp/burst-echo-server $PEER_BURST_PORT' 2>/dev/null || true)
        if [ -n \"\$pids\" ]; then
            kill -KILL \$pids 2>/dev/null || true
            sleep 0.3
        fi
        nohup /tmp/burst-echo-server $PEER_BURST_PORT >/tmp/burst-echo-server.log 2>&1 </dev/null &
        disown
        sleep $PEER_RESTART_DELAY
        pgrep -fx '/tmp/burst-echo-server $PEER_BURST_PORT' >/dev/null
    " >>"$LOG_FILE" 2>&1 || log "    WARN: peer burst-echo-server restart failed (see $LOG_FILE)"
}

# run_one <desc> <output-csv> <command...>
#
# Runs the bench command under `timeout $RUN_ONE_TIMEOUT`, appending stdout+stderr
# to the per-arm log file (and the suite log). Outputs OK / FAIL / TIMEOUT
# message. Never aborts the suite.
run_one() {
    local desc="$1" outcsv="$2"
    shift 2
    local arm_log
    arm_log="$RESULTS_DIR/$(basename "$outcsv" .csv).log"

    local started ended elapsed
    started=$(ts_now)
    log ">>> $desc"
    log "    csv:   $outcsv"
    log "    log:   $arm_log"

    {
        printf '=== %s ===\n' "$desc"
        printf 'cmd: %s\n' "$*"
        printf 'started: %s timeout=%ss\n' "$(date -u -Iseconds)" "$RUN_ONE_TIMEOUT"
    } >"$arm_log"

    # `timeout --foreground` lets Ctrl+C reach the bench process; `-k 30`
    # SIGKILLs 30s after SIGTERM if the bench refuses to exit.
    if timeout --foreground -k 30 "$RUN_ONE_TIMEOUT" "$@" >>"$arm_log" 2>&1; then
        ended=$(ts_now)
        elapsed=$((ended - started))
        log "    OK ($elapsed s)"
        OKS+=("$desc")
        OK_COUNT=$((OK_COUNT + 1))
        printf 'OK %s elapsed=%ds\n' "$desc" "$elapsed" >>"$arm_log"
        return 0
    else
        local rc=$?
        ended=$(ts_now)
        elapsed=$((ended - started))
        local tag="FAIL"
        if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
            tag="TIMEOUT"
        fi
        log "    $tag rc=$rc ($elapsed s) — see $arm_log"
        FAILS+=("$desc ($tag rc=$rc, log=$arm_log)")
        FAIL_COUNT=$((FAIL_COUNT + 1))
        printf '%s rc=%d %s elapsed=%ds\n' "$tag" "$rc" "$desc" "$elapsed" >>"$arm_log"

        # TIMEOUT path: the outer `timeout` SIGKILLed only the foreground
        # process; EAL worker / F-Stack callout threads may still be running
        # and holding hugepage maps + DMA mappings. Sweep the entire bench-*
        # process tree before returning to caller. The post-run
        # reset_dpdk_state() in run_bench_* handles the hugepage cleanup,
        # but the kill here closes the race where reset_dpdk_state() runs
        # before the zombie has had a chance to exit.
        if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
            sudo pkill -9 -f '/target/release/bench-(rtt|tx-burst|tx-maxtp|rx-burst)' 2>/dev/null || true
            sleep 1
        fi
        return 0  # don't abort the suite
    fi
}

# ---------------------------------------------------------------------------
# Local-loopback servers for linux_kernel arms.
#
# Why local: the dev-host container intercepts every outbound TCP packet via a
# transparent SOCKS5 proxy (REDSOCKS, see iptables OUTPUT chain). Direct
# kernel-TCP to the remote peer ends up tunneled and runs at ~250 ms/iter,
# blowing the per-arm RUN_ONE_TIMEOUT. Local 127.0.0.1 traffic is on the
# REDSOCKS RETURN allowlist, so the loopback path stays unproxied and the
# kernel-TCP arm completes in microseconds-per-iter. Comparison value to
# dpdk_net is preserved — the linux-vs-dpdk delta is socket-call overhead,
# not wire trip.
# ---------------------------------------------------------------------------

start_local_linux_servers() {
    log "spawn local linux servers: echo on $LOCAL_LINUX_PEER_IP:$LOCAL_LINUX_ECHO_PORT, burst on $LOCAL_LINUX_PEER_IP:$LOCAL_LINUX_BURST_PORT"

    # Kill any stale instances from a previous run on the same ports.
    pkill -KILL -f "echo-server $LOCAL_LINUX_ECHO_PORT" 2>/dev/null || true
    pkill -KILL -f "burst-echo-server $LOCAL_LINUX_BURST_PORT" 2>/dev/null || true
    sleep 0.2

    "$LOCAL_ECHO_SERVER_BIN" "$LOCAL_LINUX_ECHO_PORT" \
        >"$RESULTS_DIR/local-echo-server.log" 2>&1 &
    LOCAL_ECHO_SERVER_PID=$!
    disown $LOCAL_ECHO_SERVER_PID 2>/dev/null || true

    "$LOCAL_BURST_ECHO_SERVER_BIN" "$LOCAL_LINUX_BURST_PORT" \
        >"$RESULTS_DIR/local-burst-echo-server.log" 2>&1 &
    LOCAL_BURST_ECHO_SERVER_PID=$!
    disown $LOCAL_BURST_ECHO_SERVER_PID 2>/dev/null || true

    sleep 0.3
    if ! kill -0 "$LOCAL_ECHO_SERVER_PID" 2>/dev/null; then
        log "FATAL: local echo-server failed to start — see $RESULTS_DIR/local-echo-server.log"
        exit 2
    fi
    if ! kill -0 "$LOCAL_BURST_ECHO_SERVER_PID" 2>/dev/null; then
        log "FATAL: local burst-echo-server failed to start — see $RESULTS_DIR/local-burst-echo-server.log"
        exit 2
    fi
    log "    local servers up: echo pid=$LOCAL_ECHO_SERVER_PID burst pid=$LOCAL_BURST_ECHO_SERVER_PID"
}

stop_local_linux_servers() {
    if [ -n "$LOCAL_ECHO_SERVER_PID" ]; then
        kill -KILL "$LOCAL_ECHO_SERVER_PID" 2>/dev/null || true
        LOCAL_ECHO_SERVER_PID=""
    fi
    if [ -n "$LOCAL_BURST_ECHO_SERVER_PID" ]; then
        kill -KILL "$LOCAL_BURST_ECHO_SERVER_PID" 2>/dev/null || true
        LOCAL_BURST_ECHO_SERVER_PID=""
    fi
    # Defensive sweep in case the PID variables were lost (e.g. set -e abort
    # between spawn and assignment).
    pkill -KILL -f "echo-server $LOCAL_LINUX_ECHO_PORT" 2>/dev/null || true
    pkill -KILL -f "burst-echo-server $LOCAL_LINUX_BURST_PORT" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Pre-flight peer reachability check.
# ---------------------------------------------------------------------------
preflight() {
    log "preflight: peer=$PEER_IP fstack_conf=$FSTACK_CONF"

    # Defensive DPDK reset at suite start: clears zombie bench processes
    # and stale `/dev/hugepages/rtemap_*` files left behind by a prior
    # crashed run. Cheap (≤3 s) and idempotent. Without this, an operator
    # who Ctrl-C'd a previous run mid-DPDK-arm sees the first dpdk_net
    # init of the new run fail with `PortInfo(0, -19)` because
    # /dev/hugepages still has the old run's rtemap files mapped.
    reset_dpdk_state

    if ! ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$PEER_SSH" \
            "pgrep -af '/tmp/echo-server' >/dev/null && pgrep -af '/tmp/linux-tcp-sink' >/dev/null && pgrep -af '/tmp/burst-echo-server' >/dev/null"; then
        log "FATAL: one or more peer servers not running — abort"
        exit 2
    fi
    log "preflight: all 3 peer servers running OK"

    # DPDK NIC must be bound to vfio-pci.
    local drv
    drv=$(readlink "/sys/bus/pci/devices/$DUT_PCI/driver" 2>/dev/null | xargs -r basename || echo "unbound")
    if [ "$drv" != "vfio-pci" ]; then
        log "FATAL: DUT NIC $DUT_PCI bound to '$drv' (need vfio-pci) — abort"
        exit 2
    fi
    log "preflight: DUT $DUT_PCI bound to vfio-pci OK"

    # Two-ENI state capture. The linux_kernel arm uses a different physical
    # NIC than dpdk_net + fstack (T57 codex-review BLOCKER B2: same peer,
    # different wire). Capture per-NIC ethtool / IRQ / qdisc / iptables /
    # route state for BOTH DUT NICs AND the peer's kernel NIC to a single
    # `nic-state.txt` so reviewers can verify that queue / coalescing /
    # offload differences across NICs explain (or don't explain) any
    # deltas in the headline numbers. See T57 "Methodology — two-ENI
    # comparison" section + SUMMARY.md NOTE block.
    if [ -x "$WORKDIR/scripts/log-nic-state.sh" ]; then
        log "preflight: capture two-ENI state → $RESULTS_DIR/nic-state.txt"
        DUT_KERNEL_NIC="$DUT_KERNEL_NIC" \
        DUT_KERNEL_NIC_IP="$DUT_KERNEL_NIC_IP" \
        DUT_DPDK_PCI="$DUT_PCI" \
        PEER_NIC="$PEER_NIC" \
        PEER_SSH="$PEER_SSH" \
            "$WORKDIR/scripts/log-nic-state.sh" "$RESULTS_DIR/nic-state.txt" \
            >>"$LOG_FILE" 2>&1 \
            || log "    WARN: log-nic-state.sh non-zero exit (capture is observational; not aborting)"
    else
        log "preflight: log-nic-state.sh missing — skipping two-ENI state capture"
    fi

    # Reset the peer's :10001 echo-server + :10003 burst-echo-server so we
    # start with no stale ESTAB connections holding worker slots. These are
    # the ONLY preemptive restarts in the suite — the hardened servers
    # (T56 v4, 2026-05-12: pthread-per-conn + TCP_USER_TIMEOUT=5s) self-
    # recover from a DUT-side SIGKILL within ~5 s, so we no longer restart
    # between every arm. The pre-suite restart remains as a defensive
    # safety net in case the peer is still running the un-hardened
    # binaries.
    peer_restart_echo_server
    peer_restart_burst_echo_server

    # linux_kernel arm routing — see header.
    if [ "$LINUX_KERNEL_VIA_NSENTER" = "1" ]; then
        log "linux_kernel routing: nsenter → host netns → real peer $PEER_IP:10001/10002/10003 (fair-comparison mode)"
    else
        log "linux_kernel routing: local-loopback fallback (sudo nsenter unavailable)"
        start_local_linux_servers
    fi
}

# ---------------------------------------------------------------------------
# Per-stack invocation helpers.
# ---------------------------------------------------------------------------

# bench-rtt
#
# Per-stack helpers — each emits one arm. Split out of run_bench_rtt so the
# parent function can iterate the shuffled $ORDER_BENCH_RTT and dispatch
# without baking a fixed dpdk_net → linux_kernel → fstack order into the
# call sequence (codex IMPORTANT I4, 2026-05-13).
#
# --raw-samples-csv: emits one row per iteration (bucket_id, iter_idx,
# rtt_ns) so write_summary can compute p99/p999 + decile signatures
# from the underlying distribution rather than relying on the per-CSV
# p50/p99/mean aggregates alone. Surfaced for T58 follow-up: fstack
# 128B/1024B showed run-to-run bimodality that p50-only summaries
# hid. See docs/bench-reports/fstack-bimodality-investigation-*.md.
run_bench_rtt_dpdk_net() {
    run_one "bench-rtt dpdk_net" \
        "$RESULTS_DIR/bench-rtt-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_RTT" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rtt-dpdk_net.csv" \
                --raw-samples-csv "$RESULTS_DIR/bench-rtt-dpdk_net-raw.csv" \
                --payload-bytes-sweep 64,128,256,1024 \
                --iterations 10000 --warmup 100
    reset_dpdk_state
}

# linux_kernel → host-netns wrapper + real peer (default) OR local-loopback (fallback).
run_bench_rtt_linux_kernel() {
    run_one "bench-rtt linux_kernel" \
        "$RESULTS_DIR/bench-rtt-linux_kernel.csv" \
        "${LINUX_NETNS_WRAPPER[@]}" "$BENCH_RTT" \
            --stack linux_kernel \
            --peer-ip "$LINUX_PEER_IP" \
            --peer-port "$LINUX_ECHO_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-rtt-linux_kernel.csv" \
            --raw-samples-csv "$RESULTS_DIR/bench-rtt-linux_kernel-raw.csv" \
            --payload-bytes-sweep 64,128,256,1024 \
            --iterations 10000 --warmup 100
}

run_bench_rtt_fstack() {
    run_one "bench-rtt fstack" \
        "$RESULTS_DIR/bench-rtt-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_RTT" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rtt-fstack.csv" \
                --raw-samples-csv "$RESULTS_DIR/bench-rtt-fstack-raw.csv" \
                --payload-bytes-sweep 64,128,256,1024 \
                --iterations 10000 --warmup 100
    reset_dpdk_state
}

run_bench_rtt() {
    log "=== bench-rtt — RTT (payload sweep 64,128,256,1024) — order: ${ORDER_BENCH_RTT[*]} ==="
    local stack
    while IFS= read -r stack; do
        case "$stack" in
            dpdk_net)      run_bench_rtt_dpdk_net ;;
            linux_kernel)  run_bench_rtt_linux_kernel ;;
            fstack)        run_bench_rtt_fstack ;;
            *) log "run_bench_rtt: unknown stack $stack — skipping" ;;
        esac
    done < <(tool_stack_order bench-rtt)
}

# bench-tx-burst — per-stack helpers + parent dispatcher (codex IMPORTANT
# I4, 2026-05-13). See run_bench_rtt above for the rationale.
run_bench_tx_burst_dpdk_net() {
    run_one "bench-tx-burst dpdk_net" \
        "$RESULTS_DIR/bench-tx-burst-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_TX_BURST" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-burst-dpdk_net.csv" \
                --burst-sizes 65536,1048576 \
                --gap-mss 0,10 \
                --bursts-per-bucket 200 --warmup 20
    reset_dpdk_state
}

# linux_kernel → host-netns wrapper + real peer (default) OR local-loopback (fallback).
run_bench_tx_burst_linux_kernel() {
    run_one "bench-tx-burst linux_kernel" \
        "$RESULTS_DIR/bench-tx-burst-linux_kernel.csv" \
        "${LINUX_NETNS_WRAPPER[@]}" "$BENCH_TX_BURST" \
            --stack linux_kernel \
            --peer-ip "$LINUX_PEER_IP" \
            --peer-port "$LINUX_ECHO_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-tx-burst-linux_kernel.csv" \
            --burst-sizes 65536,1048576 \
            --gap-mss 0,10 \
            --bursts-per-bucket 200 --warmup 20
}

run_bench_tx_burst_fstack() {
    run_one "bench-tx-burst fstack" \
        "$RESULTS_DIR/bench-tx-burst-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_TX_BURST" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-burst-fstack.csv" \
                --burst-sizes 65536,1048576 \
                --gap-mss 0,10 \
                --bursts-per-bucket 200 --warmup 20
    reset_dpdk_state
}

run_bench_tx_burst() {
    log "=== bench-tx-burst — K x G grid (K={64K,1M}, G={0,10}) — order: ${ORDER_BENCH_TX_BURST[*]} ==="
    local stack
    while IFS= read -r stack; do
        case "$stack" in
            dpdk_net)      run_bench_tx_burst_dpdk_net ;;
            linux_kernel)  run_bench_tx_burst_linux_kernel ;;
            fstack)        run_bench_tx_burst_fstack ;;
            *) log "run_bench_tx_burst: unknown stack $stack — skipping" ;;
        esac
    done < <(tool_stack_order bench-tx-burst)
}

# bench-tx-maxtp — per-stack helpers + parent dispatcher (codex IMPORTANT
# I4, 2026-05-13). See run_bench_rtt above for the rationale.
run_bench_tx_maxtp_dpdk_net() {
    run_one "bench-tx-maxtp dpdk_net" \
        "$RESULTS_DIR/bench-tx-maxtp-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_TX_MAXTP" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-maxtp-dpdk_net.csv" \
                --write-sizes 4096,16384,65536 \
                --conn-counts 1,4,16 \
                --warmup-secs 2 --duration-secs 10
    reset_dpdk_state
}

# linux_kernel → host-netns wrapper + real peer's :10002 linux-tcp-sink
# (default) OR local-loopback fallback. The `--local-ip` flag is
# documented as dpdk-only but bench-tx-maxtp's peer-rwnd probe path
# still parses it as IPv4 for every stack, so we pass DUT_LOCAL_IP here
# too — it's a no-op for the linux arm itself.
run_bench_tx_maxtp_linux_kernel() {
    run_one "bench-tx-maxtp linux_kernel" \
        "$RESULTS_DIR/bench-tx-maxtp-linux_kernel.csv" \
        "${LINUX_NETNS_WRAPPER[@]}" "$BENCH_TX_MAXTP" \
            --stack linux_kernel \
            --local-ip "$DUT_LOCAL_IP" \
            --peer-ip "$LINUX_PEER_IP" \
            --peer-port "$LINUX_SINK_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-tx-maxtp-linux_kernel.csv" \
            --write-sizes 4096,16384,65536 \
            --conn-counts 1,4,16 \
            --warmup-secs 2 --duration-secs 10
}

run_bench_tx_maxtp_fstack() {
    run_one "bench-tx-maxtp fstack" \
        "$RESULTS_DIR/bench-tx-maxtp-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_TX_MAXTP" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-port "$PEER_ECHO_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-tx-maxtp-fstack.csv" \
                --write-sizes 4096,16384,65536 \
                --conn-counts 1,4,16 \
                --warmup-secs 2 --duration-secs 10
    reset_dpdk_state
}

run_bench_tx_maxtp() {
    log "=== bench-tx-maxtp — W x C grid (W={4K,16K,64K}, C={1,4,16}) — order: ${ORDER_BENCH_TX_MAXTP[*]} ==="
    local stack
    while IFS= read -r stack; do
        case "$stack" in
            dpdk_net)      run_bench_tx_maxtp_dpdk_net ;;
            linux_kernel)  run_bench_tx_maxtp_linux_kernel ;;
            fstack)        run_bench_tx_maxtp_fstack ;;
            *) log "run_bench_tx_maxtp: unknown stack $stack — skipping" ;;
        esac
    done < <(tool_stack_order bench-tx-maxtp)
}

# bench-rx-burst — per-stack helpers + parent dispatcher (codex IMPORTANT
# I4, 2026-05-13). See run_bench_rtt above for the rationale.
#
# T56 v4 (2026-05-12): preflight already restarted burst-echo-server.
# The hardened server (pthread-per-conn + TCP_USER_TIMEOUT=5s) clears
# wedged worker threads within 5 s, so we no longer need to restart it
# between arms.
run_bench_rx_burst_dpdk_net() {
    run_one "bench-rx-burst dpdk_net" \
        "$RESULTS_DIR/bench-rx-burst-dpdk_net.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" \
            "$BENCH_RX_BURST" \
                --stack dpdk_net \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-control-port "$PEER_BURST_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --tool fast-iter-suite \
                --feature-set dpdk_net \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rx-burst-dpdk_net.csv" \
                --segment-sizes 64,128,256 \
                --burst-counts 16,64,256 \
                --measure-bursts 200 --warmup-bursts 20
    reset_dpdk_state
}

# linux_kernel → host-netns wrapper + real peer (default) OR local-loopback (fallback).
run_bench_rx_burst_linux_kernel() {
    run_one "bench-rx-burst linux_kernel" \
        "$RESULTS_DIR/bench-rx-burst-linux_kernel.csv" \
        "${LINUX_NETNS_WRAPPER[@]}" "$BENCH_RX_BURST" \
            --stack linux_kernel \
            --peer-ip "$LINUX_PEER_IP" \
            --peer-control-port "$LINUX_BURST_PORT" \
            --tool fast-iter-suite \
            --feature-set linux_kernel \
            --precondition-mode lenient \
            --output-csv "$RESULTS_DIR/bench-rx-burst-linux_kernel.csv" \
            --segment-sizes 64,128,256 \
            --burst-counts 16,64,256 \
            --measure-bursts 200 --warmup-bursts 20
}

run_bench_rx_burst_fstack() {
    run_one "bench-rx-burst fstack" \
        "$RESULTS_DIR/bench-rx-burst-fstack.csv" \
        sudo -E env "PATH=$PATH" PEER_IP="$PEER_IP" FSTACK_CONF="$FSTACK_CONF" \
            "$BENCH_RX_BURST" \
                --stack fstack \
                --local-ip "$DUT_LOCAL_IP" \
                --gateway-ip "$DUT_GATEWAY" \
                --peer-ip "$PEER_IP" \
                --peer-control-port "$PEER_BURST_PORT" \
                --eal-args "$EAL_ARGS" \
                --lcore "$DUT_LCORE" \
                --fstack-conf "$FSTACK_CONF" \
                --tool fast-iter-suite \
                --feature-set fstack \
                --precondition-mode lenient \
                --output-csv "$RESULTS_DIR/bench-rx-burst-fstack.csv" \
                --segment-sizes 64,128,256 \
                --burst-counts 16,64,256 \
                --measure-bursts 200 --warmup-bursts 20
    reset_dpdk_state
}

run_bench_rx_burst() {
    log "=== bench-rx-burst — W x N grid (W={64,128,256}, N={16,64,256}) — order: ${ORDER_BENCH_RX_BURST[*]} ==="
    local stack
    while IFS= read -r stack; do
        case "$stack" in
            dpdk_net)      run_bench_rx_burst_dpdk_net ;;
            linux_kernel)  run_bench_rx_burst_linux_kernel ;;
            fstack)        run_bench_rx_burst_fstack ;;
            *) log "run_bench_rx_burst: unknown stack $stack — skipping" ;;
        esac
    done < <(tool_stack_order bench-rx-burst)
}

# verify-rack-tlp
run_verify_rack_tlp() {
    if [ -n "${SKIP_VERIFY_RACK_TLP:-}" ]; then
        log "=== verify-rack-tlp SKIPPED (SKIP_VERIFY_RACK_TLP=$SKIP_VERIFY_RACK_TLP) ==="
        return 0
    fi
    log "=== verify-rack-tlp — netem scenario matrix (ITERS=$VERIFY_RACK_ITERS) ==="

    local artifacts="$RESULTS_DIR/verify-rack-tlp"
    mkdir -p "$artifacts"

    # verify-rack-tlp runs 6 scenarios sequentially (5 peer-egress loss +
    # 1 peer-ingress reorder via ifb — the rack_reorder_4k cell added by
    # codex B3 repair, 2026-05-13). Post-trim the 6 scenarios total
    # ~13-16 min; allow 35 min before SIGKILL to leave generous slack
    # (RTO-bound scenarios have wide per-run spread, and the rack_reorder
    # setup adds ~10 s for ifb load + tcp_sack flip on top of the ~30 s
    # bench run). Override via VERIFY_RACK_TLP_TIMEOUT env if needed.
    local prev_timeout="$RUN_ONE_TIMEOUT"
    RUN_ONE_TIMEOUT="${VERIFY_RACK_TLP_TIMEOUT:-2100}"
    run_one "verify-rack-tlp" \
        "$artifacts/verify-rack-tlp.log" \
        env \
            PEER_IP="$PEER_IP" \
            PEER_SSH="$PEER_SSH" \
            PEER_NIC="$PEER_NIC" \
            PEER_ECHO_PORT="$PEER_ECHO_PORT" \
            DUT_IP="$DUT_LOCAL_IP" \
            DUT_GATEWAY="$DUT_GATEWAY" \
            DUT_PCI="$DUT_PCI" \
            DUT_LCORE="$DUT_LCORE" \
            DUT_EAL_ARGS="$EAL_ARGS" \
            ARTIFACTS_DIR="$artifacts" \
            ITERS="$VERIFY_RACK_ITERS" \
            PRECONDITION_MODE=lenient \
            BENCH_RTT_BIN="$BENCH_RTT" \
            "$VERIFY_RACK_TLP"
    RUN_ONE_TIMEOUT="$prev_timeout"
}

# ---------------------------------------------------------------------------
# Summary generation (parse CSVs into SUMMARY.md).
# ---------------------------------------------------------------------------

# CSV schema (spec §14, bench_common::csv_row::CsvRow). The columns we rely on
# here are `test_case`, `feature_set`, `dimensions_json` (JSON-encoded grid
# coords), `metric_name`, `metric_unit`, `metric_value`, `metric_aggregation`.
# One row per (bucket, metric, aggregation) tuple — typically 7 aggregations
# per metric per bucket (p50/p99/p999/mean/stddev/ci95_lower/ci95_upper).
#
# This summarizer pivots into (bucket × metric) tables with p50/p99/mean cols.

summarize_one_csv() {
    local csv="$1"
    if [ ! -s "$csv" ]; then
        printf '_(no data — CSV missing or empty)_\n\n'
        return
    fi
    python3 - "$csv" <<'PY' 2>&1 || true
import csv, json, sys
path = sys.argv[1]
try:
    with open(path) as f:
        rows = list(csv.DictReader(f))
except Exception as e:
    print(f"_(error reading CSV: {e})_")
    sys.exit(0)
if not rows:
    print("_(empty CSV — header only)_")
    sys.exit(0)

# (dim_tuple, metric_name, aggregation) → (value, unit)
#
# Pivot bug history: a single-pass build of `data` and `buckets` keyed on
# `dim_keys_order` (which can GROW as new dims like `bucket_invalid` appear
# mid-stream) produced length-mismatched tuple keys — earlier rows stored
# short tuples, later lookups used padded long tuples, so early cells
# silently rendered as `—` even with valid CSV values (T57 follow-up #3,
# observed on bench-tx-maxtp fstack where bucket_invalid surfaced only on
# the W=64K rows). Fix: two-pass — first pass discovers all dim keys, second
# pass keys `data`/`buckets` with the final dim_keys_order shape.
data = {}
metrics = []
seen_metrics = set()
buckets = []
seen_buckets = set()
dim_keys_order = []

# Pass 1: discover the full dim_keys_order across ALL rows. Required so that
# the per-row dim_tup constructed in pass 2 already matches the final shape.
parsed_dims = []
for r in rows:
    try:
        dims = json.loads(r.get("dimensions_json", "{}") or "{}")
    except Exception:
        dims = {}
    # Drop the `stack` dim (constant per CSV).
    dims.pop("stack", None)
    for k in dims:
        if k not in dim_keys_order:
            dim_keys_order.append(k)
    parsed_dims.append(dims)

# Pass 2: build data/buckets with tuples shaped by the FINAL dim_keys_order.
for r, dims in zip(rows, parsed_dims):
    dim_tup = tuple(str(dims.get(k, "")) for k in dim_keys_order)
    metric = r.get("metric_name", "")
    unit = r.get("metric_unit", "")
    agg = r.get("metric_aggregation", "")
    val = r.get("metric_value", "")
    data[(dim_tup, metric, agg)] = (val, unit)
    if metric and metric not in seen_metrics:
        metrics.append(metric)
        seen_metrics.add(metric)
    if dim_tup not in seen_buckets:
        buckets.append(dim_tup)
        seen_buckets.add(dim_tup)

for metric in metrics:
    # Pick the unit from the first matching row.
    unit = next((u for ((_, m, _), (_, u)) in data.items() if m == metric), "")
    print(f"**metric: `{metric}`** ({unit})")
    print()
    hdr = list(dim_keys_order) + ["p50", "p99", "mean"]
    print("| " + " | ".join(hdr) + " |")
    print("|" + "|".join(["---"] * len(hdr)) + "|")
    for b in buckets:
        row = list(b)
        for agg in ("p50", "p99", "mean"):
            val, _ = data.get((b, metric, agg), ("—", ""))
            row.append(val if val else "—")
        print("| " + " | ".join(row) + " |")
    print()
PY
}

# summarize_rtt_with_raw: bench-rtt-specific summarizer that consumes BOTH
# the aggregate summary CSV (for metric labels / order) AND the raw-sample
# sidecar (for p50/p99/p999/mean recomputed from the underlying samples).
# The aggregate CSV only carries p50/p99/mean from emit_csv, so p999
# requires the raw sidecar. Falls back to summarize_one_csv if no raw
# CSV is present (e.g. older runs or a missing sidecar).
summarize_rtt_with_raw() {
    local summary_csv="$1"
    local raw_csv="$2"
    if [ ! -s "$raw_csv" ]; then
        summarize_one_csv "$summary_csv"
        return
    fi
    python3 - "$raw_csv" <<'PY' 2>&1 || true
import csv, statistics, sys
from collections import defaultdict
path = sys.argv[1]
try:
    by_bucket = defaultdict(list)
    with open(path) as f:
        for r in csv.DictReader(f):
            bid = r.get("bucket_id", "")
            try:
                by_bucket[bid].append(float(r["rtt_ns"]))
            except (KeyError, ValueError):
                continue
except Exception as e:
    print(f"_(error reading raw CSV: {e})_")
    sys.exit(0)
if not by_bucket:
    print("_(raw CSV had no rows)_")
    sys.exit(0)

def pct(sorted_xs, p):
    if not sorted_xs:
        return float("nan")
    n = len(sorted_xs)
    # Nearest-rank percentile, matching bench_common's typical convention.
    idx = max(0, min(n - 1, int(round(p * (n - 1)))))
    return sorted_xs[idx]

# Each bucket_id is "payload_<bytes>" — strip the prefix for display.
def bid_label(bid):
    if bid.startswith("payload_"):
        return bid[len("payload_"):]
    return bid

print("**metric: `rtt_ns`** (ns, recomputed from raw samples)")
print()
hdr = ["payload_bytes", "n", "p50", "p99", "p999", "mean"]
print("| " + " | ".join(hdr) + " |")
print("|" + "|".join(["---"] * len(hdr)) + "|")
# Sort numerically by payload size so the table reads 64,128,256,1024.
def sort_key(bid):
    try:
        return int(bid_label(bid))
    except ValueError:
        return 1 << 62
for bid in sorted(by_bucket.keys(), key=sort_key):
    xs = sorted(by_bucket[bid])
    n = len(xs)
    p50 = pct(xs, 0.50)
    p99 = pct(xs, 0.99)
    p999 = pct(xs, 0.999)
    mean = statistics.fmean(xs) if xs else float("nan")
    print(f"| {bid_label(bid)} | {n} | {p50:.0f} | {p99:.0f} | {p999:.0f} | {mean:.0f} |")
print()

# Decile signature — exposes bimodality. A single jump between two
# adjacent deciles (e.g. d4=200µs, d5=300µs) signals a bimodal
# distribution; a smooth march signals unimodal.
print("**deciles (d1..d9, ns) — for bimodality screening**")
print()
hdr2 = ["payload_bytes", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9"]
print("| " + " | ".join(hdr2) + " |")
print("|" + "|".join(["---"] * len(hdr2)) + "|")
for bid in sorted(by_bucket.keys(), key=sort_key):
    xs = sorted(by_bucket[bid])
    cells = [bid_label(bid)]
    for d in range(1, 10):
        cells.append(f"{pct(xs, d / 10.0):.0f}")
    print("| " + " | ".join(cells) + " |")
print()
PY
}

write_summary() {
    local summary="$RESULTS_DIR/SUMMARY.md"
    {
        printf '# fast-iter-suite SUMMARY — %s\n\n' "$UTC_TS"
        printf '**Results directory:** `%s`\n\n' "$RESULTS_DIR"
        printf '**Peer:** `%s` (ens5)  •  **DUT:** `%s` (PCI `%s`, lcore %s)\n\n' \
            "$PEER_IP" "$DUT_LOCAL_IP" "$DUT_PCI" "$DUT_LCORE"
        printf '**Wallclock:** %s — %s\n\n' "$WALLCLOCK_START_HUMAN" "$WALLCLOCK_END_HUMAN"
        printf '**Outcome:** %d OK, %d FAIL\n\n' "$OK_COUNT" "$FAIL_COUNT"
        printf '**Seed:** `%s` (per-tool stack-order randomization, codex IMPORTANT I4)\n\n' "$SEED"

        # Per-tool stack-order matrix — codex IMPORTANT I4, 2026-05-13.
        # The pre-fix suite ran dpdk_net → linux_kernel → fstack for every
        # tool, so the third-run stack was systematically disadvantaged by
        # AWS ENA bandwidth-allowance drain over the ~35-min wallclock.
        # Order is now derived from $SEED + tool_index (Fisher-Yates), and
        # logged here so reviewers can verify the comparison is order-
        # symmetric across runs. Re-run with --seed $SEED to reproduce the
        # exact ordering.
        printf '## Stack-order matrix (codex I4 randomization)\n\n'
        printf '> Per-tool stack execution order, shuffled from the master `seed` above.\n'
        printf '> See `metadata.json` for the machine-readable form.\n\n'
        printf '| tool | 1st | 2nd | 3rd |\n'
        printf '|---|---|---|---|\n'
        local _t
        for _t in "${TOOLS[@]}"; do
            local _o1 _o2 _o3
            local _arr=()
            mapfile -t _arr < <(tool_stack_order "$_t")
            _o1="${_arr[0]:-}"; _o2="${_arr[1]:-}"; _o3="${_arr[2]:-}"
            printf '| %s | %s | %s | %s |\n' "$_t" "$_o1" "$_o2" "$_o3"
        done
        printf '\n'

        # Two-ENI methodology NOTE — addresses T57 codex-review BLOCKER B2.
        # The linux_kernel arm drives a DIFFERENT physical NIC than the
        # dpdk_net / fstack arms (separate AWS ENA ENIs, same subnet, same
        # peer). Same peer, different wire. The NIC state snapshot at
        # `nic-state.txt` (captured during preflight) lets reviewers verify
        # what queue / coalescing / IRQ / offload differences exist between
        # the two ENIs at run time.
        printf '## Methodology — two-ENI comparison\n\n'
        printf '> **The three stacks do NOT all drive the same physical NIC.**'
        printf ' dpdk_net + fstack use the DPDK NIC at `%s` (vfio-pci,' "$DUT_PCI"
        printf ' polled at lcore `%s`). linux_kernel uses a separate kernel' "$DUT_LCORE"
        printf ' NIC `%s` (`%s`) bound to the in-tree `ena` driver, reached' \
            "$DUT_KERNEL_NIC" "$DUT_KERNEL_NIC_IP"
        printf ' via `sudo nsenter -t 1 -n` to escape the dev-host REDSOCKS'
        printf ' proxy. Both NICs are AWS ENA on the same subnet to the same'
        printf ' peer (`%s`), so the wire physics are identical (same' "$PEER_IP"
        printf ' NIC family, same kernel, same switch fabric), but RX/TX'
        printf ' queue counts, IRQ steering, interrupt coalescing, and ENA'
        printf ' offload defaults can differ across the two ENIs.\n'
        printf '>\n'
        printf '> **Run-time NIC state snapshot:** `nic-state.txt` (alongside'
        printf ' this SUMMARY) captures `ip -s link`, full `ethtool`'
        printf ' (`-c`/`-k`/`-l`/`-S`), `/proc/interrupts`, `tc qdisc`,'
        printf ' `iptables`, and `ip route` for BOTH DUT NICs AND the peer'
        printf ' kernel NIC — reviewers can diff across runs to confirm'
        printf ' queue / IRQ / coalescing parity (or call out where they'
        printf ' diverge). See T57 "Methodology — two-ENI comparison"'
        printf ' section for the full disclosure + future-work plan for an'
        printf ' absolute-numbers-grade same-physical-NIC comparison via'
        printf ' vfio/ena rebinding.\n\n'

        printf '## bench-rtt — RTT (ns), payload sweep\n\n'
        printf '> **Note — all three stacks tested at `--connections 1` only.**\n'
        printf '> fstack RTT arm currently lacks multi-conn support\n'
        printf '> (`tools/bench-rtt/src/main.rs` bails on `--connections > 1`;\n'
        printf '> per-conn `ff_socket` + `ff_poll` multiplexing inside a\n'
        printf '> request/response inner loop is tracked as a Phase 6+\n'
        printf '> follow-up — see T57 follow-up #6). dpdk_net and linux_kernel\n'
        printf '> arms do support `--connections > 1`, but this suite invocation\n'
        printf '> (`run_bench_rtt` above) omits `--connections` so it defaults\n'
        printf '> to `1` across all three stacks — fair comparison within that\n'
        printf '> constraint, not a multi-conn comparison.\n\n'
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            # T58 follow-up: use raw-sample sidecar for p50/p99/p999/mean +
            # decile signature (exposes the fstack 128B/1024B bimodality
            # the aggregate-only summary hid). Falls back to the aggregate
            # CSV pivot if the raw sidecar is absent.
            summarize_rtt_with_raw \
                "$RESULTS_DIR/bench-rtt-$stack.csv" \
                "$RESULTS_DIR/bench-rtt-$stack-raw.csv"
            printf '\n'
        done

        printf '## bench-tx-burst — burst throughput (bps) + initiation (ns)\n\n'
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            summarize_one_csv "$RESULTS_DIR/bench-tx-burst-$stack.csv"
            printf '\n'
        done

        printf '## bench-tx-maxtp — sustained goodput (bps)\n\n'
        printf '> linux_kernel arm points at peer port %s (linux-tcp-sink); dpdk_net and fstack at %s (echo-server).\n\n' \
            "$PEER_SINK_PORT" "$PEER_ECHO_PORT"
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            summarize_one_csv "$RESULTS_DIR/bench-tx-maxtp-$stack.csv"
            printf '\n'
        done

        printf '## bench-rx-burst — per-segment RX latency (ns)\n\n'
        for stack in dpdk_net linux_kernel fstack; do
            printf '### %s\n\n' "$stack"
            summarize_one_csv "$RESULTS_DIR/bench-rx-burst-$stack.csv"
            printf '\n'
        done

        printf '## verify-rack-tlp — netem scenarios\n\n'
        if [ -n "${SKIP_VERIFY_RACK_TLP:-}" ]; then
            printf '_Skipped (SKIP_VERIFY_RACK_TLP=%s)_\n\n' "$SKIP_VERIFY_RACK_TLP"
        elif [ -f "$RESULTS_DIR/verify-rack-tlp/verify-rack-tlp.log" ]; then
            printf '```\n'
            # Pull the summary block from the verify-rack-tlp log.
            sed -n '/verify-rack-tlp summary/,/^======/p' "$RESULTS_DIR/verify-rack-tlp/verify-rack-tlp.log" | head -25
            printf '```\n\n'
        else
            printf '_(no verify-rack-tlp log found)_\n\n'
        fi

        if [ "$FAIL_COUNT" -gt 0 ]; then
            printf '## Failed runs (%d)\n\n' "$FAIL_COUNT"
            for f in "${FAILS[@]}"; do
                # `--` ends printf option parsing so the leading `-` in the
                # format isn't interpreted as a flag (`printf: - : invalid
                # option`).
                printf -- '- %s\n' "$f"
            done
            printf '\n'
        fi

        printf '## Artifacts\n\n'
        # Include nic-state.txt alongside CSVs + logs so reviewers can find
        # the two-ENI state snapshot referenced by the Methodology section.
        find "$RESULTS_DIR" -maxdepth 2 -type f \
            \( -name '*.csv' -o -name '*.log' -o -name 'nic-state.txt' \) | sort \
            | sed "s|^$RESULTS_DIR/|- |"
    } >"$summary"
    log "summary written: $summary"
}

# ---------------------------------------------------------------------------
# Top-level orchestration.
# ---------------------------------------------------------------------------

WALLCLOCK_START=$(ts_now)
WALLCLOCK_START_HUMAN="$(date -u -Iseconds)"

on_exit() {
    local rc=$?
    # Tear down local linux servers iff loopback mode is active.
    if [ "$LINUX_KERNEL_VIA_NSENTER" != "1" ]; then
        stop_local_linux_servers
    fi
    # Final DPDK reset: if the suite was killed mid-DPDK-arm (Ctrl-C,
    # outer timeout), leave the host in a clean state for the next run.
    # No-op when the suite exited cleanly (idempotent + bounded by sleep 2).
    reset_dpdk_state 2>/dev/null || true
    WALLCLOCK_END=$(ts_now)
    WALLCLOCK_END_HUMAN="$(date -u -Iseconds)"
    local elapsed=$((WALLCLOCK_END - WALLCLOCK_START))
    log "=== suite done — elapsed ${elapsed}s, $OK_COUNT OK, $FAIL_COUNT FAIL (rc=$rc) ==="
    write_summary || log "summary generation failed"

    # Final compact stdout summary so the operator gets a single screen.
    echo
    echo "================================================================================"
    echo "fast-iter-suite summary  ($UTC_TS)"
    echo "================================================================================"
    echo "results: $RESULTS_DIR"
    echo "summary: $RESULTS_DIR/SUMMARY.md"
    echo "wallclock: ${elapsed}s"
    echo "outcome: $OK_COUNT OK, $FAIL_COUNT FAIL"
    if [ "$FAIL_COUNT" -gt 0 ]; then
        echo
        echo "failed runs:"
        for f in "${FAILS[@]}"; do echo "  - $f"; done
    fi
    echo "================================================================================"
}
# Compute per-tool stack orders + emit metadata.json (codex IMPORTANT I4,
# 2026-05-13). Must happen BEFORE preflight so --dry-run can short-circuit
# without touching the peer / NIC.
compute_stack_orders
write_metadata_json
log "seed=$SEED dry_run=$DRY_RUN"
log "stack order — bench-rtt:       ${ORDER_BENCH_RTT[*]}"
log "stack order — bench-tx-burst:  ${ORDER_BENCH_TX_BURST[*]}"
log "stack order — bench-tx-maxtp:  ${ORDER_BENCH_TX_MAXTP[*]}"
log "stack order — bench-rx-burst:  ${ORDER_BENCH_RX_BURST[*]}"

if [ "$DRY_RUN" = "1" ]; then
    # --dry-run: planned order matrix only, no bench / preflight / peer side
    # effects. Skip the EXIT trap's reset_dpdk_state + write_summary too,
    # since RESULTS_DIR is intentionally empty (no CSVs to summarize).
    trap - EXIT
    log "--dry-run: stack order matrix emitted to $RESULTS_DIR/metadata.json (skipping preflight + bench)"
    cat "$RESULTS_DIR/metadata.json"
    exit 0
fi

trap on_exit EXIT

log "fast-iter-suite start — results=$RESULTS_DIR seed=$SEED"
preflight

# DPDK NIC exclusivity: must be strictly sequential across DPDK/fstack arms,
# but the helpers themselves already serialize correctly because each is a
# `run_one` invocation. The PER-TOOL order is randomized per the codex I4
# fix; see compute_stack_orders above.
run_bench_rtt
run_bench_tx_burst
run_bench_tx_maxtp
run_bench_rx_burst
run_verify_rack_tlp

exit 0
