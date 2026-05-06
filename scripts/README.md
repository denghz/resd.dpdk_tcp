# CI scripts — Jenkins wiring

This directory holds the CI entry points that Jenkins pipelines call
stage-by-stage. No `Jenkinsfile` is vendored in this repo; the Jenkins
pipeline definition lives in the pipeline tooling alongside other
services. This README documents the contract between Jenkins and these
scripts.

Every `scripts/ci-*.sh` orchestrator:

- starts with `#!/usr/bin/env bash` and `set -euo pipefail`,
- `cd`s to the repo root so it can be invoked from any working directory,
- prints `=== <name>: PASS ===` (or `<name>: PASS`) on success,
- exits non-zero on the first failure.

## 1. Agent requirements

Per-merge pipelines expect the Jenkins agent to provide:

- **Passwordless `sudo`** for the Jenkins user.
  - Required by `ci-install-deps.sh` (`apt-get install`).
  - Required by the two TAP stages (`hardening-cpp-sanitizers.sh`,
    `hardening-no-alloc.sh`), both of which export
    `DPDK_NET_TEST_TAP=1` and invoke DPDK under `sudo -E` to create TAP
    devices.
- **`CAP_NET_ADMIN`** (implied by root/sudo) so DPDK can create TAP devices.
- **Standard DPDK host prereqs** (`libdpdk-dev`, `pkg-config`, `clang`,
  `libclang-dev`, `libnuma-dev`, etc.) — installed by
  `ci-install-deps.sh` on first run and treated as a no-op thereafter.
- **Concurrency guard for TAP stages.** TAP interface names are
  host-global; two concurrent TAP-stage builds on the same agent would
  collide. Jenkins must either pin the two TAP stages
  (`hardening-cpp-sanitizers.sh`, `hardening-no-alloc.sh`) to a dedicated
  agent label with `disableConcurrentBuilds`, or wrap them in a
  `lock(resource: 'dpdk-tap')` block. The 13 non-TAP stages can
  parallelize freely across any number of agents — each script `cd`s to
  the repo root and owns its own build artifacts under `target/`.

The one-time agent bootstrap is:

```sh
bash scripts/ci-install-deps.sh
```

Safe to re-run; `apt-get install` is a no-op once packages are present,
`rustup toolchain install` is a no-op if the toolchain is already there,
and `cargo install cargo-fuzz` is guarded by `command -v`.

## 2. Per-merge stages (16)

Run in this order — cheapest-fails-first, with TAP-privileged stages
last so the first 14 stages can fan out across a fleet of
non-privileged agents.

| # | Stage | Script | Notes |
|---|---|---|---|
| 1 | Install deps | `scripts/ci-install-deps.sh` | idempotent; apt + rustup (stable+nightly) + miri + cargo-fuzz + pip scapy |
| 2 | Workspace-feature unification gate | `scripts/check-workspace-features.sh` | Pattern P1 / pressure-test plan T0 — fails if test/bench-only `dpdk-net-core` features unify into the production resolution |
| 3 | C-ABI dead-field audit | `scripts/check-cabi-fields.sh` | Pattern P5 / cross-phase-fixes Task A6 — fails if `dpdk_net_engine_config_t` carries a field that has no reader and no `// REMOVE-BY: A<N>` marker |
| 4 | Header drift (cbindgen) | `scripts/check-header.sh` | fails if `include/dpdk_net.h` ≠ cbindgen output |
| 5 | Fault-injector compile | `scripts/ci-fault-injector-compile.sh` | `cargo check -p dpdk-net-core --features fault-injector` |
| 6 | Panic firewall | `scripts/hardening-panic-firewall.sh` | SIGABRT firewall test (`--features test-panic-entry`) |
| 7 | Workspace unit + doc tests | `scripts/ci-unit-tests.sh` | per-package `cargo test -p <crate>` over all members + workspace-wide `cargo test --workspace --doc` |
| 8 | Feature matrix (8 builds) | `scripts/ci-feature-matrix.sh` | `dpdk-net-core` × 8 feature configs per spec §13 |
| 9 | Miri (pure-compute UB) | `scripts/hardening-miri.sh` | `cargo +nightly miri test` over `--lib` |
| 10 | Counter / obs / knob coverage | `scripts/ci-counter-coverage.sh` | static ×2 + dynamic + obs smoke + knob-coverage |
| 11 | Tcpreq probes (M4) | `scripts/ci-tcpreq-gate.sh` | `timeout 300 cargo test -p tcpreq-runner -- --test-threads=1` |
| 12 | Fuzz smoke (per-merge) | `scripts/fuzz-smoke.sh` | TIME=30; 7 libFuzzer targets, ~3.5 min total |
| 13 | Scapy corpus replay | `scripts/ci-scapy-replay.sh` | regenerates corpus + feeds through `scapy-fuzz-runner` |
| 14 | Packetdrill corpus (3 sets) | `scripts/ci-packetdrill-corpus.sh` | shim build + server smoke + ligurio + shivansh + google |
| 15 | **C++ sanitizers (sudo+TAP)** | `scripts/hardening-cpp-sanitizers.sh` | ASan+UBSan+LSan; full runtime exercise on TAP peer |
| 16 | **No-alloc hot-path (sudo+TAP)** | `scripts/hardening-no-alloc.sh` | CountingAllocator audit; `--features bench-alloc-audit` |

The two TAP stages (15 + 16) are the only ones requiring `sudo` at
runtime and the only ones contending for the host-global TAP namespace.

### Single-node aggregator

For a Jenkins node that runs everything on a single agent serially (or
for developers reproducing CI locally):

```sh
bash scripts/ci-all.sh
```

`ci-all.sh` runs all 16 stages in the order above, fast-fails at
startup if passwordless `sudo` is unavailable, and prints
`=== ci-all: ALL PASSED ===` on success. Developers iterating locally
can skip the apt/pip/rustup bootstrap with:

```sh
CI_ALL_SKIP_INSTALL=1 bash scripts/ci-all.sh
```

### Example Jenkinsfile fragment

```groovy
stage('per-merge (non-TAP, parallelizable)') {
    steps {
        sh 'bash scripts/check-workspace-features.sh'
        sh 'bash scripts/check-cabi-fields.sh'
        sh 'bash scripts/check-header.sh'
        sh 'bash scripts/ci-fault-injector-compile.sh'
        sh 'bash scripts/hardening-panic-firewall.sh'
        sh 'bash scripts/ci-unit-tests.sh'
        sh 'bash scripts/ci-feature-matrix.sh'
        sh 'bash scripts/hardening-miri.sh'
        sh 'bash scripts/ci-counter-coverage.sh'
        sh 'bash scripts/ci-tcpreq-gate.sh'
        sh 'bash scripts/fuzz-smoke.sh'
        sh 'bash scripts/ci-scapy-replay.sh'
        sh 'bash scripts/ci-packetdrill-corpus.sh'
    }
}

stage('per-merge (TAP, privileged agent)') {
    agent { label 'dpdk-tap' }
    options { lock(resource: 'dpdk-tap') }
    environment { DPDK_NET_TEST_TAP = '1' }
    steps {
        sh 'sudo -E bash scripts/hardening-cpp-sanitizers.sh'
        sh 'sudo -E bash scripts/hardening-no-alloc.sh'
    }
}
```

Any of the 13 non-TAP working stages (2–14) may be split across
parallel Jenkins stages on separate agents — each script is
self-contained (it `cd`s to the repo root and builds into its own
`target/` tree).

## 3. Nightly jobs (separate schedule)

Not part of per-merge; scheduled by Jenkins on its own cadence.

| Job | Script | Notes |
|---|---|---|
| Nightly fuzz (10 min/target × 7) | `scripts/ci-fuzz-nightly.sh` | TIME=600; shares fuzz-smoke.sh's crash-detection loop |

## 4. Out-of-band (manual / end-of-phase)

Kept in-tree for reproducibility, **not** invoked from Jenkins:

| Script | Purpose |
|---|---|
| `scripts/fuzz-long-run.sh` | 72h stage-cut fuzz run; dedicated box. |
| `scripts/a7-perf-baseline.sh` | Perf baseline stub; opt-in via `A7_RUN_PERF=1`. |
| `scripts/audit-panics.sh` | Report-only panic inventory. |
| `scripts/fetch-rfcs.sh` | Doc-set utility to refresh vendored RFCs. |
| `scripts/hardening-all.sh` | Manual hardening aggregator used at end-of-phase gate review; not the Jenkins CI path. |
