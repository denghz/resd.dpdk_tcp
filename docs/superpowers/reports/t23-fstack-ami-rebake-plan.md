# T23 — F-Stack AMI Rebake plan (subagent-produced 2026-05-05)

**Source:** general-purpose subagent (opus 4.7), task `a8ca151ea4547091f`.

**Goal:** rebake the bench-pair AMI to include `libfstack.a` + `libff_dpdk_kni.so` + `bench-peer-fstack` so F-Stack becomes a working 3rd comparator in `bench-vs-mtcp` (RTT + throughput).

## 1. Diff summary (sister project `/home/ubuntu/resd.aws-infra-setup`)

- **`lib/image_builder/_components/04b-install-f-stack.yaml`** (NEW, 145 lines):
  - Clones `github.com/F-Stack/f-stack` (depth 1, HEAD pinned via `/opt/f-stack/COMMIT_SHA.txt`).
  - Patches `lib/ff_dpdk_if.c` to comment out `rte_timer_meta_init()` (F-Stack-fork-only symbol absent in upstream DPDK 23.11).
  - Builds `libfstack.a` against system DPDK 23.11 → `/opt/f-stack/{lib,include}/`.
  - Drops `config.ini` to `/etc/f-stack.conf`.
  - Builds `bench-peer-fstack` from `/opt/src/bench-peer-fstack.c` if present, else stub.
  - Verify phase: `ls libfstack.a`, `nm | grep ff_socket/ff_connect/ff_write`.

- **`lib/image_builder/_components/04-install-mtcp.yaml`** (modified, +160/-30):
  - Pivots from "source-only drop" to a fully-built mTCP arm.
  - Adds DPDK-20.11 LTS sidecar at `/usr/local/dpdk-20.11/`.
  - Patches mTCP's `Makefile.in` (`pkg-config` instead of legacy `rte.vars.mk`, drops `-Werror`, adds `-fcommon`).
  - Patches `core.c` with a layout-mirroring `lcore_config_dpdk2011` shim.
  - Builds `libmtcp.a` → `/opt/mtcp/`.
  - **Co-stages with 04b but is unrelated to F-Stack.**

- **`lib/image_builder/recipes/bench_host.py`** (modified, +6/-0):
  - Inserts `"04b-install-f-stack"` into `COMPONENT_ORDER` immediately after `04-install-mtcp`, before `05-configure-grub`. Single edit.

## 2. Read order before commit

1. `lib/image_builder/_components/04b-install-f-stack.yaml`
2. `lib/image_builder/_components/04-install-mtcp.yaml`
3. `lib/image_builder/recipes/bench_host.py`
4. `cdk.json` — current `default-ami-id=ami-05ae5cb6a9a7022b9`
5. `README.md` — `bake-image` CLI auto-writes new AMI ID to `cdk.json` (`bake_image.py:174`), so the post-bake bump is automatic.

## 3. Recommended commit shape — TWO commits

Reason: `cdk.json`'s AMI ID is overwritten by `bake-image` after the bake completes. Committing a stale ID with the components is meaningless; commit components first → bake → commit the bumped `cdk.json`. Mirrors prior practice (`f730a5c`, `654ba28`).

### Commit 1 (pre-bake) — components + recipe

```
image-builder: add F-Stack comparator (04b) + activate mTCP build (04)

Adds 04b-install-f-stack: clones F-Stack, patches one
F-Stack-fork-only symbol (rte_timer_meta_init) for upstream DPDK 23.11
compatibility, builds libfstack.a + headers to /opt/f-stack/, drops
config.ini to /etc/f-stack.conf, and stages bench-peer-fstack.

Activates 04-install-mtcp's previously-deferred libmtcp.a build via
a sidecar DPDK-20.11 LTS install at /usr/local/dpdk-20.11 (mTCP's
last working DPDK ABI; 21.11 dropped the required PKT_RX_*/ETH_MQ_RX_*
symbols). Patches Makefile.in to use pkg-config and core.c to provide
a public-API shim for lcore_config[].

Registers 04b in bench-host COMPONENT_ORDER after 04-install-mtcp,
before 05-configure-grub. Both stacks now ship side-by-side as
cross-stack comparators alongside dpdk_net + Linux.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Commit 2 (post-bake) — AMI bump

```
cdk.json: bump default-ami-id to <NEW_AMI> (1.0.4)

Output of `resd-aws-infra bake-image --recipe-version 1.0.4` against
the F-Stack + activated-mTCP component set. New AMI carries
/opt/f-stack/lib/libfstack.a + /opt/mtcp/lib/libmtcp.a in addition
to the existing DPDK 23.11 + WC-patched vfio-pci payload.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

## 4. Bake trigger steps

```bash
cd /home/ubuntu/resd.aws-infra-setup

# (A) Pre-bake commit
git add lib/image_builder/_components/04-install-mtcp.yaml \
        lib/image_builder/_components/04b-install-f-stack.yaml \
        lib/image_builder/recipes/bench_host.py
git commit -m "..."   # message above
git push origin main

# (B) Bake. Image Builder recipe versions are immutable, so bump.
#     Previous shipped: 1.0.3 (cdk.json comment in commit f730a5c).
source .venv/bin/activate
resd-aws-infra bake-image \
    --recipe bench-host --recipe-version 1.0.4 \
    --subnet-id <bake-subnet> --security-group-id <bake-sg>
# Runs ~45 min. On success, bake_image.py auto-writes the new AMI ID
# to cdk.json's context.default-ami-id (see cli/.../bake_image.py:174).

# (C) Capture and commit the bump
git diff cdk.json    # verify only default-ami-id changed
git add cdk.json
git commit -m "cdk.json: bump default-ami-id to <NEW_AMI> (1.0.4) ..."
git push origin main
```

No re-deploy of `bench-pair` is strictly needed — `setup` reads `default-ami-id` from `cdk.json` at the next invocation (`setup.py:72`).

## 5. Validation

```bash
# (A) Spin up bench-pair against the new AMI:
resd-aws-infra setup bench-pair --operator-ssh-cidr "$(curl -s ifconfig.me)/32" --json

# (B) SSH and verify both new payloads:
ssh ubuntu@<DutSshEndpoint> '
  set -eux
  test -f /opt/f-stack/lib/libfstack.a
  test -f /opt/f-stack/include/ff_api.h
  cat /opt/f-stack/COMMIT_SHA.txt
  nm /opt/f-stack/lib/libfstack.a | grep -c " T ff_socket"
  test -f /opt/mtcp/lib/libmtcp.a
  test -f /usr/local/dpdk-20.11/lib/x86_64-linux-gnu/pkgconfig/libdpdk.pc
  /usr/local/bin/check-bench-preconditions --mode strict | jq .overall_pass
'
```

## 6. Risks / gotchas

1. **F-Stack HEAD is unpinned** (`git clone --depth 1`). If upstream pushes a breaking change to `lib/ff_dpdk_if.c` between bake and re-bake, the `sed` patch's `grep -q '^    rte_timer_meta_init();$'` guard will *silently skip* and leave the build broken. Mitigation: pin to a known-good SHA (e.g., `git checkout <sha>` after clone). Recommend logging the resolved SHA to bake output.
2. **04b shares `/opt/src/` with 04** — both run `mkdir -p /opt/src` and `clone-*` is idempotent via `[ ! -d ... ]` guards, fine.
3. **mTCP changes piggyback** — the DPDK-20.11 sidecar adds ~250 MB to the AMI and ~10 min to bake. Confirm with operator that activating mTCP is in scope for this rebake (otherwise revert `04-install-mtcp.yaml` before commit 1 and ship F-Stack-only).
4. **Recipe version collision** — `1.0.4` must be unused. Last-shipped per `cdk.json` comment in `f730a5c` is `1.0.3`. Confirm in Image Builder console: `aws imagebuilder list-image-recipes --filters name=name,values=bench-host` before baking.
5. **`bench-peer-fstack` source dependency** — 04b's `build-bench-peer-fstack` step expects `/opt/src/bench-peer-fstack.c`, staged by component 08. Per the YAML's own comment, 08 runs *after* 04b in `COMPONENT_ORDER` (recipe puts 08 at position 9), so on first bake the stub branch fires. Real binary comes from a follow-up CI rebake (per `08-install-bench-tools.yaml`). This is the documented pattern for mTCP and now F-Stack — no action needed, just be aware the validation `/opt/f-stack-peer/bench-peer` will be the stub on this AMI.
6. **vfio binding race** — out of scope for 04b (handled by 03 + first-boot user-data); F-Stack at runtime will rebind the data ENI to vfio-pci just like dpdk_net. Operator must ensure the data ENI MAC is the second interface.
7. **gawk update-alternatives** — 04b runs `update-alternatives --set awk /usr/bin/gawk || true`; the `|| true` swallows failures on AMIs where mawk isn't registered as the awk alternative. F-Stack's `freebsd/tools/makeobjops.awk` will silently miscompile on mawk. Verify `awk --version` in step (A) of validation reports gawk.

## Status

T23 is **plan-ready, execution-pending**. The user/operator runs sections 4 + 5 + 6 manually since:
- Sister-project commits/pushes are not in the dpdk_tcp repo
- AMI bake hits AWS (~45 min wall, $1-2 cost)
- Recipe-version assignment requires checking Image Builder console state
