# uProf host-capabilities snapshot — 2026-04-23

Canonical capability record for the A10-perf effort's profiling host. Referenced by every family report as ground truth for what measurement tooling is available.

**Host:** AWS EC2 instance, AMD EPYC 7R13 (Family 0x19 / Zen 3 Milan, Model 0x01), 8 vCPU, KVM hypervisor.
**Kernel:** 6.8.0-1052-aws.
**uProf:** 5.2.606.0 installed from `/home/ubuntu/amduprof_5.2-606_amd64.deb` to `/opt/AMDuProf_5.2-606/`.
**PATH:** symlinks at `/usr/local/bin/AMDuProfCLI` + `/usr/local/bin/AMDuProfPcm` so shell defaults pick them up.

## Capability matrix — what works on this host

| uProf feature | Available? | Notes |
|---|---|---|
| TBP (time-based profiling) | **Yes** | Genuine timer-interrupt sampling — produces hotspot attribution weighted by wall-clock time, not hardware cycles. Sufficient for "where is time spent" questions. |
| IBS (Instruction-Based Sampling) | **No** | `amd_ibs` / `ibs` kernel module not present on this kernel; `info --system` reports `IBS: No`. KVM does not pass through IBS hardware. |
| Core PMC | **No** | `info --system` reports `Core PMC: No`. EC2 KVM does not virtualize hardware counters. |
| L3 PMC | **No** | Same. |
| DF / UMC PMC | **No** | Same. |
| PERF TS | **No** | Same. |
| RAPL / CEF | **No** | Same — no power-domain counters. |
| AMDuProfPcm | **Installed but non-functional** | Depends on PMCs which are not virtualized here. Any `AMDuProfPcm` step in Procedure P2 will fail; drop silently or skip with a note. |

## Consequences for Procedure P2

Amend P2's per-family capture recipe as follows until this host snapshot changes:

```bash
# Input: family F, iteration label L
F="$1"; L="$2"; D="profile/${F}-${L}"
mkdir -p "$D"

# TBP — the only available primary signal on this host
AMDuProfCLI collect --config tbp -d 30 --output "$D/tbp" \
  cargo bench --bench "$F" -- --profile-time 30
AMDuProfCLI report --import-dir "$D/tbp" --report-output "$D/tbp.html"

# IBS, PCM — NOT RUN on this host (KVM doesn't expose them).
# If this effort moves to a bare-metal or PMC-virtualized host,
# restore the IBS + AMDuProfPcm legs per plan §Procedure P2.
```

## Consequences for spec D5 exit gate

D5 says "top hotspot < 5% of cycles". On a host without cycle counters, "cycles" is actually "TBP sample time" — an approximation of cycle attribution weighted by wall-clock, not exact cycle counts. This is acceptable for the hotspot-identification use case: if a function takes > 5% of wall-clock time in a bench, it's a real hotspot regardless of whether the metric is cycles or time. The exit gate rule stays unchanged, with "cycles" read as "TBP-attributed time" on this host.

## Consequences for family reports

Each family baseline report's "Top hotspots" section comes exclusively from TBP. The table schema stays the same; the `metric` column (if present) is "TBP time %" rather than "cycle %".

Drop the "Top 10 by IBS retire latency" and "Top 10 by L1/L2 miss source lines" rows from baseline reports — those require IBS. Replace with a single note: *"IBS + PCM not available on this KVM host; see docs/superpowers/reports/perf-host-capabilities.md."*

## Install-time workaround (debugfs / tracefs / bpf mounts)

The uProf DEB postinst failed twice on missing filesystem mounts. Manual fix applied during install:

```bash
sudo mount -t debugfs none /sys/kernel/debug
sudo mount -t tracefs none /sys/kernel/tracing
sudo mount -t bpf bpf /sys/fs/bpf
sudo dpkg --configure amduprof   # re-run the failed postinst
```

These mounts are NOT persistent across reboot. If this host reboots, the uProf bpftracer capabilities may fail until the mounts are re-applied. **If the host reboots during this effort, repeat the three `mount` commands before any `AMDuProfCLI collect` call.**

Candidate fix (out of scope for A10-perf): add a systemd unit or `/etc/fstab` entries to make the mounts persistent. File against host-setup, not this effort.

## Follow-on items filed

- If future work needs IBS / PMC precision, provision a bare-metal instance (`c6a.metal`, `c7a.metal`) or enable EC2 "Enhanced Performance Monitoring" if available on newer instance families.
- Consider `perf` via `perf_event_paranoid = -1` as an alternative primary signal — even on KVM, Linux `perf` with software events (page-faults, context-switches, wall-clock cycles via `task-clock`) complements TBP.
