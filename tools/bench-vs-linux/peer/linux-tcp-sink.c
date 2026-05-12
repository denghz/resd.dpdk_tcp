/*
 * bench-vs-linux peer — TCP echo sink.
 *
 * Same shape as bench-e2e/peer/echo-server.c (spec §8: "symlink or
 * duplicate at the peer-deployment step"). Kept as a separate file so
 * the two peer binaries can be deployed independently and versioned
 * alongside their respective bench tools.
 *
 * Listens on argv[1] port; for every accepted connection, reads
 * chunks and echoes them back. Used by mode A (RTT comparison): both
 * dpdk_net and linux_kernel stacks share this peer, so any per-stack
 * delta in measured RTT is strictly client-side (kernel socket path
 * vs. dpdk-net engine) — peer-side timing noise cancels.
 *
 * Build: gcc -O2 -pthread -o linux-tcp-sink linux-tcp-sink.c
 * (or: clang-22 -O2 -pthread -o linux-tcp-sink linux-tcp-sink.c)
 *
 * Correctness + discipline notes (mirrors echo-server.c):
 * - SO_REUSEADDR on the listen socket so a crashed run can re-bind
 *   immediately instead of waiting for TIME-WAIT to drain.
 * - TCP_NODELAY on listen + every accepted socket so Nagle's
 *   algorithm doesn't add 40ms to the first echo on each connection.
 * - SIGPIPE ignored so a write() to a half-closed socket returns
 *   EPIPE on the offending thread instead of terminating the server.
 * - One pthread per connection, detached. Main loop re-accepts
 *   indefinitely; SIGTERM to stop.
 *
 * Robustness (T56 v4, 2026-05-12): TCP_USER_TIMEOUT=5s + 4 MiB sock
 * buffers, identical rationale to echo-server.c.
 */

#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef TCP_USER_TIMEOUT
#define TCP_USER_TIMEOUT 18
#endif

#define SINK_SOCK_BUF_BYTES (4 * 1024 * 1024)
#define SINK_USER_TIMEOUT_MS 5000u

static void *handle(void *arg) {
    int fd = *(int *)arg;
    free(arg);
    char buf[8192];
    while (1) {
        ssize_t n = read(fd, buf, sizeof buf);
        if (n <= 0) break;
        ssize_t m = 0;
        while (m < n) {
            ssize_t w = write(fd, buf + m, n - m);
            if (w <= 0) goto done;
            m += w;
        }
    }
done:
    close(fd);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: linux-tcp-sink <port>\n");
        return 1;
    }
    int port = atoi(argv[1]);
    if (port <= 0 || port > 65535) {
        fprintf(stderr, "invalid port: %s\n", argv[1]);
        return 1;
    }
    signal(SIGPIPE, SIG_IGN);
    int s = socket(AF_INET, SOCK_STREAM, 0);
    if (s < 0) {
        perror("socket");
        return 1;
    }
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    int sock_buf = SINK_SOCK_BUF_BYTES;
    setsockopt(s, SOL_SOCKET, SO_SNDBUF, &sock_buf, sizeof sock_buf);
    setsockopt(s, SOL_SOCKET, SO_RCVBUF, &sock_buf, sizeof sock_buf);
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = htons((unsigned short)port);
    addr.sin_addr.s_addr = INADDR_ANY;
    if (bind(s, (struct sockaddr *)&addr, sizeof addr) < 0) {
        perror("bind");
        return 1;
    }
    if (listen(s, 64) < 0) {
        perror("listen");
        return 1;
    }
    while (1) {
        int c = accept(s, NULL, NULL);
        if (c < 0) {
            if (errno == EINTR) continue;
            perror("accept");
            continue;
        }
        setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
        setsockopt(c, SOL_SOCKET, SO_SNDBUF, &sock_buf, sizeof sock_buf);
        setsockopt(c, SOL_SOCKET, SO_RCVBUF, &sock_buf, sizeof sock_buf);
        unsigned int user_timeout_ms = SINK_USER_TIMEOUT_MS;
        if (setsockopt(c, IPPROTO_TCP, TCP_USER_TIMEOUT,
                       &user_timeout_ms, sizeof user_timeout_ms) < 0) {
            perror("setsockopt TCP_USER_TIMEOUT");
        }
        int *cli_p = malloc(sizeof *cli_p);
        if (!cli_p) {
            perror("malloc");
            close(c);
            continue;
        }
        *cli_p = c;
        pthread_t t;
        if (pthread_create(&t, NULL, handle, cli_p) != 0) {
            perror("pthread_create");
            free(cli_p);
            close(c);
            continue;
        }
        pthread_detach(t);
    }
    /* NOTREACHED */
    return 0;
}
