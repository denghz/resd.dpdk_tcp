/*
 * burst-echo-server.c — peer-side burst-push server for bench-rx-burst.
 *
 * Phase 8 of the 2026-05-09 bench-suite overhaul. Replaces the placeholder
 * bench-rx-zero-copy purpose: closes the small-packet RX-burst latency gap
 * (claims C-A3, C-B3, C-C2) by giving the DUT a peer that, on a single
 * TCP connection, ships N back-to-back segments of W bytes each carrying a
 * 16-byte header [be64 seq_idx | be64 peer_send_ns]. The DUT-side
 * bench-rx-burst tool drives this server from a control connection and
 * captures per-segment app-delivery latency.
 *
 * Build: gcc -O2 -Wall -Wextra -o burst-echo-server burst-echo-server.c
 * (or: clang-22 -O2 ... — pure C stdlib, no pthread, no external deps).
 *
 * Protocol (ASCII line-oriented, both sides on the same TCP socket):
 *
 *   client (DUT) -> server (peer):  "BURST <N> <W>\n"
 *   server -> client:               <N segments of W bytes each, each
 *                                    starting with [be64 seq_idx |
 *                                    be64 peer_send_ns] and the rest
 *                                    payload-padding>
 *   client (DUT) -> server (peer):  "QUIT\n"        (or socket EOF)
 *
 * Design notes
 * ------------
 *
 * - Clock: clock_gettime(CLOCK_REALTIME, ...). This is NOT PTP-precise.
 *   The DUT and peer are different hosts, so CLOCK_MONOTONIC (per-host)
 *   is useless for cross-host latency. CLOCK_REALTIME on AWS EC2 is
 *   typically NTP-disciplined; same-AZ instances see ~100 µs offset.
 *   The resulting per-segment latency measured by the DUT is therefore
 *   skewed by NTP offset by that much. Phase 9 of the overhaul wires
 *   c7i HW RX-TS to tighten the cross-host bound; for now CLOCK_REALTIME
 *   gives "good enough" steady-state cadence + relative ordering.
 *
 * - TCP_NODELAY on the accepted socket. Each segment is a distinct
 *   write() call (no MSG_MORE, no per-burst buffering) so the kernel
 *   TCP stack emits them as separate segments wherever MSS allows. For
 *   small W (e.g. 64 B), the kernel may still coalesce multiple writes
 *   into one MSS-sized segment under back-pressure; the DUT-side parser
 *   handles that case (walks the byte stream in W-byte steps).
 *
 * - Single accept loop, single connection at a time. bench-rx-burst
 *   opens one control connection per stack pass and reuses it across
 *   all (W, N) buckets. We don't need pthreads for echo concurrency.
 *
 * - Min payload size: 16 B (the header itself). The plan's lower
 *   bound is W=64 B which is fine.
 *
 * - SIGPIPE: ignored; a half-closed peer write surfaces as EPIPE and
 *   we close the connection.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

/* MAX_W bounds a single BURST <N> <W> segment. The plan sweeps
 * W in {64, 128, 256}; 1 MiB is a generous ceiling that keeps the
 * preallocated buffer small while leaving headroom for future
 * sweeps.  */
#define MAX_W (1024 * 1024)

/* Big-endian u64 store. htobe64 is glibc-specific; portable by hand. */
static inline void store_be64(uint8_t *p, uint64_t v) {
    for (int i = 7; i >= 0; --i) {
        p[i] = (uint8_t)(v & 0xff);
        v >>= 8;
    }
}

/* Send N segments of W bytes back-to-back. Each segment payload starts
 * with [be64 seq_idx | be64 peer_send_ns]; the rest is zero-fill from
 * `buf` (caller supplies a W-sized scratch buffer). Returns 0 on
 * success, -1 on error (errno set by the failing write). */
static int send_burst(int fd, uint64_t n, uint64_t w, uint8_t *buf) {
    if (w < 16) {
        errno = EINVAL;
        return -1;
    }
    /* Zero-fill once; only the 16-byte header is rewritten per
     * segment. */
    memset(buf, 0, (size_t)w);
    for (uint64_t i = 0; i < n; ++i) {
        struct timespec ts;
        if (clock_gettime(CLOCK_REALTIME, &ts) != 0) {
            return -1;
        }
        uint64_t now_ns =
            (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
        store_be64(buf + 0, i);
        store_be64(buf + 8, now_ns);

        size_t off = 0;
        while (off < (size_t)w) {
            ssize_t r = write(fd, buf + off, (size_t)w - off);
            if (r < 0) {
                if (errno == EINTR) continue;
                return -1;
            }
            if (r == 0) {
                /* TCP write should never return 0; treat as EOF. */
                errno = EPIPE;
                return -1;
            }
            off += (size_t)r;
        }
    }
    return 0;
}

/* Read one '\n'-terminated line into `out`. Returns the number of
 * bytes read (including the '\n') on success, or -1 on EOF/error. */
static int read_line(int fd, char *out, size_t cap) {
    if (cap < 2) {
        errno = EINVAL;
        return -1;
    }
    size_t n = 0;
    while (n < cap - 1) {
        char c;
        ssize_t r = read(fd, &c, 1);
        if (r == 0) return -1;     /* EOF */
        if (r < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        out[n++] = c;
        if (c == '\n') break;
    }
    out[n] = '\0';
    return (int)n;
}

int main(int argc, char **argv) {
    int port = 10003;
    if (argc > 1) {
        port = atoi(argv[1]);
        if (port <= 0 || port > 65535) {
            fprintf(stderr, "burst-echo-server: invalid port %s\n", argv[1]);
            return 1;
        }
    }

    /* See echo-server.c — same SIGPIPE rationale. */
    signal(SIGPIPE, SIG_IGN);

    int srv = socket(AF_INET, SOCK_STREAM, 0);
    if (srv < 0) {
        perror("socket");
        return 1;
    }
    int one = 1;
    setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET;
    sa.sin_addr.s_addr = htonl(INADDR_ANY);
    sa.sin_port = htons((uint16_t)port);
    if (bind(srv, (struct sockaddr *)&sa, sizeof sa) < 0) {
        perror("bind");
        return 1;
    }
    if (listen(srv, 16) < 0) {
        perror("listen");
        return 1;
    }
    fprintf(stderr, "burst-echo-server: listening on port %d\n", port);

    uint8_t *buf = (uint8_t *)malloc(MAX_W);
    if (!buf) {
        perror("malloc");
        return 1;
    }

    for (;;) {
        int cli = accept(srv, NULL, NULL);
        if (cli < 0) {
            if (errno == EINTR) continue;
            perror("accept");
            continue;
        }
        int yes = 1;
        setsockopt(cli, IPPROTO_TCP, TCP_NODELAY, &yes, sizeof yes);

        char line[64];
        for (;;) {
            int r = read_line(cli, line, sizeof line);
            if (r <= 0) break;
            uint64_t n = 0, w = 0;
            if (sscanf(line, "BURST %llu %llu",
                       (unsigned long long *)&n,
                       (unsigned long long *)&w) == 2) {
                if (w < 16 || w > MAX_W) {
                    fprintf(stderr,
                            "burst-echo-server: W=%llu out of range "
                            "[16, %d]\n",
                            (unsigned long long)w, MAX_W);
                    break;
                }
                if (send_burst(cli, n, w, buf) < 0) {
                    perror("send_burst");
                    break;
                }
            } else if (strncmp(line, "QUIT", 4) == 0) {
                break;
            } else {
                /* Unknown command — log and keep reading. */
                fprintf(stderr,
                        "burst-echo-server: unknown command: %s",
                        line);
            }
        }
        close(cli);
    }

    free(buf);
    close(srv);
    return 0;
}
