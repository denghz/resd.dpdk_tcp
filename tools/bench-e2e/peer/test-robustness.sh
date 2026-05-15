#!/usr/bin/env bash
#
# test-robustness.sh — smoke test for the three peer servers' SIGKILL
# resilience (T56 v4, 2026-05-12).
#
# Each server now does pthread-per-connection + TCP_USER_TIMEOUT=5s.
# Property under test: a SIGKILLed client doesn't wedge the accept
# loop, and other concurrent clients keep making progress.
#
# Test plan per server:
#   1. Spawn server on 127.0.0.1:<unused port>.
#   2. Open conn-A with `nc` reading a long-ish data stream (or `nc -i`
#      keeping the socket open).
#   3. Open conn-B similarly.
#   4. SIGKILL conn-A's nc process mid-stream.
#   5. Verify conn-B still reads new data within ~2s.
#   6. Open conn-C from scratch; verify accept loop took it.
#   7. Tear everything down; assert server still running (didn't crash
#      from a SIGPIPE escape, malloc failure, etc).
#
# Notes:
# - Uses ports 21001/21002/21003 on 127.0.0.1 (above the bench peer
#   range so a stray running real-bench peer doesn't clash).
# - Requires `nc` and `python3` on PATH.
# - Cleans up on exit via trap.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

# Helper: SIGKILL silently. Bash prints "Killed" to stderr when a foreground
# / background job is reaped via SIGKILL; we don't want that noise.
quiet_kill() {
    { kill -KILL "$1" 2>/dev/null; wait "$1" 2>/dev/null; } 2>/dev/null || true
}

ECHO_BIN="$(pwd)/echo-server"
BURST_BIN="$(pwd)/burst-echo-server"
SINK_BIN="$(cd ../../bench-vs-linux/peer && pwd)/linux-tcp-sink"

for bin in "$ECHO_BIN" "$BURST_BIN" "$SINK_BIN"; do
    [ -x "$bin" ] || {
        echo "test-robustness: $bin not built — run \`make -C tools/bench-e2e/peer all\` + \`make -C tools/bench-vs-linux/peer\` first" >&2
        exit 2
    }
done
command -v nc      >/dev/null || { echo "test-robustness: nc missing"      >&2; exit 2; }
command -v python3 >/dev/null || { echo "test-robustness: python3 missing" >&2; exit 2; }

# All server PIDs go here so the trap can kill them.
declare -a SERVER_PIDS=()
declare -a CLIENT_PIDS=()
TMPDIR_T=$(mktemp -d)

cleanup() {
    for pid in "${CLIENT_PIDS[@]}"; do
        quiet_kill "$pid"
    done
    for pid in "${SERVER_PIDS[@]}"; do
        quiet_kill "$pid"
    done
    rm -rf "$TMPDIR_T"
}
trap cleanup EXIT

fail() {
    printf '\nFAIL: %s\n' "$*" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Helper: wait for a TCP port to be listening (max 2 s).
# ---------------------------------------------------------------------------
wait_for_port() {
    local port="$1"
    for _ in $(seq 1 40); do
        if (echo >/dev/tcp/127.0.0.1/"$port") 2>/dev/null; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

# ---------------------------------------------------------------------------
# Test 1: echo-server — full-duplex echo.
# Open two concurrent connections, SIGKILL one, verify the other still echoes.
# ---------------------------------------------------------------------------
echo "[1/3] echo-server SIGKILL-resilience"
ECHO_PORT=21001
"$ECHO_BIN" "$ECHO_PORT" >"$TMPDIR_T/echo.log" 2>&1 &
SERVER_PIDS+=($!)
ECHO_SERVER_PID=$!
wait_for_port "$ECHO_PORT" || fail "echo-server didn't open port $ECHO_PORT"

# Conn-A: python loop sending data forever; we SIGKILL it later.
python3 -u -c "
import socket, time, sys
s = socket.socket()
s.connect(('127.0.0.1', $ECHO_PORT))
s.settimeout(2.0)
while True:
    s.send(b'A' * 64)
    s.recv(64)
    time.sleep(0.01)
" >"$TMPDIR_T/echo-A.log" 2>&1 &
CONN_A_PID=$!
CLIENT_PIDS+=("$CONN_A_PID")

# Conn-B: python sender that writes a known byte string and verifies echo.
# Captures a marker file once it has confirmed at least 5 round-trips.
python3 -u -c "
import socket
s = socket.socket()
s.connect(('127.0.0.1', $ECHO_PORT))
s.settimeout(3.0)
for i in range(50):
    msg = f'B{i:08d}'.encode()
    s.send(msg)
    got = s.recv(len(msg))
    assert got == msg, (i, got, msg)
    if i == 4:
        open('$TMPDIR_T/echo-B-precheck', 'w').close()
open('$TMPDIR_T/echo-B-ok', 'w').close()
" >"$TMPDIR_T/echo-B.log" 2>&1 &
CONN_B_PID=$!
CLIENT_PIDS+=("$CONN_B_PID")

# Wait for B to get past 5 round-trips, then SIGKILL A mid-stream.
for _ in $(seq 1 60); do
    [ -f "$TMPDIR_T/echo-B-precheck" ] && break
    sleep 0.05
done
[ -f "$TMPDIR_T/echo-B-precheck" ] || fail "echo: conn-B never reached precheck (server stuck?)"

quiet_kill "$CONN_A_PID"

# B must now finish all 50 round-trips within ~5 s.
for _ in $(seq 1 100); do
    [ -f "$TMPDIR_T/echo-B-ok" ] && break
    sleep 0.05
done
[ -f "$TMPDIR_T/echo-B-ok" ] || fail "echo: conn-B stalled after conn-A SIGKILL (accept loop wedged or thread starvation)"

# Conn-C: brand-new connection AFTER the kill. Must connect + echo.
python3 -u -c "
import socket
s = socket.socket()
s.settimeout(3.0)
s.connect(('127.0.0.1', $ECHO_PORT))
s.send(b'CCCCC')
got = s.recv(5)
assert got == b'CCCCC', got
" >"$TMPDIR_T/echo-C.log" 2>&1 || fail "echo: conn-C couldn't connect/echo after kill (cat $TMPDIR_T/echo-C.log)"

# Server must still be alive.
kill -0 "$ECHO_SERVER_PID" 2>/dev/null || fail "echo-server: process died"
echo "    ok: B finished + new conn-C succeeded"

# ---------------------------------------------------------------------------
# Test 2: linux-tcp-sink — same shape (it's an echo too, with sink port).
# ---------------------------------------------------------------------------
echo "[2/3] linux-tcp-sink SIGKILL-resilience"
SINK_PORT=21002
"$SINK_BIN" "$SINK_PORT" >"$TMPDIR_T/sink.log" 2>&1 &
SERVER_PIDS+=($!)
SINK_SERVER_PID=$!
wait_for_port "$SINK_PORT" || fail "linux-tcp-sink didn't open port $SINK_PORT"

python3 -u -c "
import socket, time
s = socket.socket()
s.connect(('127.0.0.1', $SINK_PORT))
s.settimeout(2.0)
while True:
    s.send(b'A' * 64)
    s.recv(64)
    time.sleep(0.01)
" >"$TMPDIR_T/sink-A.log" 2>&1 &
CONN_A_PID=$!
CLIENT_PIDS+=("$CONN_A_PID")

python3 -u -c "
import socket
s = socket.socket()
s.connect(('127.0.0.1', $SINK_PORT))
s.settimeout(3.0)
for i in range(50):
    msg = f'B{i:08d}'.encode()
    s.send(msg)
    got = s.recv(len(msg))
    assert got == msg, (i, got, msg)
    if i == 4:
        open('$TMPDIR_T/sink-B-precheck', 'w').close()
open('$TMPDIR_T/sink-B-ok', 'w').close()
" >"$TMPDIR_T/sink-B.log" 2>&1 &
CONN_B_PID=$!
CLIENT_PIDS+=("$CONN_B_PID")

for _ in $(seq 1 60); do
    [ -f "$TMPDIR_T/sink-B-precheck" ] && break
    sleep 0.05
done
[ -f "$TMPDIR_T/sink-B-precheck" ] || fail "sink: conn-B never reached precheck"

quiet_kill "$CONN_A_PID"

for _ in $(seq 1 100); do
    [ -f "$TMPDIR_T/sink-B-ok" ] && break
    sleep 0.05
done
[ -f "$TMPDIR_T/sink-B-ok" ] || fail "sink: conn-B stalled after conn-A SIGKILL"

python3 -u -c "
import socket
s = socket.socket()
s.settimeout(3.0)
s.connect(('127.0.0.1', $SINK_PORT))
s.send(b'CCCCC')
got = s.recv(5)
assert got == b'CCCCC', got
" >"$TMPDIR_T/sink-C.log" 2>&1 || fail "sink: conn-C couldn't connect/echo after kill"

kill -0 "$SINK_SERVER_PID" 2>/dev/null || fail "linux-tcp-sink: process died"
echo "    ok: B finished + new conn-C succeeded"

# ---------------------------------------------------------------------------
# Test 3: burst-echo-server — BURST <N> <W> protocol.
# A wedged write() in conn-A used to freeze the accept loop. Verify a
# concurrent conn-B still gets its BURST response, and conn-C connects
# fresh after the kill.
# ---------------------------------------------------------------------------
echo "[3/3] burst-echo-server SIGKILL-resilience"
BURST_PORT=21003
"$BURST_BIN" "$BURST_PORT" >"$TMPDIR_T/burst.log" 2>&1 &
SERVER_PIDS+=($!)
BURST_SERVER_PID=$!
wait_for_port "$BURST_PORT" || fail "burst-echo-server didn't open port $BURST_PORT"

# Conn-A: ask for a huge burst and DON'T read it. Kernel send buffer
# fills + server's write() blocks. SIGKILL the client to leave the
# server's worker thread stuck in retransmit limbo.
python3 -u -c "
import socket, time
s = socket.socket()
s.connect(('127.0.0.1', $BURST_PORT))
# 100k segs × 1 KiB = 100 MiB; well past the 4 MiB sock buf so the
# server's write() must block waiting for ACKs we will never send.
s.send(b'BURST 100000 1024\n')
time.sleep(60)
" >"$TMPDIR_T/burst-A.log" 2>&1 &
CONN_A_PID=$!
CLIENT_PIDS+=("$CONN_A_PID")

# Give conn-A a moment to send its BURST command + the server to start
# wedging on the write.
sleep 0.5

# Conn-B: small burst, drain it. Must complete even though conn-A is wedged.
python3 -u -c "
import socket
s = socket.socket()
s.connect(('127.0.0.1', $BURST_PORT))
s.settimeout(5.0)
# 32 segs × 64 B = 2 KiB. Should fly in well under a second.
s.send(b'BURST 32 64\n')
remain = 32 * 64
buf = b''
while remain > 0:
    chunk = s.recv(min(remain, 4096))
    if not chunk:
        raise RuntimeError('EOF mid-burst')
    buf += chunk
    remain -= len(chunk)
assert len(buf) == 32 * 64, len(buf)
open('$TMPDIR_T/burst-B-ok', 'w').close()
" >"$TMPDIR_T/burst-B.log" 2>&1 &
CONN_B_PID=$!
CLIENT_PIDS+=("$CONN_B_PID")

# Wait up to 5 s for B to complete. If the accept loop or the server
# is single-threaded, B never gets served until A unwedges (~15 min).
for _ in $(seq 1 100); do
    [ -f "$TMPDIR_T/burst-B-ok" ] && break
    sleep 0.05
done
[ -f "$TMPDIR_T/burst-B-ok" ] || fail "burst: conn-B never finished — accept loop wedged or single-threaded handler"

# Now SIGKILL conn-A and verify a third connection still gets served.
quiet_kill "$CONN_A_PID"

python3 -u -c "
import socket
s = socket.socket()
s.settimeout(5.0)
s.connect(('127.0.0.1', $BURST_PORT))
s.send(b'BURST 16 64\n')
remain = 16 * 64
while remain > 0:
    chunk = s.recv(min(remain, 4096))
    if not chunk:
        raise RuntimeError('EOF mid-burst')
    remain -= len(chunk)
" >"$TMPDIR_T/burst-C.log" 2>&1 || fail "burst: conn-C couldn't connect/burst after kill"

kill -0 "$BURST_SERVER_PID" 2>/dev/null || fail "burst-echo-server: process died"
echo "    ok: B finished + new conn-C succeeded"

echo
echo "all 3 servers passed SIGKILL-resilience smoke (T56 v4)"
