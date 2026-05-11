#!/usr/bin/env python3
"""T54 fast-iter result analyzer — dpdk_net vs fstack vs linux comparison.
   Key change vs T53: fstack_maxtp now reports wire rate via echo counting (commit 0a523cb).
"""
import csv, json, sys
from collections import defaultdict

T54 = "/home/ubuntu/resd.dpdk_tcp-a10-perf/target/bench-results/t54"

def load_csv(path, stack_filter=None):
    rows = []
    try:
        with open(path) as f:
            for row in csv.DictReader(f):
                dims = json.loads(row["dimensions_json"])
                if stack_filter and dims.get("stack") != stack_filter:
                    continue
                rows.append((dims, row["metric_name"], row["metric_aggregation"],
                              float(row["metric_value"]), row.get("bucket_invalid","").strip()))
    except FileNotFoundError:
        print(f"  [missing: {path}]")
    return rows

def pivot(rows, metric, agg, key_fn):
    d = {}
    for dims, mn, ma, mv, inv in rows:
        if mn == metric and ma == agg:
            d[key_fn(dims)] = (mv, inv)
    return d

K_LABELS = {"65536":"64K", "262144":"256K", "1048576":"1M",
            "4194304":"4M", "16777216":"16M"}
G_VALS = [0, 1, 10, 100]
K_VALS = [65536, 262144, 1048576, 4194304, 16777216]

def fmt_gbps(entry):
    if entry is None: return "  —  "
    v, inv = entry
    if inv: return " FAIL"
    return f"{v/1e9:6.3f}"

def fmt_us(entry):
    if entry is None: return "  —  "
    v, inv = entry
    if inv: return " FAIL"
    return f"{v/1e3:6.1f}"

def burst_key(dims):
    return (int(dims.get("K_bytes",0)), int(float(dims.get("G_ms",0))))

# --------------------------------------------------------------------------
print("=" * 70)
print("BURST — throughput_per_burst_bps (Gbps, mean)")
print("  dpdk_net: wire rate (ACKed bytes / time)")
print("  fstack:   buffer-fill rate (NOT wire — values >2.5G are artifacts)")
print("=" * 70)

dpdk_b = load_csv(f"{T54}/fast-burst-dpdk.csv", "dpdk_net")
fstk_b = load_csv(f"{T54}/fast-burst-fstack.csv", "fstack")

dpdk_tp  = pivot(dpdk_b, "throughput_per_burst_bps", "mean", burst_key)
fstk_tp  = pivot(fstk_b, "throughput_per_burst_bps", "mean", burst_key)
dpdk_ini = pivot(dpdk_b, "burst_initiation_ns", "p50", burst_key)
fstk_ini = pivot(fstk_b, "burst_initiation_ns", "p50", burst_key)

for label, tbl in [("dpdk_net", dpdk_tp), ("fstack [buf-fill]", fstk_tp)]:
    print(f"\n{label} throughput_per_burst_bps (Gbps):")
    print(f"{'K':>9}  {'G=0ms':>7} {'G=1ms':>7} {'G=10ms':>7} {'G=100ms':>8}")
    for k in K_VALS:
        row = [fmt_gbps(tbl.get((k,g))) for g in G_VALS]
        print(f"{K_LABELS.get(str(k),str(k)):>9}  {row[0]:>7} {row[1]:>7} {row[2]:>7} {row[3]:>8}")

print(f"\nburst_initiation_ns p50 (µs):")
print(f"{'K':>9}  {'stack':>18}  {'G=0ms':>7} {'G=1ms':>7} {'G=10ms':>7} {'G=100ms':>8}")
for k in K_VALS:
    for label, tbl in [("dpdk_net", dpdk_ini), ("fstack", fstk_ini)]:
        row = [fmt_us(tbl.get((k,g))) for g in G_VALS]
        print(f"{K_LABELS.get(str(k),str(k)):>9}  {label:>18}  {row[0]:>7} {row[1]:>7} {row[2]:>7} {row[3]:>8}")

# --------------------------------------------------------------------------
print("\n" + "=" * 70)
print("MAXTP — sustained_goodput_bps (Gbps, mean)")
print("  dpdk_net: ACKed bytes / window (wire rate)")
print("  linux:    same (wire rate — drain fix applied in T52)")
print("  fstack:   echo bytes / window (wire rate — T54 first with echo counting)")
print("=" * 70)

dpdk_m = load_csv(f"{T54}/fast-maxtp-dpdk-linux.csv", "dpdk_net")
lnx_m  = load_csv(f"{T54}/fast-maxtp-dpdk-linux.csv", "linux")
fstk_m = load_csv(f"{T54}/fast-maxtp-fstack.csv", "fstack")

def maxtp_key(dims):
    return (int(dims.get("W_bytes",0)), int(dims.get("C",0)))

dpdk_gp = pivot(dpdk_m, "sustained_goodput_bps", "mean", maxtp_key)
lnx_gp  = pivot(lnx_m,  "sustained_goodput_bps", "mean", maxtp_key)
fstk_gp = pivot(fstk_m, "sustained_goodput_bps", "mean", maxtp_key)

W_VALS = [64, 256, 1024, 4096, 16384, 65536, 262144]
C_VALS = [1, 4, 16, 64]

for stack_label, tbl in [("dpdk_net", dpdk_gp), ("linux", lnx_gp), ("fstack [wire]", fstk_gp)]:
    print(f"\n{stack_label} sustained_goodput_bps (Gbps):")
    print(f"{'W':>9}  {'C=1':>7} {'C=4':>7} {'C=16':>7} {'C=64':>7}")
    for w in W_VALS:
        row = [fmt_gbps(tbl.get((w,c))) for c in C_VALS]
        print(f"{w:>9}B {row[0]:>7} {row[1]:>7} {row[2]:>7} {row[3]:>7}")

# --------------------------------------------------------------------------
print("\n" + "=" * 70)
print("PERFORMANCE GAP SUMMARY")
print("=" * 70)

def peak(tbl):
    vals = [v for (v, inv) in tbl.values() if not inv]
    return max(vals, default=0)

dpdk_pk = peak(dpdk_gp)
lnx_pk  = peak(lnx_gp)
fstk_pk = peak(fstk_gp)

print(f"\nMaxtp peak goodput:")
print(f"  dpdk_net: {dpdk_pk/1e9:.3f} Gbps  (wire rate)")
print(f"  linux:    {lnx_pk/1e9:.3f} Gbps  (wire rate, echo-limited)")
print(f"  fstack:   {fstk_pk/1e9:.3f} Gbps  (wire rate via echo counting — T54)")
if dpdk_pk > 0 and lnx_pk > 0:
    print(f"  dpdk_net / linux ratio: {dpdk_pk/lnx_pk:.2f}x")
if dpdk_pk > 0 and fstk_pk > 0:
    ratio = dpdk_pk / fstk_pk
    if ratio >= 1.0:
        print(f"  dpdk_net / fstack ratio: {ratio:.2f}x  (dpdk_net advantage)")
    else:
        print(f"  fstack / dpdk_net ratio: {1/ratio:.2f}x  (fstack advantage)")

print(f"\nBurst initiation p50 at K=64KiB, G=1ms (stable comparator):")
dpdk_i = dpdk_ini.get((65536,1))
fstk_i = fstk_ini.get((65536,1))
if dpdk_i and not dpdk_i[1]:
    print(f"  dpdk_net: {dpdk_i[0]/1e3:.1f} µs")
if fstk_i and not fstk_i[1]:
    print(f"  fstack:   {fstk_i[0]/1e3:.1f} µs")
if dpdk_i and fstk_i and not dpdk_i[1] and not fstk_i[1]:
    ratio = fstk_i[0] / dpdk_i[0]
    print(f"  fstack is {ratio:.1f}x slower at burst initiation" if ratio > 1
          else f"  stacks within {1/ratio:.1f}x of each other")

print(f"\nBurst initiation p50 at K=1MiB, G=1ms:")
dpdk_i1m = dpdk_ini.get((1048576,1))
fstk_i1m = fstk_ini.get((1048576,1))
if dpdk_i1m and not dpdk_i1m[1]:
    print(f"  dpdk_net: {dpdk_i1m[0]/1e3:.1f} µs")
if fstk_i1m and not fstk_i1m[1]:
    print(f"  fstack:   {fstk_i1m[0]/1e3:.1f} µs")
if dpdk_i1m and fstk_i1m and not dpdk_i1m[1] and not fstk_i1m[1]:
    ratio = fstk_i1m[0] / dpdk_i1m[0]
    print(f"  fstack is {ratio:.1f}x slower at K=1MiB burst initiation")

print(f"\nMaxtp C=1 comparison (wire rate):")
for w in [64, 256, 1024, 4096, 16384]:
    d = dpdk_gp.get((w,1))
    f = fstk_gp.get((w,1))
    if d and f and not d[1] and not f[1]:
        dv, fv = d[0], f[0]
        if dv > 0 and fv > 0:
            ratio = dv / fv
            note = f"dpdk {ratio:.1f}x faster" if ratio > 1.1 else \
                   f"fstack {1/ratio:.1f}x faster" if ratio < 0.9 else "comparable"
            print(f"  W={w:>7}B  dpdk_net={dv/1e9:.3f}G  fstack={fv/1e9:.3f}G  ({note})")

print(f"\ndpdk_net maxtp grid completeness:")
missing = [(w,c) for w in W_VALS for c in C_VALS if dpdk_gp.get((w,c)) is None]
failed  = [(w,c) for w in W_VALS for c in C_VALS if dpdk_gp.get((w,c)) and dpdk_gp[(w,c)][1]]
print(f"  {len(W_VALS)*len(C_VALS)} buckets expected, {len(missing)} missing, {len(failed)} failed")
if not missing and not failed:
    print("  All 28 dpdk_net maxtp buckets present — pool-open retry fix holding")
else:
    print(f"  Missing: {missing}")

print()
