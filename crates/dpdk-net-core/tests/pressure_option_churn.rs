//! Pressure Suite — `pressure-option-churn-256cycles`.
//! A11.3 Lane C.
//!
//! Workload: drive 256 sequential TCP connect/send-1-byte/recv-echo/close
//! cycles over a real TAP interface.  Each cycle exercises the full TCP
//! option handshake (MSS, SACK, WSCALE, Timestamps) negotiated between
//! the engine and the kernel's TCP stack.  The suite stresses the option-
//! parse and option-encode paths under sustained connection churn.
//!
//! Counters asserted (deltas across all 256 cycles):
//!   * `tcp.conn_open` >= N_CYCLES  — every active-open completed.
//!   * `tcp.conn_close` >= N_CYCLES — every connection reached CLOSED.
//!   * `tcp.conn_open` == `tcp.conn_close`  — open/close FSM parity.
//!   * `tcp.rx_bad_option` == 0  — no malformed option was received or
//!       emitted across 256 negotiation rounds.
//!   * `obs.events_dropped` == 0  — event-queue cap not breached.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0  — refcount clean.
//!
//! Engine config:
//!   * `max_connections = 32` — fits N_CYCLES concurrent opens plus
//!       TIME_WAIT headroom.
//!   * `tcp_msl_ms = 10` — TIME_WAIT = 20ms, keeps the test fast.
//!   * `tcp_timestamps = true`, `tcp_sack = true` — full option set.
//!
//! Gated behind the `pressure-test` cargo feature.
//! Skipped unless `DPDK_NET_TEST_TAP=1` (requires sudo for TAP vdev).

#![cfg(feature = "pressure-test")]

mod common;

use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};
use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "resdtap40";
const OUR_IP: u32 = 0x0a_63_28_02; // 10.99.40.2
const PEER_IP: u32 = 0x0a_63_28_01; // 10.99.40.1
const PEER_IP_STR: &str = "10.99.40.1";
const OUR_IP_STR: &str = "10.99.40.2";
const PEER_PORT: u16 = 5040;

/// Number of sequential connect/close cycles.
const N_CYCLES: u32 = 256;

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
    let _ = Command::new("ip").args(["link", "set", iface, "up"]).status();
    let _ = Command::new("ip")
        .args(["addr", "add", "10.99.40.1/24", "dev", iface])
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
fn pressure_option_churn_256cycles() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-option-churn",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap40",
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
        // Enough slots for N_CYCLES sequential opens plus TIME_WAIT overlap.
        max_connections: 32,
        // Very short TIME_WAIT so slot reclamation does not bottleneck the
        // 256-cycle run.
        tcp_msl_ms: 10,
        // Enable the full option set so each handshake exercises MSS +
        // SACK + WSCALE + Timestamps.
        tcp_timestamps: true,
        tcp_sack: true,
        ..Default::default()
    };
    let engine = Engine::new(cfg).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // ── Kernel echo peer ───────────────────────────────────────────────
    //
    // Single listener; each accepted connection echoes one byte and closes.
    // Running N_CYCLES accepts sequentially (no concurrency needed since
    // the engine opens one connection at a time).
    let listener = TcpListener::bind((PEER_IP_STR, PEER_PORT)).expect("bind echo");
    listener.set_nonblocking(false).ok();
    let (peer_done_tx, peer_done_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        for _ in 0..N_CYCLES {
            let (mut sock, _) = match listener.accept() {
                Ok(p) => p,
                Err(_) => break,
            };
            sock.set_nodelay(true).ok();
            let mut buf = [0u8; 1];
            if sock.read_exact(&mut buf).is_ok() {
                let _ = sock.write_all(&buf);
            }
            drop(sock);
        }
        let _ = peer_done_tx.send(());
    });

    let bucket = PressureBucket::open(
        "pressure-option-churn",
        "256cycles",
        engine.counters(),
    );

    // ── 256 sequential connect/send/recv/close cycles ──────────────────
    //
    // Each cycle:
    //   1. connect() → wait for Connected event
    //   2. send 1 byte
    //   3. wait for Readable (echo byte back from kernel)
    //   4. close_conn() → wait for Closed event
    //
    // Running sequentially (not in parallel) to keep the open/close FSM
    // parity strict and avoid TIME_WAIT slot pressure.
    let cycle_timeout = Duration::from_secs(30);

    for cycle in 0..N_CYCLES {
        // Connect.
        let h = engine.connect(PEER_IP, PEER_PORT, 0).expect("connect");

        let conn_deadline = Instant::now() + cycle_timeout;
        loop {
            if Instant::now() >= conn_deadline {
                panic!("cycle {cycle}: connect timeout");
            }
            engine.poll_once();
            let mut connected = false;
            engine.drain_events(16, |ev, _| {
                if let InternalEvent::Connected { conn, .. } = ev {
                    if *conn == h {
                        connected = true;
                    }
                }
            });
            if connected {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        // Send 1 byte.
        let send_deadline = Instant::now() + cycle_timeout;
        let mut sent = 0u32;
        while sent < 1 {
            if Instant::now() >= send_deadline {
                panic!("cycle {cycle}: send timeout");
            }
            match engine.send_bytes(h, &[0x42]) {
                Ok(n) => sent = sent.saturating_add(n),
                Err(_) => {} // back-pressure; pump and retry
            }
            engine.poll_once();
            engine.drain_events(16, |_, _| {});
            thread::sleep(Duration::from_millis(1));
        }

        // Wait for Readable (echo byte).
        let read_deadline = Instant::now() + cycle_timeout;
        loop {
            if Instant::now() >= read_deadline {
                panic!("cycle {cycle}: readable timeout");
            }
            engine.poll_once();
            let mut readable = false;
            engine.drain_events(16, |ev, _| {
                if let InternalEvent::Readable { conn, .. } = ev {
                    if *conn == h {
                        readable = true;
                    }
                }
            });
            if readable {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        // Close and wait for Closed.
        let _ = engine.close_conn(h);
        let close_deadline = Instant::now() + cycle_timeout;
        loop {
            if Instant::now() >= close_deadline {
                panic!("cycle {cycle}: close timeout");
            }
            engine.poll_once();
            let mut closed = false;
            engine.drain_events(16, |ev, _| {
                if let InternalEvent::Closed { conn, .. } = ev {
                    if *conn == h {
                        closed = true;
                    }
                }
            });
            if closed {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    // Wait for kernel acceptor to drain all N_CYCLES accepts.
    let _ = peer_done_rx.recv_timeout(Duration::from_secs(10));

    // Settle: drain tail events after the last cycle's Closed.
    let settle = Instant::now() + Duration::from_millis(500);
    while Instant::now() < settle {
        engine.poll_once();
        engine.drain_events(64, |_, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(engine.counters());
    let delta = after.delta_since(&bucket.before);

    // Every cycle opened and closed successfully.
    assert_delta(&delta, "tcp.conn_open",  Relation::Ge(N_CYCLES as i64));
    assert_delta(&delta, "tcp.conn_close", Relation::Ge(N_CYCLES as i64));

    // Open/close parity: no connection leaked in a half-open state.
    let opens  = delta.delta.get("tcp.conn_open").copied().unwrap_or(0);
    let closes = delta.delta.get("tcp.conn_close").copied().unwrap_or(0);
    assert_eq!(
        opens, closes,
        "option_churn: conn_open ({opens}) ≠ conn_close ({closes})"
    );

    // Zero bad options across 256 negotiation rounds.
    assert_delta(&delta, "tcp.rx_bad_option", Relation::Eq(0));

    // Event-queue cap not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // Flow-table fully drained after the settle window.
    let active = engine.flow_table().active_conns();
    assert_eq!(
        active, 0,
        "option_churn: {active} connections still active after settle"
    );

    bucket.finish_ok();
}
