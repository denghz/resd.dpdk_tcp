# resd.dpdk_tcp — Design Spec

Date: 2026-04-17
Status: Draft, pending user approval

## 1. Purpose and Scope

`resd.dpdk_tcp` is a DPDK-based userspace network stack implemented in Rust and exposed to C++ applications via a stable C ABI. It is purpose-built for low-latency trading infrastructure: a trading strategy process connects to a small number (≤100) of exchange venues over REST (HTTP/1.1) and WebSocket, both carried on TCP/TLS. The stack runs alongside user application code on the same DPDK lcore in a run-to-completion loop, with no cross-lcore rings on the hot path.

Non-goals: server-side TCP in production, IPv6, HTTP/2, HTTP/3, WebSocket compression, TCP Fast Open, sophisticated congestion control by default, millions of connections, kernel-compatible socket emulation.

### 1.1 Design tenets

- **Latency over throughput**: defaults favor low latency even when they diverge from RFC-recommended behavior. Any aggregation feature is opt-in.
- **Stability is a first-class feature**: safe languages, memory-correct parsers, small attack surface, WAN-tested under induced loss/reorder.
- **Observability through primitives, not framework**: stack exports raw counters, timestamps on every event, and state-change events. Aggregation (histograms, tracing, export endpoints) happens in the application using existing infrastructure.
- **RFC behavior is tested, not claimed**: conformance is proved by running opensource RFC-conformance suites against the stack; anything unclear is resolved by referring to the RFC.
- **Flexible API**: epoll-like pull model for stage 1, callback-style can layer on top later.

## 2. Architecture

```
  ┌─────────────────────────────────────────────────────────────────┐
  │                 C++ Application (strategy, order mgr)           │
  └───────────────────────────┬─────────────────────────────────────┘
                              │ cbindgen C ABI header (resd_net.h)
  ┌───────────────────────────▼─────────────────────────────────────┐
  │  libresd_net  (Rust)                                             │
  │                                                                  │
  │  Public API (extern "C"):                                        │
  │    engine lifecycle, connection, HTTP, (TLS), (WS), poll, timers│
  │                                                                  │
  │  Per-lcore engine (run-to-completion loop):                     │
  │    rx_burst → ip → tcp → tls → http/ws → user callback          │
  │                                        ↓                         │
  │                                 user sends →  http/ws → tls      │
  │                                              → tcp → tx_burst    │
  │                                                                  │
  │  Modules:                                                        │
  │    l2/l3/ip, tcp, tls (rustls), http/1.1, ws/rfc6455,           │
  │    flow table, timers, mempools, observability-primitives        │
  └──────────────────────────────┬──────────────────────────────────┘
                                 │ DPDK EAL, PMD, mempool (DPDK 23.11)
                                 ▼
                           NIC (SR-IOV VF / PF)
```

### 2.1 Phases

- **Stage 1 (MVP)**: IPv4 + TCP + HTTP/1.1 + epoll-like API + observability primitives. No TLS, no WebSocket. End-to-end gate: place an order against a staging exchange over plaintext HTTP/1.1.
- **Stage 2**: Inline TLS 1.3 (rustls with `aws-lc-rs` backend); TLS 1.2 behind a feature flag for legacy venues.
- **Stage 3**: WebSocket (RFC 6455) client. Client-initiated close, client-side masking, ping/pong autoreply. No `permessage-deflate`.
- **Stage 4**: Hardening — WAN A/B harness, fuzz-at-scale, documented RFC compliance matrix, shadow-mode deployment.

### 2.2 Build / language / FFI

- Rust workspace, `cargo` build, pinning DPDK LTS 23.11 via `bindgen`.
- `cbindgen` generates `resd_net.h` for C++ consumers.
- Public API uses `extern "C"` with primitive / opaque-pointer types only — no Rust-only types leak.
- C++ integration sample ships as a test consumer.

## 3. Threading and Runtime Model

- **One engine per lcore.** Caller pins itself to an lcore before calling `resd_net_engine_create(lcore_id, &cfg)`.
- **User code lives on the same lcore as the stack.** Run-to-completion: the user's event loop repeatedly calls `resd_net_poll`, which runs rx_burst → stack → emits events → user handles events inline → user-initiated sends batch into the next tx_burst.
- **No cross-lcore rings on the hot path.** Connections are pinned to lcores at `connect()` time; the application chooses the assignment.
- **Typical deployment**: one lcore per market-data feed (high-pps inbound WebSocket), one lcore for order entry (few latency-critical REST/WS connections), plus strategy/business-logic cores communicating with the stack lcores via the application's own existing mechanisms.
- **Callback safety contract**: user code invoked on the stack lcore MUST be `noexcept` (C++) and non-panicking (Rust). An escaped C++ exception or Rust panic across the FFI boundary is undefined behavior and will abort the process. Callbacks that can fail should set an error flag the poll loop inspects, not throw. A CI test asserts this by running fuzzed callbacks that throw/panic and confirming the abort vs. UB diagnosis is clean.

## 4. Public API (Stage 1)

```c
/* ===== Engine ===== */
typedef struct resd_net_engine resd_net_engine_t;

typedef struct {
    uint16_t port_id;
    uint16_t rx_queue_id;
    uint16_t tx_queue_id;
    uint32_t max_connections;      /* sized ≥ expected, e.g. 16 */
    uint32_t recv_buffer_bytes;    /* per-conn; default 256KiB */
    uint32_t send_buffer_bytes;    /* per-conn; default 256KiB */
    uint32_t tcp_mss;              /* 0 = derive from PMTUD */
    bool     tcp_timestamps;       /* RFC 7323, default true */
    bool     tcp_sack;             /* RFC 2018, default true */
    bool     tcp_ecn;              /* RFC 3168, default false */
    uint8_t  cc_mode;              /* 0=off (default), 1=reno, 2=cubic (later) */
    bool     tcp_per_packet_events; /* emit RESD_NET_EVT_TCP_RETRANS etc. per packet;
                                       state-change and alert events are always emitted
                                       regardless of this flag. default false */
} resd_net_engine_config_t;

resd_net_engine_t* resd_net_engine_create(uint16_t lcore_id,
                                          const resd_net_engine_config_t* cfg);
void resd_net_engine_destroy(resd_net_engine_t*);

/* ===== Connection ===== */
typedef uint64_t resd_net_conn_t;    /* opaque handle; 0 = invalid */

typedef struct {
    struct sockaddr_in peer;
    struct sockaddr_in local;        /* 0.0.0.0:0 = pick */
    uint32_t connect_timeout_ms;
    uint32_t idle_keepalive_sec;     /* 0 = off (default) */
} resd_net_connect_opts_t;

int resd_net_connect(resd_net_engine_t*,
                     const resd_net_connect_opts_t*,
                     resd_net_conn_t* out);
int resd_net_close(resd_net_engine_t*, resd_net_conn_t);
int resd_net_shutdown(resd_net_engine_t*, resd_net_conn_t, int how);

/* ===== HTTP/1.1 ===== */
typedef struct {
    const char*     method;
    const char*     path;
    const char*     host;
    const resd_net_header_t* headers;
    uint32_t        headers_count;
    const uint8_t*  body;                /* borrowed until resd_net_poll returns */
    uint32_t        body_len;
} resd_net_http_request_t;

int resd_net_http_request(resd_net_engine_t*,
                          resd_net_conn_t,
                          const resd_net_http_request_t*,
                          uint64_t* req_id_out);

/* ===== Poll ===== */
typedef enum {
    RESD_NET_EVT_CONNECTED = 1,
    RESD_NET_EVT_CLOSED,
    RESD_NET_EVT_ERROR,
    RESD_NET_EVT_HTTP_RESPONSE_HEAD,
    RESD_NET_EVT_HTTP_RESPONSE_BODY,
    RESD_NET_EVT_HTTP_RESPONSE_DONE,
    RESD_NET_EVT_TIMER,
    RESD_NET_EVT_TCP_RETRANS,          /* stability-visibility events */
    RESD_NET_EVT_TCP_LOSS_DETECTED,
    RESD_NET_EVT_TCP_STATE_CHANGE,
    RESD_NET_EVT_TLS_ALERT,            /* stage 2+ */
} resd_net_event_kind_t;

typedef struct {
    resd_net_event_kind_t kind;
    resd_net_conn_t       conn;
    uint64_t              req_id;
    uint16_t              http_status;
    const resd_net_header_t* headers;
    uint32_t              headers_count;
    const uint8_t*        data;            /* borrowed; valid until next poll */
    uint32_t              data_len;
    uint64_t              rx_hw_ts_ns;     /* NIC HW timestamp when available */
    uint64_t              enqueued_ts_ns;  /* TSC when event entered user-visible form */
    int32_t               err;
} resd_net_event_t;

int resd_net_poll(resd_net_engine_t*,
                  resd_net_event_t* events_out,
                  uint32_t max_events,
                  uint64_t timeout_ns);

void resd_net_flush(resd_net_engine_t*);   /* force rte_eth_tx_burst now */

/* ===== Timers & clock ===== */
uint64_t resd_net_now_ns(resd_net_engine_t*);
int      resd_net_timer_add(resd_net_engine_t*, uint64_t deadline_ns, uint64_t user_data);
int      resd_net_timer_cancel(resd_net_engine_t*, uint64_t timer_id);

/* ===== Observability primitives ===== */
const resd_net_counters_t* resd_net_counters(resd_net_engine_t*);
```

### 4.1 Usage pattern

```c
engine = resd_net_engine_create(my_lcore, &cfg);
resd_net_connect(engine, &opts, &conn);

resd_net_event_t events[64];
while (running) {
    int n = resd_net_poll(engine, events, 64, 0);  /* 0 = busy-poll */
    for (int i = 0; i < n; i++) {
        switch (events[i].kind) {
        case RESD_NET_EVT_CONNECTED:
            resd_net_http_request(engine, conn, &req, &req_id);
            resd_net_flush(engine);
            break;
        case RESD_NET_EVT_HTTP_RESPONSE_DONE:
            process_fill(events[i].data, events[i].data_len);
            break;
        }
    }
}
```

### 4.2 API contracts

- `req_id` lets the caller pipeline multiple requests and match responses without ordering assumptions.
- Headers and body pointers in events are **borrowed views** into mbuf memory, valid from the moment `resd_net_poll` returns until the next `resd_net_poll` call on the same engine. The stack refcount-pins every mbuf referenced by any event in `events_out[0..n]` for that window; internal stack processing (including later bursts within the same poll) must not free those mbufs. Caller must `memcpy` out if they need to hold bytes longer.
- `resd_net_flush` drains the current TX batch via exactly one `rte_eth_tx_burst`; no-op when empty; safe to call from inside a user callback (same-lcore, single-threaded re-entry). Call it after a latency-critical send to avoid end-of-poll batching delay.
- `rx_hw_ts_ns` is 0 when the NIC/PMD does not fill hardware timestamps (no `PTYPE_HWTIMESTAMP` support, or dyn-field unavailable). Callers that rely on RX timing must either check for 0 and fall back to `enqueued_ts_ns`, or refuse to run on an unsupported NIC.
- HTTP request body: if `body_len` exceeds a single tx mbuf's payload capacity, the stack splits the body across an mbuf chain internally. The caller passes a single contiguous buffer; the stack allocates the chain from `tx_data_mempool`. If mempool exhaustion prevents allocation, the call returns an error and emits no packets.
- HTTP response body events: each `RESD_NET_EVT_HTTP_RESPONSE_BODY` corresponds to one contiguous mbuf-data region delivered to the parser. Chunked encoding is decoded; the caller sees the decoded (post-chunk-header) bytes. Multiple events may fire per chunk if the chunk crosses mbuf boundaries; `RESD_NET_EVT_HTTP_RESPONSE_DONE` fires once when the final chunk / `Content-Length` body is complete.

## 5. Data Flow

### 5.1 Per-lcore main loop

```c
while (!stop) {
    n = rte_eth_rx_burst(port, q, mbufs, BURST);
    for (i = 0; i < n; i++) {
        pkt = mbufs[i];
        if (!l2_decode(pkt)) { free(pkt); continue; }
        if (!ip_decode(pkt)) { free(pkt); continue; }
        conn = tcp_lookup(pkt);
        if (!conn) { reply_rst(pkt); free(pkt); continue; }
        tcp_input(conn, pkt);
        /* stage 2+: tls_input(conn) decrypts into conn->recv_plaintext */
        http_input(conn);              /* parse, emit events */
        user_callback(conn, event);    /* user may call resd_net_send_* inline */
    }
    tcp_tick(now);                     /* retransmit, RTO, TLP, keepalive, delayed-ACK-off-by-default */
    n = rte_eth_tx_burst(port, q, tx_mbufs, tx_count);
}
```

### 5.2 `resd_net_send` call chain (synchronous, in-line)

```
resd_net_http_request
  → http1_encode    (serialize request line + headers + body into a tx mbuf chain:
                     head mbuf holds line+headers at reserved headroom offset;
                     body > MSS-headroom-bytes is split across chained mbufs
                     allocated from tx_data_mempool)
  → tls_write       (stage 2+; rustls writes record directly into mbuf chain)
  → tcp_output      (segment to MSS-aligned chunks; prepend TCP hdr in reserved
                     headroom of each segment's head mbuf; track for retransmit
                     via mbuf refcount)
  → ip_output       (prepend IP + eth hdrs in reserved headroom)
  → push to TX batch (flushed at end of poll iter, or immediately on flush())
```

### 5.3 Buffer ownership

- RX mbufs owned by stack; delivered to user as `&[u8]` view. Any mbuf referenced by a delivered event is refcount-pinned from `resd_net_poll` return to the next `resd_net_poll` entry; internal stack processing (including later bursts within the same poll iteration) does not free these mbufs until the caller hands the poll back.
- TX mbufs allocated from per-lcore mempool, filled bottom-up with pre-reserved headroom for eth+IP+TCP+TLS-record headers, pushed to next tx_burst. Bodies larger than one mbuf's payload are sent as an mbuf chain (DPDK segmented mbuf).
- Retransmit queue holds mbuf pointers with bumped refcount; on ACK the ref drops and the mbuf returns to the mempool.
- **Retransmit mbuf policy**: a retransmit allocates a fresh mbuf from `tx_hdr_mempool` (header-only, chained back to the original data mbuf) rather than editing the original in place. This costs one allocation per retransmit (rare; negligible aggregate) and eliminates the race where an mbuf currently queued for or in-flight via `rte_eth_tx_burst` would have its TCP options edited under the NIC's DMA. The original data mbuf's refcount is held for the duration; only the TCP/IP/eth header mbuf is fresh. RFC 7323 timestamp-option refresh is done by writing the new `TSval` into the fresh header mbuf.

## 6. TCP Layer

### 6.1 State machine

Full RFC 9293 §3.3.2 eleven-state FSM implemented for client side, including CLOSING / LAST_ACK / TIME_WAIT. Never transition to LISTEN in production. TIME_WAIT duration: 2×MSL (MSL default 30s, tunable).

### 6.2 Per-connection state

```rust
struct TcpConn {
    four_tuple: FourTuple,
    state: TcpState,

    // sequence space (RFC 9293 §3.3.1)
    snd_una: u32, snd_nxt: u32, snd_wnd: u32, snd_wl1: u32, snd_wl2: u32, iss: u32,
    rcv_nxt: u32, rcv_wnd: u32, irs: u32,

    // timers
    rto: Duration, srtt: Option<Duration>, rttvar: Option<Duration>,
    rtx_timer: Option<Instant>, tlp_timer: Option<Instant>,
    delayed_ack_timer: Option<Instant>, keepalive_timer: Option<Instant>,

    // options negotiated at handshake
    ws_shift_out: u8, ws_shift_in: u8,           // RFC 7323
    ts_enabled: bool, ts_recent: u32, ts_recent_age: u64,  // RFC 7323 / PAWS
    sack_enabled: bool,                           // RFC 2018
    ecn_enabled: bool,                            // RFC 3168

    // buffers
    recv: RecvQueue,   // out-of-order + in-order, as mbuf chain
    snd:  SendQueue,   // mbuf refs for retransmit; SACK scoreboard

    // loss detection
    rack: RackState,   // RFC 8985

    // congestion control: None (default); Some(RenoState) when cc_mode=reno
    cc: Option<RenoState>,

    stats: ConnStats,
}
```

### 6.3 RFC compliance matrix (Stage 1 target)

| RFC | Feature | Scope | Notes |
|---|---|---|---|
| 791 | IPv4 | full for client send/recv | TOS/DSCP passthrough, DF always set |
| 792 | ICMP | frag-needed + dest-unreachable (in-only) | drives PMTUD; drop others silently |
| 1122 §3.3.2 | IPv4 reassembly | **not implemented** | RX fragments are dropped and counted (`ip.rx_frag`); we set DF on all TX, so we never fragment outbound |
| 1122 | Host requirements (TCP §4.2) | client-side items only | deviations documented below |
| 1191 | PMTUD | yes | driven by ICMP messages |
| 9293 | TCP | client FSM complete | no LISTEN/accept |
| 7323 | Timestamps + Window Scale | yes | enables RTT + PAWS + large windows |
| 2018 | SACK | yes | essential for WAN loss recovery |
| 5681 | Congestion control | off-by-default; Reno via `cc_mode` | |
| 6298 | RTO | yes | minRTO=20ms (tunable) |
| 6582 | NewReno | with Reno mode | |
| 6691 | MSS | yes | clamp to local MTU |
| 3168 | ECN | off-by-default (flag) | |
| 8985 | RACK-TLP | yes | primary loss detection; replaces 3-dup-ACK |
| 6528 | ISS generation | yes | `ISS = (ticks_since_boot_at_4µs) + SipHash(4-tuple \|\| secret \|\| boot_nonce)` — clock outside the hash for monotonicity across reconnects |
| 5961 | Blind-data-attack mitigations | yes | challenge-ACK on out-of-window seqs |
| 7413 | TCP Fast Open | **NO** | not useful for long-lived connections |

### 6.4 Deviations from RFC defaults (by design, for trading latency)

| Default | RFC stance | Our default | Rationale |
|---|---|---|---|
| Delayed ACK | RFC 1122 SHOULD (§4.2.3.2 ≤500ms, ≥1/2 full-size segments) | **off** | 200ms ACK delay is catastrophic for trading. Within each `resd_net_poll` iteration the stack emits at most one ACK per connection covering the in-order RX delta (burst-scope coalescing, not time-based delay). This bounds ACK rate at `(conn_count × poll_rate)` Hz. On bulk inbound market-data bursts, ACK rate can approach line-rate/MSS — acceptable because the TX path to the exchange is low-volume and not bandwidth-starved. |
| Nagle (`TCP_NODELAY` inverse) | RFC 896 | **off** | user sends complete requests; coalescing is their choice |
| TCP keepalive | optional | **off** | exchanges close idle; application heartbeats are preferred |
| minRTO | RFC 6298 RECOMMENDS 1s | **20ms** (tunable) | intra-region WAN RTT is 1–10ms |
| Congestion control | RFC 5681 MUST | **off-by-default** | ≤100 connections, well-provisioned WAN; Reno available behind `cc_mode` for A/B-vs-Linux and RFC-compliance modes |
| PermitTFO (RFC 7413) | optional | **disabled** | long-lived connections don't benefit; adds 0-RTT security complexity |

### 6.5 Implementation choices

- **Flow table**: flat `Vec<Option<TcpConn>>` indexed by handle id + a hash map `(4-tuple) → handle` for RX lookup. Expected cost: ~40ns hot, ~200ns cold due to bucket cacheline miss; acceptable at ≤100 connections. If per-connection latency budget tightens, switch to a small pre-warmed array (≤8 candidates per RSS-bucket) with linear scan — faster under cache pressure at this scale.
- **Segment-level mbuf tracking**: every TX segment holds an mbuf refcount until ACK or RST. **Retransmit allocates a fresh header mbuf** chained to the original data mbuf (see §5.3) — never edits an in-flight mbuf in place.
- **ISS**: `ISS = (monotonic_time_4µs_ticks_low_32) + SipHash64(local_ip || local_port || remote_ip || remote_port || secret || boot_nonce)` per RFC 6528 §3. Clock value is added outside the hash so reconnects to the same 4-tuple within MSL yield monotonically-increasing ISS. `secret` is a 128-bit per-process random constant; `boot_nonce` survives reboots via `/proc/sys/kernel/random/boot_id` or equivalent.
- **SYN retransmit**: schedule respects `connect_timeout_ms` from `resd_net_connect_opts_t`. Default: 3 attempts with initial backoff `max(initial_rto_ms, minRTO)` (config default: `initial_rto_ms=50`), exponential up to the total budget. Never exceed `connect_timeout_ms` in total; the connection fails fast for trading, not per RFC 6298's 1s recommendation.
- **RTO timer re-arm**: lazy. On ACK, update `snd.una`; the existing wheel entry fires at its originally-scheduled deadline. When it fires the callback re-checks `snd.una` vs `snd.nxt` — if fully ACKed, the timer cancels itself; otherwise it retransmits and re-arms. Avoids remove+insert on every ACK.
- **TIME_WAIT shortening**: `resd_net_close(..., FORCE_TW_SKIP)` is honored only when RFC 6191 / RFC 7323 §5 conditions are met — specifically, timestamps are enabled on both sides AND `SEG.TSval > TS.Recent` at reconnect. When conditions aren't met, the flag is ignored and the connection stays in TIME_WAIT; a `RESD_NET_EVT_ERROR` with `err=EPERM_TW_REQUIRED` is emitted so the caller knows.

## 7. Memory and Buffer Model

### 7.1 Mempools (per-lcore, no cross-lcore allocation)

```
rx_mempool       : 2× NIC rx ring size × max_lcores
                   MBUF_SIZE = 2048 + RTE_PKTMBUF_HEADROOM(192) + TAILROOM(32)
                   HEADROOM sized for eth(14) + ip(20..60) + tcp(20..60) + tls_hdr(5)
                   TAILROOM reserves 16B TLS 1.3 AEAD tag + 1B inner-content-type + pad
tx_hdr_mempool   : small mbufs for ACK-only / RST / control / retransmit-header
tx_data_mempool  : large mbufs for request bodies (chained when body > mbuf capacity)
timer_mempool    : fixed-object pool for timer nodes
```

On mempool exhaustion: `rx_mempool` alloc failure drops the inbound packet and increments `eth.rx_drop_nomem`. `tx_*_mempool` failure causes `resd_net_http_request` (or internal retransmit scheduling) to return `-ENOMEM`, emits `RESD_NET_EVT_ERROR{err=ENOMEM}`, and does not corrupt the in-flight connection state. A CI test pins mempool size to a tiny value and verifies surfacing.

### 7.2 Per-connection buffers

- `recv_reorder`: out-of-order segment list, each element holds `(seq_range, mbuf_ref)`. Merged into `recv_contig` as gaps fill. Capped at `recv_buffer_bytes`.
- `recv_contig`: in-order mbuf chain ready for HTTP parser.
- `snd_retrans`: `(seq, mbuf_ref, first_tx_ts)` list. Capped at `send_buffer_bytes`.
- `tls_conn` (stage 2+): `rustls::ClientConnection` with `aws-lc-rs` backend; vectored AEAD reads/writes point at mbuf data.

### 7.3 Zero-copy path

```
RX:  NIC DMA → mbuf.data → rustls.read_tls(&mbuf.data)
                            → plaintext mbuf chain
                              → HTTP parser &plaintext_mbuf.data
                                → user &event.data

TX:  &user_bytes → http_encode writes into tx mbuf at headroom
                   → rustls.write_tls(&mut tx_mbuf) encrypts in-place
                     → tcp_output prepends TCP hdr in reserved headroom
                       → ip_output prepends IP+eth in reserved headroom
                         → NIC DMA reads mbuf.data
```

Only unavoidable copy on the TX path: user body bytes into the TX mbuf at `resd_net_http_request` time. On RX, reassembly may copy only if the HTTP parser needs a contiguous view crossing an mbuf boundary — empirically rare with 1500/9000 MTU and typical REST responses.

### 7.4 Timer wheel

- Hashed timing wheel, 8 levels × 256 buckets, per-lcore arena.
- Resolution: 10 µs. Horizon: ~68 s. Longer timers (2×MSL=60s) demoted to higher-level wheel.
- Wheel advancement is gated on `now_tick > last_ticked_tick`; when no tick elapsed since last poll (common at high poll rates), wheel walking is skipped entirely.
- Per-connection timer-list: each `TcpConn` owns a linked list of its scheduled timer IDs. On connection close, the close path walks this list and marks each timer cancelled (O(k) for k per-conn timers, k typically ≤4). Cancellation is a tombstone (generation counter bump); fire handlers check the generation and no-op on a stale fire.

### 7.5 Clock

- TSC-based; invariant TSC required (`CPUID.80000007H.EDX.InvariantTSC[bit 8]` set). Check at engine creation; fail fast otherwise.
- **Single calibration shared across all engines on the host**: the first `resd_net_engine_create` call performs `(tsc0, t0) = calibrate_against(CLOCK_MONOTONIC_RAW)`; subsequent engines reuse the same epoch via a process-global atomic-init. All engines therefore share one `ns_per_tsc` conversion; cross-lcore event correlation has bounded skew (only the per-core invariant-TSC startup offset, <100ns typical on bare metal; measured and reported in WAN A/B harness).
- `resd_net_now_ns` uses `rdtsc` (not `rdtscp`) — serialization is unnecessary for ms/µs-scale latency attribution. An inline variant `resd_net_now_ns_inline()` is exposed as a `static inline` in `resd_net.h` (reads TSC via compiler intrinsic, applies `(tsc-tsc0)*ns_per_tsc + t0` with engine-calibrated constants) so users can avoid the FFI call on tight hot-paths.
- NIC hardware timestamp: dyn-field offset and flag are resolved **once** at `engine_create` via `rte_mbuf_dynfield_lookup("rte_dynfield_timestamp")` and `rte_mbuf_dynflag_lookup("rte_dynflag_rx_timestamp")`. Hot-path reads are a direct `*(uint64_t*)((char*)mbuf + ts_offset)` behind an always-inline accessor. When the NIC/PMD doesn't register the dynfield, the accessor returns 0 and `rx_hw_ts_ns` in every event is 0; callers fall back to `enqueued_ts_ns`.

## 8. Hardware Assumptions

- NIC: Mellanox ConnectX-6/7 or Intel E810 class, 25/100 GbE.
- RSS enabled for connection→lcore pinning via NIC hash.
- RX hardware timestamping on.
- Checksum offload on (IP + TCP).
- TSO/LRO **off** — LRO breaks per-segment timing attribution on the RX path.
- SR-IOV VF or PF; works with bifurcated driver.
- DPDK 23.11 LTS.
- ARP: static gateway MAC seeded at startup via netlink helper (one-shot), refreshed via gratuitous ARP every N seconds. No dynamic ARP resolution on the data path.
- DNS: resolved out-of-band via `getaddrinfo()` on a control thread before `resd_net_connect`.

## 9. Observability (Primitives Only)

Stack emits primitives; application computes histograms, routes logs, runs exporters using its own existing infrastructure.

### 9.1 Counters

Per-lcore struct of `AtomicU64` counts, cacheline-grouped, lock-free-readable via:

```c
const resd_net_counters_t* resd_net_counters(resd_net_engine_t*);
```

Counter groups: `eth`, `ip`, `tcp`, `tls` (stage 2+), `http`, `poll`. Examples in `eth`: `rx_pkts`, `rx_bytes`, `rx_drop_miss_mac`, `rx_drop_nomem`, `tx_pkts`, `tx_bytes`, `tx_drop_full_ring`, `tx_drop_nomem`. In `tcp`: `rx_syn_ack`, `rx_data`, `rx_out_of_order`, `tx_retrans`, `tx_rto`, `tx_tlp`, `state_trans[11][11]`, `conn_open`, `conn_close`, `conn_rst`.

Hot-path writes are `store(val+1, Ordering::Relaxed)` on the owning lcore (zero cost on x86_64 vs. plain store). Cross-lcore snapshot readers use `load(Ordering::Relaxed)` — no torn reads. `Relaxed` is sufficient because counters are non-ordering observability data, not synchronization primitives. The alternative — "plain `u64` + `memcpy`" — is a data race by the Rust / C++ abstract machine and is rejected.

### 9.2 Timestamps on events

Every `resd_net_event_t` carries:
- `rx_hw_ts_ns` — NIC hardware timestamp (ground truth for RX).
- `enqueued_ts_ns` — TSC when the event entered user-visible form (set inside `resd_net_poll`).

For TX: the HTTP request returns after pushing to the TX batch; the application records its own wall-clock at that moment if it cares. `resd_net_flush` is where the NIC actually sees the packet; applications that want ground-truth TX timing can read `resd_net_now_ns()` immediately before/after flush.

### 9.3 Stability-visibility events

Delivered through the normal `resd_net_poll` interface:
- `RESD_NET_EVT_TCP_RETRANS` — seq, rtx_count.
- `RESD_NET_EVT_TCP_LOSS_DETECTED` — RACK or RTO trigger.
- `RESD_NET_EVT_TCP_STATE_CHANGE` — from/to state.
- `RESD_NET_EVT_TLS_ALERT` (stage 2+) — alert level/description.
- `RESD_NET_EVT_ERROR` with `err=ENOMEM` — mempool exhaustion on TX or internal allocation; with `err=EPERM_TW_REQUIRED` — `FORCE_TW_SKIP` flag ignored because RFC 6191 conditions not met.

State changes, alerts, and ERROR events are always emitted. Per-packet TCP trace events (`TCP_RETRANS`, `TCP_LOSS_DETECTED`) are gated by the `tcp_per_packet_events` config flag so they don't clutter `resd_net_poll` results when not wanted.

### 9.4 What the stack explicitly does NOT provide

- No histograms (application computes from counters + event timestamps)
- No event ring infrastructure (application uses its existing event ring)
- No admin socket, no Prometheus endpoint, no log writer
- No OpenTelemetry spans on the data path
- No string-formatted logs on the data path (ever)

### 9.5 API-boundary instrumentation

Optional cargo feature `obs-stable-boundaries`: when enabled, public API entry points are marked `#[inline(never)]` with `#[no_mangle]` for stable eBPF uprobe / PMU attachment. Disabled by default — default build lets the compiler inline for minimum latency.

## 10. Test Plan

Layered testing, phased so Stage 1 ships with a defensible test story and later stages extend.

### 10.1 Layer A — Unit tests (cargo test, all stages)

- Per-module TCP state machine tests; RFC 9293 §3.10 ("Event Processing") is the oracle.
- Parser tests: HTTP/1.1 encode/decode; malformed inputs (chunked encoding edge cases, header folding, CRLF in values).
- TLS record layer shim tests (stage 2+).
- Timer wheel, flow table, mempool wrappers.

### 10.2 Layer B — RFC conformance via packetdrill (Luna-pattern shim)

- `tools/packetdrill-shim`: links against libresd_net, redirects packetdrill's TUN read/write to stack rx/tx hooks, and provides a **synchronous socket-shim wrapper** that drives `resd_net_poll` internally to fake blocking-syscall semantics for `connect`/`write`/`read`/`close`.
- **Honest scope**: this exercises our wire-level TCP FSM, not our real asynchronous API. Packetdrill tests that depend on specific socket semantics we don't implement are not runnable:
  - Anything using `SIGIO`, `FIONREAD`, `SO_RCVLOWAT`, `MSG_PEEK`, `TCP_DEFER_ACCEPT`, `TCP_CORK`, or similar socket options.
  - Tests that assert partial-`read()` return values at specific buffer boundaries.
  - Tests that depend on Linux-specific timer behavior around delayed-ACK and 3-dup-ACK (we document these as deviations).
  The untranslatable subset is enumerated in `tools/packetdrill-shim/SKIPPED.md`; stage gates count pass rate among *runnable* scripts only.
- Run these corpora:
  - `github.com/ligurio/packetdrill-testcases` — pre-written scripts covering TCP RFC behavior (RFC 793/9293 FSM, 7323 timestamps, 2018 SACK, 5681 CC, 6298 RTO, 8985 RACK, 5961 mitigations). Exact RFC coverage: verify and record per-script on first import.
  - `github.com/shivansh/TCP-IP-Regression-TestSuite` — FreeBSD regression suite.
  - `github.com/google/packetdrill` upstream — TCP FSM and options.
  - Our scripts for RFC 7323 PAWS edge cases, RFC 2018 SACK reneging / out-of-order SACK blocks, RFC 8985 RACK reorder detection and TLP trigger, RFC 5961 challenge-ACK.

### 10.3 Layer C — RFC 793bis MUST/SHOULD via tcpreq

`tcpreq` probes a TCP endpoint (typically a server) for conformance. Since `resd_net` is client-only in production, this layer requires the **stage-1 loopback-test-server** (see §10.12): a minimal server-side build of the stack behind a cargo feature flag, used only by the test harness. `tcpreq` is pointed at the loopback server and produces a pass/fail table aligned to RFC 793bis / RFC 9293 requirements (checksum validation, RST processing, MSS, illegal/unknown option handling). Output feeds the RFC compliance matrix automatically.

Client-side conformance is covered indirectly by packetdrill-shim scripts that assert on the SYN/ACK/options *we emit* given specific peer behaviors.

### 10.4 Layer D — TTCN-3 via intel/net-test-suites

Black-box mode for bring-up; white-box mode (JSON protocol) when enough internal hooks exist for state assertions.

### 10.5 Layer E — Differential fuzzing via TCP-Fuzz

- `github.com/zouyonghao/TCP-Fuzz` in differential mode: identical packet+syscall sequences fed to `libresd_net` and Linux TCP; divergence is a bug.
- **Configuration requirement**: differential-vs-Linux is meaningful only in **RFC-compliance preset** — i.e., `cc_mode=reno`, delayed-ACK on (Linux-equivalent 40ms timer), `minRTO=200ms`, Nagle default. In this mode the two stacks should produce byte-identical wire behavior for equivalent inputs. Production-config differential fuzzing would produce false-positive divergences on every documented deviation (§6.4) and is explicitly **not** a useful test.
- A second fuzz track regresses against **our own previous release**: same input → same output across versions. Catches unintended behavior changes that differential-vs-Linux misses (anything where both our old and new code diverge from Linux).
- Motivation from prior work: TCP-Fuzz found semantic bugs across multiple userspace TCP stacks that sanitizer-based fuzzing does not catch, per USENIX ATC '22 reporting.
- CI: smoke run per merge. 72h continuous run per stage cut.

### 10.6 Layer F — Property / bespoke fuzzing

- `proptest`: round-trip identities on HTTP, TLS records (stage 2+), TCP options.
- `cargo-fuzz` / libFuzzer targets: HTTP response parser (seeded from real exchange captures), `tcp_input` with random pre-established state and arbitrary bytes (invariants: no panics, no UB, `snd.una ≤ snd.nxt`, rcv window monotonic), rustls + mbuf glue (stage 2+).
- `scapy` for adversarial hand-crafted packets: overlapping segments, malformed options, port-reuse races, timestamp wraparound.
- `smoltcp`'s `FaultInjector` pattern ported in: stackable RX-path middleware that randomly drops/duplicates/reorders/corrupts with configurable rates, enabled via env var for local soak-testing without netem.

### 10.7 Layer G — WAN A/B vs Linux (stage 4)

```
Producer(strategy) ─► [lcore: resd_net stack]  ─┐
                                                 ├─► exchange testnet
Producer(strategy) ─► [kernel Linux socket   ]  ─┘
```

- Inbound market data replayed from a captured pcap via fan-out to both stacks, preserving inter-arrival timing via HW timestamping.
- Identical outbound order sequences.
- Comparison: wire-level captures via **hardware tap** (NOT switch mirror port; tap insertion jitter <100ns documented and quantified via a loopback calibration run before each comparison session); end-to-end `tx_req → rx_resp` latency distributions (p50/p99/p999/max) per exchange; retransmit rate, dup-ACK rate, SACK-block usage; send-window utilization vs RTT.
- Measurement protocol: tap jitter baseline is subtracted from measured deltas; reported latency parity is `(resd_net_p999 − Linux_p999) − tap_jitter_p999`.
- Pass gate: `resd_net` p999 latency ≤ Linux p999 latency (after jitter subtraction) on all tested venues; zero RFC-conformance deltas from replay through the packetdrill shim.

### 10.8 Layer H — WAN-condition fault injection (stage 4)

Via `tc netem` on an intermediate Linux box inline between stack NIC and exchange:
- Delay: +20ms, +50ms, +200ms, jittered.
- Loss: 0.1% / 1% / 5% random, 1% correlated bursts.
- Duplication, reordering (3-segment depth), corruption.
- PMTU blackholing (drop ICMP frag-needed; force stack to detect via RTO + MSS probe).
- Asserts: no stuck connections, no unbounded retransmit, state transitions remain valid, counters show the expected signals.

### 10.9 Layer I — Online shadow mode (stage 4)

Run `resd_net` alongside Linux-stack path in production. Same requests over both. Gate promotion on zero response-body divergence and p99/p999 parity for 7 days.

### 10.10 Stage gates

- **Stage 1 ship**:
  - Layer A: 100% unit pass.
  - Layer B: packetdrill-shim's runnable (non-skipped) scripts from ligurio + shivansh passing on TCP FSM subset.
  - Layer C: tcpreq MUST rules passing against loopback-test-server.
  - **Observability gate**: assert-exact counter values in a controlled scenario. A unit test produces N retransmits, M state transitions, K HTTP requests; the exposed counters match exactly (not "approximately"). State-change and ERROR events are delivered in the expected order and count.
  - End-to-end smoke: place an order against the chosen staging exchange over plaintext HTTP/1.1.
- **Stage 2 (TLS) ship**: + rustls fuzz targets; TLS 1.3 interop against an exchange staging endpoint.
- **Stage 3 (WebSocket) ship**: + `crossbario/autobahn-testsuite` `fuzzingserver` mode passing with 0 failures; skipped-case list (the `permessage-deflate`-related cases, expected to be approximately 60 out of ~300) committed under `tests/autobahn-skipped.txt`.
- **Stage 4 (hardening) ship**: + Layers E/G/H passing; 7-day prod shadow with zero response divergence.

### 10.11 Tooling

- `tools/packetdrill-shim` — Luna-pattern adapter + socket-shim wrapper + `SKIPPED.md` enumeration.
- `tools/tcpreq-runner` — tcpreq wrapper with RFC-compliance report output.
- `tools/tcp-fuzz-differential` — TCP-Fuzz driver with Linux oracle; runs in RFC-compliance-preset config.
- `tools/replay` — pcap replay preserving HW timestamps.
- `tools/ab-bench` — dual-stack comparison harness + reporting with tap-jitter baseline subtraction.
- `tools/fuzz-corpus` — shared corpora, auto-updated from production pcaps.

### 10.12 Loopback test server (Stage 1 tooling)

A minimal server-side build of the stack behind the cargo feature flag `test-server`. Supports `accept` on a single listening port, plaintext HTTP/1.1 echo/fixed-response semantics. Used only by test harnesses (packetdrill-shim, tcpreq). Not compiled into production builds. This is Stage 1 scope — without it, Stage 1's tcpreq gate is unachievable.

## 11. Out of Scope for Stage 1

- Server-side TCP in production (test-only loopback server is in Stage 1 tooling; see §10.12)
- IPv6 / RFC 2460 / 8200 / 4443 / 4861 / 4862
- HTTP/2 (RFC 9113), HTTP/3 (RFC 9114)
- TLS (moves to Stage 2)
- WebSocket (moves to Stage 3)
- WebSocket `permessage-deflate` (RFC 7692) — not planned for any stage
- TCP Fast Open (RFC 7413)
- Full dynamic ARP state machine (static + gratuitous refresh only)
- DNS resolver on the data path

## 12. Open Questions to Resolve Before Stage 1 Starts

**[BLOCKER]** — must be answered before implementation begins:
- **Staging exchange venue** for the Stage-1 end-to-end gate. The choice determines whether the "plaintext HTTP/1.1" gate is even reachable — some testnets are HTTPS-only, others require auth handshakes Stage 1 doesn't support. Candidate list and selected venue must be committed before Stage 1 starts.
- **Rust nightly in CI, or stable-only.** Some DPDK-adjacent and low-latency crates require nightly features (`core::intrinsics::unlikely`, specific atomic intrinsics, certain allocator APIs). If stable-only, architectural choices need re-evaluation (manual `#[cold]` vs. intrinsic `unlikely`, workarounds for allocator hooks).

**[NICE-TO-HAVE]** — can be resolved during implementation:
- Specific NIC model and firmware version for the initial target hardware. Mitigable if the design stays PMD-agnostic, but HW-timestamp dyn-field behavior varies across mlx5 vs. ice; §7.5 already handles the unsupported case.
- Ownership of `tools/packetdrill-shim` — fork the packetdrill repo, or vendor it.

## 13. References

- RFC 9293 (TCP, 2022 consolidated), 7323 (timestamps + window scale), 2018 (SACK), 5681 (Reno congestion control), 6298 (RTO), 8985 (RACK-TLP), 5961 (blind-data mitigations), 6528 (ISS), 6191 (TIME-WAIT reduction using timestamps), 6691 (MSS), 3168 (ECN), 791/792 (IP/ICMP), 1122 (host requirements, incl. reassembly), 1191 (PMTUD), 9110/9112 (HTTP semantics + /1.1).
- mTCP: `github.com/mtcp-stack/mtcp` (reference only, not forked).
- Alibaba Luna userspace TCP + packetdrill adaptation (referenced for the shim pattern).
- Test suites: packetdrill (Google), ligurio/packetdrill-testcases, shivansh/TCP-IP-Regression-TestSuite, TheJokr/tcpreq, intel/net-test-suites, zouyonghao/TCP-Fuzz (USENIX ATC '22), smoltcp-rs/smoltcp (for `FaultInjector` pattern), crossbario/autobahn-testsuite.
