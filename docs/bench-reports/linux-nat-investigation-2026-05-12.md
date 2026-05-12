# linux_kernel arm "NAT hang" investigation — 2026-05-12

**Triggering symptom:** `bench-rtt --stack linux_kernel` and
`bench-rx-burst --stack linux_kernel` against the fast-iter peer at
`10.4.1.228` get a few iterations through and then sit on `read_exact`
"forever" (the suite hits its 300 s `RUN_ONE_TIMEOUT`).
**Root cause (1-liner):** the dev-host container has a transparent
**SOCKS5 proxy (REDSOCKS)** intercepting *every* outbound TCP packet
from this netns. Egress is not NAT'd — it is proxy-tunneled. Per-hop
latency goes from ~75 µs (direct NIC) to ~250 ms (proxy bounce);
9 k+ iterations × 250 ms ≈ 38 min, so the suite's per-arm timeout
fires long before the workload completes.

## Topology

Captured 2026-05-12T05:24Z on `dpdk-dev-box.canary.bom.aws` from
inside the same namespace bench-rtt runs in.

```
$ ip addr show
1: lo: 127.0.0.1/8
4: vethpxtn0@if5: 10.99.1.2/24            ← container side of a veth pair

$ ip route show
default via 10.99.1.1 dev vethpxtn0       ← all egress hits the gateway
10.99.1.0/24 dev vethpxtn0 src 10.99.1.2

$ curl -s --max-time 3 http://169.254.169.254/latest/meta-data/local-ipv4
10.4.1.139                                ← host's mgmt ENI (NOT visible here)
```

The host has two ENIs — `02:93:67:85:c3:4f` (mgmt, ena driver) and
`02:94:4a:94:57:8b` (data NIC, vfio-pci) — but the container netns
this bench runs in sees neither: only `lo` and the veth into the
container bridge `10.99.1.0/24`.

So far this is consistent with a plain NAT egress. **The actual
mechanism is different.** Inspecting iptables NAT rules:

```
$ sudo iptables -t nat -L OUTPUT -v -n
Chain OUTPUT (policy ACCEPT 0 packets, 0 bytes)
 pkts bytes target     prot opt in     out     source               destination
 2515  151K REDSOCKS   tcp  --  *      *       0.0.0.0/0            0.0.0.0/0
 1245 87110 RETURN     udp  --  *      *       0.0.0.0/0            127.0.0.1   udp dpt:53
 1000 69460 RETURN     udp  --  *      *       0.0.0.0/0            10.4.0.2    udp dpt:53
    0     0 REDIRECT   udp  --  *      *       0.0.0.0/0            0.0.0.0/0   udp dpt:53 redir ports 10053

$ sudo iptables -t nat -L REDSOCKS -v -n
... (RETURN for 10.99.1.0/24, 10.4.0.2, 10.2.1.11 — i.e. local + DNS + ...)
 1163 69780 REDIRECT   tcp  --  *      *       0.0.0.0/0            0.0.0.0/0   redir ports 12345
```

Every TCP `OUTPUT` packet (except those whose destination is
`127.0.0.0/8`, `169.254.0.0/16`, `224/4`, `240/4`, `10.99.1.0/24`,
`10.4.0.2`, or `10.2.1.11`) is REDIRECTed to `127.0.0.1:12345`. That
port belongs to a `redsocks` daemon:

```
$ sudo ss -tnlp | grep 12345
LISTEN 0      128        127.0.0.1:12345      0.0.0.0:*    users:(("redsocks",pid=3060,fd=6))

$ sudo cat /etc/redsocks.conf
redsocks {
    local_ip = 127.0.0.1;
    local_port = 12345;
    ip = 127.0.0.1;
    port = 1080;
    type = socks5;
}
```

**Path for a `bench-rtt --stack linux_kernel` connect to `10.4.1.228:10001`:**

```
bench-rtt → TCP SYN to 10.4.1.228:10001
         → iptables OUTPUT NAT: REDIRECT to 127.0.0.1:12345
         → redsocks accepts, opens a SOCKS5 tunnel via 127.0.0.1:1080
         → outbound proxy (whatever 127.0.0.1:1080 is) → 10.4.1.228:10001
         ← all responses traverse the same chain in reverse
```

Per-iteration RTT measured at the bench:

| path                         | p50 RTT | p99 RTT |
|------------------------------|--------:|--------:|
| dpdk_net, direct NIC         |  ~75 µs |  ~97 µs |
| linux_kernel via SOCKS proxy |  ~250 ms | varies   |
| linux_kernel via 127.0.0.1   |  ~37 µs |  ~50 µs |

`ss -tnp` while the bench is running:

```
ESTAB 0      0          10.99.1.2:36322    10.4.1.228:10001 users:(("bench-rtt",pid=...,fd=3))
ESTAB 0      0          127.0.0.1:12345   10.99.1.2:36322  users:(("redsocks",pid=3060,fd=...))
```

The bench sees its socket as `→ 10.4.1.228:10001`; redsocks has the
mirror image on `127.0.0.1:12345`. Connections aren't "hung" — they
are slowly trickling through the proxy at ~4 RTTs/sec. Confirmed by
running 100, 200, then 1000 iterations: 100 completes in ~25 s, 200
in ~50 s, 1000 hits the 300 s timeout because we'd need ~250 s on
this proxy path. The earlier `read_exact` "indefinite hang" theory
was actually just extreme slowness within the timeout window.

The fstack and dpdk_net arms aren't affected: they bypass the kernel
TCP stack and the `OUTPUT` chain entirely — they write Ethernet
frames straight to the DPDK port.

## Why this happened

The dev host's container is set up with a transparent SOCKS5 proxy
for security/policy reasons (presumably to log+inspect all outbound
TCP). The bench tool was designed for nightly runs on a different
host (`/home/ubuntu/resd.dpdk_tcp-a10-perf`) which IS in the data
subnet and DOES have direct routing. The fast-iter dev cycle running
on the canary box hits the proxy unintentionally because nothing in
the bench code knows to route around it.

## Decision

**Option B taken** (per task brief, the recommended primary fix):
spawn local echo / burst-echo servers on the DUT itself and point
the linux_kernel arm at `127.0.0.1`. This is the right move because:

1. Local kernel-TCP loopback IS a meaningful baseline — it measures
   the same kernel socket → kernel socket path the comparison cares
   about (vs. dpdk_net). The fact that there's no NIC in the path
   is fine: the linux_kernel-vs-dpdk_net delta is dominated by the
   socket-call overhead, not the wire trip.
2. Doesn't need host network reconfiguration (which we may not be
   permitted to do in this environment).
3. Keeps the dpdk_net + fstack arms exactly as they were — they
   continue to use the real peer for end-to-end measurement.

**Not taken:**

- *Option A* (host iptables fix): would need ACCEPT rule for
  `10.4.1.228` in the REDSOCKS chain. May be locked down; out of
  scope of "bench tool" changes.
- *Option C* (TCP_USER_TIMEOUT): masks the symptom but loses the
  measurement entirely.
- *Option D* (docs only): T55 already documented it; without a
  code-side fix the suite still produces a TIMEOUT row for every
  fast-iter run, which is noise.

## Fix shape (commit-level)

`scripts/fast-iter-suite.sh` gains:

- `LOCAL_LINUX_ECHO_PORT` (default 19001) — `echo-server` spawned on
  `127.0.0.1` before the linux_kernel arms run.
- `LOCAL_LINUX_BURST_PORT` (default 19003) — `burst-echo-server`
  spawned on `127.0.0.1` for bench-rx-burst linux.
- A `start_local_linux_servers` / `stop_local_linux_servers` pair
  using `trap` so the servers always clean up.
- Each linux_kernel arm's `--peer-ip` is rewritten to `127.0.0.1`
  and `--peer-port` / `--peer-control-port` to the local port. All
  three non-linux arms (dpdk_net, fstack) are unchanged.

`bench-tx-burst` and `bench-tx-maxtp` linux arms are *also* routed
to the local echo for consistency — T55 had them as "OK" against the
remote peer, but the numbers were inflated noise (write-buffer fill,
not wire rate). Local loopback at least gives a coherent
kernel-TCP-loopback number to compare against.

## Smoke-test result (post-fix)

`linux_kernel` arm at `--iterations 5000 --warmup 100` across the
4-payload sweep:

```
payload=   64B  p50=37.22 µs
payload=  128B  p50=37.47 µs
payload=  256B  p50=38.83 µs
payload= 1024B  p50=38.94 µs
```

Total wallclock for the 4-bucket × 5 k-iter run: **<1 second**, vs.
the previous `300 s TIMEOUT`. bench-rx-burst linux at default grid
(W={64,128,256} × N={16,64,256}, 200 measure bursts) completes in
**833 ms** with per-event p50 around 7-10 µs. Suite-level smoke run
in the commit body.

## Open follow-ups carried forward

- If a future bench wants to measure kernel-TCP-over-real-NIC on a
  host without the SOCKS proxy in the way, route via Option A. For
  this dev box it isn't relevant.
- T55 follow-up #2 (fstack SIGSEGV) is independent and still open.
- T55 follow-up #3 (bench-tx-maxtp `--local-ip`) is independent and
  still open.
