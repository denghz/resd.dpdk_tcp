# bench-nightly on bench-pair (c6a.12xlarge × 2) — 2026-05-02 attempt

**Status:** RUN INCOMPLETE — every DPDK-using bench failed at "gateway ARP did not resolve within 10s" on the DUT data ENI. Stack torn down cleanly. Total spend: ~$4–5.

## What worked

| Step | Status |
|---|---|
| AWS profile `resd-infra-operator` access | ✓ valid |
| `resd-aws-infra` CLI installation (via Python wrapper at `~/.local/bin/resd-aws-infra` since pip install was broken on this host's setuptools) | ✓ |
| CDK deploy `bench-pair` stack (c6a.12xlarge × 2) via `INSTANCE_TYPE` env override (new in `scripts/bench-nightly.sh`, see edit below) | ✓ 192s deploy, 23 CFN resources, both instances reachable |
| SSH bring-up + binary deploy to both DUT and peer | ✓ (with new retry wrapper in `scripts/bench-nightly.sh` step [5/12], handled the SSH `kex_exchange_identification` cloud-init race) |
| Peer netem prep + echo-server startup | ✓ |
| Stack teardown (manual after run cancellation) | ✓ no orphans |

## What failed

**Every DPDK-using bench (bench-e2e, bench-stress 4 scenarios, bench-vs-linux mode A DpdkNet arm, bench-offload-ab, bench-obs-overhead, bench-vs-mtcp burst, bench-vs-mtcp maxtp)** failed with `Error: gateway ARP did not resolve within 10s`. Symptom is identical across benches because they all go through the same dpdk_net engine bring-up.

## Investigation findings

Probed the live DUT (PCI topology + driver bindings + AWS ENI state) before tear-down:

### PCI / driver state — looks correct

```
00:05.0  ENA  drv=ena       (kernel)  → ens5, IP 10.0.0.152 (primary/management ENI)
00:06.0  ENA  drv=vfio-pci  (DPDK-ready, no kernel netdev)  → data ENI

dmesg: vfio-pci 0000:00:06.0 vfio-noiommu device opened by user
       (multiple bench processes — bench-e2e, bench-stress, etc. successfully open it)
```

DPDK is binding the data ENI properly. EAL init succeeds (no error from bring-up).

### Kernel ARP works via primary ENI

```
ping -c2 10.0.0.1   → 2/2 received, 0.058 ms
ip neigh:           → 10.0.0.1 dev ens5 lladdr 02:45:d3:b4:aa:27 REACHABLE
default route:      → default via 10.0.0.1 dev ens5
```

So gateway 10.0.0.1 is reachable from the primary ENI's IP `10.0.0.152` and responds to ARP normally.

### AWS ENI state — both ENIs in same subnet

```
Primary ENI:  eni-0a08e5c1b0ea120e8  10.0.0.152  subnet-0135828dff2ac3cd7  SourceDestCheck=True
Data ENI:     eni-0f84618e36cf14a2a  10.0.0.217  subnet-0135828dff2ac3cd7  SourceDestCheck=True  MAC 02:87:f7:50:ac:67
```

Same subnet (`10.0.0.0/24`). Both have SourceDestCheck=True (default). Gateway is reachable from this subnet (proved by primary ping above). Data ENI is correctly attached + in-use.

### The mystery

The dpdk_net stack:
1. Opens vfio-pci device `00:06.0` ✓
2. Initializes EAL with `EAL_ARGS=-l 2-3 -n 4 --in-memory --huge-unlink -a 0000:00:06.0,large_llq_hdr=1,miss_txc_to=3` ✓
3. Configures port + sets local IP `10.0.0.217` (the data ENI's correct IP) ✓
4. Sends ARP request for gateway `10.0.0.1` from `10.0.0.217` via the data ENI
5. **Never receives the ARP reply within 10s** ✗

While simultaneously, kernel-side ARP from the SAME instance via the primary ENI works fine.

## Probable root causes (unconfirmed)

In rough probability order:

1. **`large_llq_hdr=1` ENA EAL devarg may not be supported / behaves differently on c6a.12xlarge** vs c6in.metal (where the script's defaults are tuned for). The devarg is a c6in.metal-class optimization for 200 Gbps NICs; on c6a's 25 Gbps NIC the LLQ header buffer differs. Could cause silent TX drop on the egress path.

2. **`vfio-noiommu` mode + AWS Nitro virtualization may not expose the same ENI capabilities** that work on bare-metal. On c6a.12xlarge (virtualized), Nitro layer might enforce stricter ENI filtering than on c6in.metal (bare metal pass-through).

3. **The ENA PMD's MAC programming on bind** — when DPDK opens the device, it should learn `02:87:f7:50:ac:67` and source frames from that MAC. If there's a mismatch (DPDK uses a different/wrong MAC), Nitro drops the egress.

4. **Subnet route table not propagated for the data ENI yet** — race between attachment and routing-table update. Unlikely (CFN reports `CREATE_COMPLETE` for the attachment), but possible.

## Next steps (not done in this session due to spend constraints)

1. **Try without `large_llq_hdr=1`**: rerun with `EAL_ARGS=-l 2-3 -n 4 --in-memory --huge-unlink -a 0000:00:06.0` (drop both `large_llq_hdr` and `miss_txc_to` since they're c6in.metal-targeted).
2. **Try with full PCI auto-discovery**: rerun with `EAL_ARGS=-l 2-3 -n 4 --in-memory --huge-unlink` (no `-a`, let DPDK probe everything bound to vfio-pci).
3. **Test with c6in.metal**: confirm the script DOES work on its documented target instance type. If it does, the issue is specifically about c6a.12xlarge. If it doesn't, the issue is in the dpdk_net stack itself, not the EAL_ARGS.
4. **DPDK testpmd ARP test**: run `dpdk-testpmd` directly on the DUT to ARP the gateway, isolating dpdk-net-core's own logic from the AWS interaction. If testpmd also fails, the issue is in the DPDK ENA layer, not our code.

## Spend summary

| Attempt | Outcome | Wall time | Approx cost |
|---|---|---|---|
| 1 | aws_cdk module missing for python3.10; CDK never deployed | ~2 min | $0 |
| 2 | Deploy succeeded; SSH `kex_exchange_identification` race during binary deploy; trap teardown clean | ~10 min | ~$0.55 |
| 3 | Deploy succeeded; binary deploy succeeded (retry helped); all DPDK benches failed at gateway ARP; manual teardown after TaskStop'd parent script | ~35-40 min | ~$2.40 |
| Re-run | (this is attempt 3 above) | | |

**Cumulative session spend: ~$3-5.** Slightly higher than ideal because Attempt 3 ran through 11/12 steps before I killed it (all DPDK benches failed but the script tolerated each failure and continued).

## Useful artifacts produced

Even though no DPDK bench succeeded, the following bench-pair artifacts were captured:

- **dmesg evidence** of DPDK successfully opening `00:06.0` repeatedly
- **AWS API state** confirming the data ENI is correctly attached, in-subnet, with SourceDestCheck=True
- **Kernel-level ARP confirmation** that the gateway is responsive from the same subnet
- **Updated `scripts/bench-nightly.sh`** with two improvements:
  - `INSTANCE_TYPE` env-var support → `--instance-type` passthrough to `resd-aws-infra setup bench-pair`
  - `retry_remote` wrapper around scp/ssh in step [5/12] for transient `kex_exchange_identification: Connection closed by remote host` errors

These edits are local-only on master at the time of this report (not yet committed). Worth committing if they prove durable across future runs.

## Recommendation

Before another bench-pair attempt:
1. Pin down which EAL devarg is the issue by trying `EAL_ARGS` overrides on a NEW shorter run (just bench-e2e, not full nightly), saving cost while iterating.
2. If the new run still fails on c6a.12xlarge but `large_llq_hdr=1` is the only change, file an issue against dpdk-net-core for "ENA PMD on virtualized AMD instances".
3. If we cannot get bench-pair to work on c6a.12xlarge, fall back to c6in.metal for the canonical run (per the script's documented target). c6in.metal × 2 = ~$11/hr — more expensive but matches the script's tested config.

NUMA-aware allocation (the original reason to pick c6a.12xlarge over c6in.metal/c6a.metal) remains relevant; we can revisit choosing c6a.12xlarge after the EAL devarg issue is understood.
