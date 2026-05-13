#!/usr/bin/env bash
# fast-iter-stats.sh — run scripts/fast-iter-suite.sh N times back-to-back
# under a single rolled-up output directory, with N seeds derived from a master
# seed. Used to drive the N-run statistical-rigor pass (codex IMPORTANT I3,
# 2026-05-13): T58 reported 3 runs as preliminary variance; this harness
# orchestrates the larger N + per-run isolation so scripts/aggregate-fast-iter.py
# can pool the raw-samples sidecars and compute bootstrap CIs +
# paired-difference statistics across stack-pairs.
#
# Per-run isolation: every iteration writes into
# $OUT_DIR/run-<NNN>-seed-<S>/, which is a vanilla fast-iter-suite output dir
# (same CSVs + raw sidecars + SUMMARY.md + metadata.json shape). The pooled
# directory layout looks like:
#
#   $OUT_DIR/
#     stats-metadata.json     # N, master-seed, per-run seeds, wallclock
#     runs.txt                # per-run status line (newline-delimited)
#     run-001-seed-42/
#       bench-rtt-*.csv
#       bench-rtt-*-raw.csv
#       ...
#       SUMMARY.md
#       metadata.json
#     run-002-seed-43/
#       ...
#     ...
#
# This script does NOT re-implement the bench logic — it shells out to
# scripts/fast-iter-suite.sh with RESULTS_DIR_OVERRIDE so the per-run dir is
# pinned. Each run gets a distinct $SEED = $MASTER_SEED + run_idx so the
# per-tool stack-order matrix differs run-to-run (codex IMPORTANT I4
# guarantees per-tool randomization; codex IMPORTANT I3 layers the N-run
# repetition on top so the aggregator can compute CIs that survive a single
# bad run).
#
# Usage:
#   ./scripts/fast-iter-stats.sh N [--seed S0] [--out-dir DIR] [--dry-run]
#
# Args:
#   N             positional, number of runs (>= 1; recommended 5-10)
#
# Flags:
#   --seed S0     Master seed for run-level reproducibility. Per-run seed
#                 derived as S0 + run_idx (run_idx starts at 0). Default: epoch.
#   --out-dir D   Top-level rollup dir (absolute or relative to $WORKDIR).
#                 Default: target/bench-results/stats-<UTC>/
#   --dry-run     Print the planned run matrix + per-run seeds and exit
#                 without invoking fast-iter-suite.sh. Useful for verifying
#                 the seed sequence + paths.
#   --skip-verify Pass SKIP_VERIFY_RACK_TLP=1 through to fast-iter-suite.sh
#                 (drops ~15min off each run; the netem matrix is a pure
#                 correctness/regression gate, not part of the I3 cross-stack
#                 comparison statistics).
#
# Pre-conditions:
#   - $WORKDIR/.fast-iter.env present (see scripts/fast-iter-setup.sh)
#   - Bench binaries at $WORKDIR/target/release/{bench-rtt,bench-tx-burst,
#     bench-tx-maxtp,bench-rx-burst} (rebuild with `cargo build --release
#     --features fstack` if missing)
#   - Peer servers up at $PEER_IP (echo / linux-tcp-sink / burst-echo)
#
# Pre-flight conflict check: aborts if any bench-* or fast-iter process is
# already running (DPDK NIC exclusivity). Override with SKIP_PREFLIGHT=1 only
# in emergencies.
#
# Exit code: 0 if all N runs returned non-zero exit code 0 from the inner
# fast-iter-suite.sh (i.e. the suite itself ran to completion; per-arm FAILs
# are still tallied in each run's SUMMARY.md). Non-zero only on catastrophic
# orchestration failure (missing binaries, can't write $OUT_DIR, conflict).

set -euo pipefail

# ---------------------------------------------------------------------------
# Paths.
# ---------------------------------------------------------------------------
WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$WORKDIR"

FAST_ITER_SUITE="$WORKDIR/scripts/fast-iter-suite.sh"
if [ ! -x "$FAST_ITER_SUITE" ]; then
    printf 'fast-iter-stats: missing %s\n' "$FAST_ITER_SUITE" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# CLI parsing.
# ---------------------------------------------------------------------------
N=""
MASTER_SEED=""
OUT_DIR=""
DRY_RUN=0
SKIP_VERIFY=0

usage() {
    sed -n '2,55p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --seed)
            if [ $# -lt 2 ]; then
                printf 'fast-iter-stats: --seed requires a value\n' >&2
                exit 2
            fi
            MASTER_SEED="$2"
            shift 2
            ;;
        --seed=*)
            MASTER_SEED="${1#--seed=}"
            shift
            ;;
        --out-dir)
            if [ $# -lt 2 ]; then
                printf 'fast-iter-stats: --out-dir requires a value\n' >&2
                exit 2
            fi
            OUT_DIR="$2"
            shift 2
            ;;
        --out-dir=*)
            OUT_DIR="${1#--out-dir=}"
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --skip-verify)
            SKIP_VERIFY=1
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        --*)
            printf 'fast-iter-stats: unknown flag %s (try --help)\n' "$1" >&2
            exit 2
            ;;
        *)
            if [ -z "$N" ]; then
                N="$1"
                shift
            else
                printf 'fast-iter-stats: unexpected positional arg %s (N already set to %s)\n' "$1" "$N" >&2
                exit 2
            fi
            ;;
    esac
done

if [ -z "$N" ]; then
    printf 'fast-iter-stats: N is required (try --help)\n' >&2
    exit 2
fi
case "$N" in
    ''|*[!0-9]*)
        printf 'fast-iter-stats: N must be a positive integer, got %q\n' "$N" >&2
        exit 2
        ;;
esac
if [ "$N" -lt 1 ]; then
    printf 'fast-iter-stats: N must be >= 1, got %s\n' "$N" >&2
    exit 2
fi

if [ -z "$MASTER_SEED" ]; then
    MASTER_SEED=$(date -u +%s)
fi
case "$MASTER_SEED" in
    ''|*[!0-9]*)
        printf 'fast-iter-stats: --seed must be a non-negative integer, got %q\n' "$MASTER_SEED" >&2
        exit 2
        ;;
esac

UTC_TS="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$WORKDIR/target/bench-results/stats-$UTC_TS"
fi
# Make absolute. If user passed a relative path, anchor to $WORKDIR (NOT cwd,
# which is unstable in the agent harness).
case "$OUT_DIR" in
    /*) ;;
    *) OUT_DIR="$WORKDIR/$OUT_DIR" ;;
esac

# ---------------------------------------------------------------------------
# Pre-flight: conflicting bench detection (codex IMPORTANT I3 spec —
# "Don't run during business hours conflicts. Check
# `ps aux | grep -i 'bench-\|fast-iter' | grep -v grep`").
# ---------------------------------------------------------------------------
if [ "${SKIP_PREFLIGHT:-0}" != "1" ] && [ "$DRY_RUN" != "1" ]; then
    if pgrep -af 'bench-(rtt|tx-burst|tx-maxtp|rx-burst)|fast-iter-suite' \
            | grep -v 'fast-iter-stats' \
            | grep -v 'pgrep' \
            >/dev/null; then
        printf 'fast-iter-stats: ABORT — another bench process is running:\n' >&2
        pgrep -af 'bench-(rtt|tx-burst|tx-maxtp|rx-burst)|fast-iter-suite' \
            | grep -v 'fast-iter-stats' | grep -v 'pgrep' >&2
        exit 2
    fi
fi

mkdir -p "$OUT_DIR"

# ---------------------------------------------------------------------------
# Pre-compute the per-run seed list and emit stats-metadata.json up front so
# a killed orchestrator still leaves enough metadata for the aggregator to
# pool whatever runs completed.
# ---------------------------------------------------------------------------
declare -a RUN_SEEDS=()
declare -a RUN_DIRS=()
for ((i = 0; i < N; i++)); do
    rs=$(( MASTER_SEED + i ))
    rd_name="$(printf 'run-%03d-seed-%s' "$((i + 1))" "$rs")"
    RUN_SEEDS+=("$rs")
    RUN_DIRS+=("$OUT_DIR/$rd_name")
done

write_stats_metadata() {
    local out="$OUT_DIR/stats-metadata.json"
    {
        printf '{\n'
        printf '  "n": %s,\n' "$N"
        printf '  "master_seed": %s,\n' "$MASTER_SEED"
        printf '  "out_dir": "%s",\n' "$OUT_DIR"
        printf '  "utc_ts": "%s",\n' "$UTC_TS"
        printf '  "skip_verify": %s,\n' "$([ "$SKIP_VERIFY" = 1 ] && printf 'true' || printf 'false')"
        printf '  "dry_run": %s,\n' "$([ "$DRY_RUN" = 1 ] && printf 'true' || printf 'false')"
        printf '  "runs": [\n'
        local i
        for ((i = 0; i < N; i++)); do
            local sep=','
            if [ "$i" -eq "$((N - 1))" ]; then sep=''; fi
            printf '    { "idx": %d, "seed": %s, "dir": "%s" }%s\n' \
                "$((i + 1))" "${RUN_SEEDS[$i]}" "${RUN_DIRS[$i]}" "$sep"
        done
        printf '  ]\n'
        printf '}\n'
    } >"$out"
}
write_stats_metadata

# ---------------------------------------------------------------------------
# Driver loop.
# ---------------------------------------------------------------------------
RUNS_LOG="$OUT_DIR/runs.txt"
: >"$RUNS_LOG"
WALLCLOCK_START=$(date -u +%s)

stats_log() {
    printf '[stats %s] %s\n' "$(date -u +%H:%M:%S)" "$*" >&2
}

stats_log "fast-iter-stats start"
stats_log "  N=$N"
stats_log "  master_seed=$MASTER_SEED"
stats_log "  out_dir=$OUT_DIR"
stats_log "  skip_verify=$SKIP_VERIFY"
stats_log "  dry_run=$DRY_RUN"

if [ "$DRY_RUN" = "1" ]; then
    stats_log "--dry-run — planned matrix:"
    for ((i = 0; i < N; i++)); do
        printf '  run %3d  seed=%-12s  dir=%s\n' \
            "$((i + 1))" "${RUN_SEEDS[$i]}" "${RUN_DIRS[$i]}" >&2
    done
    printf '%s\n' "$OUT_DIR/stats-metadata.json"
    exit 0
fi

# Stagger between runs to let kernel buffers / TCP TIME_WAITs / hugepage maps
# fully release. The fast-iter-suite's reset_dpdk_state() already covers the
# DPDK side; a short sleep here just gives the kernel / peer extra slack.
INTER_RUN_SLEEP_SECS="${INTER_RUN_SLEEP_SECS:-30}"

declare -i OK_RUNS=0
declare -i FAIL_RUNS=0
declare -a FAIL_DETAILS=()

for ((i = 0; i < N; i++)); do
    run_idx=$((i + 1))
    seed="${RUN_SEEDS[$i]}"
    rd="${RUN_DIRS[$i]}"

    stats_log "================================================================================"
    stats_log "run $run_idx / $N  (seed=$seed)"
    stats_log "  dir: $rd"
    stats_log "================================================================================"

    mkdir -p "$rd"

    run_start=$(date -u +%s)

    declare -a suite_env=(
        env
        "RESULTS_DIR_OVERRIDE=$rd"
    )
    if [ "$SKIP_VERIFY" = "1" ]; then
        suite_env+=("SKIP_VERIFY_RACK_TLP=1")
    fi

    if "${suite_env[@]}" "$FAST_ITER_SUITE" --seed "$seed" >"$rd/stats-driver.log" 2>&1; then
        rc=0
    else
        rc=$?
    fi

    run_end=$(date -u +%s)
    elapsed=$((run_end - run_start))

    if [ "$rc" -eq 0 ]; then
        OK_RUNS=$((OK_RUNS + 1))
        printf 'OK   run=%03d seed=%s elapsed=%ds dir=%s\n' \
            "$run_idx" "$seed" "$elapsed" "$rd" >>"$RUNS_LOG"
        stats_log "  run $run_idx OK ($elapsed s)"
    else
        FAIL_RUNS=$((FAIL_RUNS + 1))
        FAIL_DETAILS+=("run $run_idx seed=$seed rc=$rc dir=$rd")
        printf 'FAIL run=%03d seed=%s elapsed=%ds rc=%d dir=%s\n' \
            "$run_idx" "$seed" "$elapsed" "$rc" "$rd" >>"$RUNS_LOG"
        stats_log "  run $run_idx FAIL rc=$rc ($elapsed s) — see $rd/stats-driver.log"
    fi

    # Sleep between runs unless this was the last one.
    if [ "$run_idx" -lt "$N" ] && [ "$INTER_RUN_SLEEP_SECS" -gt 0 ]; then
        stats_log "  sleep ${INTER_RUN_SLEEP_SECS}s between runs (NIC / peer settle)"
        sleep "$INTER_RUN_SLEEP_SECS"
    fi
done

WALLCLOCK_END=$(date -u +%s)
TOTAL_ELAPSED=$((WALLCLOCK_END - WALLCLOCK_START))

stats_log "================================================================================"
stats_log "fast-iter-stats done"
stats_log "  total wallclock: ${TOTAL_ELAPSED}s ($((TOTAL_ELAPSED / 60))m)"
stats_log "  ok=$OK_RUNS  fail=$FAIL_RUNS"
stats_log "  out: $OUT_DIR"
stats_log "  next: python3 scripts/aggregate-fast-iter.py $OUT_DIR"
stats_log "================================================================================"

# Append final tallies to runs.txt for downstream consumers.
{
    printf '\n--- summary ---\n'
    printf 'total_runs: %d\n' "$N"
    printf 'ok_runs: %d\n' "$OK_RUNS"
    printf 'fail_runs: %d\n' "$FAIL_RUNS"
    printf 'wallclock_seconds: %d\n' "$TOTAL_ELAPSED"
    if [ "$FAIL_RUNS" -gt 0 ]; then
        printf 'failed_runs:\n'
        for f in "${FAIL_DETAILS[@]}"; do
            printf -- '- %s\n' "$f"
        done
    fi
} >>"$RUNS_LOG"

printf '%s\n' "$OUT_DIR"
exit 0
