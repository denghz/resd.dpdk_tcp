//! T21 regression test — rcv_wnd stuck at 65535 + zero-window deadlock.
//!
//! Requires DPDK_NET_TEST_TAP=1 AND root (DPDK TAP vdev + `ip neigh`
//! manipulation). Brings up `dpdktap7` on the kernel side with
//! 10.99.7.1/24, opens a kernel data-sender server, and has the kernel
//! push 100 KiB of data to the engine.
//!
//! T21 Bug 1 (rcv_wnd stuck): `conn.rcv_wnd` was initialised to
//! `min(recv_buf, u16::MAX) = 65535` by `new_client`.  After WS
//! negotiation `emit_ack` advertised 256 KiB (ws_shift_out=3), but the
//! in-window seq check in `handle_established` used the stale 65535 gate.
//! When the peer filled the 256 KiB advertised window, segments above
//! rcv_nxt+65535 were rejected as bad_seq (~92 K/s in bench T24).
//!
//! T21 Bug 2 (snd_wnd stuck): a pure window-reopen ACK (seg.ack ==
//! snd_una, window field going 0→positive) fell into the dup-ACK else
//! branch without updating snd_wnd, permanently deadlocking the TX path.
//!
//! Post-fix assertion: after the kernel pushes 100 KiB of data to the
//! engine, `tcp.rx_bad_seq == 0`.  We drive the engine's RX path only
//! (engine sends ACKs but no data), so the test bypasses the DPDK 23.11
//! TAP TX-burst reliability limitation that plagues large-data echo tests.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "dpdktap7";
const OUR_IP: u32 = 0x0a_63_07_02; // 10.99.7.2
const PEER_IP: u32 = 0x0a_63_07_01; // 10.99.7.1
const PEER_PORT: u16 = 7777;

/// 100 KiB — well above the pre-fix 65535-byte rcv_wnd gate.  The kernel
/// will push all of this into the connection once it sees the engine's
/// 256 KiB advertised window (ws_shift_out=3 → win=32768 on wire).
const TOTAL_BYTES: usize = 100 * 1024;

fn skip_if_not_tap() -> bool {
    if std::env::var("DPDK_NET_TEST_TAP").ok().as_deref() != Some("1") {
        eprintln!("t21_large_window_tap: skipping; set DPDK_NET_TEST_TAP=1 to run");
        return true;
    }
    false
}

fn read_kernel_tap_mac(iface: &str) -> [u8; 6] {
    let path = format!("/sys/class/net/{iface}/address");
    let s = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut out = [0u8; 6];
    for (i, part) in s.trim().split(':').enumerate() {
        out[i] = u8::from_str_radix(part, 16).expect("hex mac");
    }
    out
}

fn bring_up_tap(iface: &str, cidr: &str) {
    let _ = std::process::Command::new("ip")
        .args(["link", "set", iface, "up"])
        .status();
    let _ = std::process::Command::new("ip")
        .args(["addr", "add", cidr, "dev", iface])
        .status();
}

fn pin_arp(iface: &str, ip: &str, mac: &str) {
    let _ = std::process::Command::new("ip")
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
fn t21_no_bad_seq_on_large_window_bulk_transfer() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-t21-tap",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=dpdktap7",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    eal_init(&args).expect("EAL init");

    bring_up_tap(TAP_IFACE, "10.99.7.1/24");
    thread::sleep(Duration::from_millis(500));

    let kernel_mac = read_kernel_tap_mac(TAP_IFACE);

    // 256 KiB recv + send buffers — exactly the config that triggers T21.
    // ws_shift_out = compute_ws_shift_for(256*1024) = 3, so the engine
    // advertises 256 KiB to the peer.  Pre-fix rcv_wnd = 65535.
    let cfg = EngineConfig {
        port_id: 0,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: kernel_mac,
        tcp_mss: 1460,
        max_connections: 4,
        tcp_msl_ms: 100,
        recv_buffer_bytes: 256 * 1024,
        send_buffer_bytes: 256 * 1024,
        ..Default::default()
    };

    let engine = Engine::new(cfg).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, "10.99.7.2", &mac_hex(our_mac));

    // Kernel data-sender: connect to the engine port (from kernel side)
    // and immediately push TOTAL_BYTES without waiting for any echo.
    // This flow tests the engine's RX path exclusively — the engine only
    // needs to send ACKs (small, reliable on TAP) rather than data.
    //
    // Implementation: the kernel side listens; the engine connects.  Once
    // connected, the kernel thread writes TOTAL_BYTES then closes.
    use std::net::TcpListener;
    let listener = TcpListener::bind("10.99.7.1:7777").expect("listener bind");
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let sender_thread = thread::spawn(move || {
        let _ = ready_tx.send(());
        if let Some(stream) = listener.incoming().next() {
            let mut s = stream.expect("accept");
            let payload: Vec<u8> = (0u8..=255).cycle().take(TOTAL_BYTES).collect();
            // Write in 4 KiB chunks so the kernel segments the data
            // naturally into MSS-sized TCP segments rather than one giant
            // write that might be coalesced.
            for chunk in payload.chunks(4096) {
                if s.write_all(chunk).is_err() {
                    break;
                }
            }
            // Drop `s` here → kernel sends FIN → engine gets CLOSE_WAIT
            // → engine.close_conn() completes the 4WHS quickly.
        }
    });
    let _ = ready_rx.recv_timeout(Duration::from_secs(2));

    // Connect.  Track Readable events here too: the kernel sender starts
    // writing immediately after accept(), which may happen while we are
    // still in the connect loop.
    let handle = engine.connect(PEER_IP, PEER_PORT, 0).expect("connect");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut connected = false;
    let mut received: usize = 0;
    while Instant::now() < deadline && !connected {
        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            match ev {
                InternalEvent::Connected { conn, .. } if *conn == handle => {
                    connected = true;
                }
                InternalEvent::Readable { conn, total_len, .. } if *conn == handle => {
                    received += *total_len as usize;
                }
                _ => {}
            }
        });
        thread::sleep(Duration::from_millis(5));
    }
    assert!(connected, "did not receive CONNECTED within deadline");

    // Verify ARP table.
    {
        let out = std::process::Command::new("ip")
            .args(["neigh", "show", "dev", TAP_IFACE])
            .output()
            .unwrap();
        eprintln!("ARP table for {TAP_IFACE}: {}", String::from_utf8_lossy(&out.stdout).trim());
        let c = engine.counters();
        eprintln!(
            "pre-rx counters: eth.tx_pkts={} eth.rx_pkts={} tx_drop_full_ring={} tcp.rx_bad_seq={}",
            c.eth.tx_pkts.load(Ordering::Relaxed),
            c.eth.rx_pkts.load(Ordering::Relaxed),
            c.eth.tx_drop_full_ring.load(Ordering::Relaxed),
            c.tcp.rx_bad_seq.load(Ordering::Relaxed),
        );
    }

    // Receive loop: poll until we've drained TOTAL_BYTES from the engine.
    // The kernel sender is pushing data; we only need to poll + drain.
    // Sleep 1 ms between polls so the kernel sender thread gets CPU and
    // the ACK path has time to drain the TAP TX ring.
    let deadline = Instant::now() + Duration::from_secs(30);

    while received < TOTAL_BYTES && Instant::now() < deadline {
        engine.poll_once();

        engine.drain_events(64, |ev, _| {
            if let InternalEvent::Readable { conn, total_len, .. } = ev {
                if *conn == handle {
                    received += *total_len as usize;
                }
            }
        });

        thread::sleep(Duration::from_millis(1));
    }

    let c = engine.counters();
    eprintln!(
        "post-rx counters: received={received}/{TOTAL_BYTES} \
         eth.tx_pkts={} eth.rx_pkts={} tx_drop_full_ring={} tcp.rx_bad_seq={}",
        c.eth.tx_pkts.load(Ordering::Relaxed),
        c.eth.rx_pkts.load(Ordering::Relaxed),
        c.eth.tx_drop_full_ring.load(Ordering::Relaxed),
        c.tcp.rx_bad_seq.load(Ordering::Relaxed),
    );

    assert!(
        received >= TOTAL_BYTES,
        "only received {received}/{TOTAL_BYTES} bytes (timeout after 30s) — \
         kernel data-sender may have stalled"
    );

    // T21 regression assertion: the bad_seq counter MUST be 0.
    // Pre-fix: rcv_wnd=65535 caused ~92K bad_seq/sec when peer filled the
    // 256 KiB advertised window.  Even a single bad_seq means the kernel
    // sent data that our seq gate rejected.
    let bad_seq = engine.counters().tcp.rx_bad_seq.load(Ordering::Relaxed);
    assert_eq!(
        bad_seq, 0,
        "tcp.rx_bad_seq = {bad_seq}; T21 regression — rcv_wnd fix may be incomplete"
    );

    engine.close_conn(handle).expect("close");

    // Drain CLOSED event.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut closed = false;
    while Instant::now() < deadline && !closed {
        engine.poll_once();
        engine.drain_events(16, |ev, _| {
            if matches!(ev, InternalEvent::Closed { conn, .. } if *conn == handle) {
                closed = true;
            }
        });
        thread::sleep(Duration::from_millis(5));
    }
    assert!(closed, "did not receive CLOSED within deadline");

    drop(sender_thread);
}
