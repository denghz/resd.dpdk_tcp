//! Pressure Suite 10c — `fin-storm-deterministic-smoke`.
//!
//! Workload: open N=64 active connections to a kernel echo peer; send one
//! byte on each; the peer echoes one byte AND closes its socket. This
//! produces a "fin storm" — every kernel-side connection FINs the engine
//! in rapid succession over a short window. The engine, as the active
//! opener, transitions ESTABLISHED → CLOSE_WAIT on each peer FIN; the
//! application then drives `close_conn` on every conn that received its
//! peer FIN, completing CLOSE_WAIT → LAST_ACK → CLOSED on the passive
//! side (no TIME_WAIT for the passively-closed half-close per
//! `test_server_passive_close.rs`).
//!
//! Counters asserted (deltas across the full storm):
//!   * `tcp.conn_open` ≥ N — every active-open completed.
//!   * `tcp.conn_close` ≥ N — every connection reached CLOSED.
//!   * `tcp.tx_rst` == 0 — the storm is clean; no spurious RSTs.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0 — RX mbuf refcount integrity.
//!   * `obs.events_dropped` == 0 — soft-cap was never exceeded under the
//!     concurrent close storm.
//!   * `tcp.rx_mempool_avail`, `tcp.tx_data_mempool_avail` — both must
//!     drift within ±32 across the storm (mempool steady-state).
//!
//! Flow-table check: `engine.flow_table().active_conns() == 0` after the
//! settle window — every slot was released, no stuck connections.
//!
//! Engine config notes:
//!   * `max_connections = 128` (must fit N_CONNS=64 simultaneous conns).
//!   * `tcp_msl_ms = 10` (TIME_WAIT = 20ms — keeps the test fast; the
//!     engine is the passive closer here so most conns skip TIME_WAIT,
//!     but any active-close edge cases still settle within the window).
//!
//! Gated behind the `pressure-test` cargo feature; default builds compile
//! to an empty test binary.

#![cfg(feature = "pressure-test")]

mod common;

use std::collections::HashSet;
use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};
use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "resdtap32";
const OUR_IP: u32 = 0x0a_63_20_02; // 10.99.32.2
const PEER_IP: u32 = 0x0a_63_20_01; // 10.99.32.1
const PEER_IP_STR: &str = "10.99.32.1";
const OUR_IP_STR: &str = "10.99.32.2";
const PEER_PORT: u16 = 5032;

/// Number of simultaneously-active connections in the storm.
const N_CONNS: u32 = 64;
/// Mempool drift tolerance for the RX / TX-data pools across the storm.
/// The pools must round-trip back to ±32 of baseline once the storm
/// settles — same tolerance long_soak_stability uses for its 100k-iter
/// soak.
const POOL_DRIFT_TOLERANCE: i64 = 32;

fn skip_if_not_tap() -> bool {
    if std::env::var("DPDK_NET_TEST_TAP").ok().as_deref() != Some("1") {
        eprintln!("skipping; set DPDK_NET_TEST_TAP=1 to run (requires sudo for TAP vdev)");
        return true;
    }
    false
}

fn read_kernel_tap_mac(iface: &str) -> [u8; 6] {
    let path = format!("/sys/class/net/{iface}/address");
    let s = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("read {path}"));
    let mut out = [0u8; 6];
    for (i, part) in s.trim().split(':').enumerate() {
        out[i] = u8::from_str_radix(part, 16).expect("hex mac");
    }
    out
}

fn bring_up_tap(iface: &str) {
    let _ = Command::new("ip")
        .args(["link", "set", iface, "up"])
        .status();
    let _ = Command::new("ip")
        .args(["addr", "add", "10.99.32.1/24", "dev", iface])
        .status();
}

fn pin_arp(iface: &str, ip: &str, mac: &str) {
    let _ = Command::new("ip")
        .args([
            "neigh", "replace", ip, "lladdr", mac, "dev", iface, "nud", "permanent",
        ])
        .status();
}

fn mac_hex(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

#[test]
fn pressure_fin_storm_n64_deterministic() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-fin-storm",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap32",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    eal_init(&args).expect("EAL init");

    bring_up_tap(TAP_IFACE);
    thread::sleep(Duration::from_millis(500));

    let kernel_mac = read_kernel_tap_mac(TAP_IFACE);

    let cfg = EngineConfig {
        port_id: 0,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: kernel_mac,
        tcp_mss: 1460,
        // Must fit N_CONNS=64 simultaneous active conns plus headroom for
        // any stragglers in TIME_WAIT during the storm overlap window.
        max_connections: 128,
        // Very short TIME_WAIT (2×MSL = 20ms) so slot reclamation does
        // not dominate the test runtime. The engine is the passive
        // closer for every conn here (kernel sends FIN first), so most
        // conns skip TIME_WAIT entirely; this knob still bounds any
        // active-close edge cases to ≤20ms.
        tcp_msl_ms: 10,
        ..Default::default()
    };
    let engine = Engine::new(cfg).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // ---- Kernel side: the "fin storm" peer ----
    //
    // Single listener, single acceptor thread. Each accepted conn echoes
    // exactly one byte and then drops the socket (kernel sends FIN to the
    // engine). With N_CONNS=64 active opens issued back-to-back, the
    // close-after-echo pattern produces all 64 peer FINs in rapid
    // succession — the storm.
    //
    // SO_REUSEADDR is requested via `set_reuseaddr` on the underlying
    // socket. We use `TcpListener::bind` directly (matching
    // `long_soak_stability`) because the test owns its own private TAP
    // /24 — no other process is competing for `(PEER_IP_STR, PEER_PORT)`.
    let listener = TcpListener::bind((PEER_IP_STR, PEER_PORT)).expect("bind echo");
    let (peer_done_tx, peer_done_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        for _ in 0..N_CONNS {
            let (mut sock, _peer) = match listener.accept() {
                Ok(p) => p,
                Err(_) => break,
            };
            sock.set_nodelay(true).ok();
            let mut buf = [0u8; 1];
            // Echo one byte; if the read returns 0 or errors, still drop
            // the socket (close = FIN to engine) so the storm semantics
            // hold even on a short-circuit.
            if sock.read_exact(&mut buf).is_ok() {
                let _ = sock.write_all(&buf);
            }
            // Explicit drop happens at end of loop iteration — kernel
            // sends FIN immediately on socket close. This is the storm.
            drop(sock);
        }
        let _ = peer_done_tx.send(());
    });

    // Open the pressure bucket BEFORE we kick off the workload so the
    // baseline snapshot doesn't include any handshake-driven counter
    // bumps from phase 1.
    let bucket = PressureBucket::open(
        "pressure-fin-storm",
        "n64_deterministic",
        engine.counters(),
    );

    // ---- Phase 1: open all N_CONNS connections ----
    //
    // Use ephemeral source ports (`local_port_hint = 0`) — the engine's
    // ephemeral allocator assigns N_CONNS distinct ports.
    let mut conns: Vec<ConnHandle> = Vec::with_capacity(N_CONNS as usize);
    for _ in 0..N_CONNS {
        let h = engine.connect(PEER_IP, PEER_PORT, 0).expect("connect");
        conns.push(h);
    }

    let mut connected: HashSet<ConnHandle> = HashSet::new();
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    while connected.len() < N_CONNS as usize && Instant::now() < connect_deadline {
        engine.poll_once();
        engine.drain_events(128, |ev, _| {
            if let InternalEvent::Connected { conn, .. } = ev {
                connected.insert(*conn);
            }
        });
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        connected.len(),
        N_CONNS as usize,
        "phase 1 connect timeout: only {}/{} conns reached Connected",
        connected.len(),
        N_CONNS
    );

    // ---- Phase 2: send 1 byte on each conn → kicks the storm ----
    //
    // After the byte is sent, the kernel echo-then-close handler fires
    // and the engine receives FIN bundled with (or right after) the echo
    // byte. The engine transitions ESTABLISHED → CLOSE_WAIT; the
    // application below issues `close_conn` on each conn whose state has
    // moved off ESTABLISHED. CLOSE_WAIT → LAST_ACK is driven by our
    // close; LAST_ACK → CLOSED arrives on the peer's ACK of our FIN.
    for &h in &conns {
        let mut sent: u32 = 0;
        let send_deadline = Instant::now() + Duration::from_secs(5);
        while sent < 1 {
            match engine.send_bytes(h, &[0x42]) {
                Ok(n) => sent = sent.saturating_add(n),
                Err(e) => {
                    if Instant::now() >= send_deadline {
                        panic!("send_bytes on conn {h}: {e:?}");
                    }
                }
            }
            // Pump in the inner loop so back-pressure can clear.
            engine.poll_once();
            engine.drain_events(32, |_ev, _| {});
        }
    }

    // Now drive the storm to completion.
    //
    // Strategy: for each conn, watch for Readable (the echo byte arriving
    // from the kernel). The kernel echoes-then-closes, so the FIN is
    // either already processed by the engine (CLOSE_WAIT) or arrives
    // immediately after. As soon as we see Readable on a conn we issue
    // `close_conn` on it — the FSM handles either case cleanly:
    //   * If the engine is in CLOSE_WAIT already, `close_conn` drives
    //     CLOSE_WAIT → LAST_ACK (passive close, no TIME_WAIT).
    //   * If the engine is still in ESTABLISHED (FIN not yet processed),
    //     `close_conn` drives ESTABLISHED → FIN_WAIT_1; the imminent
    //     peer FIN then merges into CLOSING → TIME_WAIT → CLOSED. With
    //     `tcp_msl_ms = 10` the TIME_WAIT path adds at most 20ms.
    //
    // The Closed event fires for both teardown paths once the conn slot
    // is released. We track Closed to know when each conn is done.
    let mut close_issued: HashSet<ConnHandle> = HashSet::new();
    let mut fully_closed: HashSet<ConnHandle> = HashSet::new();
    let storm_deadline = Instant::now() + Duration::from_secs(30);
    while fully_closed.len() < N_CONNS as usize && Instant::now() < storm_deadline {
        engine.poll_once();
        let mut newly_readable: Vec<ConnHandle> = Vec::new();
        engine.drain_events(256, |ev, _| match ev {
            InternalEvent::Readable { conn, .. } => {
                newly_readable.push(*conn);
            }
            InternalEvent::Closed { conn, .. } => {
                fully_closed.insert(*conn);
            }
            _ => {}
        });
        for h in newly_readable {
            if close_issued.insert(h) {
                let _ = engine.close_conn(h);
            }
        }
        thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(
        fully_closed.len(),
        N_CONNS as usize,
        "phase 2 storm timeout: only {}/{} conns reached Closed (close_issued={})",
        fully_closed.len(),
        N_CONNS,
        close_issued.len()
    );

    // Wait for the kernel acceptor thread to drain all 64 accepts.
    let _ = peer_done_rx.recv_timeout(Duration::from_secs(5));

    // ---- Settle window ----
    //
    // Drain any tail events (final state-changes, TIME_WAIT reaps for
    // any active-close edge cases) so the post-snapshot reflects the
    // fully-quiesced engine.
    let settle_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < settle_deadline {
        engine.poll_once();
        engine.drain_events(64, |_ev, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    // ---- Snapshot + assertions ----
    let after = CounterSnapshot::capture(engine.counters());
    let delta = after.delta_since(&bucket.before);

    // Conn lifecycle accounting. Ge allows ARP-warmup retries; the strict
    // open/close parity below catches any FSM accounting asymmetry.
    assert_delta(&delta, "tcp.conn_open", Relation::Ge(N_CONNS as i64));
    assert_delta(&delta, "tcp.conn_close", Relation::Ge(N_CONNS as i64));
    let opens = delta.delta.get("tcp.conn_open").copied().unwrap_or(0);
    let closes = delta.delta.get("tcp.conn_close").copied().unwrap_or(0);
    assert_eq!(
        opens, closes,
        "conn_open ({opens}) ≠ conn_close ({closes}) — FSM accounting parity error"
    );

    // No spurious RSTs — the fin storm must be clean. Any RST means a
    // protocol bug or a state-machine race during the close burst.
    assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));

    // Resource integrity: zero unexpected mbuf-refcount drops, zero
    // dropped events.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mempool drift: both pools must round-trip to ±32 of baseline. A
    // sustained leak in either direction surfaces here.
    assert_delta(
        &delta,
        "tcp.rx_mempool_avail",
        Relation::Range(-POOL_DRIFT_TOLERANCE, POOL_DRIFT_TOLERANCE),
    );
    assert_delta(
        &delta,
        "tcp.tx_data_mempool_avail",
        Relation::Range(-POOL_DRIFT_TOLERANCE, POOL_DRIFT_TOLERANCE),
    );

    // Flow-table sanity: every slot must be released after the settle
    // window. Stuck connections (e.g. orphaned in CloseWait because our
    // close_conn was lost in flight) would surface here.
    let active = engine.flow_table().active_conns();
    assert_eq!(
        active, 0,
        "flow_table.active_conns() = {active} after settle — expected 0; \
         storm left {active} stuck connections"
    );

    bucket.finish_ok();
}
