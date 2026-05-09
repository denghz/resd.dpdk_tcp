/*
 * bench-vs-mtcp **client-side** mTCP driver.
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
 * # Status: implemented
 *
 * Workload pump mirrors the dpdk_net implementations (dpdk_burst.rs +
 * dpdk_maxtp.rs) so cross-stack comparison is apples-to-apples:
 *
 *   - burst: 1 connection, K bytes per burst, capture TSC at first +
 *     last segment, sleep G ms between bursts. Output one f64 sample
 *     per recorded (post-warmup) burst.
 *   - maxtp: C connections, round-robin pump for warmup + duration.
 *     Goodput estimated via bytes echoed back (peer is an echo server
 *     so received-bytes ≈ ACKed bytes). pps = round-trip packets / T.
 *
 * mTCP exposes no TX HW timestamping and no TCP_INFO snd_una probe
 * (mtcp_getsockopt only supports SO_ERROR). So the driver always
 * emits `tx_ts_mode: "tsc_fallback"` for burst and `"n/a"` for maxtp,
 * matching the Rust wrapper's expectations.
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

#include <arpa/inet.h>
#include <errno.h>
#include <getopt.h>
#include <inttypes.h>
#include <limits.h>
#include <math.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <x86intrin.h>

#include <mtcp_api.h>
#include <mtcp_epoll.h>

/* --------------------------------------------------------------------
 * Tunables.
 * -------------------------------------------------------------------- */

/* Max concurrent connections we expect to drive; bench-vs-mtcp's maxtp
 * grid tops out at C=128 per spec §11.2. */
#define MAX_CONNS         256
/* Drain scratch buffer — bench echo-server returns the same bytes; if
 * we don't drain them recv buffer fills and snd_wnd shrinks to 0. */
#define DRAIN_BUF_BYTES   (16 * 1024)
/* mtcp_epoll_wait timeout when polling for first-segment / drain
 * progress. Short enough that we re-check the wall-clock deadline
 * every ~1ms but not so short we burn the CPU. */
#define EPOLL_TIMEOUT_MS  1
/* Soft per-operation hang deadline. The Rust wrapper's outer timeout
 * is 60s by default (see MtcpConfig::timeout); we bail at 60s on any
 * single hung operation so the wrapper sees a JSON error rather than
 * a SIGKILL. */
#define OP_DEADLINE_SECS  60
/* Max events per epoll_wait return. With C ≤ 256 and one event per
 * conn per round, 1024 is plenty of headroom. */
#define MAX_EVENTS        1024

/* --------------------------------------------------------------------
 * CLI args.
 * -------------------------------------------------------------------- */

struct args {
    /* Common */
    const char *workload;
    const char *mtcp_conf;
    const char *peer_ip;
    int         peer_port;
    int         mss;
    int         num_cores;
    /* Burst */
    uint64_t    burst_bytes;
    uint64_t    gap_ms;
    uint64_t    bursts;
    uint64_t    warmup;
    /* Maxtp */
    uint64_t    write_bytes;
    uint64_t    conn_count;
    uint64_t    warmup_secs;
    uint64_t    duration_secs;
};

/* --------------------------------------------------------------------
 * Error-reporting helpers. The Rust wrapper parses stderr as a single
 * JSON object on non-zero exit, so we keep emit_error_json() as the
 * single path out of any failure.
 * -------------------------------------------------------------------- */

static void
emit_error_json(const char *reason, int err)
{
    /* Escape backslashes and quotes in reason to keep the JSON well-
     * formed. Reason strings are static format-spec text; keep this
     * minimal. */
    fprintf(stderr, "{\"error\": \"%s\", \"errno\": %d}\n", reason, err);
}

/* Wallclock helper for soft deadlines — independent of TSC, used only
 * for timing out a wedged operation. */
static double
wall_now_s(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

/* TSC reads — for per-burst throughput timestamps. Same instruction
 * the dpdk_net side uses (`dpdk_net_core::clock::rdtsc`) so the
 * sample math is unit-comparable. */
static inline uint64_t
rdtsc_now(void)
{
    return (uint64_t)__rdtsc();
}

/* Calibrate the TSC frequency once at startup. We sleep 50ms of
 * monotonic wall time and divide TSC delta by elapsed seconds.
 * Accurate to ~1% which is more than enough for per-burst f64
 * throughput rows (the absolute number is in the trillions of
 * cycles/sec and 1% drift is well below the bench-pair p50 noise
 * floor on c6in.metal). */
static double
calibrate_tsc_hz(void)
{
    uint64_t tsc0 = rdtsc_now();
    double w0 = wall_now_s();
    /* 50ms is short enough not to noticeably delay startup, long
     * enough that the TSC delta dominates clock_gettime jitter. */
    struct timespec ts = {.tv_sec = 0, .tv_nsec = 50 * 1000 * 1000};
    nanosleep(&ts, NULL);
    uint64_t tsc1 = rdtsc_now();
    double w1 = wall_now_s();
    double elapsed_s = w1 - w0;
    if (elapsed_s <= 0.0) {
        return 0.0;
    }
    return (double)(tsc1 - tsc0) / elapsed_s;
}

/* --------------------------------------------------------------------
 * Connection setup.
 * -------------------------------------------------------------------- */

/* Build the peer sockaddr from the CLI peer_ip + peer_port. */
static int
parse_peer_addr(const char *peer_ip, int peer_port, struct sockaddr_in *out)
{
    memset(out, 0, sizeof(*out));
    out->sin_family = AF_INET;
    out->sin_port = htons((uint16_t)peer_port);
    if (inet_pton(AF_INET, peer_ip, &out->sin_addr) != 1) {
        return -1;
    }
    return 0;
}

/* Open one non-blocking mTCP socket and connect it. Returns the
 * sockid on success, or -1 on failure (sets errno). Drives the
 * connect to completion via epoll_wait + EPOLLOUT (mTCP's connect
 * is non-blocking). 60s deadline on the connect itself. */
static int
open_one_connection(mctx_t mctx, int ep, const struct sockaddr_in *peer)
{
    int sock;
    int rc;
    struct mtcp_epoll_event ev;
    struct mtcp_epoll_event events[1];
    double deadline;

    sock = mtcp_socket(mctx, AF_INET, SOCK_STREAM, 0);
    if (sock < 0) {
        return -1;
    }
    if (mtcp_setsock_nonblock(mctx, sock) < 0) {
        mtcp_close(mctx, sock);
        return -1;
    }

    rc = mtcp_connect(mctx, sock, (const struct sockaddr *)peer,
                      sizeof(*peer));
    if (rc < 0 && errno != EINPROGRESS) {
        mtcp_close(mctx, sock);
        return -1;
    }

    /* Register for EPOLLOUT — fires when connect completes. */
    ev.events = MTCP_EPOLLOUT;
    ev.data.sockid = sock;
    if (mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_ADD, sock, &ev) < 0) {
        mtcp_close(mctx, sock);
        return -1;
    }

    deadline = wall_now_s() + (double)OP_DEADLINE_SECS;
    while (wall_now_s() < deadline) {
        int n = mtcp_epoll_wait(mctx, ep, events, 1, EPOLL_TIMEOUT_MS);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            mtcp_close(mctx, sock);
            return -1;
        }
        if (n == 0) {
            continue;
        }
        if (events[0].data.sockid != sock) {
            /* Spurious — keep looping. */
            continue;
        }
        if (events[0].events & (MTCP_EPOLLERR | MTCP_EPOLLHUP)) {
            errno = ECONNREFUSED;
            mtcp_close(mctx, sock);
            return -1;
        }
        if (events[0].events & MTCP_EPOLLOUT) {
            /* Connect completed. Re-arm for IN+OUT (we'll drain
             * echoed bytes via EPOLLIN; OUT stays armed so the
             * caller can detect resumed write-ready after EAGAIN). */
            ev.events = MTCP_EPOLLIN;
            ev.data.sockid = sock;
            mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_MOD, sock, &ev);
            return sock;
        }
    }
    /* Timed out. */
    errno = ETIMEDOUT;
    mtcp_close(mctx, sock);
    return -1;
}

/* Drain any echoed bytes from the connection without blocking. Returns
 * the number of bytes drained on this call (>=0), or -1 on hard error
 * (peer closed mid-stream — caller decides whether to abort).
 *
 * EAGAIN is treated as "no more bytes ready" and returns the running
 * count (which may be 0). The caller is responsible for re-driving
 * epoll_wait if more progress is needed. */
static ssize_t
drain_echo_nonblock(mctx_t mctx, int sock, char *scratch, size_t scratch_len)
{
    ssize_t total = 0;
    while (1) {
        ssize_t r = mtcp_read(mctx, sock, scratch, scratch_len);
        if (r < 0) {
            if (errno == EAGAIN) {
                return total;
            }
            return -1;
        }
        if (r == 0) {
            /* Peer closed half — return what we drained, signal EOF
             * via errno so caller can distinguish it. */
            errno = ECONNRESET;
            return total > 0 ? total : -1;
        }
        total += r;
        if ((size_t)r < scratch_len) {
            /* Short read — recv buffer drained for now. */
            return total;
        }
        /* Full buffer — keep draining. */
    }
}

/* --------------------------------------------------------------------
 * Burst workload.
 *
 * Mirrors dpdk_burst.rs::run_bucket:
 *   - 1 connection, opened up-front, persistent.
 *   - For each burst (warmup + bursts):
 *       t0 = TSC pre-first-write
 *       write K bytes (looped, with poll/drain for backpressure)
 *       t_first_wire = TSC after first non-zero write returns
 *       t1 = TSC after final write completes (closest observable to
 *            "last byte hit the wire" without HW TX TS)
 *       sample_bps = K * 8 / ((t1 - t0) seconds)
 *   - Sleep gap_ms between bursts.
 *   - Drain echoed bytes (peer is echo server) so peer's snd_wnd
 *     stays open.
 * -------------------------------------------------------------------- */

/* Send K bytes over `sock`. Loops mtcp_write until all bytes are
 * accepted, draining echoed bytes between writes to keep the peer's
 * recv buffer (and our send window) flowing.
 *
 * On success, populates *out_t_first_wire_tsc with the TSC sampled
 * immediately after the first non-zero mtcp_write call returned, and
 * returns 0. On hard error returns -1 with errno set; on soft hang
 * (no forward progress in OP_DEADLINE_SECS) returns -1 with
 * errno=ETIMEDOUT. */
static int
send_burst_bytes(mctx_t mctx, int ep, int sock,
                 const char *payload, size_t k,
                 char *drain_scratch, size_t drain_len,
                 uint64_t *out_t_first_wire_tsc,
                 uint64_t *out_bytes_drained)
{
    size_t sent = 0;
    int captured_first = 0;
    double last_progress = wall_now_s();
    /* Code-quality review T22 defect #4: events[] is a stack-yield
     * sentinel — `n` is discarded on the call-site below and the
     * payload is never inspected. Shrink to size 1 (and pass 1 as
     * maxevents) so we don't blow 16-32 KB of stack per burst. */
    struct mtcp_epoll_event events[1];

    while (sent < k) {
        ssize_t w = mtcp_write(mctx, sock, payload + sent,
                               k - sent);
        if (w > 0) {
            if (!captured_first) {
                /* First non-zero write — capture TSC right now. */
                *out_t_first_wire_tsc = rdtsc_now();
                captured_first = 1;
            }
            sent += (size_t)w;
            last_progress = wall_now_s();
        } else if (w < 0 && errno == EAGAIN) {
            /* Peer window or send buffer full — drain echoes and
             * wait briefly for progress. Don't tight-spin. */
            ssize_t drained = drain_echo_nonblock(mctx, sock,
                                                  drain_scratch,
                                                  drain_len);
            if (drained > 0) {
                *out_bytes_drained += (uint64_t)drained;
                last_progress = wall_now_s();
            } else if (drained < 0 && errno != EAGAIN) {
                return -1;
            }
            /* Wait for write-ready or read-ready event briefly. */
            int n = mtcp_epoll_wait(mctx, ep, events,
                                    1, EPOLL_TIMEOUT_MS);
            (void)n;
            if (wall_now_s() - last_progress >
                (double)OP_DEADLINE_SECS) {
                errno = ETIMEDOUT;
                return -1;
            }
        } else {
            /* Hard error or w == 0 with no errno (shouldn't happen). */
            if (w == 0) {
                /* Treat as transient — but still bound by deadline. */
                if (wall_now_s() - last_progress >
                    (double)OP_DEADLINE_SECS) {
                    errno = ETIMEDOUT;
                    return -1;
                }
                continue;
            }
            return -1;
        }
    }
    return 0;
}

/* Drain remaining echoed bytes after a burst's final write. Loops
 * drain_echo_nonblock + epoll_wait until either we've received >= K
 * bytes back (echo of the burst payload) or the deadline expires.
 *
 * Returns 0 on success (echo fully received), -1 on timeout/error.
 * Updates *bytes_drained_total with bytes drained on this call. */
static int
wait_for_burst_echo(mctx_t mctx, int ep, int sock,
                    char *drain_scratch, size_t drain_len,
                    size_t expected,
                    uint64_t *bytes_drained_total)
{
    size_t got = 0;
    double deadline = wall_now_s() + (double)OP_DEADLINE_SECS;
    /* See defect #4 above — events[] is yield-only, shrunk to 1. */
    struct mtcp_epoll_event events[1];

    while (got < expected) {
        ssize_t drained = drain_echo_nonblock(mctx, sock, drain_scratch,
                                              drain_len);
        if (drained < 0) {
            return -1;
        }
        got += (size_t)drained;
        *bytes_drained_total += (uint64_t)drained;
        if (got >= expected) {
            break;
        }
        int n = mtcp_epoll_wait(mctx, ep, events, 1,
                                EPOLL_TIMEOUT_MS);
        (void)n;
        if (wall_now_s() >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
    }
    return 0;
}

static void
maybe_sleep_gap_ms(uint64_t gap_ms)
{
    if (gap_ms == 0) {
        return;
    }
    struct timespec ts;
    ts.tv_sec = (time_t)(gap_ms / 1000);
    ts.tv_nsec = (long)((gap_ms % 1000) * 1000 * 1000);
    nanosleep(&ts, NULL);
}

/* Compute per-burst bps: K bits / (t1 - t0) seconds. Returns the
 * sample, or 0.0 on degenerate timing (caller filters NaN/0 rows). */
static double
compute_burst_bps(uint64_t k_bytes, uint64_t t0_tsc, uint64_t t1_tsc,
                  double tsc_hz)
{
    if (t1_tsc <= t0_tsc || tsc_hz <= 0.0) {
        return 0.0;
    }
    double cycles = (double)(t1_tsc - t0_tsc);
    double elapsed_s = cycles / tsc_hz;
    return ((double)k_bytes * 8.0) / elapsed_s;
}

/* T51: gated as unused while main() emits ENOSYS before dispatch.
 * Kept compiled-in so the implementation doesn't bit-rot — drop the
 * attribute when the dispatch in main() is re-enabled. */
__attribute__((unused))
static int
run_burst_workload(const struct args *a)
{
    mctx_t mctx = NULL;
    int ep = -1;
    int sock = -1;
    char *payload = NULL;
    char *drain_scratch = NULL;
    double *samples_bps = NULL;
    int rc = 1;
    int mtcp_inited = 0;
    struct sockaddr_in peer;
    uint64_t bytes_sent_total = 0;
    uint64_t bytes_acked_total = 0; /* echo received as ACK-by-proxy */
    uint64_t i;
    double tsc_hz;
    int saved_errno = 0;

    if (mtcp_init(a->mtcp_conf) != 0) {
        emit_error_json("mtcp_init failed", errno ? errno : EIO);
        return 1;
    }
    mtcp_inited = 1;

    /* Override num_cores from CLI before creating contexts. */
    {
        struct mtcp_conf mcfg;
        if (mtcp_getconf(&mcfg) == 0) {
            mcfg.num_cores = a->num_cores;
            mtcp_setconf(&mcfg);
        }
    }

    /* mTCP requires the application thread to be affinitised to its
     * core BEFORE creating the context. Single-threaded driver pins
     * to core 0 (the bench-vs-mtcp wrapper passes --num-cores 1). */
    if (mtcp_core_affinitize(0) < 0) {
        emit_error_json("mtcp_core_affinitize failed",
                        errno ? errno : EIO);
        goto out;
    }
    mctx = mtcp_create_context(0);
    if (!mctx) {
        emit_error_json("mtcp_create_context failed",
                        errno ? errno : EIO);
        goto out;
    }

    ep = mtcp_epoll_create(mctx, MAX_EVENTS);
    if (ep < 0) {
        emit_error_json("mtcp_epoll_create failed",
                        errno ? errno : EIO);
        goto out;
    }

    if (parse_peer_addr(a->peer_ip, a->peer_port, &peer) != 0) {
        emit_error_json("invalid --peer-ip", EINVAL);
        goto out;
    }

    sock = open_one_connection(mctx, ep, &peer);
    if (sock < 0) {
        saved_errno = errno;
        emit_error_json("connect to peer failed",
                        saved_errno ? saved_errno : EIO);
        goto out;
    }

    /* Allocate payload + drain scratch + samples buffer. */
    payload = malloc(a->burst_bytes);
    drain_scratch = malloc(DRAIN_BUF_BYTES);
    samples_bps = malloc(sizeof(*samples_bps) * a->bursts);
    if (!payload || !drain_scratch || !samples_bps) {
        emit_error_json("out of memory", ENOMEM);
        goto out;
    }
    /* Fill payload with a non-zero pattern. The peer is an echo
     * server; the byte values are irrelevant to throughput. */
    memset(payload, 0xab, a->burst_bytes);

    tsc_hz = calibrate_tsc_hz();
    if (tsc_hz <= 0.0) {
        emit_error_json("TSC calibration failed", EIO);
        goto out;
    }

    /* Warmup bursts — pump but discard timing samples. */
    for (i = 0; i < a->warmup; i++) {
        uint64_t t_first_wire = 0;
        if (send_burst_bytes(mctx, ep, sock, payload, a->burst_bytes,
                             drain_scratch, DRAIN_BUF_BYTES,
                             &t_first_wire,
                             &bytes_acked_total) != 0) {
            saved_errno = errno;
            emit_error_json("warmup burst failed",
                            saved_errno ? saved_errno : EIO);
            goto out;
        }
        if (wait_for_burst_echo(mctx, ep, sock, drain_scratch,
                                DRAIN_BUF_BYTES, a->burst_bytes,
                                &bytes_acked_total) != 0) {
            saved_errno = errno;
            emit_error_json("warmup echo wait failed",
                            saved_errno ? saved_errno : EIO);
            goto out;
        }
        bytes_sent_total += a->burst_bytes;
        maybe_sleep_gap_ms(a->gap_ms);
    }

    /* Measurement bursts — record one sample each. */
    for (i = 0; i < a->bursts; i++) {
        uint64_t t0 = rdtsc_now();
        uint64_t t_first_wire = 0;
        if (send_burst_bytes(mctx, ep, sock, payload, a->burst_bytes,
                             drain_scratch, DRAIN_BUF_BYTES,
                             &t_first_wire,
                             &bytes_acked_total) != 0) {
            saved_errno = errno;
            emit_error_json("measurement burst failed",
                            saved_errno ? saved_errno : EIO);
            goto out;
        }
        if (wait_for_burst_echo(mctx, ep, sock, drain_scratch,
                                DRAIN_BUF_BYTES, a->burst_bytes,
                                &bytes_acked_total) != 0) {
            saved_errno = errno;
            emit_error_json("measurement echo wait failed",
                            saved_errno ? saved_errno : EIO);
            goto out;
        }
        uint64_t t1 = rdtsc_now();
        bytes_sent_total += a->burst_bytes;

        /* Sanity: t1 must be > t0. On a healthy invariant TSC this
         * always holds; on a bizarre hiccup, write 0.0 (the Rust
         * parser accepts >=0, and bench-report filters near-zero
         * rows). */
        double bps = compute_burst_bps(a->burst_bytes, t0, t1, tsc_hz);
        if (!isfinite(bps) || bps < 0.0) {
            bps = 0.0;
        }
        samples_bps[i] = bps;

        (void)t_first_wire; /* captured for symmetry with dpdk_burst;
                             * Rust wrapper only uses end-to-end bps */
        maybe_sleep_gap_ms(a->gap_ms);
    }

    /* Emit JSON to stdout. Layout matches parse_burst_json in
     * src/mtcp.rs (line 425). */
    fputs("{\"workload\": \"burst\", \"samples_bps\": [", stdout);
    for (i = 0; i < a->bursts; i++) {
        if (i > 0) {
            fputs(", ", stdout);
        }
        /* %.17g preserves the f64 exactly across the JSON round-trip
         * the Rust serde_json parser will perform. */
        fprintf(stdout, "%.17g", samples_bps[i]);
    }
    fprintf(stdout,
            "], \"tx_ts_mode\": \"tsc_fallback\", "
            "\"bytes_sent_total\": %" PRIu64 ", "
            "\"bytes_acked_total\": %" PRIu64 "}\n",
            bytes_sent_total, bytes_acked_total);
    fflush(stdout);
    rc = 0;

out:
    free(payload);
    free(drain_scratch);
    free(samples_bps);
    if (sock >= 0) {
        mtcp_close(mctx, sock);
    }
    if (mctx) {
        mtcp_destroy_context(mctx);
    }
    /* mtcp_destroy() must only run if mtcp_init() succeeded —
     * calling it on an uninited library can fault inside DPDK EAL
     * teardown. (Code-quality review T22 defect #2.) */
    if (mtcp_inited) {
        mtcp_destroy();
    }
    return rc;
}

/* --------------------------------------------------------------------
 * Maxtp workload.
 *
 * Mirrors dpdk_maxtp.rs::run_bucket:
 *   - C connections opened up-front.
 *   - Warmup phase: round-robin pump for warmup_secs seconds, no
 *     sampling.
 *   - Measurement phase: round-robin pump for duration_secs.
 *     Track bytes echoed back (= bytes ACKed-by-proxy, since peer
 *     can only echo what it received). Track bytes written too as
 *     a sanity counter.
 *   - goodput_bps = bytes_acked * 8 / duration_secs.
 *   - pps         = total ECN-write segments / duration_secs.
 *     Without an mTCP packet counter, we approximate pps via the
 *     write-call count × ceil(W/MSS); this is close to
 *     `eth.tx_pkts` for steady-state pumps where Nagle/coalescing
 *     are off (mTCP defaults). Document this in the comment so the
 *     bench-report consumer doesn't conflate it with HW pps.
 * -------------------------------------------------------------------- */

struct maxtp_conn_state {
    int       sock;
    /* drain scratch is shared across conns to keep total memory
     * bounded; per-conn read state isn't needed because echo data is
     * opaque from this side. */
};

/* Round-robin write to every conn once; drain echoes; return on any
 * hard error. Updates the running counters. The caller drives the
 * outer loop and the deadline. */
static int
maxtp_pump_one_round(mctx_t mctx, int ep,
                     struct maxtp_conn_state *conns, uint64_t conn_count,
                     const char *payload, size_t write_bytes,
                     char *drain_scratch, size_t drain_len,
                     uint64_t *bytes_written,
                     uint64_t *bytes_echoed,
                     uint64_t *write_calls)
{
    uint64_t i;
    /* See defect #4 — yield-only events buffer, shrunk to 1. */
    struct mtcp_epoll_event events[1];

    for (i = 0; i < conn_count; i++) {
        ssize_t w = mtcp_write(mctx, conns[i].sock, payload, write_bytes);
        if (w > 0) {
            *bytes_written += (uint64_t)w;
            *write_calls += 1;
        } else if (w < 0 && errno == EAGAIN) {
            /* Peer window full on this conn — skip this slot, the
             * next round will retry. Mirrors dpdk_maxtp's "move on
             * to the next conn" on Ok(0). */
        } else if (w < 0) {
            /* Hard error. */
            return -1;
        }
    }

    /* Drain echoes from every conn. */
    for (i = 0; i < conn_count; i++) {
        ssize_t drained = drain_echo_nonblock(mctx, conns[i].sock,
                                              drain_scratch, drain_len);
        if (drained > 0) {
            *bytes_echoed += (uint64_t)drained;
        } else if (drained < 0 && errno != EAGAIN) {
            return -1;
        }
    }

    /* Yield to the mTCP IO thread so it can flush the TX ring +
     * service incoming ACKs. mtcp_epoll_wait with a 1ms timeout is
     * the closest thing mTCP exposes to "let the stack run for a
     * tick". */
    int n = mtcp_epoll_wait(mctx, ep, events, 1,
                            EPOLL_TIMEOUT_MS);
    (void)n;
    return 0;
}

/* T51: gated as unused while main() emits ENOSYS before dispatch.
 * Kept compiled-in so the implementation doesn't bit-rot — drop the
 * attribute when the dispatch in main() is re-enabled. */
__attribute__((unused))
static int
run_maxtp_workload(const struct args *a)
{
    mctx_t mctx = NULL;
    int ep = -1;
    char *payload = NULL;
    char *drain_scratch = NULL;
    struct maxtp_conn_state *conns = NULL;
    int rc = 1;
    int mtcp_inited = 0;
    struct sockaddr_in peer;
    uint64_t bytes_written = 0;
    uint64_t bytes_echoed = 0;
    uint64_t write_calls = 0;
    uint64_t i;
    int saved_errno = 0;

    if (a->conn_count > MAX_CONNS) {
        emit_error_json("--conn-count exceeds MAX_CONNS", EINVAL);
        return 1;
    }

    if (mtcp_init(a->mtcp_conf) != 0) {
        emit_error_json("mtcp_init failed", errno ? errno : EIO);
        return 1;
    }
    mtcp_inited = 1;

    {
        struct mtcp_conf mcfg;
        if (mtcp_getconf(&mcfg) == 0) {
            mcfg.num_cores = a->num_cores;
            mtcp_setconf(&mcfg);
        }
    }

    if (mtcp_core_affinitize(0) < 0) {
        emit_error_json("mtcp_core_affinitize failed",
                        errno ? errno : EIO);
        goto out;
    }
    mctx = mtcp_create_context(0);
    if (!mctx) {
        emit_error_json("mtcp_create_context failed",
                        errno ? errno : EIO);
        goto out;
    }

    ep = mtcp_epoll_create(mctx, MAX_EVENTS);
    if (ep < 0) {
        emit_error_json("mtcp_epoll_create failed",
                        errno ? errno : EIO);
        goto out;
    }

    if (parse_peer_addr(a->peer_ip, a->peer_port, &peer) != 0) {
        emit_error_json("invalid --peer-ip", EINVAL);
        goto out;
    }

    payload = malloc(a->write_bytes);
    drain_scratch = malloc(DRAIN_BUF_BYTES);
    conns = calloc(a->conn_count, sizeof(*conns));
    if (!payload || !drain_scratch || !conns) {
        emit_error_json("out of memory", ENOMEM);
        goto out;
    }
    memset(payload, 0xab, a->write_bytes);
    for (i = 0; i < a->conn_count; i++) {
        conns[i].sock = -1;
    }

    /* Open C connections sequentially. */
    for (i = 0; i < a->conn_count; i++) {
        int s = open_one_connection(mctx, ep, &peer);
        if (s < 0) {
            saved_errno = errno;
            emit_error_json("connect to peer failed (maxtp)",
                            saved_errno ? saved_errno : EIO);
            goto out;
        }
        conns[i].sock = s;
    }

    /* Warmup phase. */
    {
        double warmup_deadline = wall_now_s() +
                                 (double)a->warmup_secs;
        while (wall_now_s() < warmup_deadline) {
            if (maxtp_pump_one_round(mctx, ep, conns, a->conn_count,
                                     payload, a->write_bytes,
                                     drain_scratch, DRAIN_BUF_BYTES,
                                     &bytes_written, &bytes_echoed,
                                     &write_calls) != 0) {
                saved_errno = errno;
                emit_error_json("maxtp warmup pump failed",
                                saved_errno ? saved_errno : EIO);
                goto out;
            }
        }
    }

    /* Reset counters at warmup-end so the measurement window's
     * goodput is bounded to the duration_secs window only. The
     * dpdk_net side does this via per-conn snd_una snapshots; we do
     * it via counter reset, which is equivalent for the bytes-echoed
     * proxy. */
    bytes_written = 0;
    bytes_echoed = 0;
    write_calls = 0;

    /* Measurement phase. */
    double measure_start = wall_now_s();
    double measure_deadline = measure_start +
                              (double)a->duration_secs;
    while (wall_now_s() < measure_deadline) {
        if (maxtp_pump_one_round(mctx, ep, conns, a->conn_count,
                                 payload, a->write_bytes,
                                 drain_scratch, DRAIN_BUF_BYTES,
                                 &bytes_written, &bytes_echoed,
                                 &write_calls) != 0) {
            saved_errno = errno;
            emit_error_json("maxtp measurement pump failed",
                            saved_errno ? saved_errno : EIO);
            goto out;
        }
    }
    double measure_end = wall_now_s();

    /* Drain any residual echoes for ~50ms so the in-flight ACKs
     * land before we close. Bounded; not a hot loop. */
    {
        double drain_deadline = measure_end + 0.05;
        /* Code-quality review T22 defect #7: on per-conn hard error
         * (peer reset, stack OOM), break the inner loop so we don't
         * spin a dead socket for the full 50ms. EAGAIN is benign
         * (no bytes ready); anything else closes the loop early. */
        int hard_err = 0;
        /* See defect #4 — yield-only events buffer, shrunk to 1. */
        struct mtcp_epoll_event events[1];
        while (wall_now_s() < drain_deadline && !hard_err) {
            for (i = 0; i < a->conn_count; i++) {
                ssize_t d = drain_echo_nonblock(mctx, conns[i].sock,
                                                drain_scratch,
                                                DRAIN_BUF_BYTES);
                if (d > 0) {
                    bytes_echoed += (uint64_t)d;
                } else if (d < 0 && errno != EAGAIN) {
                    /* Hard error on this conn: stop draining the
                     * whole set rather than spin. We're past the
                     * measurement window so skipping ~50ms of
                     * residual echo is acceptable. */
                    hard_err = 1;
                    break;
                }
            }
            if (hard_err) {
                break;
            }
            mtcp_epoll_wait(mctx, ep, events, 1,
                            EPOLL_TIMEOUT_MS);
        }
    }

    /* Compute metrics. The Rust wrapper expects `goodput_bps` and
     * `pps` (see parse_maxtp_json in src/mtcp.rs). */
    double duration_s = measure_end - measure_start;
    if (duration_s <= 0.0) {
        emit_error_json("maxtp duration was non-positive", EIO);
        goto out;
    }

    /* goodput: bytes echoed back (peer can only echo what it
     * received and ACKed) ÷ duration. The dpdk_net side uses
     * per-conn snd_una deltas — semantically equivalent for an
     * echo-server peer where echo arrival == peer's ACK. */
    double goodput_bps = ((double)bytes_echoed * 8.0) / duration_s;

    /* pps: write-call count × segments-per-write. Each write of W
     * bytes generates ceil(W/MSS) TCP segments under mTCP defaults
     * (no Nagle / coalescing on a tight pump). This is the closest
     * proxy to eth.tx_pkts the dpdk_net side reports — without a
     * direct counter from libmtcp, the number is bounded above by
     * write-calls × ceil(W/MSS) and below by write-calls (1 segment
     * each in the "fits in MSS" case). */
    uint64_t segs_per_write = 1;
    if (a->mss > 0 && a->write_bytes > (uint64_t)a->mss) {
        segs_per_write = (a->write_bytes + (uint64_t)a->mss - 1) /
                         (uint64_t)a->mss;
    }
    double pps = ((double)(write_calls * segs_per_write)) / duration_s;

    fprintf(stdout,
            "{\"workload\": \"maxtp\", "
            "\"goodput_bps\": %.17g, \"pps\": %.17g, "
            "\"tx_ts_mode\": \"n/a\", "
            "\"bytes_sent_total\": %" PRIu64 "}\n",
            goodput_bps, pps, bytes_written);
    fflush(stdout);
    rc = 0;

out:
    free(payload);
    free(drain_scratch);
    if (conns) {
        for (i = 0; i < a->conn_count; i++) {
            if (conns[i].sock >= 0) {
                mtcp_close(mctx, conns[i].sock);
            }
        }
        free(conns);
    }
    if (mctx) {
        mtcp_destroy_context(mctx);
    }
    /* See burst arm comment: only destroy if init succeeded. */
    if (mtcp_inited) {
        mtcp_destroy();
    }
    return rc;
}

/* --------------------------------------------------------------------
 * CLI parsing.
 *
 * Long-options table is frozen — Rust wrapper builds these flags via
 * src/mtcp.rs::build_burst_argv and ::build_maxtp_argv. Adding /
 * removing / renaming a flag here will break the wrapper.
 * -------------------------------------------------------------------- */

static int
parse_u64_flag(const char *s, uint64_t *out)
{
    char *end;
    errno = 0;
    unsigned long long v = strtoull(s, &end, 10);
    if (errno != 0 || *end != '\0') {
        return -1;
    }
    *out = (uint64_t)v;
    return 0;
}

static int
parse_int_flag(const char *s, int *out)
{
    char *end;
    errno = 0;
    long v = strtol(s, &end, 10);
    if (errno != 0 || *end != '\0' || v < INT_MIN || v > INT_MAX) {
        return -1;
    }
    *out = (int)v;
    return 0;
}

static void
usage(FILE *out)
{
    fprintf(out,
        "usage: mtcp-driver --workload {burst|maxtp} "
        "--mtcp-conf <path> --peer-ip <ip> --peer-port <port> "
        "[other workload-specific flags — see source]\n");
}

int
main(int argc, char **argv)
{
    struct args a;
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

    memset(&a, 0, sizeof(a));
    a.peer_port = 10001;
    a.mss = 1460;
    a.num_cores = 1;

/* Compact dispatch macros — keep one line per flag and avoid
 * boilerplate. PARSE_U64 / PARSE_INT bail with a JSON error matching
 * the long-option name on parse failure so the wrapper can attribute
 * the bad flag. */
#define PARSE_U64(field, name)                                       \
    do {                                                             \
        if (parse_u64_flag(optarg, &a.field) != 0) {                 \
            emit_error_json("invalid --" name, EINVAL);              \
            return 2;                                                \
        }                                                            \
    } while (0)
#define PARSE_INT(field, name)                                       \
    do {                                                             \
        if (parse_int_flag(optarg, &a.field) != 0) {                 \
            emit_error_json("invalid --" name, EINVAL);              \
            return 2;                                                \
        }                                                            \
    } while (0)

    while (-1 != (o = getopt_long(argc, argv, "", long_options,
                                  &long_index))) {
        switch (o) {
        case 'w': a.workload  = optarg; break;
        case 'f': a.mtcp_conf = optarg; break;
        case 'i': a.peer_ip   = optarg; break;
        case 'p': PARSE_INT(peer_port,    "peer-port");    break;
        case 'm': PARSE_INT(mss,          "mss");          break;
        case 'N': PARSE_INT(num_cores,    "num-cores");    break;
        case 'K': PARSE_U64(burst_bytes,  "burst-bytes");  break;
        case 'g': PARSE_U64(gap_ms,       "gap-ms");       break;
        case 'b': PARSE_U64(bursts,       "bursts");       break;
        case 'W': PARSE_U64(warmup,       "warmup");       break;
        case 'B': PARSE_U64(write_bytes,  "write-bytes");  break;
        case 'C': PARSE_U64(conn_count,   "conn-count");   break;
        case 's': PARSE_U64(warmup_secs,  "warmup-secs");  break;
        case 'd': PARSE_U64(duration_secs,"duration-secs");break;
        case 'h': usage(stdout); return 0;
        default:
            emit_error_json("unknown flag", EINVAL);
            usage(stderr);
            return 2;
        }
    }
#undef PARSE_U64
#undef PARSE_INT

    if (!a.workload) {
        emit_error_json("missing --workload", EINVAL);
        return 2;
    }
    if (!a.mtcp_conf) {
        emit_error_json("missing --mtcp-conf", EINVAL);
        return 2;
    }
    if (!a.peer_ip) {
        emit_error_json("missing --peer-ip", EINVAL);
        return 2;
    }
    if (a.peer_port <= 0 || a.peer_port > 65535) {
        emit_error_json("--peer-port out of range", EINVAL);
        return 2;
    }

    if (strcmp(a.workload, "burst") == 0) {
        if (a.burst_bytes == 0 || a.bursts == 0) {
            emit_error_json("burst requires --burst-bytes and --bursts",
                            EINVAL);
            return 2;
        }
        /* T51: keep the burst arm gated as ENOSYS at the harness seam.
         * The pump body in run_burst_workload() is implemented (T22),
         * but on this AMI mtcp_init() trips on a libmtcp config-key
         * mismatch before the pump can run, which surfaces to the
         * bench wrapper as DriverFailed with a noisy banner instead
         * of a clean "stub" status. Until the AMI side stabilises,
         * we emit ENOSYS to stderr BEFORE mtcp_init so the wrapper
         * (src/mtcp.rs::invoke_driver) parses errno=38 and maps to
         * Error::DriverUnimplemented — exactly the contract the
         * Rust side has documented in its module header. Note: the
         * ENOSYS path lives ABOVE run_*_workload() so libmtcp never
         * prints its own startup banner to stderr; the only thing
         * the wrapper sees is our single JSON line. */
        emit_error_json("not implemented", ENOSYS);
        return 1;
        /* When the AMI-side libmtcp config is fixed, drop the two
         * lines above and re-enable: return run_burst_workload(&a); */
    } else if (strcmp(a.workload, "maxtp") == 0) {
        if (a.write_bytes == 0 || a.conn_count == 0 ||
            a.duration_secs == 0) {
            emit_error_json(
                "maxtp requires --write-bytes, --conn-count, "
                "--duration-secs",
                EINVAL);
            return 2;
        }
        /* T51: same rationale as the burst arm — ENOSYS-before-init
         * keeps the harness seam clean while the AMI-side libmtcp
         * config issue is sorted. */
        emit_error_json("not implemented", ENOSYS);
        return 1;
        /* When the AMI-side libmtcp config is fixed, drop the two
         * lines above and re-enable: return run_maxtp_workload(&a); */
    } else {
        emit_error_json("--workload must be 'burst' or 'maxtp'",
                        EINVAL);
        return 2;
    }
}
