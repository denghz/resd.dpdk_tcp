# Claude Code — Project Instructions

## CRITICAL: Working Directory

**ALL work for this project MUST be done in `/home/ubuntu/resd.dpdk_tcp-a10-perf`.**

- This is the git worktree for the `a10-perf-23.11` branch.
- **NEVER commit to `/home/ubuntu/resd.dpdk_tcp` (master).** Master is for TCP stack core changes only — it is NOT the bench workspace.
- Every file read, edit, write, build, and bench run must use absolute paths under `/home/ubuntu/resd.dpdk_tcp-a10-perf/`.
- The shell CWD resets to master between commands — always use absolute paths, never rely on CWD.

## Branch / Repo Layout

| Path | Branch | Purpose |
|------|--------|---------|
| `/home/ubuntu/resd.dpdk_tcp` | `master` | TCP stack core |
| `/home/ubuntu/resd.dpdk_tcp-a10-perf` | `a10-perf-23.11` | Perf-A10 benchmarks — **THIS is where all work happens** |

## Bench Tooling

- Binary: built with `cargo build --release --features fstack` from `/home/ubuntu/resd.dpdk_tcp-a10-perf`
- Nightly script: `/home/ubuntu/resd.dpdk_tcp-a10-perf/scripts/bench-nightly.sh`
- Reports go to: `/home/ubuntu/resd.dpdk_tcp-a10-perf/docs/bench-reports/`

## Build Toolchain

- Rust: latest stable via rustup
- CC/CXX: clang-22 from llvm.org + libstdc++
- DPDK 23.11 at `/usr/local/dpdk-23.11`
- F-Stack at `/opt/f-stack`
- mTCP driver at `/opt/mtcp-peer/mtcp-driver`

## Stack Port Assignments (bench-pair)

| Stack | Port |
|-------|------|
| dpdk_net | 10001 |
| linux | 10002 |
| fstack | 10001 (same dpdk echo-server) |
| mTCP | 10001 |
