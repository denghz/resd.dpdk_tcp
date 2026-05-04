# bench-vs-mtcp F-Stack peer.
#
# Builds bench-peer-fstack.c against libfstack.a + DPDK 23.11.
# This Makefile is only invoked on the AMI where libfstack.a + DPDK
# are installed (image-builder component 04b-install-f-stack.yaml).
# Dev-host build is a smoke-check; real nightly runs rebuild on the
# bench-pair AMI against that host's DPDK + libfstack.
#
# F-Stack install layout (per 04b-install-f-stack.yaml):
#   /opt/f-stack/lib/libfstack.a
#   /opt/f-stack/include/  (ff_api.h, ff_config.h, ff_event.h, ...)
#
# DPDK 23.11 is on the system pkg-config path.

CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -Wno-unused-result
FF_PATH ?= /opt/f-stack
PKGCONF ?= pkg-config

CFLAGS += -I$(FF_PATH)/include
CFLAGS += $(shell $(PKGCONF) --cflags libdpdk)

LIBS = -L$(FF_PATH)/lib -Wl,--whole-archive,-lfstack,--no-whole-archive
LIBS += $(shell $(PKGCONF) --static --libs libdpdk)
LIBS += -Wl,--no-whole-archive -lrt -lm -ldl -lcrypto -pthread -lnuma

bench-peer: bench-peer-fstack.c
	$(CC) $(CFLAGS) -DINET6 -o $@ $< $(LIBS)

clean:
	rm -f bench-peer

.PHONY: clean
