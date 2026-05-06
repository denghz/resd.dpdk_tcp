//! Pressure-correctness suite: socket-buffer underrun via reassembly-split path.
//!
//! Suite goal — exercise `tcp.rx_partial_read_splits` (counters.rs:273-279),
//! the slow-path counter that fires once per `deliver_readable` call where
//! the byte count to deliver lands in the middle of a front
//! `InOrderSegment` (so the segment must be split mid-mbuf). This is
//! the socket-buffer "underrun" path: the application asks for fewer
//! bytes than the head segment carries, or — equivalently for our event
//! model — the reassembly queue drains a merged run of segments and the
//! `total_delivered` count happens to fall on a non-boundary.
//!
//! Workload — single connection across a kernel-side TAP echo peer,
//! with `tc qdisc netem` configured for `delay 1ms reorder 50% gap 2`
//! to force a steady stream of out-of-order segment arrivals. The
//! engine alternates very small (1B) and very large (64KiB) writes,
//! so the kernel echo peer mirrors that pattern back at us. The mix of
//! tiny + jumbo segments arriving out-of-order through the engine's
//! reassembly queue is the configuration most likely to land
//! `total_delivered` in the middle of an `InOrderSegment`'s mbuf,
//! tripping the split path.
//!
//! Assertions:
//!   * `tcp.rx_partial_read_splits` > 0  — primary: split path fired
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0 — refcount integrity
//!   * `obs.events_dropped` == 0 — event soft-cap not exceeded
//!   * `tcp.rx_mempool_avail` drift ±32 — RX leak class
//!   * `tcp.tx_data_mempool_avail` drift ±32 — TX-data leak class
//!   * `FlowTable::active_conns()` == 0 after close — FSM integrity
//!   * total bytes echoed back >= a healthy fraction of bytes sent
//!     (substitute for CRC integrity — no crate dep added)
//!
//! Failure-bundle pattern: the workload + assertions run under
//! `catch_unwind` so any panic feeds the `PressureBucket` failure
//! bundle (counter snapshots, EngineConfig debug, recent events,
//! error string).

#![cfg(feature = "pressure-test")]

mod common;

use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use common::pressure::{
    assert_delta, CounterSnapshot, PressureBucket, Relation,
};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "resdtap37";
const OUR_IP: u32 = 0x0a_63_25_02; // 10.99.37.2
const PEER_IP: u32 = 0x0a_63_25_01; // 10.99.37.1
const PEER_IP_STR: &str = "10.99.37.1";
const OUR_IP_STR: &str = "10.99.37.2";
const PEER_PORT: u16 = 5037;

/// Wall-clock budget for the workload portion of the test.
const DURATION_SECS: u64 = 30;
/// Max signed drift permitted on either mempool level counter.
const POOL_DRIFT_TOLERANCE: i64 = 32;
/// Number of 1-byte writes per "small batch" round.
const SMALL_BATCH_COUNT: usize = 256;
/// Size of the single "large" write.
const LARGE_WRITE_SIZE: usize = 65_536;

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
        .args(["addr", "add", "10.99.37.1/24", "dev", iface])
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

/// RAII guard for `tc qdisc add dev <iface> root netem <spec>`. Modeled
/// directly on the `NetemGuard` in
/// `crates/dpdk-net-core/tests/tx_mempool_no_leak_under_retrans.rs`.
/// Drop best-effort issues `tc qdisc del dev <iface> root` so a panic
/// in the test body doesn't leave a stale netem qdisc that would
/// corrupt subsequent runs.
struct NetemGuard {
    iface: &'static str,
}

impl NetemGuard {
    fn apply(iface: &'static str, spec: &str) -> Self {
        // Best-effort pre-clean of any leftover qdisc from a previous
        // crashed run.
        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", iface, "root"])
            .stderr(std::process::Stdio::null())
            .status();
        let mut args = vec!["qdisc", "add", "dev", iface, "root", "netem"];
        args.extend(spec.split_whitespace());
        let status = Command::new("tc")
            .args(&args)
            .status()
            .expect("invoke tc");
        assert!(
            status.success(),
            "tc qdisc add netem failed (exit {:?}); is iproute2 installed and is the test running as sudo?",
            status.code()
        );
        eprintln!("[pressure-sbuf-underrun] applied netem on {iface}: {spec}");
        Self { iface }
    }
}

impl Drop for NetemGuard {
    fn drop(&mut self) {
        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", self.iface, "root"])
            .stderr(std::process::Stdio::null())
            .status();
        eprintln!("[pressure-sbuf-underrun] removed netem on {}", self.iface);
    }
}

#[test]
fn pressure_socket_buffer_underrun() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-sbuf-underrun",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap37",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    eal_init(&args).expect("EAL init");

    bring_up_tap(TAP_IFACE);
    thread::sleep(Duration::from_millis(500));

    // Apply netem AFTER eal_init because the TAP iface is created by
    // the `net_tap0` vdev. `delay 1ms reorder 50% gap 2`: every 2nd
    // packet has a 50% chance of arriving in-order; the rest are
    // delayed 1ms, which lands them after subsequent packets and
    // therefore out-of-order at the engine.
    let _netem = NetemGuard::apply(TAP_IFACE, "delay 1ms reorder 50% gap 2");

    let kernel_mac = read_kernel_tap_mac(TAP_IFACE);

    let cfg = EngineConfig {
        port_id: 0,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: kernel_mac,
        tcp_mss: 1460,
        max_connections: 4,
        tcp_msl_ms: 50,
        recv_buffer_bytes: 256 * 1024,
        ..Default::default()
    };
    let engine = Engine::new(cfg.clone()).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // Kernel-side echo server. A single connection — accept once,
    // echo bytes verbatim for the lifetime of the workload. The
    // engine drives the alternating small/large pattern; the kernel
    // simply mirrors it back. With netem reorder on the iface, both
    // directions experience OOO so the engine's reassembly queue
    // sees the merged-segment drains that trip the split path.
    let listener = TcpListener::bind((PEER_IP_STR, PEER_PORT)).expect("bind echo");
    listener.set_nonblocking(false).ok();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut sock = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            sock.set_nodelay(true).ok();
            thread::spawn(move || {
                let mut buf = vec![0u8; 65_536];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    // Open the bucket BEFORE the workload so the entry-snapshot
    // covers engine bring-up state, not mid-workload state.
    let bucket = PressureBucket::open(
        "pressure-socket-buffer-underrun",
        "mixed_sizes_reorder_30s",
        engine.counters(),
    );
    let baseline = CounterSnapshot::capture(engine.counters());
    eprintln!(
        "[pressure-sbuf-underrun] baseline active_conns={}",
        engine.flow_table().active_conns()
    );

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_workload(&engine, &baseline);
    }));

    match result {
        Ok(()) => {
            bucket.finish_ok();
        }
        Err(payload) => {
            let mut events: Vec<InternalEvent> = Vec::with_capacity(1024);
            engine.drain_events(1024, |ev, _| {
                events.push(ev.clone());
            });
            let err_msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    payload
                        .downcast_ref::<&'static str>()
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "<non-string panic>".to_string());
            let bundle_dir = bucket.finish_fail(
                engine.counters(),
                &cfg,
                events,
                err_msg.clone(),
            );
            std::panic::resume_unwind(Box::new(format!(
                "pressure_socket_buffer_underrun panicked: {err_msg}; \
                 forensic bundle at {bundle_dir:?}"
            )));
        }
    }
}

/// Workload body. Runs under `catch_unwind` in the caller; panics on
/// assertion failure so the failure-bundle path captures forensics.
fn run_workload(engine: &Engine, baseline: &CounterSnapshot) {
    // Single connection. local_port_hint=0 lets the engine pick an
    // ephemeral port.
    let conn = engine.connect(PEER_IP, PEER_PORT, 0).expect("connect");

    // Wait for Connected. Generous deadline because the SYN itself
    // can be reordered by netem.
    let connect_deadline = Instant::now() + Duration::from_secs(15);
    let mut connected = false;
    while Instant::now() < connect_deadline && !connected {
        engine.poll_once();
        engine.drain_events(32, |ev, _| {
            if matches!(ev, InternalEvent::Connected { conn: c, .. } if *c == conn) {
                connected = true;
            }
        });
        thread::sleep(Duration::from_millis(5));
    }
    assert!(connected, "connect timeout under netem reorder");
    eprintln!("[pressure-sbuf-underrun] connected");

    // Pre-built payloads. Distinct fill bytes for small vs. large
    // simplify any post-hoc forensic byte-pattern inspection.
    let small_payload = [0x55u8; 1];
    let large_payload = vec![0xAAu8; LARGE_WRITE_SIZE];

    let mut total_sent: usize = 0;
    let mut total_recvd: usize = 0;
    let mut splits_at_round_boundary: u64 = 0;
    let mut round: u32 = 0;

    let workload_deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
    'outer: while Instant::now() < workload_deadline {
        round = round.saturating_add(1);

        // Phase A: SMALL_BATCH_COUNT × 1-byte writes. Each call enqueues
        // one byte; under MSS=1460 the engine coalesces opportunistically
        // but the small-cadence stream produces many tiny TCP segments.
        for _ in 0..SMALL_BATCH_COUNT {
            if Instant::now() >= workload_deadline {
                break 'outer;
            }
            let send_deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match engine.send_bytes(conn, &small_payload) {
                    Ok(0) => {
                        // snd_wnd full — drain echoes to free up
                        // window, then retry. Sleep 1ms to avoid
                        // burning CPU in a tight spin while the peer
                        // drains and re-opens the window.
                        engine.poll_once();
                        engine.drain_events(32, |ev, _| {
                            if let InternalEvent::Readable {
                                conn: c, total_len, ..
                            } = ev
                            {
                                if *c == conn {
                                    total_recvd =
                                        total_recvd.saturating_add(*total_len as usize);
                                }
                            }
                        });
                        thread::sleep(Duration::from_millis(1));
                    }
                    Ok(n) => {
                        total_sent = total_sent.saturating_add(n as usize);
                        break;
                    }
                    Err(_) if Instant::now() >= send_deadline => {
                        // Stuck — bail out of the workload; the
                        // assertions will evaluate what fired so far.
                        break 'outer;
                    }
                    Err(_) => {
                        engine.poll_once();
                        engine.drain_events(32, |ev, _| {
                            if let InternalEvent::Readable {
                                conn: c, total_len, ..
                            } = ev
                            {
                                if *c == conn {
                                    total_recvd =
                                        total_recvd.saturating_add(*total_len as usize);
                                }
                            }
                        });
                    }
                }
            }
            // Pump after every small write so the segments hit the
            // wire promptly — the goal is many small TCP segments,
            // not one big coalesced run.
            engine.poll_once();
            engine.drain_events(32, |ev, _| {
                if let InternalEvent::Readable {
                    conn: c, total_len, ..
                } = ev
                {
                    if *c == conn {
                        total_recvd = total_recvd.saturating_add(*total_len as usize);
                    }
                }
            });
        }

        // Phase B: one 64 KiB write. Splits across many MSS-sized
        // segments at the wire, so the receive-side reassembly queue
        // sees a long train. Combined with netem reorder, sub-runs of
        // this train arrive OOO and are merged on delivery — exactly
        // the configuration that lands `total_delivered` mid-segment.
        let mut large_sent: usize = 0;
        let large_deadline = Instant::now() + Duration::from_secs(10);
        while large_sent < LARGE_WRITE_SIZE {
            if Instant::now() >= workload_deadline {
                break 'outer;
            }
            match engine.send_bytes(conn, &large_payload[large_sent..]) {
                Ok(0) => { /* snd_wnd full — drain below */ }
                Ok(n) => {
                    large_sent = large_sent.saturating_add(n as usize);
                    total_sent = total_sent.saturating_add(n as usize);
                }
                Err(_) if Instant::now() >= large_deadline => {
                    break 'outer;
                }
                Err(_) => { /* transient — drain and retry */ }
            }
            engine.poll_once();
            engine.drain_events(32, |ev, _| {
                if let InternalEvent::Readable {
                    conn: c, total_len, ..
                } = ev
                {
                    if *c == conn {
                        total_recvd = total_recvd.saturating_add(*total_len as usize);
                    }
                }
            });
        }

        // Per-round visibility: the split counter is the load-bearing
        // signal here; `eprintln!` it so CI logs let us see the split
        // path catching steady state vs. ramping.
        let splits_now = engine
            .counters()
            .tcp
            .rx_partial_read_splits
            .load(Ordering::Relaxed);
        let splits_round = splits_now - splits_at_round_boundary;
        splits_at_round_boundary = splits_now;
        let active = engine.flow_table().active_conns();
        eprintln!(
            "[pressure-sbuf-underrun] round={round} sent={total_sent} \
             recvd={total_recvd} splits_now={splits_now} \
             splits_round={splits_round} active_conns={active}"
        );
    }
    let total_rounds = round;
    eprintln!(
        "[pressure-sbuf-underrun] workload complete: rounds={total_rounds} \
         duration={DURATION_SECS}s sent={total_sent} recvd={total_recvd}"
    );
    assert!(
        total_rounds >= 1,
        "no complete round in {DURATION_SECS}s — engine or echo-server not making progress"
    );

    // Active-close. close_conn tolerates already-closing FSM states.
    let _ = engine.close_conn(conn);

    // Drain until Closed or deadline. With netem reorder the FIN train
    // can take a few hundred ms to retire.
    let mut closed = false;
    let close_deadline = Instant::now() + Duration::from_secs(20);
    while !closed && Instant::now() < close_deadline {
        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            match ev {
                InternalEvent::Closed { conn: c, .. } if *c == conn => {
                    closed = true;
                }
                InternalEvent::Readable {
                    conn: c, total_len, ..
                } if *c == conn => {
                    total_recvd = total_recvd.saturating_add(*total_len as usize);
                }
                _ => {}
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    assert!(closed, "close drain timeout for conn {conn:?}");

    // Final settle — pump for 50 polls so any in-flight mbufs (NIC RX
    // ring residue, lcore cache, FIN/ACK in flight, TIME_WAIT reaper
    // tick) drain before the post-snapshot.
    for _ in 0..50 {
        engine.poll_once();
        engine.drain_events(64, |_ev, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    // ─── Assertions ────────────────────────────────────────────────────
    let post = CounterSnapshot::capture(engine.counters());
    let delta = post.delta_since(baseline);

    // Primary: split path actually fired during the workload.
    assert_delta(&delta, "tcp.rx_partial_read_splits", Relation::Gt(0));
    // Resource integrity.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));
    // Mempool drift ±32.
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

    // FSM integrity.
    let active_conns_post = engine.flow_table().active_conns();
    assert_eq!(
        active_conns_post, 0,
        "active_conns = {active_conns_post} after close drain — FSM integrity violation"
    );

    // Byte-count integrity (substitute for CRC — no new crate dep
    // taken). Under sustained reorder some bytes can still be in
    // flight at close, but the working stack should land at least
    // half the sent bytes back as echoes within the workload window.
    // The 50% threshold is a lower bound: under healthy operation
    // recvd ≈ sent. A regression that loses payload would land far
    // below this.
    assert!(
        total_sent > 0,
        "no bytes sent — workload didn't run; total_sent={total_sent} total_recvd={total_recvd}"
    );
    let halfway = total_sent / 2;
    assert!(
        total_recvd >= halfway,
        "echoed bytes {total_recvd} < half of sent {total_sent} (threshold {halfway}) \
         — payload integrity regression"
    );

    // Forensic gate: zero mbufs hit the unexpected-refcnt-drop guard.
    let drop_unexpected = engine
        .counters()
        .tcp
        .mbuf_refcnt_drop_unexpected
        .load(Ordering::Relaxed);
    assert_eq!(
        drop_unexpected, 0,
        "mbuf_refcnt_drop_unexpected fired {drop_unexpected}× during workload — leak signal"
    );
}
