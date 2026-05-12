# bench-e2e peer echo-server.
#
# Deployed to the peer host (a second EC2 instance in the same
# placement group) via scripts/bench-nightly.sh. The dev-host build
# here is a smoke-check only — real nightly runs rebuild on the peer
# host so the binary is linked against that host's libc.
#
# Phase 8 of the 2026-05-09 bench-suite overhaul adds the
# `burst-echo-server` peer binary used by `bench-rx-burst`. Both
# binaries are pure C stdlib + pthread, so the same CC/CFLAGS apply.
# T56 v4 (2026-05-12) hardened burst-echo-server with pthread-per-
# connection so it now also needs the pthread runtime.

CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -pthread

all: echo-server burst-echo-server

echo-server: echo-server.c
	$(CC) $(CFLAGS) -o $@ $<

burst-echo-server: burst-echo-server.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f echo-server burst-echo-server

.PHONY: all clean
