#!/usr/bin/env python3
"""aggregate-fast-iter.py — ingest N fast-iter-suite runs and emit a
publication-grade Markdown report with bootstrap CIs + paired-difference
statistics (codex IMPORTANT I3, 2026-05-13).

This script consumes the output of `scripts/fast-iter-stats.sh N --seed S0`,
which produces a top-level rollup directory containing N per-run
sub-directories (each a vanilla fast-iter-suite output dir) plus a
`stats-metadata.json` describing the N seeds.

Per-cell computations:

  Pooled mean + 95% CI:
    Across N runs, take the per-run aggregate (the `mean` aggregation row
    from each run's tool CSV) for each (tool, stack, dim_tuple, metric)
    cell. Compute the mean and a percentile-based 95% bootstrap confidence
    interval using 1000 resamples (stdlib only — no scipy / numpy).

  Pooled tail percentiles (p50 / p99 / p999):
    For bench-rtt, the per-run `*-raw.csv` sidecar carries one row per
    iteration (bucket_id, iter_idx, rtt_ns). Pool ALL raw samples across
    all N runs and compute p50 / p99 / p999 on the pooled distribution.
    For tools without a raw sidecar (bench-tx-burst / bench-tx-maxtp /
    bench-rx-burst), fall back to averaging the per-run aggregate's p50 /
    p99 (the suite does not emit p999 in the aggregate CSV for those
    tools).

  Per-cell CV across runs:
    100 * stdev(per_run_means) / mean(per_run_means). Computed from the
    `mean` aggregation row across runs. Reflects run-to-run noise, not
    within-run jitter.

  Paired-difference (stack A vs stack B, per dim_tuple):
    For each (tool, dim_tuple, metric), pair the per-run means of stack A
    against stack B (matched by run index — paired across the same
    wallclock window). Compute mean_diff = mean(A_i - B_i) and a 95%
    paired-bootstrap CI of the mean difference using 1000 resamples of the
    paired diff vector. Report as significant when 0 is outside the CI
    (alpha = 0.05, two-sided).

  Effect size:
    Cohen's d on the paired diffs: mean_diff / stdev(diffs). Standardized
    to make cross-payload comparisons readable.

Output layout — one Markdown table per (tool, metric) tuple:

  | dims... | stack | mean | 95% CI | CV% | p50 | p99 | p999 | n_runs |

Followed by the per-pair paired-difference table:

  | dims... | A | B | mean_diff | 95% CI | Cohen's d | sig? |

CLI:

  python3 scripts/aggregate-fast-iter.py STATS_DIR
      [--out-md PATH]      # default: STATS_DIR/AGGREGATE.md
      [--bootstrap N]      # default: 1000
      [--alpha A]          # default: 0.05 (two-sided)
      [--seed S]           # default: 12345 (bootstrap RNG seed)

Exits 0 if at least one (tool, stack, metric) cell was successfully
aggregated. Non-zero only on catastrophic failure (missing STATS_DIR,
unparseable metadata).
"""

# pylint: disable=invalid-name
from __future__ import annotations

import argparse
import csv
import json
import math
import os
import random
import statistics
import sys
from collections import OrderedDict, defaultdict
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple

# ---------------------------------------------------------------------------
# Constants — schema names + the four bench tools the suite emits.
# ---------------------------------------------------------------------------

TOOLS = (
    "bench-rtt",
    "bench-tx-burst",
    "bench-tx-maxtp",
    "bench-rx-burst",
)
STACKS = ("dpdk_net", "linux_kernel", "fstack")

# Per-tool metric → unit + tail-percentile policy.
#
# `pooled_tail`: if True we recompute p50/p99/p999 from the pooled raw-samples
# sidecar (only bench-rtt's rtt_ns is wired through `--raw-samples-csv` today).
# Otherwise the aggregator falls back to averaging the per-run aggregate's
# `p50`/`p99` rows (no p999 available since the aggregate doesn't carry it
# for non-RTT tools).
#
# Note on cross-stack paired comparison:
#   - bench-tx-burst emits `pmd_handoff_rate_bps` (dpdk_net), legacy
#     `throughput_per_burst_bps` (pre-I2 dpdk_net), and
#     `write_acceptance_rate_bps` (linux_kernel + fstack). These three are
#     structurally distinct measurements (see codex IMPORTANT I2,
#     2026-05-13): pmd_handoff captures PMD-queue admission; write_acceptance
#     captures socket-buffer admission. They MUST NOT be compared as if they
#     were the same metric. The paired-comparison rows are emitted per
#     metric, so the cross-stack pairs simply won't materialize for these
#     stacks-vs-stacks combinations (the aggregator pairs by exact metric
#     name + dim tuple).
#   - `burst_initiation_ns` and `burst_steady_bps` ARE emitted by all 3
#     stacks for bench-tx-burst, so cross-stack paired comparisons on those
#     are meaningful and DO appear in the paired table.
TOOL_METRICS: "Dict[str, List[Tuple[str, str, bool]]]" = {
    "bench-rtt": [("rtt_ns", "ns", True)],
    "bench-tx-burst": [
        ("pmd_handoff_rate_bps", "bits_per_sec", False),
        ("throughput_per_burst_bps", "bits_per_sec", False),
        ("write_acceptance_rate_bps", "bits_per_sec", False),
        ("burst_initiation_ns", "ns", False),
        ("burst_steady_bps", "bits_per_sec", False),
    ],
    "bench-tx-maxtp": [
        ("sustained_goodput_bps", "bits_per_sec", False),
        ("tx_pps", "pps", False),
    ],
    "bench-rx-burst": [("latency_ns", "ns", False)],
}

# Per-tool dim-key ordering for the Markdown row prefix. The suite-level
# pivot uses `dim_keys_order` discovered on the fly; we pin a stable order
# here so the aggregate tables are reader-friendly (smallest payload first,
# K-then-G for burst, W-then-C for maxtp, etc.). Keys NOT in this list are
# appended after these in insertion order (defensive — schemas can grow).
TOOL_DIMS_ORDER: "Dict[str, List[str]]" = {
    "bench-rtt": ["connections", "payload_bytes"],
    "bench-tx-burst": ["K_bytes", "G_ms", "tx_ts_mode", "workload"],
    "bench-tx-maxtp": ["C", "W_bytes", "tx_ts_mode", "workload"],
    "bench-rx-burst": ["segment_size_bytes", "burst_count"],
}

# ---------------------------------------------------------------------------
# Pure stdlib stats helpers.
# ---------------------------------------------------------------------------


def nearest_rank_percentile(sorted_xs: Sequence[float], p: float) -> float:
    """Nearest-rank percentile, matching bench_common's emit_csv convention."""
    if not sorted_xs:
        return float("nan")
    n = len(sorted_xs)
    idx = max(0, min(n - 1, int(round(p * (n - 1)))))
    return sorted_xs[idx]


def bootstrap_mean_ci(
    xs: Sequence[float],
    *,
    n_resamples: int = 1000,
    alpha: float = 0.05,
    rng: random.Random,
) -> Tuple[float, float, float]:
    """Percentile-bootstrap 95% (or 1-alpha) CI of the mean.

    Returns (mean, ci_lower, ci_upper). Uses a percentile-based bootstrap
    with `n_resamples` resamples; for n=1 returns (xs[0], xs[0], xs[0]) — no
    CI is meaningful from a single observation.

    Pure stdlib — random.choices() does the resampling.
    """
    n = len(xs)
    if n == 0:
        return (float("nan"), float("nan"), float("nan"))
    xs_list = list(xs)
    m = statistics.fmean(xs_list)
    if n == 1:
        return (m, m, m)
    means: List[float] = [0.0] * n_resamples
    for i in range(n_resamples):
        sample = rng.choices(xs_list, k=n)
        means[i] = sum(sample) / n
    means.sort()
    lo_idx = max(0, int(math.floor((alpha / 2) * n_resamples)))
    hi_idx = min(n_resamples - 1, int(math.ceil((1 - alpha / 2) * n_resamples)) - 1)
    return (m, means[lo_idx], means[hi_idx])


def paired_bootstrap_diff_ci(
    a: Sequence[float],
    b: Sequence[float],
    *,
    n_resamples: int = 1000,
    alpha: float = 0.05,
    rng: random.Random,
) -> Optional[Tuple[float, float, float, float, bool]]:
    """Paired-bootstrap 95% CI of mean(a_i - b_i).

    a and b must be same-length sequences; each index i is a paired
    observation (typically: run_i of stack A vs run_i of stack B).

    Returns (mean_diff, ci_lower, ci_upper, cohens_d, significant?) or None
    if inputs are unusable.

    `significant?` is True when 0 is outside the [ci_lower, ci_upper]
    interval (equivalent to a two-sided percentile-bootstrap test at
    `alpha`).

    `cohens_d` is mean_diff / stdev(diffs), the standardized paired
    effect size. NaN when stdev(diffs) is 0 (e.g. all diffs identical).
    """
    if len(a) != len(b) or len(a) < 2:
        return None
    diffs = [ai - bi for ai, bi in zip(a, b)]
    n = len(diffs)
    m = statistics.fmean(diffs)
    s = statistics.stdev(diffs) if n >= 2 else 0.0
    d = m / s if s > 0 else float("nan")
    means: List[float] = [0.0] * n_resamples
    for i in range(n_resamples):
        sample = rng.choices(diffs, k=n)
        means[i] = sum(sample) / n
    means.sort()
    lo_idx = max(0, int(math.floor((alpha / 2) * n_resamples)))
    hi_idx = min(n_resamples - 1, int(math.ceil((1 - alpha / 2) * n_resamples)) - 1)
    lo = means[lo_idx]
    hi = means[hi_idx]
    significant = not (lo <= 0.0 <= hi)
    return (m, lo, hi, d, significant)


def cv_percent(xs: Sequence[float]) -> float:
    """100 * stdev / mean — undefined for mean=0 (returns NaN)."""
    n = len(xs)
    if n < 2:
        return float("nan")
    m = statistics.fmean(xs)
    if m == 0:
        return float("nan")
    s = statistics.stdev(xs)
    return 100.0 * s / m


# ---------------------------------------------------------------------------
# CSV ingestion.
# ---------------------------------------------------------------------------


def parse_dims(s: str) -> Dict[str, Any]:
    """Parse the dimensions_json column. Drops `stack` (carried separately)."""
    try:
        d = json.loads(s or "{}")
    except (ValueError, TypeError):
        return {}
    if isinstance(d, dict):
        d.pop("stack", None)
        return d
    return {}


def dim_tuple(
    dims: Dict[str, Any], ordered_keys: Sequence[str]
) -> Tuple[Tuple[str, str], ...]:
    """Stable (key, str(value)) tuple in `ordered_keys` order, then any
    surplus keys (sorted) appended for forward-compat.

    The string-cast is intentional — bench CSVs emit ints, floats, and
    strings for dims; tupling on str() avoids mixing types in dict keys.
    """
    used = []
    seen = set()
    for k in ordered_keys:
        if k in dims:
            used.append((k, str(dims[k])))
            seen.add(k)
    for k in sorted(dims):
        if k not in seen:
            used.append((k, str(dims[k])))
    return tuple(used)


def dims_label(dt: Tuple[Tuple[str, str], ...]) -> str:
    """Render a dim tuple for Markdown cell text — `k=v, k=v, ...`."""
    return ", ".join(f"{k}={v}" for k, v in dt)


def load_run_aggregate(
    run_dir: str, tool: str
) -> Dict[Tuple[str, Tuple[Tuple[str, str], ...], str], Dict[str, float]]:
    """Load one run's aggregate CSV for `tool`.

    Returns {(stack, dim_tuple, metric_name): {agg_name: value}}. agg_name
    is the `metric_aggregation` column (p50 / p99 / p999 / mean / stddev /
    ci95_lower / ci95_upper).

    Missing/empty CSV → empty dict.
    """
    csv_path = os.path.join(run_dir, f"{tool}.csv")
    # The suite emits per-stack files: `bench-rtt-dpdk_net.csv`, etc.
    # Discover them lazily; the merged-tool layout is one CSV per stack.
    out: Dict[Tuple[str, Tuple[Tuple[str, str], ...], str], Dict[str, float]] = {}
    for stack in STACKS:
        csv_path = os.path.join(run_dir, f"{tool}-{stack}.csv")
        if not os.path.exists(csv_path) or os.path.getsize(csv_path) == 0:
            continue
        ordered = TOOL_DIMS_ORDER.get(tool, [])
        try:
            with open(csv_path, encoding="utf-8") as f:
                rows = list(csv.DictReader(f))
        except (OSError, csv.Error):
            continue
        for r in rows:
            dims = parse_dims(r.get("dimensions_json", "{}"))
            # `stack` dim is constant within the per-stack CSV — confirm.
            metric = r.get("metric_name") or ""
            agg = r.get("metric_aggregation") or ""
            try:
                val = float(r.get("metric_value", "") or "nan")
            except ValueError:
                continue
            dt = dim_tuple(dims, ordered)
            key = (stack, dt, metric)
            out.setdefault(key, {})[agg] = val
    return out


def load_run_raw_rtt(run_dir: str) -> Dict[Tuple[str, str], List[float]]:
    """For bench-rtt: load the raw-samples sidecar per stack.

    Returns {(stack, bucket_id): [rtt_ns, rtt_ns, ...]}. Missing sidecar →
    empty dict; partial sidecar (one stack missing) → that stack just
    won't appear in the returned mapping.

    bucket_id is "payload_<bytes>" in the suite's emission.
    """
    out: Dict[Tuple[str, str], List[float]] = {}
    for stack in STACKS:
        raw_path = os.path.join(run_dir, f"bench-rtt-{stack}-raw.csv")
        if not os.path.exists(raw_path) or os.path.getsize(raw_path) == 0:
            continue
        try:
            with open(raw_path, encoding="utf-8") as f:
                reader = csv.DictReader(f)
                for r in reader:
                    bid = r.get("bucket_id", "") or ""
                    try:
                        v = float(r.get("rtt_ns", "") or "nan")
                    except ValueError:
                        continue
                    if not math.isfinite(v):
                        continue
                    out.setdefault((stack, bid), []).append(v)
        except (OSError, csv.Error):
            continue
    return out


# ---------------------------------------------------------------------------
# Aggregation top-level.
# ---------------------------------------------------------------------------


def discover_runs(stats_dir: str) -> Tuple[Dict[str, Any], List[str]]:
    """Read stats-metadata.json; return (metadata, [absolute_run_dir, ...])
    where the list contains only runs whose dirs actually exist on disk.
    """
    meta_path = os.path.join(stats_dir, "stats-metadata.json")
    if not os.path.exists(meta_path):
        # Backwards-compat: if no stats-metadata.json, treat each subdir
        # matching run-* as a run dir.
        runs = sorted(
            os.path.join(stats_dir, d)
            for d in os.listdir(stats_dir)
            if d.startswith("run-")
            and os.path.isdir(os.path.join(stats_dir, d))
        )
        return ({"n": len(runs), "master_seed": None, "fallback": True}, runs)
    with open(meta_path, encoding="utf-8") as f:
        meta = json.load(f)
    runs = []
    for r in meta.get("runs", []):
        d = r.get("dir")
        if d and os.path.isdir(d):
            runs.append(d)
    return (meta, runs)


def aggregate(
    stats_dir: str, *, n_resamples: int, alpha: float, rng_seed: int
) -> Dict[str, Any]:
    """Build the full aggregation structure.

    Schema:
      {
        "metadata": {...},                           # from stats-metadata.json
        "n_runs_present": int,
        "per_tool": {
          "bench-rtt": {
            "cells": {
              (stack, dt, metric): {
                "n_runs": int,
                "per_run_means": [...],
                "mean": float, "ci_lo": float, "ci_hi": float,
                "cv_pct": float,
                "p50": float, "p99": float, "p999": float,
                "p_source": "pooled_raw" | "per_run_p50_mean",
              }, ...
            },
            "paired": {
              (dt, metric, stack_a, stack_b): {
                "mean_diff": ..., "ci_lo": ..., "ci_hi": ...,
                "cohens_d": ..., "significant": bool,
              }, ...
            },
            "ordered_dts": [dt, ...],                # display order
            "metrics_order": [metric, ...],
          }, ...
        }
      }
    """
    meta, run_dirs = discover_runs(stats_dir)
    rng = random.Random(rng_seed)

    per_tool: Dict[str, Any] = {}

    for tool in TOOLS:
        # Pass 1 — load per-run aggregates + (for bench-rtt) raw samples.
        per_run_aggs: List[
            Dict[Tuple[str, Tuple[Tuple[str, str], ...], str], Dict[str, float]]
        ] = []
        per_run_raw_rtt: List[Dict[Tuple[str, str], List[float]]] = []
        for rd in run_dirs:
            per_run_aggs.append(load_run_aggregate(rd, tool))
            if tool == "bench-rtt":
                per_run_raw_rtt.append(load_run_raw_rtt(rd))

        # Discover the union of (stack, dt, metric) keys we saw in ANY run.
        all_keys: "OrderedDict[Tuple[str, Tuple[Tuple[str, str], ...], str], None]" = (
            OrderedDict()
        )
        for run_agg in per_run_aggs:
            for k in run_agg:
                all_keys.setdefault(k, None)

        cells: Dict[Tuple[Any, ...], Dict[str, Any]] = {}
        ordered_dts: "OrderedDict[Tuple[Tuple[str, str], ...], None]" = OrderedDict()
        metrics_order: "OrderedDict[str, None]" = OrderedDict()

        for key in all_keys:
            stack, dt, metric = key
            # Restrict to metrics we recognize for this tool. The suite
            # emits a few derived metrics we don't want in the rolled-up
            # tables (e.g. tx_bps_p99 alongside sustained_goodput_bps).
            allowed = {m for (m, _u, _t) in TOOL_METRICS.get(tool, [])}
            if metric not in allowed:
                continue
            ordered_dts.setdefault(dt, None)
            metrics_order.setdefault(metric, None)

            per_run_means: List[float] = []
            per_run_p50s: List[float] = []
            per_run_p99s: List[float] = []
            for run_agg in per_run_aggs:
                aggs = run_agg.get(key, {})
                m_val = aggs.get("mean")
                if m_val is not None and math.isfinite(m_val):
                    per_run_means.append(m_val)
                p50_val = aggs.get("p50")
                if p50_val is not None and math.isfinite(p50_val):
                    per_run_p50s.append(p50_val)
                p99_val = aggs.get("p99")
                if p99_val is not None and math.isfinite(p99_val):
                    per_run_p99s.append(p99_val)

            n_runs = len(per_run_means)

            # Mean + CI from per-run means.
            mean_v, ci_lo, ci_hi = bootstrap_mean_ci(
                per_run_means, n_resamples=n_resamples, alpha=alpha, rng=rng
            )
            cv = cv_percent(per_run_means)

            # Tail percentiles.
            p_source = "per_run_p50_mean"
            p50_v = (
                statistics.fmean(per_run_p50s) if per_run_p50s else float("nan")
            )
            p99_v = (
                statistics.fmean(per_run_p99s) if per_run_p99s else float("nan")
            )
            p999_v = float("nan")

            if tool == "bench-rtt":
                # Pool raw samples for the matching payload_bytes dim across
                # all runs that have the raw sidecar. bucket_id format is
                # "payload_<bytes>".
                pooled: List[float] = []
                payload_v = None
                for k, v in dt:
                    if k == "payload_bytes":
                        payload_v = v
                        break
                if payload_v is not None:
                    bid = f"payload_{payload_v}"
                    for raw in per_run_raw_rtt:
                        samples = raw.get((stack, bid))
                        if samples:
                            pooled.extend(samples)
                if pooled:
                    pooled.sort()
                    p50_v = nearest_rank_percentile(pooled, 0.50)
                    p99_v = nearest_rank_percentile(pooled, 0.99)
                    p999_v = nearest_rank_percentile(pooled, 0.999)
                    p_source = "pooled_raw"

            cells[key] = {
                "n_runs": n_runs,
                "per_run_means": per_run_means,
                "mean": mean_v,
                "ci_lo": ci_lo,
                "ci_hi": ci_hi,
                "cv_pct": cv,
                "p50": p50_v,
                "p99": p99_v,
                "p999": p999_v,
                "p_source": p_source,
            }

        # Paired comparisons — only meaningful when both stacks have the
        # SAME (dt, metric) across N_paired >= 2 runs. Pair by run index.
        paired: Dict[Tuple[Tuple[Tuple[str, str], ...], str, str, str], Dict[str, Any]] = {}
        for dt in ordered_dts:
            for metric in metrics_order:
                # For each stack-pair, build the paired per-run means.
                for i_a, a in enumerate(STACKS):
                    for b in STACKS[i_a + 1 :]:
                        a_vals: List[float] = []
                        b_vals: List[float] = []
                        for run_agg in per_run_aggs:
                            a_aggs = run_agg.get((a, dt, metric), {})
                            b_aggs = run_agg.get((b, dt, metric), {})
                            a_m = a_aggs.get("mean")
                            b_m = b_aggs.get("mean")
                            if (
                                a_m is not None
                                and math.isfinite(a_m)
                                and b_m is not None
                                and math.isfinite(b_m)
                            ):
                                a_vals.append(a_m)
                                b_vals.append(b_m)
                        res = paired_bootstrap_diff_ci(
                            a_vals,
                            b_vals,
                            n_resamples=n_resamples,
                            alpha=alpha,
                            rng=rng,
                        )
                        if res is None:
                            continue
                        m_d, lo_d, hi_d, d_d, sig_d = res
                        paired[(dt, metric, a, b)] = {
                            "mean_diff": m_d,
                            "ci_lo": lo_d,
                            "ci_hi": hi_d,
                            "cohens_d": d_d,
                            "significant": sig_d,
                            "n_paired": len(a_vals),
                        }

        per_tool[tool] = {
            "cells": cells,
            "paired": paired,
            "ordered_dts": list(ordered_dts.keys()),
            "metrics_order": list(metrics_order.keys()),
        }

    return {
        "metadata": meta,
        "n_runs_present": len(run_dirs),
        "per_tool": per_tool,
    }


# ---------------------------------------------------------------------------
# Markdown rendering.
# ---------------------------------------------------------------------------


def fmt_val(v: float, unit: str) -> str:
    """Format a value with unit-appropriate precision."""
    if not math.isfinite(v):
        return "—"
    if unit in ("bits_per_sec", "pps"):
        # Round to integer for readability.
        return f"{v:,.0f}"
    if unit == "ns":
        return f"{v:,.0f}"
    return f"{v:,.2f}"


def fmt_ci(lo: float, hi: float, unit: str) -> str:
    if not (math.isfinite(lo) and math.isfinite(hi)):
        return "—"
    return f"[{fmt_val(lo, unit)}, {fmt_val(hi, unit)}]"


def fmt_cv(cv: float) -> str:
    if not math.isfinite(cv):
        return "—"
    return f"{cv:.2f}%"


def render_markdown(agg: Dict[str, Any]) -> str:
    """Render the aggregation as a long Markdown report.

    Layout:
      # title + run metadata header
      ## tool sections, each with:
        ### per-metric cell tables (one table per (tool, metric))
        ### paired-comparison tables (one per metric, all stack-pairs together)
    """
    out: List[str] = []
    meta = agg.get("metadata", {})
    n_present = agg.get("n_runs_present", 0)

    out.append("# fast-iter-suite aggregated statistics (N-run rollup)\n")
    out.append(
        "Generated by `scripts/aggregate-fast-iter.py` (codex IMPORTANT I3, "
        "publication-grade rigor pass).\n"
    )
    out.append("")
    out.append("## Run metadata\n")
    if meta.get("master_seed") is not None:
        out.append(f"- **N target:** {meta.get('n')}")
        out.append(f"- **N present on disk:** {n_present}")
        out.append(f"- **Master seed:** `{meta.get('master_seed')}`")
        out.append(f"- **Output dir:** `{meta.get('out_dir', '')}`")
        out.append(f"- **UTC ts:** `{meta.get('utc_ts', '')}`")
        out.append(f"- **skip_verify:** `{meta.get('skip_verify', False)}`")
    else:
        out.append(f"- **N present on disk:** {n_present}")
        out.append(
            "- _(no stats-metadata.json; discovered runs from `run-*` subdirs)_"
        )
    out.append("")
    out.append("**Method:**")
    out.append(
        "- Per-cell mean + 95% CI: percentile bootstrap (1000 resamples) over "
        "per-run means."
    )
    out.append(
        "- Per-cell p50 / p99 / p999 (bench-rtt only): pooled across all "
        "raw-sample sidecars, nearest-rank percentile."
    )
    out.append(
        "- Per-cell p50 / p99 (other tools): mean of per-run aggregate "
        "p50 / p99 rows (the suite does not emit p999 in the aggregate "
        "CSV for non-RTT tools)."
    )
    out.append(
        "- Per-cell CV: 100 × stdev(per-run means) / mean(per-run means)."
    )
    out.append(
        "- Paired-difference: paired bootstrap of mean(A_i - B_i) over "
        "matched run indices; 0 outside the 95% CI ⇒ significant at "
        "α = 0.05 two-sided."
    )
    out.append(
        "- Effect size: Cohen's d = mean_diff / stdev(diffs) on the paired "
        "diffs."
    )
    out.append(
        "- Stack-order randomization (codex I4) handled at the suite level — "
        "each run's seed = master_seed + run_idx."
    )
    out.append("")

    per_tool = agg.get("per_tool", {})
    if not per_tool:
        out.append("_(no tool data parsed — aggregation produced 0 cells)_\n")
        return "\n".join(out) + "\n"

    for tool in TOOLS:
        td = per_tool.get(tool)
        if not td or not td.get("cells"):
            continue
        out.append(f"## {tool}\n")
        metrics = td["metrics_order"]
        ordered_dts = td["ordered_dts"]
        for metric in metrics:
            # Pick unit from TOOL_METRICS so the table header reflects it.
            unit = next(
                (u for (m, u, _t) in TOOL_METRICS.get(tool, []) if m == metric),
                "",
            )
            out.append(f"### metric: `{metric}` ({unit})\n")
            # Header — dim cols, stack, mean ± CI, CV%, p50, p99, p999, n_runs.
            dim_keys = TOOL_DIMS_ORDER.get(tool, [])
            # But the actual dt may include extra keys; pull them from the
            # first dt observed.
            all_dim_keys: "OrderedDict[str, None]" = OrderedDict()
            for dk in dim_keys:
                all_dim_keys[dk] = None
            for dt in ordered_dts:
                for k, _v in dt:
                    all_dim_keys[k] = None
            dim_keys = list(all_dim_keys.keys())
            hdr = dim_keys + [
                "stack",
                "mean",
                "95% CI",
                "CV%",
                "p50",
                "p99",
                "p999",
                "n_runs",
            ]
            out.append("| " + " | ".join(hdr) + " |")
            out.append("|" + "|".join("---" for _ in hdr) + "|")
            for dt in ordered_dts:
                dt_d = dict(dt)
                for stack in STACKS:
                    cell = td["cells"].get((stack, dt, metric))
                    if cell is None:
                        continue
                    row: List[str] = []
                    for k in dim_keys:
                        row.append(dt_d.get(k, ""))
                    row.append(stack)
                    row.append(fmt_val(cell["mean"], unit))
                    row.append(fmt_ci(cell["ci_lo"], cell["ci_hi"], unit))
                    row.append(fmt_cv(cell["cv_pct"]))
                    row.append(fmt_val(cell["p50"], unit))
                    row.append(fmt_val(cell["p99"], unit))
                    row.append(fmt_val(cell["p999"], unit))
                    row.append(str(cell["n_runs"]))
                    out.append("| " + " | ".join(row) + " |")
            out.append("")

            # Paired-comparison table (per metric, all stack-pairs).
            paired = td.get("paired", {})
            paired_rows: List[List[str]] = []
            for dt in ordered_dts:
                dt_d = dict(dt)
                for i_a, a in enumerate(STACKS):
                    for b in STACKS[i_a + 1 :]:
                        p = paired.get((dt, metric, a, b))
                        if p is None:
                            continue
                        row = []
                        for k in dim_keys:
                            row.append(dt_d.get(k, ""))
                        row.extend(
                            [
                                a,
                                b,
                                fmt_val(p["mean_diff"], unit),
                                fmt_ci(p["ci_lo"], p["ci_hi"], unit),
                                (
                                    f"{p['cohens_d']:.2f}"
                                    if math.isfinite(p["cohens_d"])
                                    else "—"
                                ),
                                "YES" if p["significant"] else "no",
                                str(p["n_paired"]),
                            ]
                        )
                        paired_rows.append(row)
            if paired_rows:
                out.append(
                    f"**Paired comparison (A − B), metric `{metric}`** — "
                    "significant when 0 is outside the 95% CI.\n"
                )
                p_hdr = dim_keys + [
                    "A",
                    "B",
                    "mean_diff",
                    "95% CI",
                    "Cohen's d",
                    "sig?",
                    "n_paired",
                ]
                out.append("| " + " | ".join(p_hdr) + " |")
                out.append("|" + "|".join("---" for _ in p_hdr) + "|")
                for row in paired_rows:
                    out.append("| " + " | ".join(row) + " |")
                out.append("")

    return "\n".join(out) + "\n"


# ---------------------------------------------------------------------------
# CLI entry point.
# ---------------------------------------------------------------------------


def main(argv: Optional[Iterable[str]] = None) -> int:
    """argparse + dispatch. Returns process exit code."""
    parser = argparse.ArgumentParser(
        prog="aggregate-fast-iter",
        description=(
            "Aggregate N fast-iter-suite runs into a publication-grade "
            "Markdown report with bootstrap CIs + paired-difference stats."
        ),
    )
    parser.add_argument(
        "stats_dir",
        help=(
            "Top-level rollup dir from scripts/fast-iter-stats.sh (contains "
            "stats-metadata.json + run-*/ subdirs)."
        ),
    )
    parser.add_argument(
        "--out-md",
        default=None,
        help="Output Markdown path (default: STATS_DIR/AGGREGATE.md)",
    )
    parser.add_argument(
        "--bootstrap",
        type=int,
        default=1000,
        help="Number of bootstrap resamples per CI (default: 1000)",
    )
    parser.add_argument(
        "--alpha",
        type=float,
        default=0.05,
        help="Significance level (two-sided), default 0.05",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=12345,
        help="RNG seed for bootstrap (default: 12345; pin for reproducibility)",
    )
    args = parser.parse_args(list(argv) if argv is not None else None)

    stats_dir = os.path.abspath(args.stats_dir)
    if not os.path.isdir(stats_dir):
        print(
            f"aggregate-fast-iter: not a directory: {stats_dir}", file=sys.stderr
        )
        return 2

    agg = aggregate(
        stats_dir,
        n_resamples=args.bootstrap,
        alpha=args.alpha,
        rng_seed=args.seed,
    )

    md = render_markdown(agg)

    out_path = args.out_md or os.path.join(stats_dir, "AGGREGATE.md")
    out_path = os.path.abspath(out_path)
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(md)
    print(f"wrote {out_path}")

    # Count non-empty cells to decide exit code.
    total_cells = sum(
        len(td.get("cells", {})) for td in agg.get("per_tool", {}).values()
    )
    if total_cells == 0:
        print("aggregate-fast-iter: 0 cells aggregated", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
