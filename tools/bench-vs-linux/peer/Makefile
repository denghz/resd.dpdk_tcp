# bench-vs-linux peer TCP sink.
#
# Deployed to the peer host (sister EC2 instance) via
# scripts/bench-nightly.sh. Same shape as bench-e2e/peer/Makefile —
# dev-host build is a smoke-check; real nightly runs rebuild on the
# peer host against that host's libc.

CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -pthread

all: linux-tcp-sink

linux-tcp-sink: linux-tcp-sink.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f linux-tcp-sink

.PHONY: all clean
