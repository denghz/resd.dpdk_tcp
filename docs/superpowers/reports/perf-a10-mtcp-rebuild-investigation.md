# mTCP comparator rebuild investigation — 2026-04-29

**Status:** **BLOCKED** — mTCP cannot build against the AMI's DPDK 23.11 without
porting work that exceeds the spec R2 escalation budget (>50 lines of upstream
patches). The "kernel 6.17 / gcc 13" comment in the AMI YAML was wrong on both
counts; the real blocker is mTCP's DPDK API surface vs. modern DPDK.

## Bottom line for the user

1. The AMI is **Ubuntu 24.04 + kernel 6.8.0-aws + clang-22 + DPDK 23.11**, not
   "kernel 6.17 / gcc 13" as the deferred component comment claimed. The YAML
   text is misleading and we should fix the comment regardless of disposition.
2. mTCP's last commit ([`0463aad5`][mtcp-head], 2021) targets **DPDK
   18.05/19.08**, uses the legacy DPDK Make build (`config/defconfig_*` +
   `tools/setup.sh`), and has **never been ported to DPDK ≥ 20.11** in upstream.
3. Compiling just one mTCP source file (`mtcp/src/dpdk_module.c`) against the
   system DPDK 23.11 surfaces **18+ distinct compile errors** in the first
   ~150 lines (struct shape changes, macro renames, removed constants).
4. mTCP itself is **dormant** — open issue ["Has the mTCP development
   stopped?"][mtcp-337] (Jul 2024) sits unanswered.
5. Spec §11 R2 escalation gate (>50 lines of patches → drop bench-vs-mtcp from
   A10) is **triggered**. The honest call is to keep the Linux comparator we
   already have and remove the "stub points to upcoming AMI" framing from the
   mTCP arm.

[mtcp-head]: https://github.com/mtcp-stack/mtcp/commit/0463aad5
[mtcp-337]: https://github.com/mtcp-stack/mtcp/issues/337

## Phase 1 — toolchain investigation (measured)

### What the AMI actually ships

From `lib/image_builder/_components/01-install-llvm-toolchain.yaml` +
`02-install-dpdk-23-11.yaml` + `recipes/bench_host.py`:

| Knob | Value | Source |
|---|---|---|
| Base | Ubuntu 24.04 (noble) | `bench_host_recipe.base_ami_ssm_param` |
| Kernel | `linux-image-aws-lts-24.04` (6.8.0-1052-aws as of bake) | `01-install-llvm-toolchain.yaml:24` |
| Default `cc` | clang-22 (via `update-alternatives`) | `01-install-llvm-toolchain.yaml:166` |
| C++ stdlib | libc++-22 (CXXFLAGS=-stdlib=libc++) | `01-install-llvm-toolchain.yaml:171` |
| DPDK | 23.11 LTS, built with clang-22, meson | `02-install-dpdk-23-11.yaml:29` |
| `gcc` available? | `gcc` package installed alongside (gcc-13 default on noble) | apt-default; not pinned by recipe |

The 04-component's **"kernel 6.17 / gcc 13"** rationale is wrong about the
kernel (real kernel is 6.8 LTS, not 6.17 HWE — the original spec called for
6.17 but the WC vfio-pci patch only supports up to 6.8, so 01-component
explicitly downgrades). It is technically correct that gcc-13 ships as the
noble default, but **the gcc version is not the real blocker** — see below.

### Can mTCP use older gcc?

Ubuntu 24.04 ships gcc-13 default. `gcc-9` is NOT available in noble's
repositories (it's gone after 22.04 jammy). Available alternatives:
`gcc-12`, `gcc-13`, `gcc-14` from the toolchain PPA.

**This isn't the issue.** mTCP's Makefile uses `-fgnu89-inline -Werror` —
modern gcc would trip on type-pun / strict-aliasing warnings introduced
2018+, but those are minor and could be silenced with `-Wno-*` flags.

The **structural** issue is the DPDK API surface, which is unaffected by
choice of gcc version.

### Measurable: actual mTCP build attempt against system DPDK 23.11

Working directory: `/home/ubuntu/resd.dpdk_tcp/third_party/mtcp` (submodule
SHA `0463aad5`, the same SHA the AMI YAML pins).

```
$ gcc -c third_party/mtcp/mtcp/src/dpdk_module.c \
      $(pkg-config --cflags libdpdk) \
      -I third_party/mtcp/mtcp/src/include \
      -I third_party/mtcp/io_engine/include \
      -fPIC -fgnu89-inline \
      -DUSE_CCP -DDISABLE_HWCSUM -DENABLELRO -D__USRLIB__ -DENABLE_DPDK \
      -o /tmp/test.o
```

First-150-line errors:

| Symbol in mTCP | DPDK 23.11 replacement | Kind |
|---|---|---|
| `ETH_MQ_RX_RSS` | `RTE_ETH_MQ_RX_RSS` | constant rename |
| `max_rx_pkt_len` (struct field) | `max_lro_pkt_size` (or moved) | struct field removed |
| `DEV_RX_OFFLOAD_CHECKSUM` | `RTE_ETH_RX_OFFLOAD_CHECKSUM` | constant rename |
| `DEV_RX_OFFLOAD_TCP_LRO` | `RTE_ETH_RX_OFFLOAD_TCP_LRO` | constant rename |
| `split_hdr_size` (struct field) | removed | struct shape |
| `enable_lro` (struct field) | removed (set via offloads bitmap) | struct shape |
| `ETH_RSS_TCP/UDP/IP/L2_PAYLOAD` | `RTE_ETH_RSS_*` | 4× constant rename |
| `ETH_MQ_TX_NONE` | `RTE_ETH_MQ_TX_NONE` | constant rename |
| `DEV_TX_OFFLOAD_IPV4_CKSUM/UDP_CKSUM/TCP_CKSUM` | `RTE_ETH_TX_OFFLOAD_*` | 3× rename |
| `PKT_RX_L4_CKSUM_BAD/IP_CKSUM_BAD` | `RTE_MBUF_F_RX_*` | 2× constant rename |

Plus from `mtcp/src/core.c`:

| Symbol | DPDK 23.11 replacement | Kind |
|---|---|---|
| `rte_get_master_lcore()` | `rte_get_main_lcore()` | function rename (DPDK 20.11) |
| `lcore_config[i].ret/state` | privatised — no public accessor | **structural — needs rewrite** |

A repo-wide grep of mtcp source for the broken-API surface returned **36
hits** across `mtcp/src/*.c` and `mtcp/src/include/*.h` — note that this is
not 36 unique patches, but it is 36 call sites that each need touching.

### Build budget vs. spec R2

Spec §11 R2 risk row: *"mTCP build breakage on clang-22 + kernel 6.17 |
Patches under `resd.aws-infra-setup/image-components/install-mtcp/patches/`;
**>50 lines → escalate to drop `bench-vs-mtcp` from A10**."*

Realistic estimate of porting work:

| Task | Estimated patch size |
|---|---|
| Rename ~16 macro constants (sed-able) | 30 lines |
| Restructure `rte_eth_rxmode` initializer | 15 lines |
| Restructure `rte_eth_conf.txmode.offloads` | 10 lines |
| Replace `lcore_config[]` direct access with public-API equivalents | 25 lines |
| Replace `rte_get_master_lcore` with `rte_get_main_lcore` | 5 lines |
| Replace `PKT_RX_*` flag uses with `RTE_MBUF_F_RX_*` | 15 lines |
| Switch Makefile.in's autoconf-DPDK detection to `pkg-config libdpdk` | 25 lines |
| Wire CFLAGS/LDFLAGS path away from `RTE_SDK/RTE_TARGET` | 20 lines |
| Drop or guard `-Werror` for modern gcc warnings (TBD on warning surface) | 10 lines |
| **Estimated total** | **~155 lines** |

This is **3× the R2 escalation budget**. And these are lower-bound estimates —
the `lcore_config[]` rewrite alone may be infeasible because mTCP's worker-pool
state machine assumes shared `rte_lcore_state` introspection, which DPDK
22.07+ explicitly hides.

### What about a maintained fork?

Searched — none found. The closest reference is the [Zobin guide][zobin] (DPDK
21.11, blog walkthrough — not a fork; identifies the same patch surface and
explicitly recommends gcc-8). Multiple stale forks exist on GitHub but none
have post-2022 commits or DPDK 22.11+ support. mTCP's upstream issue tracker
shows zero merged PRs for DPDK ≥ 20.11.

[zobin]: https://zobinhuang.github.io/sec_learning/Tech_System_And_Network/DPDK_mTCP_Compiled/

## Phase 2 — AMI YAML disposition

The user asked us NOT to trigger an AMI rebake regardless. Given the Phase 1
finding (build infeasible inside escalation budget), the right YAML change is
to **fix the wrong rationale comment** rather than land a build that won't
work. Two options:

### Option A (recommended): keep the source-only stub, fix the comment

Patch `04-install-mtcp.yaml` to reflect the **measured** reason mTCP isn't
built (DPDK 23.11 API divergence), drop the "kernel 6.17 / gcc 13" framing,
and link this report. No behavioural change to the AMI. Diff:

```diff
- description: Checkout mTCP source + install stub bench-peer; libmtcp.a build deferred (mTCP's bundled DPDK is too old to compile on kernel 6.17 / gcc 13)
+ description: Checkout mTCP source + install stub bench-peer; libmtcp.a build dropped (mTCP upstream targets DPDK 18.05/19.08, fails to compile against system DPDK 23.11 — see docs/superpowers/reports/perf-a10-mtcp-rebuild-investigation.md)
```

And in the `install-mtcp-source-only` step's heredoc:

```diff
-              # Build of mTCP itself is intentionally DEFERRED. The
-              # github.com/mtcp-stack/mtcp bundled DPDK subtree is from
-              # ~DPDK 17 and doesn't compile against modern kernel headers
-              # (kernel 6.17) or modern gcc (gcc 13). A future rebake will
-              # either:
-              #   (a) apply an in-tree patchset to port the build to DPDK 23.11, or
-              #   (b) pull a maintained mTCP fork, or
-              #   (c) write resd.dpdk_tcp/tools/bench-vs-mtcp as a direct
-              #       DPDK application (no mTCP dependency; Plan 2 T21).
-              # For v0.1.0 of this AMI we ship only the source tree so
-              # later operators can iterate in-place.
+              # Build of mTCP itself is intentionally NOT performed. mTCP
+              # upstream (HEAD 0463aad5 from 2021, our submodule pin) was
+              # written for DPDK 18.05/19.08 and uses the legacy DPDK
+              # Make-based config (config/defconfig_* + tools/setup.sh).
+              # DPDK 22.07 privatised lcore_config[]; DPDK 22.11 dropped
+              # the legacy Make build entirely; DPDK 23.11 (this AMI's
+              # version) renamed ~16 macros mTCP relies on.
+              #
+              # An empirical compile attempt against system DPDK 23.11
+              # produced 18+ distinct errors in the first ~150 lines of
+              # mtcp/src/dpdk_module.c. Total porting estimate is ~155
+              # lines of patches, which exceeds spec §11 R2's 50-line
+              # escalation budget. Per that gate, bench-vs-mtcp drops the
+              # mTCP comparator and uses the Linux kernel TCP arm
+              # (already wired in tools/bench-vs-mtcp/src/linux_maxtp.rs)
+              # for cross-stack comparison.
+              #
+              # See docs/superpowers/reports/perf-a10-mtcp-rebuild-
+              # investigation.md for the full breakage inventory and
+              # disposition rationale.
```

Doesn't trigger a rebake (the YAML's verbs don't change, only the comments
and `description`). Pipeline behaviour identical.

### Option B (NOT recommended): try to port mTCP to DPDK 23.11

Estimated 1-2 weeks of focused engineering effort, low likelihood of producing
a maintainable result, no upstream uptake (mTCP is dormant). Not pursued.

### Option C (alternative — for follow-up): kernel-TCP "mtcp-shaped" peer

If the goal is purely "have a peer process at `/opt/mtcp-peer/bench-peer`
that absorbs traffic and is named *mtcp* in CSV", we could ship a kernel-TCP
sink with an mTCP-flavoured wrapper (named mtcp-peer for AMI-path
compatibility, but really `linux-tcp-sink` underneath). This deceives the
comparison axis (it'd really be Linux-vs-Linux) and **the user explicitly
said they want a real mTCP comparison**, so this is documented as
not-recommended unless intent shifts.

## Phase 3 — Rust mTCP arm disposition

The arm at `tools/bench-vs-mtcp/src/mtcp.rs` already returns
`Error::Unimplemented` with shape-validating wrappers. Without a working
`libmtcp.a` to link against, **there is nothing to wire**. The right
disposition is to:

1. Update `tools/bench-vs-mtcp/src/mtcp.rs`'s module docs to point at this
   investigation instead of "AMI not yet baked" — the AMI is baked, mTCP just
   doesn't build, and the user should know that's a structural state, not a
   timing issue.
2. Update `tools/bench-vs-mtcp/src/lib.rs`'s comment to match.
3. Keep the `Error::Unimplemented` return path. The CLI still validates the
   config shape, so operators who pass `--stacks mtcp` get a fast,
   informative error. Lenient mode already drops mTCP from the stack list
   with a WARN ([`main.rs:215-224`][lenient]).

[lenient]: ../../tools/bench-vs-mtcp/src/main.rs#L215-L224

The Linux maxtp arm (`linux_maxtp.rs`) already substitutes for the comparison
axis and **has been producing meaningful comparison data** in the last two
bench-pair runs (see `perf-a10-bench-pair-final-2026-05-04.md`).

No `bindgen` or `cc-rs` build.rs is added in this dispatch, because:
- there is no `libmtcp.a` to bind against
- even if we wrote bindings against the header, the underlying library can't
  link, so the resulting Rust crate could only compile in test-doubles mode
- adding a half-wired binding layer creates the misleading impression that a
  real implementation is "almost there"

## Phase 4 — what the user could do next (for real)

If a real second-stack throughput comparator is wanted later, the cheapest
options ranked by effort:

| Option | Effort | Output |
|---|---|---|
| Keep the Linux maxtp arm (already shipped) | 0 | dpdk_net vs Linux kernel TCP, sustained-throughput grid |
| Add a `seastar`-based comparator (modern, maintained, DPDK-native) | ~1 week | dpdk_net vs Seastar TCP, throughput + latency |
| Add an `f-stack`-based comparator (FreeBSD TCP on DPDK, actively maintained, DPDK 23.11 supported) | ~1 week | dpdk_net vs F-Stack, full TCP-stack comparison |
| Resurrect mTCP via 155-line in-tree patchset | ~2 weeks of porting + ongoing maintenance burden | dpdk_net vs (our patched) mTCP, but stale upstream |

[F-Stack][fstack] is probably the most valuable target — actively maintained,
explicitly supports DPDK 23.11, and is widely used in production CDN
deployments. But that's a follow-up phase, not a Plan B addendum.

[fstack]: https://github.com/F-Stack/f-stack

## Time consumed

Wall clock: ~70 minutes (well under the 2.5 h cap).

- Phase 1 toolchain investigation: ~25 min (incl. compile attempt)
- Phase 2 AMI YAML disposition: ~10 min
- Phase 3 Rust arm disposition: ~15 min
- Phase 4 alternatives + write-up: ~20 min

## Files referenced

- `tools/bench-vs-mtcp/src/mtcp.rs` — current Rust stub (kept; updated docs in companion edit)
- `tools/bench-vs-mtcp/src/lib.rs` — Stack enum + Linux comparator dispatch
- `tools/bench-vs-mtcp/src/linux_maxtp.rs` — working substitute comparator
- `third_party/mtcp/` — submodule pin `0463aad5` (mTCP HEAD as of phase-a10 branch point)
- `third_party/mtcp/mtcp/src/Makefile.in:96` — `PS_DIR=../../io_engine`
- `third_party/mtcp/mtcp/src/Makefile.in:GCC_OPT` — `-fgnu89-inline -Werror`
- `third_party/mtcp/mtcp/src/dpdk_module.c:112-153` — broken `rte_eth_conf` initialisation
- `third_party/mtcp/mtcp/src/core.c:1329-1649` — `rte_get_master_lcore` + `lcore_config[]` uses
- `~/resd.aws-infra-setup/lib/image_builder/_components/04-install-mtcp.yaml` — AMI YAML for fix
- `docs/superpowers/specs/2026-04-21-stage1-phase-a10-benchmark-harness-design.md:651,774` — spec context (kernel pin + R2 escalation gate)
