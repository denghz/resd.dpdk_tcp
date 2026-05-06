/*
 * bench-vs-mtcp peer echo server (mTCP-side, DPDK 20.11 sidecar build).
 *
 * A multi-core mTCP echo server. Listens on the port supplied via -P and
 * mirrors every byte received back to the sender. Mirrors the shape of
 * tools/bench-e2e/peer/echo-server.c (which is the kernel-TCP arm) so
 * dimensions_json.peer_topology stays comparable across stacks.
 *
 * Design points (deliberate divergences from the upstream `epserver.c`
 * sample mTCP ships):
 *   - No HTTP / file-cache layer. We read whatever bytes arrive and
 *     write them straight back via mtcp_write(). Bench traffic is
 *     opaque from the server's perspective.
 *   - One mTCP context per core (mTCP's RSS-pinned design — one core
 *     handles one RSS queue's worth of flows). Cores 0..N-1 are
 *     pthread-launched; the last core stays free for the kernel.
 *   - Non-blocking sockets + mtcp_epoll wait loop so a single thread
 *     handles many concurrent connections — bench-vs-mtcp drives up to
 *     C=128 concurrent sockets per arm in spec §11.2's maxtp grid.
 *   - SIGINT triggers a graceful shutdown: every per-core mtcp context
 *     is destroyed and the process exits 0 so the orchestrator can
 *     diff peer logs deterministically.
 *
 * Build: see ./Makefile (depends on libmtcp.a + DPDK 20.11 pkg-config).
 * Runtime: requires the mTCP startup config file (-f) and a CPU mask /
 * lcore count consistent with the mtcp_conf.num_cores setting. mTCP
 * also requires hugepages + vfio-pci-bound NIC, exactly as the
 * dpdk_net engine does.
 *
 * Usage:
 *   bench-peer -f mtcp.conf -P 10001 [-N <num_cores>] [-c <starting_cpu>]
 *
 * Spec §11.1 / §11.2 wire it as the mTCP comparator:
 *   /opt/mtcp-peer/bench-peer -f /opt/mtcp-peer/mtcp.conf -P 10001
 */

#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>

#include <mtcp_api.h>
#include <mtcp_epoll.h>

/*
 * Tunables. Set to a generous cap so the same binary handles bench-vs-
 * mtcp's burst grid (long-lived 1 connection / core) and the maxtp grid
 * (up to 128 concurrent connections in aggregate).
 */
#define MAX_CPUS         16
#define MAX_FLOW_NUM     (8 * 1024)
#define MAX_EVENTS       (MAX_FLOW_NUM * 2)
#define READ_BUF_BYTES   (16 * 1024)

static volatile sig_atomic_t done_flag[MAX_CPUS];

struct thread_args {
    int    core;
    int    listen_port;
};

static void
SignalHandler(int sig)
{
    int i;
    (void)sig;
    /* Asking each core's epoll-loop to exit on next iteration. */
    for (i = 0; i < MAX_CPUS; i++) {
        done_flag[i] = 1;
    }
}

/*
 * Per-flow state. We only need to track whether a write is in-flight
 * (because the previous mtcp_write returned short under EAGAIN). For
 * a pure echo server, the read-side state is the source of truth:
 * we carry over no application-level framing.
 */
struct flow_state {
    /* Bytes pending to drain back to the peer. echo-server.c uses a
     * stack buffer; we stash it on the flow so we can resume across
     * EPOLLOUT cycles without losing data. */
    char     buf[READ_BUF_BYTES];
    int      pending_off;
    int      pending_len;
};

/*
 * Epoll-event-driven echo loop on one mTCP context. epserver.c uses
 * the same shape — we keep this trimmed to the strict echo case so
 * the binary stays small (~30 KB on disk vs ~120 KB for epserver).
 */
static void *
RunEchoCore(void *arg)
{
    struct thread_args *targs = (struct thread_args *)arg;
    int core = targs->core;
    int listen_port = targs->listen_port;
    mctx_t mctx;
    int ep;
    int listener;
    struct mtcp_epoll_event ev;
    struct mtcp_epoll_event *events;
    struct sockaddr_in saddr;
    struct flow_state *flows;
    int i;
    int n;
    int do_accept;

    /* mTCP requires we affinitise the application thread to its core
     * BEFORE creating the mTCP context. */
    if (mtcp_core_affinitize(core) < 0) {
        fprintf(stderr, "[core %d] mtcp_core_affinitize failed\n", core);
        return NULL;
    }

    mctx = mtcp_create_context(core);
    if (!mctx) {
        fprintf(stderr, "[core %d] mtcp_create_context failed\n", core);
        return NULL;
    }

    ep = mtcp_epoll_create(mctx, MAX_EVENTS);
    if (ep < 0) {
        fprintf(stderr, "[core %d] mtcp_epoll_create failed\n", core);
        mtcp_destroy_context(mctx);
        return NULL;
    }

    flows = calloc(MAX_FLOW_NUM, sizeof(*flows));
    if (!flows) {
        fprintf(stderr, "[core %d] calloc(flows) failed\n", core);
        mtcp_destroy_context(mctx);
        return NULL;
    }

    events = calloc(MAX_EVENTS, sizeof(*events));
    if (!events) {
        fprintf(stderr, "[core %d] calloc(events) failed\n", core);
        free(flows);
        mtcp_destroy_context(mctx);
        return NULL;
    }

    listener = mtcp_socket(mctx, AF_INET, SOCK_STREAM, 0);
    if (listener < 0) {
        fprintf(stderr, "[core %d] mtcp_socket failed\n", core);
        goto cleanup;
    }

    if (mtcp_setsock_nonblock(mctx, listener) < 0) {
        fprintf(stderr, "[core %d] mtcp_setsock_nonblock(listener) failed\n",
                core);
        goto cleanup;
    }

    memset(&saddr, 0, sizeof(saddr));
    saddr.sin_family = AF_INET;
    saddr.sin_addr.s_addr = INADDR_ANY;
    saddr.sin_port = htons((unsigned short)listen_port);

    if (mtcp_bind(mctx, listener,
                  (struct sockaddr *)&saddr, sizeof(saddr)) < 0) {
        fprintf(stderr, "[core %d] mtcp_bind(port=%d) failed\n",
                core, listen_port);
        goto cleanup;
    }

    if (mtcp_listen(mctx, listener, /*backlog=*/4096) < 0) {
        fprintf(stderr, "[core %d] mtcp_listen failed\n", core);
        goto cleanup;
    }

    ev.events = MTCP_EPOLLIN;
    ev.data.sockid = listener;
    mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_ADD, listener, &ev);

    fprintf(stdout, "[core %d] bench-peer ready on port %d\n",
            core, listen_port);
    fflush(stdout);

    while (!done_flag[core]) {
        n = mtcp_epoll_wait(mctx, ep, events, MAX_EVENTS, /*timeout=*/-1);
        if (n < 0) {
            if (errno == EINTR)
                continue;
            fprintf(stderr, "[core %d] mtcp_epoll_wait failed: %s\n",
                    core, strerror(errno));
            break;
        }

        do_accept = 0;
        for (i = 0; i < n; i++) {
            int sockid = events[i].data.sockid;

            if (sockid == listener) {
                do_accept = 1;
                continue;
            }

            if (events[i].events & MTCP_EPOLLERR) {
                mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_DEL, sockid, NULL);
                mtcp_close(mctx, sockid);
                if (sockid >= 0 && sockid < MAX_FLOW_NUM) {
                    flows[sockid].pending_off = 0;
                    flows[sockid].pending_len = 0;
                }
                continue;
            }

            if (events[i].events & MTCP_EPOLLIN) {
                /* Drain readable bytes and echo them back. We loop
                 * because mTCP's EPOLLIN is edge-triggered-ish — the
                 * next wait won't refire until more bytes arrive, so
                 * we have to drain the current backlog now. */
                while (1) {
                    ssize_t r;
                    char *buf;
                    int wrote;

                    if (sockid < 0 || sockid >= MAX_FLOW_NUM) {
                        break;
                    }
                    /* If a previous write left bytes pending, retry
                     * those first before reading more. */
                    if (flows[sockid].pending_len > 0) {
                        wrote = mtcp_write(mctx, sockid,
                            flows[sockid].buf + flows[sockid].pending_off,
                            flows[sockid].pending_len);
                        if (wrote < 0) {
                            if (errno == EAGAIN) {
                                /* re-arm OUT and bail — caller will
                                 * be re-notified on EPOLLOUT */
                                ev.events = MTCP_EPOLLIN | MTCP_EPOLLOUT;
                                ev.data.sockid = sockid;
                                mtcp_epoll_ctl(mctx, ep,
                                    MTCP_EPOLL_CTL_MOD, sockid, &ev);
                                break;
                            }
                            mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_DEL,
                                           sockid, NULL);
                            mtcp_close(mctx, sockid);
                            flows[sockid].pending_len = 0;
                            break;
                        }
                        flows[sockid].pending_off += wrote;
                        flows[sockid].pending_len -= wrote;
                        if (flows[sockid].pending_len > 0)
                            break;
                    }

                    buf = flows[sockid].buf;
                    r = mtcp_read(mctx, sockid, buf, READ_BUF_BYTES);
                    if (r < 0) {
                        if (errno == EAGAIN)
                            break;
                        mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_DEL,
                                       sockid, NULL);
                        mtcp_close(mctx, sockid);
                        break;
                    }
                    if (r == 0) {
                        /* peer half-closed */
                        mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_DEL,
                                       sockid, NULL);
                        mtcp_close(mctx, sockid);
                        break;
                    }

                    wrote = mtcp_write(mctx, sockid, buf, r);
                    if (wrote < 0) {
                        if (errno == EAGAIN) {
                            flows[sockid].pending_off = 0;
                            flows[sockid].pending_len = (int)r;
                            ev.events = MTCP_EPOLLIN | MTCP_EPOLLOUT;
                            ev.data.sockid = sockid;
                            mtcp_epoll_ctl(mctx, ep,
                                MTCP_EPOLL_CTL_MOD, sockid, &ev);
                            break;
                        }
                        mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_DEL,
                                       sockid, NULL);
                        mtcp_close(mctx, sockid);
                        break;
                    }
                    if (wrote < r) {
                        /* partial write — stash remainder for the
                         * EPOLLOUT path next time around */
                        memmove(flows[sockid].buf, buf + wrote, r - wrote);
                        flows[sockid].pending_off = 0;
                        flows[sockid].pending_len = (int)(r - wrote);
                        ev.events = MTCP_EPOLLIN | MTCP_EPOLLOUT;
                        ev.data.sockid = sockid;
                        mtcp_epoll_ctl(mctx, ep,
                            MTCP_EPOLL_CTL_MOD, sockid, &ev);
                        break;
                    }
                }
                continue;
            }

            if (events[i].events & MTCP_EPOLLOUT) {
                /* Drain pending bytes from a previous short write. */
                if (sockid >= 0 && sockid < MAX_FLOW_NUM
                        && flows[sockid].pending_len > 0) {
                    int wrote = mtcp_write(mctx, sockid,
                        flows[sockid].buf + flows[sockid].pending_off,
                        flows[sockid].pending_len);
                    if (wrote < 0) {
                        if (errno != EAGAIN) {
                            mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_DEL,
                                           sockid, NULL);
                            mtcp_close(mctx, sockid);
                            flows[sockid].pending_len = 0;
                        }
                    } else {
                        flows[sockid].pending_off += wrote;
                        flows[sockid].pending_len -= wrote;
                        if (flows[sockid].pending_len == 0) {
                            /* drop OUT, keep IN */
                            ev.events = MTCP_EPOLLIN;
                            ev.data.sockid = sockid;
                            mtcp_epoll_ctl(mctx, ep,
                                MTCP_EPOLL_CTL_MOD, sockid, &ev);
                        }
                    }
                }
                continue;
            }
        }

        if (do_accept) {
            while (1) {
                int c = mtcp_accept(mctx, listener, NULL, NULL);
                if (c < 0) {
                    if (errno != EAGAIN) {
                        fprintf(stderr,
                            "[core %d] mtcp_accept error: %s\n",
                            core, strerror(errno));
                    }
                    break;
                }
                if (mtcp_setsock_nonblock(mctx, c) < 0) {
                    mtcp_close(mctx, c);
                    continue;
                }
                if (c >= 0 && c < MAX_FLOW_NUM) {
                    flows[c].pending_off = 0;
                    flows[c].pending_len = 0;
                }
                ev.events = MTCP_EPOLLIN;
                ev.data.sockid = c;
                mtcp_epoll_ctl(mctx, ep, MTCP_EPOLL_CTL_ADD, c, &ev);
            }
        }
    }

cleanup:
    free(events);
    free(flows);
    mtcp_destroy_context(mctx);
    return NULL;
}

static void
usage(const char *argv0)
{
    fprintf(stderr,
        "usage: %s -f <mtcp.conf> [-P <port>] [-N <num_cores>] [-c <start_cpu>]\n"
        "  -f  mTCP startup configuration file (required)\n"
        "  -P  TCP listen port (default 10001)\n"
        "  -N  number of mTCP cores to spin up (default = num CPUs)\n"
        "  -c  index of starting CPU (default 0)\n"
        "  -h  print this help\n",
        argv0);
}

int
main(int argc, char **argv)
{
    const char *conf_file = NULL;
    int listen_port = 10001;
    int core_limit = -1;
    int start_cpu = 0;
    int o;
    int i;
    int rc;
    int num_cpus;
    pthread_t threads[MAX_CPUS];
    struct thread_args args[MAX_CPUS];
    struct mtcp_conf mcfg;

    while (-1 != (o = getopt(argc, argv, "f:P:N:c:h"))) {
        switch (o) {
        case 'f': conf_file = optarg; break;
        case 'P': listen_port = atoi(optarg); break;
        case 'N': core_limit = atoi(optarg); break;
        case 'c': start_cpu = atoi(optarg); break;
        case 'h': usage(argv[0]); return 0;
        default:  usage(argv[0]); return 2;
        }
    }

    if (!conf_file) {
        fprintf(stderr, "ERROR: -f <mtcp.conf> is required\n");
        usage(argv[0]);
        return 2;
    }
    if (listen_port <= 0 || listen_port > 65535) {
        fprintf(stderr, "ERROR: invalid -P %d\n", listen_port);
        return 2;
    }

    num_cpus = sysconf(_SC_NPROCESSORS_ONLN);
    if (num_cpus <= 0)
        num_cpus = 1;
    if (num_cpus > MAX_CPUS)
        num_cpus = MAX_CPUS;
    if (core_limit < 0 || core_limit > num_cpus)
        core_limit = num_cpus;

    /* Pin core_limit on the mTCP config BEFORE mtcp_init so the EAL
     * thread topology matches our pthread layout. */
    rc = mtcp_init(conf_file);
    if (rc) {
        fprintf(stderr, "ERROR: mtcp_init(%s) failed (%d)\n", conf_file, rc);
        return 1;
    }

    if (mtcp_getconf(&mcfg) == 0) {
        mcfg.num_cores = core_limit;
        mtcp_setconf(&mcfg);
    }

    /* mTCP installs its own signal handlers on init; route them at
     * the application thread so SIGINT triggers our graceful shutdown
     * via done_flag[]. */
    mtcp_register_signal(SIGINT, SignalHandler);
    mtcp_register_signal(SIGTERM, SignalHandler);

    for (i = 0; i < core_limit; i++) {
        done_flag[i] = 0;
        args[i].core = start_cpu + i;
        args[i].listen_port = listen_port;
        if (pthread_create(&threads[i], NULL, RunEchoCore, &args[i]) != 0) {
            fprintf(stderr, "ERROR: pthread_create(core %d) failed\n",
                    args[i].core);
            return 1;
        }
    }

    for (i = 0; i < core_limit; i++) {
        pthread_join(threads[i], NULL);
    }

    mtcp_destroy();
    return 0;
}
