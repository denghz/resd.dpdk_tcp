# A10-perf Implementation Plan — bench-micro hot-path optimization + DPDK 24.11 adoption

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive every `tools/bench-micro/` family below its Stage-1 design §11.2 order-of-magnitude target using uProf-guided optimization on DPDK 23.11, while running a parallel DPDK 24.11 LTS rebase + target-API adoption experiment, and produce an evidence-based recommendation on 24.11 pinning.

**Architecture:** Two git worktrees branched from `phase-a10` tip `671062a` (`/home/ubuntu/resd.dpdk_tcp-a10-perf` on `a10-perf-23.11`; `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24` on `a10-dpdk24-adopt`). Base commit amended 2026-04-23 from `671062a` → `671062a` after A10 phase completed. A new `bench-internals` cargo feature gates an `EngineNoEalHarness` plus pub-for-bench surface that lets bench-micro exercise real library code without DPDK EAL init. uProf (`tbp` + `ibs` + `AMDuProfPcm`) profiles each bench; optimization lands per-family via a hypothesize-implement-measure cycle. The DPDK-24 worktree rebases, re-baselines, then adopts target APIs (`rte_lcore_var`, `rte_ptr_compress`, `rte_bit_atomic_*`) one at a time, each an A/B. End-of-effort two-stage review (spec-compliance + code-quality reviewer subagents, opus 4.7) gates cherry-picks onto master.

**Tech Stack:** Rust 2021 (stable rustup toolchain), criterion 0.5, AMD uProf 5.2, DPDK 23.11 LTS + 24.11 LTS, clang-22 + libstdc++, cargo workspaces, AMD EPYC 7R13 (Zen 3 / Milan) on EC2 KVM.

**Spec:** `docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md`

---

## File Structure

### Created

**On master (`/home/ubuntu/resd.dpdk_tcp`):**
- `scripts/check-perf-host.sh` — host-config precondition checker; cherry-picked into each worktree

**On `a10-perf-23.11` worktree (and cherry-picked to `a10-dpdk24-adopt`):**
- `crates/dpdk-net-core/src/engine/test_support.rs` — `EngineNoEalHarness` (feature-gated)
- `docs/superpowers/reports/perf-23.11/<family>-baseline.md` — 7 files, one per bench family
- `docs/superpowers/reports/perf-23.11/<family>-iter-<N>.md` — per-iteration reports, N variable per family
- `docs/superpowers/reports/perf-23.11/summary.md` — worktree-1 outcome

**On `a10-dpdk24-adopt` worktree:**
- `docs/superpowers/reports/perf-dpdk24/baseline-rebase.md`
- `docs/superpowers/reports/perf-dpdk24/adopt-<api>.md` — per target API
- `docs/superpowers/reports/perf-dpdk24/port-forward-<change>.md` — per cherry-picked 23.11 win
- `docs/superpowers/reports/perf-dpdk24/summary.md` — worktree-2 outcome + 24.11 recommendation

**On master (end-of-effort):**
- `docs/superpowers/reports/perf-sync/sync-YYYY-MM-DD.md` — weekly sync reports (committed on master after each sync)
- `docs/superpowers/reports/perf-a10-postphase.md` — cross-worktree summary

### Modified

**`crates/dpdk-net-core/Cargo.toml`:** add `bench-internals = []` additive feature
**`crates/dpdk-net-core/src/tcp_timer_wheel.rs`:** audit `pub(crate)` surface; flip needed items to `pub` under `#[cfg(feature = "bench-internals")]`
**`crates/dpdk-net-core/src/engine.rs`:** add `mod test_support;` under `#[cfg(feature = "bench-internals")]`; expose any internal accessors `EngineNoEalHarness` needs, feature-gated
**`crates/dpdk-net-core/src/lib.rs`:** re-export `engine::test_support` under feature gate
**`tools/bench-micro/Cargo.toml`:** consume `dpdk-net-core` with `features = ["bench-internals"]`
**`tools/bench-micro/benches/poll.rs`:** rewrite against `EngineNoEalHarness::poll_once()`
**`tools/bench-micro/benches/timer.rs`:** rewrite against `EngineNoEalHarness` timer surface
**`tools/bench-micro/benches/send.rs`:** either rewrite against harness OR document why stub stays (see T2.7)
**`tools/bench-micro/benches/*.rs` (all 7):** add per-iteration RDTSC sidecar capture; add new CSV-metadata columns via `bench-common`
**`tools/bench-micro/src/bin/summarize.rs`:** remove freshly-unblocked benches from `STUB_TARGETS`; add new CSV columns
**`tools/bench-common/src/csv_row.rs`** (inspect path): add `cpu_family`, `cpu_model_name`, `dpdk_version`, `worktree_branch`, `uprof_session_id` columns

**`crates/dpdk-net-sys/build.rs`** (only on `a10-dpdk24-adopt`): bump `atleast_version("23.11")` → `atleast_version("24.11")`
**`docs/superpowers/plans/stage1-phase-roadmap.md`** (end-of-effort, on master): append A10 row with `→ A10-perf follow-on` note

### Untouched

- All other `tools/bench-*` crates (scope is bench-micro only)
- Production wire behavior; `preset=rfc_compliance` defaults
- `/home/ubuntu/resd.dpdk_tcp-a10` existing worktree — stays frozen as phase-a10 reference

---

## Cross-Cutting Procedures

Referenced by many tasks. Do not inline the full code/steps into every task — the procedures below are canonical; tasks reference them by name.

### Procedure P1 — Per-family optimization cycle

Used by each family task in Phase 3. Each cycle iteration creates one or more child tasks ad-hoc; the cycle body is the same for every family. Family-specific data (bench names, §11.2 target, hotspot seeds) lives in the family task itself.

1. **Baseline lock.** `./scripts/check-perf-host.sh` must exit 0. Then:
   ```bash
   cargo bench --bench <family> -- --save-baseline base-pre-opt --measurement-time 30
   ```
   Criterion writes `target/criterion/<bench-name>/base-pre-opt`. Run 3× consecutively; keep the median run's sidecar as the canonical baseline.
2. **uProf capture.** Run the full per-§4.5 recipe of the spec:
   ```bash
   mkdir -p profile/<family>-baseline
   AMDuProfCLI collect --config tbp -d 30 --output profile/<family>-baseline/tbp \
     cargo bench --bench <family> -- --profile-time 30
   AMDuProfCLI collect --config ibs -d 30 --output profile/<family>-baseline/ibs \
     cargo bench --bench <family> -- --profile-time 30
   AMDuProfPcm -m l3,memory -d 30 -o profile/<family>-baseline/pcm.csv &
   cargo bench --bench <family> -- --profile-time 30
   wait
   AMDuProfCLI report --import-dir profile/<family>-baseline/tbp \
     --report-output profile/<family>-baseline/tbp.html
   AMDuProfCLI report --import-dir profile/<family>-baseline/ibs \
     --report-output profile/<family>-baseline/ibs.html
   ```
3. **Write baseline report** at `docs/superpowers/reports/perf-23.11/<family>-baseline.md` with fields:
   ```
   - Host snapshot (output of check-perf-host.sh)
   - Bench names + criterion median + p50 + p99 + stddev + sample count
   - §11.2 target range and current gap
   - Top 10 hotspots by cycle % (from tbp.html)
   - Top 10 by IBS retire latency (from ibs.html)
   - Top 10 by L1/L2 miss source lines (from ibs.html)
   - PCM memory-bandwidth observation (from pcm.csv)
   - Hypotheses: H1, H2, H3 (ranked by expected ns × confidence)
   ```
   Commit: `git commit -m "a10-perf-23.11: <family> baseline + hypotheses"`
4. **Implement top unranked hypothesis.** Smallest possible diff in one file where possible. Commit: `git commit -m "a10-perf-23.11: <family> — <change> (hypothesis: <one-liner>)"`.
5. **Re-measure.**
   ```bash
   cargo bench --bench <family> -- --baseline base-pre-opt --measurement-time 30
   ```
   Re-run uProf capture into `profile/<family>-iter-<N>/`. Criterion's output shows `% change` per bench and "No change in performance" / "Performance has improved/regressed" verdict.
6. **Pass / fail decision.**
   - **Pass** = criterion median improved by ≥ noise floor **AND** p99 did not worsen **AND** §11.2 target still met (if already beat) **AND** no unit test regressed.
   - **Fail** = any of those conditions violate.
7. **If pass:** lock new baseline (`--save-baseline base-pre-opt` is overwritten if you pass `--save-baseline` again, else keep as rolling). Append `<family>-iter-<N>.md` with before/after + new hotspot list + H1/H2/H3 for next iter. Loop to step 4 with next hypothesis.
8. **If fail:** `git revert HEAD` (not `git reset`). Annotate the family report's hypothesis line: "H<k> rejected by measurement — no improvement / p99 regressed / test fail". Loop to step 4 with next hypothesis.
9. **Exit the cycle** when any of:
   - Criterion median ≤ §11.2 upper bound **AND** p99 ≤ 2× upper bound **AND** top hotspot < 5% cycles (or documented floor)
   - 3 consecutive rejected hypotheses (hard stop; document wall)
   - Required change would touch public API/ABI surface (hard stop; file as spec-change candidate)

### Procedure P2 — uProf capture recipe (standalone)

Used by P1 and also by Phase 4 target-API adoption tasks. Invoke:
```bash
# Inputs: family F, iteration label L (e.g. "baseline" or "iter-3")
F="$1"; L="$2"; D="profile/${F}-${L}"
mkdir -p "$D"
AMDuProfCLI collect --config tbp -d 30 --output "$D/tbp" cargo bench --bench "$F" -- --profile-time 30
AMDuProfCLI collect --config ibs -d 30 --output "$D/ibs" cargo bench --bench "$F" -- --profile-time 30
AMDuProfPcm -m l3,memory -d 30 -o "$D/pcm.csv" &
cargo bench --bench "$F" -- --profile-time 30
wait
AMDuProfCLI report --import-dir "$D/tbp" --report-output "$D/tbp.html"
AMDuProfCLI report --import-dir "$D/ibs" --report-output "$D/ibs.html"
```
Committed alongside iteration reports (filenames referenced; large `.caperf` blobs stay gitignored).

### Procedure P3 — Weekly sync

Run every 7 days or on closing a family/API task, whichever first. On master:
1. `git fetch` in each worktree; note current HEAD per worktree.
2. Run `./scripts/check-perf-host.sh` in both worktrees; diff outputs. Any drift = both re-baseline before comparing.
3. Write `docs/superpowers/reports/perf-sync/sync-$(date -u +%Y-%m-%d).md`:
   ```
   - Worktree heads: a10-perf-23.11 @ <sha>, a10-dpdk24-adopt @ <sha>
   - Per-family numbers (criterion median + p99 on each side + §11.2 target + gap)
   - Cross-pollination queue: wins to port in either direction
   - Kill-criterion status: family-weeks-with-no-progress
   ```
4. Commit on master.
5. Cherry-pick any API-agnostic wins (cacheline layout, branch prediction, slab reuse) across worktrees the same day.
6. **Never cross-pick** `crates/dpdk-net-sys/build.rs` — that's the DPDK-version boundary.

---

## Phase 0 — Shared setup (on master)

### Task 0.1: Install uProf on the shared dev host

**Files:**
- No source changes; host-level install

- [ ] **Step 1: Run sudo install**

```bash
sudo apt install -y /home/ubuntu/amduprof_5.2-606_amd64.deb
```
Expected: `Setting up amduprof (5.2-606) ...` line in output; no unresolved-dependency errors.

- [ ] **Step 2: Load required kernel modules**

```bash
sudo modprobe msr
sudo modprobe amd_ibs || sudo modprobe ibs || true
```
Note: module name may be `amd_ibs`, `ibs`, or absent under KVM. Absence doesn't block TBP collection, only IBS. Proceed with the best-available; document in §4.1.

- [ ] **Step 3: Verify CLI is on PATH**

```bash
which AMDuProfCLI AMDuProfPcm
AMDuProfCLI --version
```
Expected: both paths resolve under `/opt/AMDuProf_*/bin/` or similar; version line prints `5.2-606`.

- [ ] **Step 4: Sanity run — TBP collection**

```bash
AMDuProfCLI info --system | head -20
mkdir -p /tmp/uprof-probe
AMDuProfCLI collect --config tbp -d 2 -o /tmp/uprof-probe /bin/echo hello
ls -lh /tmp/uprof-probe/*.caperf
```
Expected: `info --system` shows `Processor Family: 25` + `Model: 1` (Zen 3 Milan); `collect` produces a non-empty `.caperf`.

- [ ] **Step 5: Sanity run — IBS collection (may fail under KVM)**

```bash
AMDuProfCLI collect --config ibs -d 2 -o /tmp/uprof-probe-ibs /bin/echo hello
ls -lh /tmp/uprof-probe-ibs/*.caperf 2>/dev/null && echo "IBS OK" || echo "IBS unavailable under this kernel — fall back to TBP-only"
```
If IBS fails: document the degradation in the first family baseline report; TBP-only still produces hotspot rankings.

- [ ] **Step 6: Commit nothing** (host-level install, not in repo)

No commit step — record the install in the first family baseline report under `Host snapshot`.

### Task 0.2: Write `scripts/check-perf-host.sh`

**Files:**
- Create: `scripts/check-perf-host.sh`

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# check-perf-host.sh — host-config precondition checker for bench runs.
# Exits 0 if the host matches the pinned bench-latency config; non-zero otherwise.
# Called at the top of every bench run. No --lenient mode in iteration.
set -euo pipefail

fail=0
say() { echo "[check-perf-host] $*"; }
bad() { echo "[check-perf-host] FAIL: $*" >&2; fail=1; }

# CPU model
model=$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo)
[[ "$model" == *"AMD EPYC 7R13"* ]] || bad "cpu model mismatch: got '$model', want EPYC 7R13"
say "cpu model: $model"

# Governor
gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo "unknown")
[[ "$gov" == "performance" ]] || bad "governor not performance: '$gov'"
say "governor: $gov"

# Frequency pinning state (record, don't fail — turbo state noted in CSV)
cur_freq=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq 2>/dev/null || echo "unknown")
max_freq=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq 2>/dev/null || echo "unknown")
say "cpu0 freq: cur=$cur_freq max=$max_freq"

# THP
thp=$(cat /sys/kernel/mm/transparent_hugepage/enabled)
[[ "$thp" == *"[never]"* ]] || bad "transparent hugepages not disabled: '$thp'"
say "THP: $thp"

# NMI watchdog
nmi=$(cat /proc/sys/kernel/nmi_watchdog)
[[ "$nmi" == "0" ]] || bad "NMI watchdog enabled: '$nmi'"
say "nmi_watchdog: $nmi"

# Huge pages
if [[ -d /mnt/huge ]]; then
  say "hugepages: /mnt/huge mounted"
else
  bad "hugepages mount /mnt/huge missing"
fi
nr_hp=$(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages 2>/dev/null || echo 0)
[[ "$nr_hp" -ge 128 ]] || bad "hugepages-2048kB count low: $nr_hp (need >= 128)"
say "hugepages-2048kB: $nr_hp"

# isolcpus / nohz_full / rcu_nocbs — present in /proc/cmdline
cmd=$(cat /proc/cmdline)
for k in isolcpus nohz_full rcu_nocbs; do
  if [[ "$cmd" == *"$k"* ]]; then
    val=$(echo "$cmd" | grep -oE "${k}=[^ ]+")
    say "$val"
  else
    say "WARNING: $k not in cmdline (dev box — ok for uProf-only workflows, NOT ok for measurement)"
  fi
done

# DPDK version visible
pkg-config --modversion libdpdk 2>/dev/null | sed 's/^/[check-perf-host] libdpdk: /'

if [[ $fail -ne 0 ]]; then
  echo "[check-perf-host] FAIL — host config mismatch; fix before running benchmarks" >&2
  exit 1
fi
echo "[check-perf-host] OK"
exit 0
```

- [ ] **Step 2: `chmod +x`**

```bash
chmod +x scripts/check-perf-host.sh
```

- [ ] **Step 3: Dry run**

```bash
./scripts/check-perf-host.sh || echo "exit=$?"
```
Expected: either `OK` with exit 0 if the dev host happens to be fully configured, or `FAIL` with specific mismatches. Mismatches that are dev-box-acceptable (missing `isolcpus`) emit `WARNING` not `FAIL` per the script's policy. Mismatches that are hard stops (wrong CPU model, THP on, NMI watchdog on) exit 1.

- [ ] **Step 4: Commit**

```bash
git add scripts/check-perf-host.sh
git commit -m "a10-perf: scripts/check-perf-host.sh — precondition checker for bench runs

$(cat <<'EOF'
Hardcoded for current dev EC2 host (AMD EPYC 7R13 / Zen 3 Milan / KVM).
Fails fast on governor, THP, NMI watchdog, huge-page mount; warns on
isolcpus/nohz_full/rcu_nocbs (dev box acceptable, bench box required).
Future host moves = spec amendment per §4.2.
EOF
)"
```

---

## Phase 1 — Worktree creation + sanity

### Task 1.1: Create both worktrees from `phase-a10` tip `671062a`

**Files:**
- Creates: `/home/ubuntu/resd.dpdk_tcp-a10-perf` (worktree); `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24` (worktree)

- [ ] **Step 1: Confirm base commit sha is reachable**

```bash
cd /home/ubuntu/resd.dpdk_tcp
git log --oneline 671062a -1
```
Expected: shows `671062a bench-nightly: lower iteration count to 5000 to dodge ~7051 failure` (the exact subject at the updated base commit).

- [ ] **Step 2: Create Worktree 1**

```bash
git worktree add -b a10-perf-23.11 /home/ubuntu/resd.dpdk_tcp-a10-perf 671062a
```
Expected: `Preparing worktree (new branch 'a10-perf-23.11')` + `HEAD is now at 671062a ...`.

- [ ] **Step 3: Create Worktree 2**

```bash
git worktree add -b a10-dpdk24-adopt /home/ubuntu/resd.dpdk_tcp-a10-dpdk24 671062a
```
Expected: analogous output.

- [ ] **Step 4: Verify**

```bash
git worktree list
```
Expected: at least 4 entries — main repo, existing `/home/ubuntu/resd.dpdk_tcp-a10` on `phase-a10`, new `-perf`, new `-dpdk24`.

- [ ] **Step 5: Commit — none.** Worktree creation doesn't itself produce a commit.

### Task 1.2: Cherry-pick master's spec + check-perf-host script into each worktree

Both `phase-a10` and the new worktree branches diverged from master long ago. The spec + checker live on master. Cherry-pick into each worktree so each branch's tree contains them.

**Files:**
- Creates (in each worktree): `docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md`, `scripts/check-perf-host.sh`

- [ ] **Step 1: Identify the two master commits**

```bash
cd /home/ubuntu/resd.dpdk_tcp
git log --oneline master -5
# Expect: 89771a2 a10-perf spec: ... (from Phase 0)
#         <task-0.2-sha> a10-perf: scripts/check-perf-host.sh ...
```
Record both SHAs. Call them `$SPEC_SHA` and `$SCRIPT_SHA`.

- [ ] **Step 2: Cherry-pick into Worktree 1**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git cherry-pick $SPEC_SHA
git cherry-pick $SCRIPT_SHA
git log --oneline -3
```
Expected: both picks succeed without conflicts (spec + script are net-new files in a distinct dir).

- [ ] **Step 3: Cherry-pick into Worktree 2**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-dpdk24
git cherry-pick $SPEC_SHA
git cherry-pick $SCRIPT_SHA
git log --oneline -3
```
Expected: analogous success.

- [ ] **Step 4: Verify files landed in both**

```bash
ls /home/ubuntu/resd.dpdk_tcp-a10-perf/scripts/check-perf-host.sh \
   /home/ubuntu/resd.dpdk_tcp-a10-perf/docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md \
   /home/ubuntu/resd.dpdk_tcp-a10-dpdk24/scripts/check-perf-host.sh \
   /home/ubuntu/resd.dpdk_tcp-a10-dpdk24/docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md
```
Expected: all 4 paths exist.

### Task 1.3: Build + test sanity in each worktree

**Files:** none modified

- [ ] **Step 1: Worktree 1 build**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 600 cargo build --workspace 2>&1 | tail -30
```
Expected: clean build; no warnings-as-errors regressions (repo has lints, some may flag — fix iff they'd block subsequent tasks).

- [ ] **Step 2: Worktree 1 test**

```bash
timeout 900 cargo test --workspace -- --skip ignored 2>&1 | tail -40
```
Expected: tests pass. DPDK-dependent `#[ignore]`d tests skipped as usual.

- [ ] **Step 3: Worktree 1 bench compile-check**

```bash
timeout 600 cargo bench --workspace --no-run 2>&1 | tail -20
```
Expected: all benches compile; no runtime started.

- [ ] **Step 4: Repeat steps 1-3 in Worktree 2**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-dpdk24
timeout 600 cargo build --workspace 2>&1 | tail -30
timeout 900 cargo test --workspace -- --skip ignored 2>&1 | tail -40
timeout 600 cargo bench --workspace --no-run 2>&1 | tail -20
```
Expected: Worktree 2 produces identical output — still on DPDK 23.11 at this point.

- [ ] **Step 5: Commit — none.** Sanity run without source changes.

---

## Phase 2 — `bench-internals` feature + harness

Executes entirely on `a10-perf-23.11`; the feature-adding commits get cherry-picked to `a10-dpdk24-adopt` at T2.10.

### Task 2.1: Add `bench-internals` cargo feature flag

**Files:**
- Modify: `crates/dpdk-net-core/Cargo.toml`

- [ ] **Step 1: Inspect existing features table**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
grep -A 20 '^\[features\]' crates/dpdk-net-core/Cargo.toml
```

- [ ] **Step 2: Add the feature**

Edit `crates/dpdk-net-core/Cargo.toml`. Add an additive `bench-internals` feature under `[features]`:

```toml
# Additive feature: exposes TimerWheel + EngineNoEalHarness for tools/bench-micro.
# Off by default. Production builds see no difference.
# See docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md §4.3
bench-internals = []
```

Place it after any existing additive features; not in `default = [...]`.

- [ ] **Step 3: Verify clean build without feature**

```bash
cargo build -p dpdk-net-core 2>&1 | tail -5
```
Expected: no changes in build output (feature unused yet).

- [ ] **Step 4: Verify clean build with feature**

```bash
cargo build -p dpdk-net-core --features bench-internals 2>&1 | tail -5
```
Expected: clean — no `#[cfg(feature = ...)]` sites yet so feature is inert.

- [ ] **Step 5: Commit**

```bash
git add crates/dpdk-net-core/Cargo.toml
git commit -m "a10-perf-23.11: add bench-internals cargo feature

Additive, off by default. Gates pub exposure of TimerWheel and the
new EngineNoEalHarness test surrogate consumed only by tools/bench-micro.
Production builds unaffected."
```

### Task 2.2: Audit `TimerWheel` publicity + gate any `pub(crate)` internals bench needs

**Files:**
- Modify: `crates/dpdk-net-core/src/tcp_timer_wheel.rs`

- [ ] **Step 1: Audit current surface**

```bash
grep -n "^pub\|pub(crate)\|pub fn\|pub(crate) fn" crates/dpdk-net-core/src/tcp_timer_wheel.rs
```
Record which of these are required by `bench_timer_add_cancel` and `bench_poll_idle_with_timers` to land. Expect at minimum: `TimerWheel::new`, `TimerWheel::add`, `TimerWheel::cancel`, `TimerWheel::advance`.

- [ ] **Step 2: For each `pub(crate)` item the benches need, flip to `pub` under feature gate**

For each item X that the benches require, replace:
```rust
pub(crate) fn add(&mut self, ...) -> TimerId { ... }
```
with:
```rust
#[cfg(not(feature = "bench-internals"))]
pub(crate) fn add(&mut self, ...) -> TimerId { ... }
#[cfg(feature = "bench-internals")]
pub fn add(&mut self, ...) -> TimerId { ... }
```
Repeat for every `pub(crate)` item the benches consume. Items that are already `pub` need no change.

- [ ] **Step 3: Verify both build paths**

```bash
cargo build -p dpdk-net-core 2>&1 | tail -3
cargo build -p dpdk-net-core --features bench-internals 2>&1 | tail -3
```
Expected: both clean.

- [ ] **Step 4: Commit**

```bash
git add crates/dpdk-net-core/src/tcp_timer_wheel.rs
git commit -m "a10-perf-23.11: tcp_timer_wheel — gate pub(crate) internals under bench-internals

Flips the specific pub(crate) surface required by bench_timer_add_cancel
and bench_poll_idle_with_timers to pub when bench-internals is enabled.
Default builds unchanged. Item list in commit diff."
```

### Task 2.3: Implement `EngineNoEalHarness`

**Files:**
- Create: `crates/dpdk-net-core/src/engine/test_support.rs`
- Modify: `crates/dpdk-net-core/src/engine.rs` (add `#[cfg(feature = "bench-internals")] pub mod test_support;`)
- Modify: `crates/dpdk-net-core/src/lib.rs` (re-export under feature gate)

- [ ] **Step 1: Identify the required surface**

Read `crates/dpdk-net-core/src/engine.rs` around `Engine::poll_once` (if named that — else the main poll body). Record the walk: (a) flow-table lookup, (b) timer-wheel advance, (c) event-queue drain, (d) conn-state dispatch. Note every `pub(crate)` / private type the harness needs to mirror. For each such type, add the same feature-gate dance as T2.2 so the harness can touch it.

- [ ] **Step 2: Create the harness file**

`crates/dpdk-net-core/src/engine/test_support.rs`:

```rust
//! EngineNoEalHarness — bench/test harness that walks the same code
//! paths as `Engine::poll_once` (flow-table lookup → timer-wheel
//! advance → event-queue drain → conn-state dispatch) without
//! calling `rte_eal_init`. Consumed by tools/bench-micro.
//!
//! # NOT A PRODUCTION ENGINE
//!
//! This is an opt-in test surrogate. Compilation is gated behind
//! `crates/dpdk-net-core/Cargo.toml`'s `bench-internals` feature,
//! which is off by default. Production builds never see this module.
//!
//! # What it covers
//!
//! - poll-loop fixed overhead (bench_poll_empty)
//! - timer-wheel walk over non-firing buckets (bench_poll_idle_with_timers)
//! - timer add/cancel round-trip (bench_timer_add_cancel)
//!
//! # What it doesn't cover
//!
//! - TX path (bench_send_small, bench_send_large_chain) — these need
//!   rte_mempool + mbuf alloc which requires real EAL. See plan T2.7
//!   for the chosen strategy (either bench-scoped EAL or stay-stub).

use crate::flow_table::{FlowTable, FourTuple};
use crate::tcp_conn::TcpConn;
use crate::tcp_events::EventQueue;
use crate::tcp_timer_wheel::{TimerId, TimerWheel};
// plus whatever minimal set the real Engine::poll_once touches; add imports as required

pub struct EngineNoEalHarness {
    pub flow_table: FlowTable,
    pub timer_wheel: TimerWheel,
    pub event_queue: EventQueue,
    // Per-iteration scratch to approximate poll_once's local vars.
    now_ns: u64,
}

impl EngineNoEalHarness {
    /// Construct with fixed capacity. No DPDK EAL, no mempool, no port.
    pub fn new(flow_capacity: usize, timer_tick_ns: u64) -> Self {
        Self {
            flow_table: FlowTable::new(flow_capacity),
            timer_wheel: TimerWheel::new(timer_tick_ns),
            event_queue: EventQueue::new_with_default_capacity(),
            now_ns: 0,
        }
    }

    /// Mirror `Engine::poll_once`'s fixed-overhead path — timer advance,
    /// event-queue no-op drain, flow-table walk on hot entries if any.
    /// No RX (no mempool), no TX (no port).
    pub fn poll_once(&mut self) {
        self.now_ns = crate::clock::now_ns();
        self.timer_wheel.advance(self.now_ns);
        self.event_queue.drain_into_sink(|_| {});
        // No RX burst — harness has no port. The flow-table walk is
        // exercised by the dedicated flow_lookup benches.
    }

    /// Pre-populate the wheel with non-firing timers so advance walks
    /// a real bucket chain.
    pub fn pre_populate_timers(&mut self, count: usize, when_ns: u64) -> Vec<TimerId> {
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            let id = self.timer_wheel.add(when_ns, /* payload */ 0);
            ids.push(id);
        }
        ids
    }

    /// Direct pass-throughs so timer benches have a stable API.
    pub fn timer_add(&mut self, when_ns: u64, payload: u64) -> TimerId {
        self.timer_wheel.add(when_ns, payload)
    }
    pub fn timer_cancel(&mut self, id: TimerId) -> bool {
        self.timer_wheel.cancel(id)
    }

    /// Install a test connection for flow-lookup benches (if they
    /// want to share the harness rather than using FlowTable directly).
    pub fn insert_conn(&mut self, conn: TcpConn) -> bool {
        self.flow_table.insert(conn).is_ok()
    }

    pub fn tuple_lookup(&self, t: &FourTuple) -> Option<usize> {
        self.flow_table.lookup_by_tuple(t)
    }
}
```

Adjust the imports and method bodies to match the actual `Engine` surface — the engineer should read `engine.rs` around `poll_once` and mirror it. Any call the real `poll_once` makes that is `pub(crate)`-only needs to be feature-gated per T2.2.

- [ ] **Step 3: Wire `mod test_support` into engine.rs**

In `crates/dpdk-net-core/src/engine.rs`, near the top of the file (after existing `mod` declarations):

```rust
#[cfg(feature = "bench-internals")]
pub mod test_support;
```

- [ ] **Step 4: Re-export from lib.rs (optional sugar for downstream)**

In `crates/dpdk-net-core/src/lib.rs`:

```rust
#[cfg(feature = "bench-internals")]
pub use engine::test_support::EngineNoEalHarness;
```

- [ ] **Step 5: Verify it compiles with the feature on**

```bash
cargo build -p dpdk-net-core --features bench-internals 2>&1 | tail -5
```

- [ ] **Step 6: Verify it doesn't compile when feature is off (file is excluded)**

```bash
cargo build -p dpdk-net-core 2>&1 | tail -5
grep -rn "EngineNoEalHarness" crates/dpdk-net-core/src/ | head -5
```
Expected: the grep finds declarations inside the test_support file only; default build stays clean.

- [ ] **Step 7: Commit**

```bash
git add crates/dpdk-net-core/src/engine/test_support.rs \
        crates/dpdk-net-core/src/engine.rs \
        crates/dpdk-net-core/src/lib.rs
git commit -m "a10-perf-23.11: engine::test_support::EngineNoEalHarness

Feature-gated (bench-internals) surrogate that walks poll_once's
timer/event/flow-table path without rte_eal_init. Consumed only by
tools/bench-micro. Not a production Engine."
```

### Task 2.4: Unit tests for `EngineNoEalHarness`

**Files:**
- Create: `crates/dpdk-net-core/tests/engine_no_eal_harness.rs` (integration test, gated on feature)

- [ ] **Step 1: Write the test module**

```rust
//! Integration tests for EngineNoEalHarness. Gated on bench-internals.
#![cfg(feature = "bench-internals")]

use dpdk_net_core::engine::test_support::EngineNoEalHarness;

#[test]
fn constructs_without_eal() {
    let _h = EngineNoEalHarness::new(64, 1_000_000);
}

#[test]
fn poll_once_is_noop_when_idle() {
    let mut h = EngineNoEalHarness::new(64, 1_000_000);
    h.poll_once();
    h.poll_once();
    h.poll_once();
}

#[test]
fn timer_add_cancel_roundtrip() {
    let mut h = EngineNoEalHarness::new(64, 1_000_000);
    let id = h.timer_add(10_000_000, 0xDEADBEEF);
    let cancelled = h.timer_cancel(id);
    assert!(cancelled);
}

#[test]
fn pre_populated_timers_do_not_fire_prematurely() {
    let mut h = EngineNoEalHarness::new(64, 1_000_000);
    let _ids = h.pre_populate_timers(32, u64::MAX / 2);
    for _ in 0..100 {
        h.poll_once();
    }
    // Expectation: no panic, harness did not attempt to fire future-
    // dated timers. (Explicit event-queue check would require exposing
    // drain count — future enhancement if a regression appears.)
}
```

- [ ] **Step 2: Run the test with the feature enabled**

```bash
timeout 120 cargo test -p dpdk-net-core --features bench-internals --test engine_no_eal_harness 2>&1 | tail -10
```
Expected: 4 tests pass.

- [ ] **Step 3: Verify default build still works**

```bash
timeout 120 cargo test -p dpdk-net-core 2>&1 | tail -10
```
Expected: the integration file is skipped (excluded by `#![cfg(feature = "bench-internals")]`); all existing tests still pass.

- [ ] **Step 4: Commit**

```bash
git add crates/dpdk-net-core/tests/engine_no_eal_harness.rs
git commit -m "a10-perf-23.11: tests for EngineNoEalHarness"
```

### Task 2.5: Rewrite `bench_poll_*` against the harness

**Files:**
- Modify: `tools/bench-micro/Cargo.toml` (add feature on dep)
- Modify: `tools/bench-micro/benches/poll.rs`

- [ ] **Step 1: Update Cargo.toml**

In `tools/bench-micro/Cargo.toml`, change the `dpdk-net-core` dependency line:

```toml
dpdk-net-core = { path = "../../crates/dpdk-net-core", features = ["bench-internals"] }
```

- [ ] **Step 2: Rewrite poll.rs**

```rust
//! bench-micro::poll — spec §11.2 targets 1 + 2.
//!
//! `bench_poll_empty` measures `EngineNoEalHarness::poll_once()` with
//! no pre-populated timers or flows — matches `Engine::poll_once`'s
//! fixed per-iteration cost in production when no RX and no timers fire.
//!
//! `bench_poll_idle_with_timers` pre-populates the wheel with 256
//! non-firing timers so `advance` walks a real bucket chain.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use std::time::Duration;

fn bench_poll_empty(c: &mut Criterion) {
    c.bench_function("bench_poll_empty", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        b.iter(|| {
            h.poll_once();
            black_box(&h);
        });
    });
}

fn bench_poll_idle_with_timers(c: &mut Criterion) {
    c.bench_function("bench_poll_idle_with_timers", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        // Timers scheduled far in the future — advance walks the
        // bucket chain but never fires anything during the bench.
        let _ids = h.pre_populate_timers(256, u64::MAX / 2);
        b.iter(|| {
            h.poll_once();
            black_box(&h);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets = bench_poll_empty, bench_poll_idle_with_timers
}
criterion_main!(benches);
```

- [ ] **Step 3: Build the bench**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
timeout 300 cargo bench --bench poll --no-run 2>&1 | tail -5
```
Expected: compile-clean.

- [ ] **Step 4: Short measurement run**

```bash
timeout 120 cargo bench --bench poll -- --measurement-time 5 2>&1 | tail -20
```
Expected: both benches produce numbers; they may differ materially from the pre-rewrite stub numbers. Record the first numbers as the "rewrite" data point in the upcoming family-baseline report.

- [ ] **Step 5: Commit**

```bash
git add tools/bench-micro/Cargo.toml tools/bench-micro/benches/poll.rs
git commit -m "a10-perf-23.11: bench-micro/poll — rewrite against EngineNoEalHarness

Removes clock::now_ns proxy stubs; exercises real timer-wheel advance
and event-queue drain via the bench-internals harness. Numbers will
shift vs the stub baseline — expected, not a regression."
```

### Task 2.6: Rewrite `bench_timer_add_cancel` against the harness

**Files:**
- Modify: `tools/bench-micro/benches/timer.rs`

- [ ] **Step 1: Read current timer.rs to see the stub shape**

```bash
cat /home/ubuntu/resd.dpdk_tcp-a10-perf/tools/bench-micro/benches/timer.rs
```

- [ ] **Step 2: Rewrite against harness**

Replace the stub body with:

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use std::time::Duration;

fn bench_timer_add_cancel(c: &mut Criterion) {
    c.bench_function("bench_timer_add_cancel", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        b.iter(|| {
            let id = h.timer_add(black_box(10_000_000), black_box(0));
            let _cancelled = h.timer_cancel(id);
            black_box(&h);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets = bench_timer_add_cancel
}
criterion_main!(benches);
```

Preserve any existing docstring; swap out only the stub body.

- [ ] **Step 3: Build + run**

```bash
timeout 300 cargo bench --bench timer --no-run 2>&1 | tail -5
timeout 120 cargo bench --bench timer -- --measurement-time 5 2>&1 | tail -10
```

- [ ] **Step 4: Commit**

```bash
git add tools/bench-micro/benches/timer.rs
git commit -m "a10-perf-23.11: bench-micro/timer — rewrite against EngineNoEalHarness"
```

### Task 2.7: Decide `bench_send_*` path

`bench_send_small` and `bench_send_large_chain` need `rte_mempool_create` + `mbuf` alloc, which require `rte_eal_init`. Options:

- **Option A — harness grows a bench-scoped mempool.** Link DPDK at bench time but bypass most of EAL init; call `rte_pktmbuf_pool_create_by_ops` into a bench-owned mempool. Feasible but brittle.
- **Option B — per-bench `rte_eal_init` in `criterion_main`.** `main` calls `rte_eal_init(...)` with a minimal argv before criterion starts. Bench-micro stops being "pure in-process" but send benches become real. Requires hugepages + root or `CAP_IPC_LOCK` on the bench host.
- **Option C — keep stub + redirect send optimization work to a non-bench path.** `bench_send_small` stays proxied; optimization of the send hot path happens by landing changes to `tcp_output` + observing other benches that already touch it (none of the current 7 do today — this route is effectively "skip `send_small` optimization"). Worst option for this effort's goals.

**Decision:** Option B. Criterion's `main` harness supports a custom entrypoint; we add a pre-criterion EAL init with a minimal argv.

**Files:**
- Modify: `tools/bench-micro/benches/send.rs`
- Modify: `tools/bench-micro/Cargo.toml` (add `dpdk-net-sys` dep if not already transitive)

- [ ] **Step 1: Read current send.rs**

```bash
cat /home/ubuntu/resd.dpdk_tcp-a10-perf/tools/bench-micro/benches/send.rs
```

- [ ] **Step 2: Rewrite send.rs with a pre-criterion EAL init**

```rust
//! bench-micro::send — spec §11.2 targets 9 + 10.
//!
//! Calls `rte_eal_init` before criterion starts so the bench can
//! exercise the real `dpdk_net_send` path with real mbufs. Departs
//! from bench-micro's "pure in-process" default for these two benches
//! only — send-path measurement needs real mempools.

use criterion::{black_box, criterion_group, Criterion};
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
// If dpdk-net-core exposes a dedicated public send surrogate for
// bench, use it here. Otherwise import whatever minimal Engine
// constructor is usable post-EAL.
use std::time::Duration;

fn init_eal_once() {
    use std::sync::Once;
    static EAL: Once = Once::new();
    EAL.call_once(|| {
        let argv = [
            std::ffi::CString::new("bench-micro-send").unwrap(),
            std::ffi::CString::new("--no-huge").unwrap(),  // if hugepages unavailable locally, else remove
            std::ffi::CString::new("-l").unwrap(),
            std::ffi::CString::new("0").unwrap(),
        ];
        let c_argv: Vec<*mut i8> = argv.iter().map(|s| s.as_ptr() as *mut i8).collect();
        let ret = unsafe { dpdk_net_sys::rte_eal_init(c_argv.len() as i32, c_argv.as_ptr() as *mut *mut i8) };
        if ret < 0 {
            panic!("rte_eal_init failed with {ret}; check hugepages / capabilities");
        }
    });
}

fn bench_send_small(c: &mut Criterion) {
    init_eal_once();
    // Obtain a small-mbuf test vehicle. Use whatever public constructor
    // dpdk-net-core exposes for bench — if none exists, add one under
    // bench-internals as a small T2.7 sub-task.
    c.bench_function("bench_send_small", |b| {
        // placeholder — replace with actual dpdk_net_send call
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        b.iter(|| {
            // Real send call goes here. If the public API shape requires
            // a real Engine (not harness), extend the harness or expose
            // a tx-only helper behind bench-internals.
            black_box(&mut h);
        });
    });
}

fn bench_send_large_chain(c: &mut Criterion) {
    init_eal_once();
    c.bench_function("bench_send_large_chain", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        b.iter(|| {
            black_box(&mut h);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets = bench_send_small, bench_send_large_chain
}

fn main() {
    init_eal_once();
    benches();
    Criterion::default().configure_from_args().final_summary();
}
```

If the `dpdk-net-sys` dep isn't already present via `dpdk-net-core`'s re-export, add it to `tools/bench-micro/Cargo.toml`:
```toml
dpdk-net-sys = { path = "../../crates/dpdk-net-sys" }
```

- [ ] **Step 3: Build**

```bash
timeout 300 cargo bench --bench send --no-run 2>&1 | tail -10
```
Expected: compile-clean. EAL symbols resolve via `dpdk-net-sys`.

- [ ] **Step 4: Short run**

```bash
timeout 180 cargo bench --bench send -- --measurement-time 5 2>&1 | tail -15
```
Expected: EAL init succeeds with `--no-huge` (or with real hugepages if mounted). Bench produces numbers. If EAL init panics with `rte_eal_init: Cannot create lock`, rerun with `sudo` or grant `CAP_IPC_LOCK` to cargo.

- [ ] **Step 5: If send bench's bench-bodies are still placeholder-only (not yet calling real `dpdk_net_send`):**

File a sub-task "T2.7b — wire real send call into bench_send_*" and complete it before proceeding to T2.8. The sub-task:
- Read `crates/dpdk-net-core/src/engine.rs` around the `send` / `tx_data_frame` methods
- Extend `EngineNoEalHarness` (under `bench-internals`) with a `send_mock` that drives `tcp_output` with a real mbuf obtained from a bench-local mempool
- Update the bench to call it
- Commit separately

- [ ] **Step 6: Commit**

```bash
git add tools/bench-micro/benches/send.rs tools/bench-micro/Cargo.toml
git commit -m "a10-perf-23.11: bench-micro/send — EAL init for real mbuf send measurement

Uses --no-huge where hugepages unavailable; rte_eal_init runs once
before criterion. Diverges from bench-micro's pure-in-process default
for these two benches only (spec §4.4 amendment)."
```

### Task 2.8: Extend bench-micro CSV schema (new metadata columns)

**Files:**
- Modify: `tools/bench-common/src/csv_row.rs` (add columns to `CsvRow`)
- Modify: `tools/bench-micro/src/bin/summarize.rs` (populate new columns)

- [ ] **Step 1: Locate `CsvRow`**

```bash
grep -n "struct CsvRow\|pub const COLUMNS" tools/bench-common/src/csv_row.rs
```

- [ ] **Step 2: Add columns**

In `tools/bench-common/src/csv_row.rs`, add fields to `CsvRow` (and the corresponding entries in `COLUMNS: &[&str]`):

```rust
pub struct CsvRow {
    // ... existing fields ...
    pub cpu_family: Option<u32>,
    pub cpu_model_name: Option<String>,
    pub dpdk_version: Option<String>,
    pub worktree_branch: Option<String>,
    pub uprof_session_id: Option<String>,
}
```

Update `COLUMNS` and the serializer / CSV writer the schema uses.

- [ ] **Step 3: Populate in summarize.rs**

In `tools/bench-micro/src/bin/summarize.rs`, populate the new fields from runtime introspection:

```rust
fn cpu_family_model() -> (Option<u32>, Option<String>) {
    // Parse /proc/cpuinfo
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let family = text.lines().find_map(|l| l.strip_prefix("cpu family\t: "))
        .and_then(|s| s.trim().parse().ok());
    let name = text.lines().find_map(|l| l.strip_prefix("model name\t: "))
        .map(|s| s.trim().to_string());
    (family, name)
}

fn dpdk_version() -> Option<String> {
    let out = std::process::Command::new("pkg-config")
        .args(["--modversion", "libdpdk"]).output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn worktree_branch() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"]).output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
```
Then fill these into each `CsvRow` during flush. `uprof_session_id` stays `None` unless the run is under a profiler — use env var `UPROF_SESSION_ID` to tag it.

- [ ] **Step 4: Verify round-trip**

```bash
timeout 120 cargo bench --bench counters -- --measurement-time 3 2>&1 | tail -5
timeout 60 cargo run -p bench-micro --bin summarize 2>&1 | tail -5
head -3 target/bench-results/bench-micro/*.csv
```
Expected: CSV header has the new columns; data rows populate `cpu_model_name` with "AMD EPYC 7R13 Processor", `dpdk_version` with "23.11.0", `worktree_branch` with "a10-perf-23.11".

- [ ] **Step 5: Commit**

```bash
git add tools/bench-common/src/csv_row.rs tools/bench-micro/src/bin/summarize.rs
git commit -m "a10-perf-23.11: extend bench CSV with host + dpdk + worktree metadata

Adds cpu_family, cpu_model_name, dpdk_version, worktree_branch,
uprof_session_id columns so cross-worktree comparisons reject
mismatched-host rows at analysis time per spec §3 / §4.4."
```

### Task 2.9: Update `STUB_TARGETS` in summarize.rs

**Files:**
- Modify: `tools/bench-micro/src/bin/summarize.rs`

- [ ] **Step 1: Remove newly-unblocked benches from the list**

Edit `STUB_TARGETS` to reflect current reality after T2.5 + T2.6 + T2.7:
```rust
const STUB_TARGETS: &[&str] = &[
    // All previously-stubbed benches were unblocked in Phase 2
    // (T2.5 poll_*, T2.6 timer_add_cancel, T2.7 send_*). List empty.
];
```
If `send_*` is kept as a partial stub per T2.7 Step 5 fallback, leave those entries in and remove only the unblocked ones.

- [ ] **Step 2: Re-run summarize to confirm no bench is tagged `feature_set = "stub"`**

```bash
timeout 30 cargo run -p bench-micro --bin summarize 2>&1 | tail -5
grep -c ',stub,' target/bench-results/bench-micro/*.csv || echo "none — OK"
```

- [ ] **Step 3: Commit**

```bash
git add tools/bench-micro/src/bin/summarize.rs
git commit -m "a10-perf-23.11: summarize — drop unblocked benches from STUB_TARGETS"
```

### Task 2.10: Cherry-pick the Phase-2 feature work into `a10-dpdk24-adopt`

The DPDK-24 worktree also needs `bench-internals` + `EngineNoEalHarness` so its baseline measurements use the same harness. Cross-pollinate.

**Files:** none modified (just cherry-picks)

- [ ] **Step 1: Collect the Phase-2 commits from Worktree 1**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git log --oneline master..HEAD | head -15
```
Expected: the sequence T2.1…T2.9 commits. Record each SHA.

- [ ] **Step 2: Cherry-pick each into Worktree 2**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-dpdk24
for sha in <list them oldest-first>; do
  git cherry-pick $sha
done
```
Expected: all clean picks (both worktrees branched from the same `671062a`; no conflict).

- [ ] **Step 3: Sanity**

```bash
timeout 300 cargo build --workspace --features bench-internals 2>&1 | tail -5
timeout 120 cargo test -p dpdk-net-core --features bench-internals --test engine_no_eal_harness 2>&1 | tail -5
```

- [ ] **Step 4: Commit — none.** Picks are their own commits.

---

## Phase 3 — Worktree 1 (`a10-perf-23.11`) optimization, per family

All tasks in this phase run in `/home/ubuntu/resd.dpdk_tcp-a10-perf`. Follow Procedure P1 from the preamble. Families proceed in priority order.

### Task 3.0: Full pre-iteration baseline + opportunity matrix

**Files:**
- Create: `docs/superpowers/reports/perf-23.11/opportunity-matrix.md`

- [ ] **Step 1: Run the full bench suite once as a cross-family baseline**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
./scripts/check-perf-host.sh
timeout 1200 cargo bench --workspace 2>&1 | tee profile/full-suite-baseline.log
timeout 60 cargo run -p bench-micro --bin summarize
```

- [ ] **Step 2: Capture TBP on a short full-suite run**

```bash
AMDuProfCLI collect --config tbp -d 60 --output profile/full-suite-baseline-tbp \
  cargo bench --workspace -- --measurement-time 15
AMDuProfCLI report --import-dir profile/full-suite-baseline-tbp \
  --report-output profile/full-suite-baseline-tbp.html
```

- [ ] **Step 3: Write the matrix**

`docs/superpowers/reports/perf-23.11/opportunity-matrix.md`:
```markdown
# A10-perf-23.11 — Opportunity Matrix (baseline)

Per §11.2 target + current measurement + estimated ns to save + expected difficulty.

| Family | §11.2 target (upper) | Current median | Gap | Top hotspot (fn, %) | Difficulty | Priority |
|---|---|---|---|---|---|---|
| poll | tens ns | <fill from runs> | <gap> | <fn, %> | <L/M/H> | 1 |
| tcp_input | 200 ns (data) | ... | ... | ... | ... | 2 |
| send | 150 ns (small) | ... | ... | ... | ... | 3 |
| flow_lookup | 40 ns (hot) | ... | ... | ... | ... | 4 |
| timer | 50 ns | ... | ... | ... | ... | 5 |
| tsc_read | 5 ns (ffi) | ... | ... | ... | ... | 6 |
| counters | 100 ns | ... | ... | ... | ... | 7 |
```

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/reports/perf-23.11/opportunity-matrix.md
git commit -m "a10-perf-23.11: cross-family baseline + opportunity matrix"
```

### Tasks 3.1–3.7: Per-family iteration cycles

Seven tasks, one per bench family, applied in priority order. Each task follows **Procedure P1**. The body of P1 is identical per family; the family-specific inputs vary. Rather than re-inline the full 9-step procedure seven times, each task below lists the inputs and a `Run P1` step.

The engineer (or subagent) dispatches each family task individually; the cycle iterations within a family are child tasks spawned during execution.

---

### Task 3.1: `poll` family — Procedure P1 iteration

**Files:**
- Modify: whichever `crates/dpdk-net-core/src/` files hypotheses point to
- Create: `docs/superpowers/reports/perf-23.11/poll-baseline.md`, `poll-iter-*.md`

**Inputs for P1:**
- Family name: `poll`
- Benches: `bench_poll_empty`, `bench_poll_idle_with_timers`
- §11.2 upper: `tens ns` — treat as 100 ns for gate purposes
- p99 gate: 200 ns (2× upper)
- Known hotspot seeds: `TimerWheel::advance` bucket-walk, `clock::now_ns` rdtsc frequency, `EventQueue::drain` empty-ring cost
- Expected cycle count: 3–6 iterations before exit

- [ ] **Step 1: Run Procedure P1 end-to-end on the `poll` family**

See preamble. Every cycle iteration produces a child task of the form "T3.1.iter-N: poll — <hypothesis>". Each child task's steps follow P1 steps 4–8.

- [ ] **Step 2: Write family summary at `docs/superpowers/reports/perf-23.11/poll-summary.md`** on exit

Enumerate retained optimizations, rejected hypotheses, final criterion numbers, final top hotspot.

- [ ] **Step 3: Commit the summary**

```bash
git add docs/superpowers/reports/perf-23.11/poll-*.md
git commit -m "a10-perf-23.11: poll — family summary (exit: <pass/hard-stop reason>)"
```

### Task 3.2: `tcp_input` family — Procedure P1 iteration

**Inputs for P1:**
- Family name: `tcp_input`
- Benches: `bench_tcp_input_data_segment`, `bench_tcp_input_ooo_segment`
- §11.2 upper: 200 ns (data), 400 ns (ooo)
- p99 gates: 400 ns (data), 800 ns (ooo)
- Known hotspot seeds: PAWS timestamp compare, SACK scoreboard scan, reassembly segment-insert, mbuf `refcnt` refcount touches, `flow_table::lookup_by_tuple` (shared with `flow_lookup` family)
- Expected cycle count: 5–10 iterations (biggest family)

- [ ] **Step 1-3: Same shape as T3.1 — run P1, write summary, commit.**

### Task 3.3: `send` family — Procedure P1 iteration

**Inputs for P1:**
- Family name: `send`
- Benches: `bench_send_small`, `bench_send_large_chain`
- §11.2 upper: 150 ns (small), 5 µs (large chain)
- p99 gates: 300 ns (small), 10 µs (large chain)
- Known hotspot seeds: mbuf chain build, TCP-header build + checksum, `rte_pktmbuf_alloc` mempool hit, TSC read for per-segment timestamp
- Notes: large-chain is memory-bandwidth-bound; PCM bandwidth data matters more than per-function cycle attribution

- [ ] **Step 1-3: Same shape as T3.1.**

### Task 3.4: `flow_lookup` family — Procedure P1 iteration

**Inputs for P1:**
- Family name: `flow_lookup`
- Benches: `bench_flow_lookup_hot`, `bench_flow_lookup_cold`
- §11.2 upper: 40 ns (hot), 200 ns (cold)
- p99 gates: 80 ns (hot), 400 ns (cold)
- Known hotspot seeds: 4-tuple hash computation, bucket chain walk, `TcpConn` cacheline layout (cold variant is the one where layout matters most)

- [ ] **Step 1-3: Same shape as T3.1.**

### Task 3.5: `timer` family — Procedure P1 iteration

**Inputs for P1:**
- Family name: `timer`
- Benches: `bench_timer_add_cancel`
- §11.2 upper: 50 ns
- p99 gate: 100 ns
- Known hotspot seeds: hashed-wheel bucket insert, `TimerId` generation, cancel-by-id lookup

- [ ] **Step 1-3: Same shape as T3.1.**

### Task 3.6: `tsc_read` family — Procedure P1 iteration

**Inputs for P1:**
- Family name: `tsc_read`
- Benches: `bench_tsc_read_ffi`, `bench_tsc_read_inline`
- §11.2 upper: 5 ns (ffi), 1 ns (inline)
- p99 gates: 10 ns (ffi), 2 ns (inline)
- Known hotspot seeds: FFI trampoline; rdtsc cost; barrier semantics (`lfence`, `mfence`)
- Notes: already near floor — polish only. Hard-stop likely after 1–2 iterations.

- [ ] **Step 1-3: Same shape as T3.1.**

### Task 3.7: `counters` family — Procedure P1 iteration

**Inputs for P1:**
- Family name: `counters`
- Benches: `bench_counters_read`
- §11.2 upper: 100 ns
- p99 gate: 200 ns
- Known hotspot seeds: atomic loads per counter group, cacheline spread across struct
- Notes: slow-path by policy (`feedback_counter_policy`); cap effort at 2 iterations max.

- [ ] **Step 1-3: Same shape as T3.1.**

### Task 3.8: Worktree 1 summary + exit gate

**Files:**
- Create: `docs/superpowers/reports/perf-23.11/summary.md`

- [ ] **Step 1: Aggregate all family results**

Write `summary.md` with:
```
# Worktree 1 (a10-perf-23.11) — summary

## Final numbers per family

| Family | §11.2 upper | Final median | Final p99 | Exit reason |
|---|---|---|---|---|
| poll | ... | ... | ... | gate met / hard stop / ... |
| ...

## Retained optimizations

- poll#1: <change description> (saved ~X ns, ref commit abc1234)
- ...

## Rejected hypotheses

- poll#3: cacheline repack of EventQueue — no measurable improvement
- ...

## Net: every family meets §11.2 upper? yes/no
## Every family's top hotspot < 5% or documented floor? yes/no
```

- [ ] **Step 2: Verify every §11.2 target beaten**

```bash
# Cross-check against the opportunity matrix
diff <(awk '/^\|/ {print $2,$4}' docs/superpowers/reports/perf-23.11/opportunity-matrix.md) \
     <(awk '/^\|/ {print $2,$4}' docs/superpowers/reports/perf-23.11/summary.md)
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/reports/perf-23.11/summary.md
git commit -m "a10-perf-23.11: worktree-1 summary — §11.2 gate $(pass|partial)"
```

---

## Phase 4 — Worktree 2 (`a10-dpdk24-adopt`) rebase + adoption

All tasks run in `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24`.

### Task 4.1: Install DPDK 24.11 LTS side-by-side

**Files:**
- No source changes; host install

- [ ] **Step 1: Download DPDK 24.11**

```bash
cd /tmp
curl -LO https://fast.dpdk.org/rel/dpdk-24.11.tar.xz
tar xJf dpdk-24.11.tar.xz
cd dpdk-24.11
```

- [ ] **Step 2: Configure + build**

```bash
CC=clang-22 CXX=clang++-22 meson setup build \
  --prefix=/usr/local/dpdk-24.11 \
  -Dplatform=native -Dtests=false -Denable_docs=false
ninja -C build
```
Expected: build completes without errors specific to our compiler toolchain.

- [ ] **Step 3: Install**

```bash
sudo ninja -C build install
```

- [ ] **Step 4: Verify side-by-side**

```bash
ls /usr/local/dpdk-24.11/lib*/pkgconfig/libdpdk.pc
PKG_CONFIG_PATH=/usr/local/dpdk-24.11/lib/x86_64-linux-gnu/pkgconfig \
  pkg-config --modversion libdpdk
```
Expected: `24.11.x` for the pinned path; existing `pkg-config --modversion libdpdk` still shows `23.11.0`.

- [ ] **Step 5: Commit — none.** Host-level install.

### Task 4.2: Worktree 2 — bump `atleast_version` + PKG_CONFIG pinning

**Files:**
- Modify: `crates/dpdk-net-sys/build.rs`
- Create: `.envrc` (worktree-local, optional) or a `scripts/use-dpdk24.sh` helper

- [ ] **Step 1: Bump `atleast_version`**

In `crates/dpdk-net-sys/build.rs`, change:
```rust
.atleast_version("23.11")
```
to:
```rust
.atleast_version("24.11")
```
Update adjacent comments and the panic message accordingly.

- [ ] **Step 2: Add a worktree-local helper script**

`scripts/use-dpdk24.sh`:
```bash
#!/usr/bin/env bash
# Source this before cargo commands in this worktree:
#   source scripts/use-dpdk24.sh
export PKG_CONFIG_PATH=/usr/local/dpdk-24.11/lib/x86_64-linux-gnu/pkgconfig:${PKG_CONFIG_PATH:-}
export LD_LIBRARY_PATH=/usr/local/dpdk-24.11/lib/x86_64-linux-gnu:${LD_LIBRARY_PATH:-}
echo "[use-dpdk24] PKG_CONFIG_PATH=$PKG_CONFIG_PATH"
pkg-config --modversion libdpdk
```

- [ ] **Step 3: `chmod +x` + source + verify**

```bash
chmod +x scripts/use-dpdk24.sh
source scripts/use-dpdk24.sh
# Expected: "24.11.x"
```

- [ ] **Step 4: Commit**

```bash
git add crates/dpdk-net-sys/build.rs scripts/use-dpdk24.sh
git commit -m "a10-dpdk24: bump atleast_version to 24.11 + PKG_CONFIG helper

Sources scripts/use-dpdk24.sh to pin the worktree to DPDK 24.11.
Reverse-pollination of this commit to a10-perf-23.11 is forbidden
(spec §7.1 rule 4: never cross the DPDK-version boundary)."
```

### Task 4.3: Bindgen regen + compile-fix loop

**Files:**
- Modify: `crates/dpdk-net-sys/build.rs` (header allowlist if new headers appeared or were removed)
- Modify: `crates/dpdk-net-core/src/*.rs` (API-drift fixes as they surface)

- [ ] **Step 1: Attempt build with fresh bindgen output**

```bash
source scripts/use-dpdk24.sh
cargo clean -p dpdk-net-sys
timeout 600 cargo build -p dpdk-net-sys 2>&1 | tee /tmp/dpdk24-sys-build.log | tail -30
```
Record the error set. Common patterns:
- `error: linking with cc failed` — missing lib; check DPDK 24.11's `.pc` file advertised libs.
- `no field 'X' on type 'rte_Y'` — renamed field.
- `undefined static Z` — removed symbol.

- [ ] **Step 2: For each error, apply the minimal fix**

Per the bindgen allowlist in `build.rs`: if new transitive headers appeared (DPDK 24.x pulls in some extras), add to the allowlist. If a struct field changed name, add an `#[cfg(feature = ...)]` compat shim OR just update the call site — since this is exploratory, prefer update over shim.

Commit each fix separately for bisection clarity:
```bash
git add <files>
git commit -m "a10-dpdk24: fix <symbol> — <24.11 rename>"
```

- [ ] **Step 3: Full workspace build**

```bash
timeout 600 cargo build --workspace --features bench-internals 2>&1 | tail -20
```
Expected: clean build.

- [ ] **Step 4: If any core API shift is non-trivial (RX/TX offload flags, rte_eth_dev_info field renames), note in the baseline-rebase report.**

### Task 4.4: Full test sweep on DPDK 24.11

**Files:** none

- [ ] **Step 1: Run all tests**

```bash
timeout 900 cargo test --workspace --features bench-internals -- --skip ignored 2>&1 | tail -40
```

- [ ] **Step 2: Hard stop on any regression**

If a test that previously passed on 23.11 now fails, investigate and fix before proceeding to baseline. Root-cause — do not mark `#[ignore]` to bypass.

- [ ] **Step 3: Run the harness test**

```bash
timeout 120 cargo test -p dpdk-net-core --features bench-internals --test engine_no_eal_harness 2>&1 | tail -5
```

- [ ] **Step 4: If any DPDK-API drift broke something, commit the fix(es) per-fix. Otherwise nothing to commit here.**

### Task 4.5: Post-rebase bench-micro baseline

**Files:**
- Create: `docs/superpowers/reports/perf-dpdk24/baseline-rebase.md`

- [ ] **Step 1: Check-perf-host + bench**

```bash
source scripts/use-dpdk24.sh
./scripts/check-perf-host.sh
timeout 1200 cargo bench --workspace 2>&1 | tee profile/rebase-baseline.log
timeout 60 cargo run -p bench-micro --bin summarize
```

- [ ] **Step 2: uProf capture on full suite**

Run Procedure P2 for each family. Place outputs under `profile/<family>-rebase-baseline/`.

- [ ] **Step 3: Write the baseline-rebase report**

`docs/superpowers/reports/perf-dpdk24/baseline-rebase.md`:
```markdown
# DPDK 24.11 rebase — bench-micro baseline vs 23.11

## Side-by-side

| Family | 23.11 median | 24.11 median | Δ | 23.11 p99 | 24.11 p99 | Δ p99 | Notes |
|---|---|---|---|---|---|---|---|
| poll | ... | ... | ... | ... | ... | ... |  |
| ...

## ENA TX regression check

`bench_send_small` + `bench_send_large_chain` ran 5× (not 3×):

| Run | 23.11 | 24.11 | Δ |
|---|---|---|---|
| 1 | ... | ... | ... |
| ...

Regression > 10%? yes/no. If yes, T4.8 (`rte_ptr_compress`) is promoted to the next task.

## Compile-fix summary

- <commit sha> <short message>
- ...
```

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/reports/perf-dpdk24/baseline-rebase.md
git commit -m "a10-dpdk24: post-rebase bench-micro baseline vs 23.11"
```

### Task 4.6: ENA TX regression decision branch

**Files:**
- Update: `docs/superpowers/reports/perf-dpdk24/baseline-rebase.md` if promoting `rte_ptr_compress`

- [ ] **Step 1: Read §6.1.5 decision rule from spec**

If `bench_send_small` or `bench_send_large_chain` median regressed > 10% vs 23.11 in T4.5 → promote task 4.8 ahead of 4.7. Otherwise run 4.7 → 4.8 → 4.9 → 4.10 in order.

- [ ] **Step 2: Document decision**

Append a `## Ordering decision` section to `baseline-rebase.md`.

- [ ] **Step 3: Commit if updated**

### Task 4.7: Adopt `rte_lcore_var` (24.11)

**Files:**
- Modify: `crates/dpdk-net-core/src/engine.rs` (replace `__rte_cache_aligned` + `RTE_CACHE_GUARD` patterns on per-core state structs)
- Modify: related per-lcore state modules if any

- [ ] **Step 1: Inventory candidate sites**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-dpdk24
grep -rn "__rte_cache_aligned\|RTE_CACHE_GUARD\|#\[repr(align" crates/dpdk-net-core/src/ | head -20
```

- [ ] **Step 2: For each candidate, decide port feasibility**

Read the DPDK 24.11 doc on `rte_lcore_var`: https://doc.dpdk.org/api-24.11/rte__lcore__var_8h.html
For the top candidate (e.g. per-engine stats arrays that are one-per-lcore today), port to `rte_lcore_var`-backed storage.

- [ ] **Step 3: Implement the port**

```rust
// Before (illustrative):
#[repr(align(64))]
struct PerLcoreStats {
    rx_bursts: u64,
    // pad to cacheline
}

// After (via dpdk-net-sys binding to rte_lcore_var):
use dpdk_net_sys::{rte_lcore_var_alloc_size, rte_lcore_var_value_addr};
// handle to a per-lcore region; value is a `PerLcoreStats` without
// manual padding — rte_lcore_var does the alignment.
```

This requires exposing `rte_lcore_var_*` through the sys binding; if the bindgen allowlist doesn't yet include them, add the header + allowlist entry.

- [ ] **Step 4: Run affected families 3× each**

```bash
source scripts/use-dpdk24.sh
./scripts/check-perf-host.sh
timeout 300 cargo bench --bench poll -- --baseline base-pre-opt --measurement-time 10
timeout 300 cargo bench --bench counters -- --baseline base-pre-opt --measurement-time 10
timeout 300 cargo bench --bench flow_lookup -- --baseline base-pre-opt --measurement-time 10
```
Run P2 to capture uProf on the affected families.

- [ ] **Step 5: Write the adoption report**

`docs/superpowers/reports/perf-dpdk24/adopt-rte_lcore_var.md`:
```markdown
# Adopt rte_lcore_var (DPDK 24.11)

## Sites ported
- crates/dpdk-net-core/src/engine.rs#<line>: PerLcoreStats

## A/B results

| Bench | Pre-port median | Post-port median | Δ | Pre p99 | Post p99 | Δ p99 |
|---|---|---|---|---|---|---|
| bench_poll_empty | ... | ... | ... | ... | ... | ... |
| bench_counters_read | ... | ... | ... | ... | ... | ... |
| bench_flow_lookup_hot | ... | ... | ... | ... | ... | ... |

## Decision

Go/no-go per §6.2 rule ("p50 improves on ≥ 1 family and p99 does not regress elsewhere").

Verdict: <adopt | revert>

## Revert diff (if not adopted)
<git diff of the revert>
```

- [ ] **Step 6: Commit** (adoption or revert per decision)

```bash
git add <files>
git commit -m "a10-dpdk24: adopt rte_lcore_var on <sites>  (<adopt|revert>)"
git add docs/superpowers/reports/perf-dpdk24/adopt-rte_lcore_var.md
git commit -m "a10-dpdk24: adopt-rte_lcore_var.md — A/B report + verdict"
```

### Task 4.8: Adopt `rte_ptr_compress` (24.07)

**Files:**
- Modify: `crates/dpdk-net-core/src/engine.rs` or `tcp_output.rs` (TX mbuf burst pointer arrays); `engine::poll_once` RX burst arrays

- [ ] **Step 1: Inventory candidate sites**

```bash
grep -rn "mbuf_burst\|rte_pktmbuf\|\*mut rte_mbuf" crates/dpdk-net-core/src/ | head -20
```
Identify pointer-array sites: the TX burst send array (`rte_eth_tx_burst` call inputs), the RX burst receive array, any mbuf-chain traversal that iterates over an array of `*mut rte_mbuf`.

- [ ] **Step 2: Read `rte_ptr_compress` docs**

https://doc.dpdk.org/api-24.11/rte__ptr__compress_8h.html

- [ ] **Step 3: Port the top site**

Replace a raw `[*mut rte_mbuf; 32]` burst array with compressed storage. Pay attention to whether the compressed form is worth it for array size N — micro-benchmark at N=32 may regress if compress overhead exceeds bandwidth savings.

- [ ] **Step 4: Run affected families 3×**

```bash
timeout 300 cargo bench --bench send -- --baseline base-pre-opt --measurement-time 10
timeout 300 cargo bench --bench poll -- --baseline base-pre-opt --measurement-time 10
```

- [ ] **Step 5: Write report + decide + commit**

`docs/superpowers/reports/perf-dpdk24/adopt-rte_ptr_compress.md` — same shape as T4.7.

### Task 4.9: Adopt `rte_bit_atomic_*` (24.11)

**Files:**
- Modify: `crates/dpdk-net-core/src/tcp_conn.rs`, `tcp_events.rs` (bit-flag sites)

- [ ] **Step 1: Inventory candidate sites**

```bash
grep -rn "AtomicU32\|AtomicU64\|\.fetch_or\|\.fetch_and" crates/dpdk-net-core/src/ | head -20
```
Pick sites that use atomic ops on bit masks (not counter arithmetic) — those are candidates.

- [ ] **Step 2: If no suitable sites found, skip cleanly**

Write `adopt-rte_bit_atomic.md` saying "no matching sites — all atomic ops are word-level arithmetic; skip cleanly per §6.2 rule". Commit.

- [ ] **Step 3: Otherwise, port the top site**

Replace `AtomicU32::fetch_or(mask, Ordering::Relaxed)` with `rte_bit_atomic_set` (or equivalent — read docs: https://doc.dpdk.org/api-24.11/rte__bitops_8h.html).

- [ ] **Step 4: Run affected families 3×; write report; decide; commit.**

### Task 4.10: ENA TX logger rework measurement (24.07)

**Files:**
- Create: `docs/superpowers/reports/perf-dpdk24/adopt-ena-tx-logger.md`

- [ ] **Step 1: Confirm logger is active**

```bash
# Check that the ENA PMD's TX path uses the reworked logger. Inspection:
grep -rn "tx_logger\|RTE_ETHDEV_LOG" /usr/local/dpdk-24.11/include/ 2>/dev/null | head -5
# No definitive single grep — this is an observability task, not a code change.
```

- [ ] **Step 2: Re-measure `bench_send_*` 5×**

```bash
timeout 600 cargo bench --bench send -- --baseline base-pre-opt --measurement-time 15
```
(Repeat 5 invocations for stability.)

- [ ] **Step 3: Write the report**

```markdown
# ENA TX logger rework (DPDK 24.07) — measurement-only

No code change. Verifies rework is active and records its effect on
bench_send_* vs the 23.11 baseline.

## Pre-rebase (23.11) vs post-rebase (24.11)

| Bench | 23.11 median | 24.11 median | Δ |
|---|---|---|---|
| bench_send_small | ... | ... | ... |
| bench_send_large_chain | ... | ... | ... |

## Notes

- If §6.1.5 flagged an ENA TX regression at rebase, does the logger
  rework mitigate it? [yes/no/partial]
- If §6.1.5 showed no regression, this task confirms the logger rework
  did not introduce one.
```

- [ ] **Step 4: Commit**

### Task 4.11: Document deferrals (PM QoS + `rte_thash_gen_key`)

**Files:**
- Create: `docs/superpowers/reports/perf-dpdk24/deferrals.md`

- [ ] **Step 1: Write the doc**

```markdown
# DPDK 24.11 deferrals — not adopted in A10-perf

## rte_power per-CPU PM QoS (24.11)

N/A for this workload. Engine is a run-to-completion busy-poll design
(spec §2, `project_context`). No wake-up path exists to optimize.
File for Stage 3 WAN hardening if idle-aware latency becomes relevant.

## rte_thash_gen_key (24.11)

Deferred to Stage 2. Current Stage 1 deployment is single-queue; no
RSS imbalance to cure. When multi-queue lands, revisit.

## Intel E830 / ice driver (24.07)

N/A — AWS ENA target.

## Event pre-scheduling (24.11)

N/A — no eventdev in the engine.
```

- [ ] **Step 2: Commit**

```bash
git add docs/superpowers/reports/perf-dpdk24/deferrals.md
git commit -m "a10-dpdk24: document 24.11 API deferrals"
```

### Task 4.12: Worktree 2 exit gate

- [ ] **Step 1: Write `docs/superpowers/reports/perf-dpdk24/summary.md`**

Aggregate all Phase-4 reports. Decision matrix row per spec §7.3. Net recommendation: stay on 23.11 or promote 24.11.

- [ ] **Step 2: Commit**

---

## Phase 5 — Port-forward + weekly syncs

### Task 5.1 … 5.N: Weekly sync (as scheduled)

Procedure P3. One task per sync.

**Files (each iteration):**
- Create: `docs/superpowers/reports/perf-sync/sync-YYYY-MM-DD.md` (on master)

- [ ] **Step 1: Run P3 from the preamble**

- [ ] **Step 2: Cross-pollinate any API-agnostic wins (never cross `dpdk-net-sys/build.rs`)**

- [ ] **Step 3: Commit the sync report on master**

```bash
cd /home/ubuntu/resd.dpdk_tcp
git add docs/superpowers/reports/perf-sync/sync-*.md
git commit -m "perf-sync: $(date -u +%Y-%m-%d) weekly sync"
```

### Task 5.N+1 … 5.M: Port each Worktree-1 win forward to Worktree 2

One task per Worktree-1 win retained after Phase 3.

**Files (each port):**
- Modify: same file(s) as in the Worktree-1 commit, adapted to 24.11 shape if necessary
- Create: `docs/superpowers/reports/perf-dpdk24/port-forward-<change>.md`

- [ ] **Step 1: Identify the Worktree-1 commit**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-perf
git log --oneline master..HEAD | grep -v "^[0-9a-f]* a10-perf-23.11: [a-z_]* baseline\|^[0-9a-f]* a10-perf-23.11: [a-z_]* summary"
```
Filter to optimization commits (not baselines / summaries).

- [ ] **Step 2: Attempt `git cherry-pick` onto Worktree 2**

```bash
cd /home/ubuntu/resd.dpdk_tcp-a10-dpdk24
git cherry-pick <sha>
```

- [ ] **Step 3: If conflict (24.11 API shape differs), resolve by re-implementing the optimization against 24.11**

Keep the commit message, annotate with `[re-implemented for 24.11]`.

- [ ] **Step 4: Run affected bench families 3×; verify no regression**

If ported change regresses on 24.11, `git revert` and note in the port-forward report.

- [ ] **Step 5: Write the port-forward report**

`docs/superpowers/reports/perf-dpdk24/port-forward-<change-slug>.md`:
```markdown
# Port forward: <23.11 commit subject> (<sha>) → 24.11

Original commit on a10-perf-23.11: <sha> <subject>

## Cherry-pick path
- [clean cherry-pick | re-implemented due to <API drift>]

## A/B results on 24.11

| Bench | Pre-port | Post-port | Δ |
|---|---|---|---|

## Decision: keep / revert / adjust
```

- [ ] **Step 6: Commit the port-forward report**

---

## Phase 6 — End-of-effort reviews + integration

### Task 6.1: Worktree 1 — spec-compliance review (subagent)

**Files:** none modified (review-only)

- [ ] **Step 1: Dispatch subagent**

Using `feedback_subagent_model`-approved opus 4.7:

```
Agent({
  description: "Spec-compliance review — a10-perf-23.11",
  subagent_type: "general-purpose",
  model: "opus",
  prompt: "You are reviewing the full diff of branch `a10-perf-23.11` in the worktree at /home/ubuntu/resd.dpdk_tcp-a10-perf against its spec at docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md.

  Check for: (a) violations of `feedback_counter_policy` (compile-time gate + batched + justified); (b) public API/ABI surface additions beyond the approved `bench-internals` feature; (c) `feedback_trading_latency_defaults` / `feedback_performance_first_flow_control` violations; (d) §11.2 target regressions — every family must show criterion median ≤ upper bound and p99 ≤ 2× upper bound in its summary; (e) hot-path additions that don't honor §8 observability / §10 scoping rules; (f) missing family-summary reports for any family claimed complete; (g) missing uProf evidence for any claimed hypothesis pass.

  Output: a single markdown file at docs/superpowers/reports/perf-23.11/review-spec-compliance.md with verdict (pass | pass-with-caveats | block) + per-commit table + per-finding table.

  Full diff: `cd /home/ubuntu/resd.dpdk_tcp-a10-perf && git log --reverse --patch master..HEAD`

  Do NOT edit code. Review only."
})
```

- [ ] **Step 2: Wait for subagent to return; read the review**

- [ ] **Step 3: If any `block` finding, open a fix task; resolve; rerun review. Else proceed.**

### Task 6.2: Worktree 1 — code-quality review (subagent)

- [ ] **Step 1: Dispatch**

```
Agent({
  description: "Code-quality review — a10-perf-23.11",
  subagent_type: "superpowers:code-reviewer",
  model: "opus",
  prompt: "Review branch `a10-perf-23.11` in /home/ubuntu/resd.dpdk_tcp-a10-perf against standard code-quality criteria: style, dead code, missing tests, hot-path regressions, comment hygiene, error-handling drift. Spec is at docs/superpowers/specs/2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md.

  Output: docs/superpowers/reports/perf-23.11/review-code-quality.md with verdict + per-finding table.

  Do NOT edit code."
})
```

- [ ] **Step 2: Wait + read**

- [ ] **Step 3: Fix blocks; rerun.**

### Task 6.3 + 6.4: Worktree 2 — same two reviews

Mirror of T6.1 + T6.2 but against `/home/ubuntu/resd.dpdk_tcp-a10-dpdk24`. Outputs land under `docs/superpowers/reports/perf-dpdk24/review-*.md`.

### Task 6.5: Cherry-pick accepted commits onto an integration branch

**Files:** none modified per-se; creates a new branch on master

- [ ] **Step 1: Create integration branch**

```bash
cd /home/ubuntu/resd.dpdk_tcp
git checkout -b integration/a10-perf-2026-04 master
```

- [ ] **Step 2: Cherry-pick from Worktree 1 (excluded: baseline commits, summaries, rejected hypotheses)**

```bash
# Review table from T6.1 review output; cherry-pick only green-lit commits.
for sha in <list>; do
  git cherry-pick $sha
done
```

- [ ] **Step 3: Cherry-pick from Worktree 2 per T6.3 review; include the `bench-internals` parent feature if it wasn't already included from Worktree 1 (it usually is, since T2 ran on Worktree 1 first)**

- [ ] **Step 4: Re-run full test + bench suite on integration branch**

```bash
timeout 900 cargo test --workspace --features bench-internals -- --skip ignored
timeout 600 cargo bench --workspace --no-run
```

- [ ] **Step 5: If green, merge to master**

```bash
git checkout master
git merge --ff-only integration/a10-perf-2026-04
```
If the fast-forward merge doesn't apply cleanly, use a merge commit.

- [ ] **Step 6: Push when user approves explicitly. Do NOT push automatically — push requires user confirmation per auto-mode guidance.**

### Task 6.6: Write `perf-a10-postphase.md` cross-worktree summary

**Files:**
- Create: `docs/superpowers/reports/perf-a10-postphase.md`

- [ ] **Step 1: Write the summary**

Consolidate both worktree summaries + the port-forward outcome + the 24.11 pinning recommendation. Template:

```markdown
# A10-perf postphase summary

Period: 2026-04-23 → <end-date>
Worktrees: a10-perf-23.11, a10-dpdk24-adopt (both branched from 671062a)

## Per-family §11.2 gate — final

| Family | 23.11 final | 24.11 final | §11.2 upper | Gate? |
|---|---|---|---|---|

## Retained optimizations

<list>

## Rejected hypotheses

<list>

## DPDK 24.11 recommendation

- Rebase-only delta: ...
- Per-API adoption deltas: ...
- Port-forward deltas: ...
- **Verdict:** [stay on 23.11 | promote 24.11 to pinned target]
- **Action:** if promote, follow-on commits: amend `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md` §8 DPDK-version clause, amend `stage1-phase-roadmap.md`, update `project_context.md` memory.

## Follow-ons filed

- <bug / enhancement / deferred>
```

- [ ] **Step 2: Commit on master**

```bash
git add docs/superpowers/reports/perf-a10-postphase.md
git commit -m "perf-a10-postphase: cross-worktree summary + 24.11 recommendation"
```

### Task 6.7: Update roadmap + memory (if 24.11 promoted)

**Files (conditional):**
- Modify: `docs/superpowers/plans/stage1-phase-roadmap.md`
- Modify: `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md` §8 (DPDK pin)
- Update: `/home/ubuntu/.claude/projects/-home-ubuntu-resd-dpdk-tcp/memory/project_context.md`

- [ ] **Step 1 (conditional): If verdict = "stay on 23.11":**

Only the roadmap needs a one-line amendment:
```
A10 complete → A10-perf follow-on: optimized microbench families on 23.11; DPDK 24 rebase evaluated and deferred (see docs/superpowers/reports/perf-a10-postphase.md).
```
Commit.

- [ ] **Step 2 (conditional): If verdict = "promote 24.11":**

- Bump spec §8 "DPDK LTS 23.11 target" → "DPDK LTS 24.11 target"
- Bump memory `project_context.md` analogous line
- Add a roadmap note with the promotion + rationale

Commit each touchpoint separately:
```bash
git commit -m "spec §8: promote DPDK LTS target 23.11 → 24.11 per A10-perf measurement"
git commit -m "memory: project_context — DPDK pin bump to 24.11"
git commit -m "roadmap: A10-perf complete — DPDK 24.11 promoted to pinned target"
```

- [ ] **Step 3: Also update `crates/dpdk-net-sys/build.rs` on master** (bump `atleast_version`), which is the sys-level encoding of the pin.

### Task 6.8: Cleanup — close out research worktrees

- [ ] **Step 1: Announce**

Ask user if they want to:
- Keep worktrees (research reference)
- Delete worktrees + branches (clean slate, branches stay reachable via reflog for a while)

- [ ] **Step 2: If user confirms delete:**

```bash
cd /home/ubuntu/resd.dpdk_tcp
git worktree remove /home/ubuntu/resd.dpdk_tcp-a10-perf
git worktree remove /home/ubuntu/resd.dpdk_tcp-a10-dpdk24
# Branches stay by default. If user wants them gone:
git branch -D a10-perf-23.11 a10-dpdk24-adopt
```

- [ ] **Step 3: No commit needed for cleanup.**

---

## Self-Review (executed inline at plan-save time)

**1. Spec coverage:**
- Spec §1 D1 (goal = bench-micro + uProf): covered by Phase 3 family tasks ✓
- Spec §1 D2 (exploratory landing + end-of-effort review): covered by Phase 6 T6.1–T6.4 ✓
- Spec §1 D3 (bench-internals feature): covered by Phase 2 T2.1–T2.4 ✓
- Spec §1 D4 (all 7 families): covered by T3.1–T3.7 ✓
- Spec §1 D5 (§11.2 exit gate): encoded in Procedure P1 exit condition ✓
- Spec §1 D6 (DPDK 24 parallel + 3 phases): covered by Phase 4 + Phase 5 port-forward ✓
- Spec §1 post-brainstorm (hot-path counters allowed if justified): covered by Procedure P1 step 5 notes + spec-compliance review prompt (T6.1) ✓
- Spec §2.1 in-scope items: ✓ (bench-internals, check-perf-host.sh, CSV schema extension, per-worktree reports, reviews, summary)
- Spec §3 worktree layout: covered by T1.1 + T1.2 ✓
- Spec §4 tooling: T0.1 (uProf), T0.2 (check-perf-host), T2.1–T2.4 (feature + harness), T2.8 (CSV extension) ✓
- Spec §5 Worktree 1 cycle: Procedure P1 + T3.1–T3.8 ✓
- Spec §6 Worktree 2 phases: T4.1–T4.12 covers Phase 1 rebase, Phase 2 per-API adoption, Phase 3 port-forward (which sits under Phase 5 in the plan) ✓
- Spec §7 sync/review/artifacts: T5.1 sync, T6.1–T6.4 reviews, T6.6 summary, T7.4 artifact layout reflected in plan's file-structure ✓
- Spec §8 risks: mitigations encoded in procedures (P1 step 6-9, P3 metadata-drift check); risks themselves don't need dedicated tasks — they're the reason certain steps exist ✓
- Spec §10 follow-ons: reflected in plan via T6.7 Step 3 (if promote) + plan's "Untouched" file-structure section ✓

**2. Placeholder scan:** No "TBD" / "TODO" / "implement later" / "similar to Task N" (without repeating). The seven family tasks T3.1–T3.7 reference Procedure P1 rather than inlining — this is legitimate because P1 is fully written once in the preamble; readers don't need to look at another TASK to understand what P1 does.

**3. Type consistency:**
- `EngineNoEalHarness` method names match between T2.3 (defined), T2.5 (used in bench_poll_*), T2.6 (used in bench_timer_*) ✓
- `CsvRow` new columns consistent between T2.8 definition and T2.8 summarize.rs population ✓
- `rte_lcore_var` / `rte_ptr_compress` / `rte_bit_atomic_*` names consistent across T4.7/T4.8/T4.9 and Phase-6 reports ✓
- One risk: T2.5 references `dpdk_net_core::engine::test_support::EngineNoEalHarness` via full path AND T2.4 uses a shorter re-export path — both forms are valid if T2.3 Step 4 (lib.rs re-export) is completed ✓

Plan looks good; save.

---

**End of plan.**
