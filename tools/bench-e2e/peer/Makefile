# bench-e2e peer echo-server.
#
# Deployed to the peer host (a second EC2 instance in the same
# placement group) via scripts/bench-nightly.sh. The dev-host build
# here is a smoke-check only — real nightly runs rebuild on the peer
# host so the binary is linked against that host's libc.
#
# Phase 8 of the 2026-05-09 bench-suite overhaul adds the
# `burst-echo-server` peer binary used by `bench-rx-burst`. Both
# binaries are pure C stdlib, so the same CC/CFLAGS apply.

CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra

all: echo-server burst-echo-server

echo-server: echo-server.c
	$(CC) $(CFLAGS) -o $@ $< -lpthread

burst-echo-server: burst-echo-server.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f echo-server burst-echo-server

.PHONY: all clean
