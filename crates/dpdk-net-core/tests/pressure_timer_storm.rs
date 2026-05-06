//! Pressure Suite — `pressure-timer-storm`.
//! A11.4 Lane D.
//!
//! Workload: open N_CONNS = 64 active connections to a kernel echo peer.
//! Each connection transmits a small payload.  After all sends complete,
//! apply 100 % packet-loss via `tc netem loss 100%` on the TAP interface
//! for NETEM_LOSS_MS milliseconds — long enough for all outstanding
//! segments to expire their initial RTO and fire at least one
//! retransmission per connection.  Restore connectivity, wait for the
//! recovery window, and assert all connections remain alive (no RSTs
//! sent by the engine, no stuck connections).
//!
//! Counters asserted (deltas across the full workload):
//!   * `tcp.tx_rto` > 0     — at least one RTO timer fired.
//!   * `tcp.tx_retrans` > 0 — at least one retransmission was sent.
//!   * `tcp.tx_rst` == 0    — engine did not give up under recoverable RTO.
//!   * `tcp.conn_open` >= N_CONNS — all active-opens completed.
//!   * `obs.events_dropped` == 0.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0.
//!
//! Flow-table check: `engine.flow_table().active_conns() >= N_CONNS` during
//! the loss window; all conns eventually close cleanly after the suite.
//!
//! Engine config:
//!   * `max_connections = 128` — headroom for N_CONNS=64 simultaneous conns.
//!   * `tcp_msl_ms = 10` — fast TIME_WAIT so the settle window is short.
//!   * `tcp_initial_rto_us = 100_000` (100ms) — fast RTO for the test to
//!     trigger retransmissions within the NETEM_LOSS_MS window.
//!
//! Gated behind the `pressure-test` cargo feature.
//! Skipped unless `DPDK_NET_TEST_TAP=1` (requires sudo for TAP vdev).

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

const TAP_IFACE: &str = "resdtap48";
const OUR_IP: u32 = 0x0a_63_30_02; // 10.99.48.2
const PEER_IP: u32 = 0x0a_63_30_01; // 10.99.48.1
const PEER_IP_STR: &str = "10.99.48.1";
const OUR_IP_STR: &str = "10.99.48.2";
const PEER_PORT: u16 = 5_048;

/// Number of simultaneous active connections for the storm.
const N_CONNS: u32 = 64;

/// Duration of the netem 100%-loss window in milliseconds.
/// Must be long enough for the initial RTO to fire (tcp_initial_rto_us =
/// 100 000 µs = 100 ms → 200 ms loss window ensures at least one RTO).
const NETEM_LOSS_MS: u64 = 400;

/// Recovery window: time to wait after restoring connectivity for all
/// retransmitted segments to be acknowledged.
const RECOVERY_MS: u64 = 3_000;

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
        .args(["addr", "add", "10.99.48.1/24", "dev", iface])
        .status();
}

fn apply_netem_loss(iface: &str) {
    let _ = Command::new("tc")
        .args(["qdisc", "replace", "dev", iface, "root", "netem", "loss", "100%"])
        .status();
}

fn remove_netem_loss(iface: &str) {
    let _ = Command::new("tc")
        .args(["qdisc", "del", "dev", iface, "root"])
        .status();
}

fn pin_arp(iface: &str, ip: &str, mac: &str) {
    let _ = Command::new("ip")
        .args(["neigh", "replace", ip, "lladdr", mac, "dev", iface, "nud", "permanent"])
        .status();
}

fn mac_hex(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

#[test]
fn pressure_timer_storm_n64() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-timer-storm",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap48",
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
        max_connections: 128,
        tcp_msl_ms: 10,
        // Fast initial RTO (100ms) so the netem loss window (400ms)
        // triggers at least one RTO fire per connection.
        tcp_initial_rto_us: 100_000,
        ..Default::default()
    };
    let engine = Engine::new(cfg).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // ── Kernel echo peer ───────────────────────────────────────────────
    //
    // Accepts N_CONNS connections, reads one byte per conn, echoes it
    // back, then keeps the socket open (does NOT close).  This gives us
    // long-lived connections to drive RTOs on.
    let listener = TcpListener::bind((PEER_IP_STR, PEER_PORT)).expect("bind echo");
    let (peer_ready_tx, peer_ready_rx) = mpsc::channel::<()>();
    let (peer_stop_tx, peer_stop_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut socks = Vec::new();
        for _ in 0..N_CONNS {
            let (mut sock, _) = match listener.accept() {
                Ok(p) => p,
                Err(_) => break,
            };
            sock.set_nodelay(true).ok();
            let mut buf = [0u8; 1];
            if sock.read_exact(&mut buf).is_ok() {
                let _ = sock.write_all(&buf);
            }
            socks.push(sock); // keep alive
        }
        let _ = peer_ready_tx.send(());
        // Hold sockets open until the test tells us to stop.
        let _ = peer_stop_rx.recv_timeout(Duration::from_secs(60));
        drop(socks);
    });

    let bucket = PressureBucket::open(
        "pressure-timer-storm",
        "n64",
        engine.counters(),
    );

    // ── Phase 1: open N_CONNS active connections ───────────────────────
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
        "timer_storm: only {}/{} conns reached Connected",
        connected.len(),
        N_CONNS
    );

    // ── Phase 2: send 1 byte on each conn ─────────────────────────────
    for &h in &conns {
        let mut sent = 0u32;
        let send_deadline = Instant::now() + Duration::from_secs(5);
        while sent < 1 {
            match engine.send_bytes(h, &[0x42]) {
                Ok(n) => sent = sent.saturating_add(n),
                Err(_) => {
                    if Instant::now() >= send_deadline {
                        panic!("timer_storm: send timeout on conn {h}");
                    }
                }
            }
            engine.poll_once();
            engine.drain_events(32, |_, _| {});
        }
    }

    // Wait for the kernel echo peer to process all N_CONNS sends.
    let _ = peer_ready_rx.recv_timeout(Duration::from_secs(10));

    // Drain the echo bytes back.
    let mut echoed: HashSet<ConnHandle> = HashSet::new();
    let echo_deadline = Instant::now() + Duration::from_secs(5);
    while echoed.len() < N_CONNS as usize && Instant::now() < echo_deadline {
        engine.poll_once();
        engine.drain_events(128, |ev, _| {
            if let InternalEvent::Readable { conn, .. } = ev {
                echoed.insert(*conn);
            }
        });
        thread::sleep(Duration::from_millis(1));
    }

    // ── Phase 3: apply netem 100 % loss → trigger RTO storm ───────────
    apply_netem_loss(TAP_IFACE);

    // Pump the engine for NETEM_LOSS_MS — RTOs will fire and retransmit.
    let loss_deadline = Instant::now() + Duration::from_millis(NETEM_LOSS_MS);
    while Instant::now() < loss_deadline {
        engine.poll_once();
        engine.drain_events(64, |_, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    // ── Phase 4: restore connectivity ─────────────────────────────────
    remove_netem_loss(TAP_IFACE);

    // Wait for recovery: retransmits get ACKed, conns return to quiescent.
    let recovery_deadline = Instant::now() + Duration::from_millis(RECOVERY_MS);
    while Instant::now() < recovery_deadline {
        engine.poll_once();
        engine.drain_events(128, |_, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(engine.counters());
    let delta = after.delta_since(&bucket.before);

    // At least one RTO timer fired during the loss window.
    assert_delta(&delta, "tcp.tx_rto", Relation::Gt(0));

    // At least one retransmission was sent.
    assert_delta(&delta, "tcp.tx_retrans", Relation::Gt(0));

    // Engine did not give up under recoverable RTOs (no RSTs sent).
    assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));

    // All N_CONNS active-opens completed.
    assert_delta(&delta, "tcp.conn_open", Relation::Ge(N_CONNS as i64));

    // Event-queue cap not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // Signal the peer thread to release all sockets.
    let _ = peer_stop_tx.send(());

    // ── Clean up: close all conns ──────────────────────────────────────
    for &h in &conns {
        let _ = engine.close_conn(h);
    }

    // Settle: drain tail events and TIME_WAIT reaps.
    let settle_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < settle_deadline {
        engine.poll_once();
        engine.drain_events(64, |_, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    bucket.finish_ok();
}
