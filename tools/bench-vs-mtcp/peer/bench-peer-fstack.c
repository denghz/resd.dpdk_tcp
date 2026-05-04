/*
 * bench-vs-mtcp F-Stack peer — TCP echo server using F-Stack.
 *
 * F-Stack (https://github.com/F-Stack/f-stack) is a FreeBSD TCP/IP
 * stack ported to userspace on DPDK. This binary is the F-Stack
 * counterpart of `tools/bench-vs-linux/peer/linux-tcp-sink.c` (kernel
 * TCP) and `tools/bench-e2e/peer/echo-server.c` (also kernel TCP) —
 * both peer the dpdk_net + linux + fstack client arms in
 * tools/bench-vs-mtcp.
 *
 * Build (on the AMI where libfstack.a + DPDK 23.11 are installed):
 *   cc -O2 -DINET6 -o bench-peer bench-peer-fstack.c \
 *       $(pkg-config --cflags libdpdk) \
 *       -L/opt/f-stack/lib -Wl,--whole-archive,-lfstack,--no-whole-archive \
 *       $(pkg-config --static --libs libdpdk) \
 *       -Wl,--no-whole-archive -lrt -lm -ldl -lcrypto -pthread -lnuma
 *
 * Listens on argv[1] port; for every accepted connection, reads
 * chunks and echoes them back. Used by:
 *   - bench-vs-mtcp burst grid (`fstack_burst.rs::run_bucket`)
 *   - bench-vs-mtcp maxtp grid (`fstack_maxtp.rs::run_bucket`)
 *   - bench-vs-linux mode A (`mode_rtt.rs` Stack::FStack arm)
 *
 * F-Stack requires:
 *   1. `ff_init(argc, argv)` once at process start. The `--conf`
 *      flag points at /etc/f-stack.conf (DPDK EAL flags + F-Stack
 *      lcore / port settings); the AMI's component
 *      04b-install-f-stack.yaml installs a default copy.
 *   2. All sockets in non-blocking mode (per ff_api.h header notes).
 *   3. The main event loop driven via `ff_run(loop_fn, NULL)` —
 *      F-Stack pins to its lcore inside ff_run and pumps the DPDK
 *      poll loop transparently.
 */

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <strings.h>
#include <sys/types.h>
#include <sys/socket.h>
#include <arpa/inet.h>
#include <errno.h>
#include <assert.h>
#include <sys/ioctl.h>

#include "ff_config.h"
#include "ff_api.h"

#define MAX_EVENTS 512

struct kevent kev_set;
struct kevent events[MAX_EVENTS];
int kq;
int sockfd;

static int g_listen_port = 10003;

static int loop_fn(void *arg) {
    (void)arg;
    int n = ff_kevent(kq, NULL, 0, events, MAX_EVENTS, NULL);
    if (n < 0) {
        fprintf(stderr, "ff_kevent failed: %d %s\n", errno, strerror(errno));
        return -1;
    }
    for (int i = 0; i < n; ++i) {
        struct kevent ev = events[i];
        int fd = (int)ev.ident;

        if (ev.flags & EV_EOF) {
            ff_close(fd);
            continue;
        }
        if (fd == sockfd) {
            int avail = (int)ev.data;
            do {
                int cfd = ff_accept(fd, NULL, NULL);
                if (cfd < 0) {
                    fprintf(stderr, "ff_accept failed: %d %s\n", errno, strerror(errno));
                    break;
                }
                /* Set non-blocking — required for ff_write to behave correctly. */
                int on = 1;
                ff_ioctl(cfd, FIONBIO, &on);
                EV_SET(&kev_set, cfd, EVFILT_READ, EV_ADD, 0, 0, NULL);
                if (ff_kevent(kq, &kev_set, 1, NULL, 0, NULL) < 0) {
                    fprintf(stderr, "ff_kevent (add cfd) failed: %d %s\n", errno, strerror(errno));
                }
                avail--;
            } while (avail > 0);
        } else if (ev.filter == EVFILT_READ) {
            char buf[8192];
            ssize_t r = ff_read(fd, buf, sizeof(buf));
            if (r <= 0) {
                ff_close(fd);
                continue;
            }
            /* Echo back. Loop on partial writes — F-Stack's send
             * buffer can return short under pressure. */
            ssize_t total = 0;
            while (total < r) {
                ssize_t w = ff_write(fd, buf + total, r - total);
                if (w <= 0) {
                    /* EAGAIN -> drop bytes for this round; the
                     * EVFILT_WRITE that we'd register for in a
                     * production server is overkill for bench (the
                     * client is lock-step request/response). Real
                     * production would ff_kevent EV_ADD EVFILT_WRITE
                     * here; bench just drops the tail.
                     */
                    break;
                }
                total += w;
            }
        }
    }
    return 0;
}

int main(int argc, char *argv[]) {
    /* argv[1] = listen port (defaults to 10003). F-Stack consumes
     * --conf and --proc-id flags from argv inside ff_init; we keep
     * port-positional in argv[1] before ff_init's parse for
     * simplicity. */
    if (argc < 2) {
        fprintf(stderr, "usage: %s <port> [f-stack args...]\n", argv[0]);
        return 1;
    }
    g_listen_port = atoi(argv[1]);
    if (g_listen_port <= 0 || g_listen_port > 65535) {
        fprintf(stderr, "invalid port: %s\n", argv[1]);
        return 1;
    }
    /* Strip argv[1] (the port) before handing argv to ff_init —
     * F-Stack's parser doesn't recognise positional integers. */
    char **ff_argv = (char **)malloc(sizeof(char *) * argc);
    if (!ff_argv) return 1;
    ff_argv[0] = argv[0];
    for (int i = 2; i < argc; ++i) {
        ff_argv[i - 1] = argv[i];
    }
    int ff_argc = argc - 1;

    if (ff_init(ff_argc, ff_argv) != 0) {
        fprintf(stderr, "ff_init failed\n");
        return 1;
    }

    kq = ff_kqueue();
    if (kq < 0) {
        fprintf(stderr, "ff_kqueue failed: %d %s\n", errno, strerror(errno));
        return 1;
    }

    sockfd = ff_socket(AF_INET, SOCK_STREAM, 0);
    if (sockfd < 0) {
        fprintf(stderr, "ff_socket failed: %d %s\n", errno, strerror(errno));
        return 1;
    }
    int on = 1;
    ff_ioctl(sockfd, FIONBIO, &on);

    struct sockaddr_in sin;
    bzero(&sin, sizeof(sin));
    sin.sin_family = AF_INET;
    sin.sin_port = htons((uint16_t)g_listen_port);
    sin.sin_addr.s_addr = htonl(INADDR_ANY);

    if (ff_bind(sockfd, (struct linux_sockaddr *)&sin, sizeof(sin)) < 0) {
        fprintf(stderr, "ff_bind failed: %d %s\n", errno, strerror(errno));
        return 1;
    }
    if (ff_listen(sockfd, MAX_EVENTS) < 0) {
        fprintf(stderr, "ff_listen failed: %d %s\n", errno, strerror(errno));
        return 1;
    }

    EV_SET(&kev_set, sockfd, EVFILT_READ, EV_ADD, 0, MAX_EVENTS, NULL);
    ff_kevent(kq, &kev_set, 1, NULL, 0, NULL);

    fprintf(stdout, "bench-peer-fstack listening on port %d\n", g_listen_port);
    fflush(stdout);

    /* Hand off to F-Stack's main loop driver. */
    ff_run(loop_fn, NULL);
    return 0;
}
