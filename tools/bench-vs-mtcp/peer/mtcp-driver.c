/*
 * bench-vs-mtcp **client-side** mTCP driver — STUB.
 *
 * # What this is
 *
 * This is the client-side counterpart of bench-peer.c (the server-side
 * mTCP echo server). bench-vs-mtcp's Rust process invokes this binary
 * via `std::process::Command` with workload params on the CLI; the
 * binary opens C connections via mTCP, runs the K×G or W×C workload,
 * and prints a single JSON object on stdout that the Rust side parses
 * back into a BurstSample / MaxtpSample.
 *
 * # Why a separate process
 *
 * bench-vs-mtcp's Rust binary already statically links DPDK 23.11 via
 * `dpdk-net-sys`. mTCP only works against DPDK 20.11 (per the
 * `04-install-mtcp.yaml` sidecar). Linking both DPDK versions into
 * one process is impossible (same symbol names, different ABI layout),
 * so the mTCP arm runs as a sibling subprocess that links libmtcp.a +
 * DPDK 20.11 cleanly.
 *
 * # Status: STUB
 *
 * The full client-side workload pump is **not yet implemented**. This
 * file establishes the interface (CLI args, JSON output schema) so the
 * Rust subprocess wrapper in `src/mtcp.rs` has a stable contract to
 * test against. The real implementation needs:
 *
 *   - mTCP startup config file parsing (-f <mtcp.conf>)
 *   - Per-core mctx_t setup (mtcp_create_context per cpu)
 *   - C-connection pump matching dpdk_burst.rs / dpdk_maxtp.rs shapes
 *   - HW TX timestamp readback via `mtcp_getsockopt(.., TCP_INFO, ..)`
 *     where supported, TSC fallback otherwise (mirroring TxTsMode)
 *   - Sanity invariant check (sent-bytes accounting)
 *   - Burst-grid (K×G) vs maxtp-grid (W×C) workload selection on a
 *     single binary, switched by --workload {burst,maxtp}
 *
 * Estimated effort: ~600 LOC mirroring dpdk_burst.rs + dpdk_maxtp.rs
 * shape against the mTCP API. Tracked separately — this stub unblocks
 * the Rust subprocess wrapper land.
 *
 * # CLI contract (frozen — changes break the Rust wrapper)
 *
 * Common flags:
 *   --workload {burst|maxtp}   (required)
 *   --mtcp-conf <path>         (mTCP startup config — passed to mtcp_init)
 *   --peer-ip <ip>             (target host, e.g. 10.0.0.42)
 *   --peer-port <port>         (target port — bench-peer-mtcp on the peer)
 *   --mss <bytes>              (must match dpdk_net side for valid xstack diff)
 *   --num-cores <N>            (mTCP core count)
 *
 * burst-specific flags:
 *   --burst-bytes <K>          (K per spec §11.1)
 *   --gap-ms <G>
 *   --bursts <count>           (post-warmup burst count)
 *   --warmup <count>
 *
 * maxtp-specific flags:
 *   --write-bytes <W>
 *   --conn-count <C>
 *   --warmup-secs <T>
 *   --duration-secs <T>
 *
 * # JSON output schema (frozen)
 *
 * burst:
 *   {"workload": "burst",
 *    "samples_bps": [<f64>...],   // one per recorded burst
 *    "tx_ts_mode": "tsc_fallback"|"hw_ts"|"unsupported",
 *    "bytes_sent_total": <u64>,
 *    "bytes_acked_total": <u64>}
 *
 * maxtp:
 *   {"workload": "maxtp",
 *    "goodput_bps": <f64>,
 *    "pps": <f64>,
 *    "tx_ts_mode": "n/a",
 *    "bytes_sent_total": <u64>}
 *
 * Errors → exit non-zero with a single JSON object on stderr:
 *   {"error": "<reason>", "errno": <int>}
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <getopt.h>

/*
 * STUB: the binary parses its CLI args + emits a placeholder JSON,
 * then exits non-zero so the Rust caller surfaces a clear "driver
 * not yet implemented" error. Once the real workload pump lands,
 * this main() is replaced with the per-workload implementation.
 */

static const char STUB_REASON[] =
    "mtcp-driver client-side workload pump not yet implemented "
    "(infrastructure stub — see peer/mtcp-driver.c module docs for the "
    "frozen CLI + JSON contracts)";

int
main(int argc, char **argv)
{
    int o;
    int long_index;
    static struct option long_options[] = {
        {"workload",       required_argument, 0, 'w'},
        {"mtcp-conf",      required_argument, 0, 'f'},
        {"peer-ip",        required_argument, 0, 'i'},
        {"peer-port",      required_argument, 0, 'p'},
        {"mss",            required_argument, 0, 'm'},
        {"num-cores",      required_argument, 0, 'N'},
        {"burst-bytes",    required_argument, 0, 'K'},
        {"gap-ms",         required_argument, 0, 'g'},
        {"bursts",         required_argument, 0, 'b'},
        {"warmup",         required_argument, 0, 'W'},
        {"write-bytes",    required_argument, 0, 'B'},
        {"conn-count",     required_argument, 0, 'C'},
        {"warmup-secs",    required_argument, 0, 's'},
        {"duration-secs",  required_argument, 0, 'd'},
        {"help",           no_argument,       0, 'h'},
        {0, 0, 0, 0},
    };

    /* Parse args so a Rust caller can validate the wrapper's command
     * construction even before the real workload pump lands. Unknown
     * flags surface a usage error — an early signal that wrapper +
     * driver have drifted. */
    while (-1 != (o = getopt_long(argc, argv, "", long_options, &long_index))) {
        switch (o) {
        case 'w': case 'f': case 'i': case 'p': case 'm':
        case 'N': case 'K': case 'g': case 'b': case 'W':
        case 'B': case 'C': case 's': case 'd':
            /* arg consumed; STUB ignores values */
            break;
        case 'h':
            fprintf(stdout,
                "usage: mtcp-driver --workload {burst|maxtp} "
                "--mtcp-conf <path> --peer-ip <ip> --peer-port <port> "
                "[other workload-specific flags — see source]\n");
            return 0;
        default:
            fprintf(stderr,
                "{\"error\": \"unknown flag\", \"errno\": %d}\n", EINVAL);
            return 2;
        }
    }

    /* Surface the not-yet-implemented status as a single JSON object
     * on stderr, exit non-zero. The Rust wrapper picks this up and
     * propagates it via Error::DriverUnimplemented. */
    fprintf(stderr, "{\"error\": \"%s\", \"errno\": %d}\n",
            STUB_REASON, ENOSYS);
    return ENOSYS;
}
