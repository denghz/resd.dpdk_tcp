//! Pressure-correctness Suite — slow-receiver back-pressure.
//!
//! Suite goal — drive the engine at sustained 64 KiB writes against a
//! deliberately-throttled kernel echo peer (≈10 MB/s drain rate). The
//! kernel side reads-then-sleeps so that the TCP receive window on the
//! peer fills up and the engine's send buffer back-pressures. The test
//! proves that, under sustained back-pressure across a 30-second window:
//!
//!   * `tcp.send_buf_full` fires at least once (back-pressure was actually
//!     surfaced to the application — the test would silently degrade to a
//!     "fast peer" run if this were 0).
//!   * `tcp.tx_window_update` fires at least once (the peer re-opened the
//!     window after draining; without window-update transmission the
//!     stream would wedge for the full 30s).
//!   * `tcp.tx_rst` == 0 (back-pressure is a steady-state condition; no
//!     resets should fire from either side).
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0 (mbuf accounting integrity
//!     under prolonged retransmit / window-probe activity).
//!   * `obs.events_dropped` == 0 (event soft-cap not exceeded even with
//!     readable events accumulating during stalls).
//!   * `tcp.rx_mempool_avail` drift ±32 (RX leak class — back-pressure
//!     stresses RX accounting because the engine processes more
//!     window-probe / dup-ACK frames than under normal flow).
//!   * `tcp.tx_data_mempool_avail` drift ±32 (TX-data leak class — segments
//!     held in retransmit queue must release cleanly once cumulative ACKs
//!     come back).
//!   * `flow_table.active_conns() == 0` after close-settle (FSM integrity
//!     under back-pressure-driven close).
//!
//! `tcp.rx_zero_window` and `tcp.tx_zero_window` are observed and
//! `eprintln!`d but not hard-asserted: depending on the timing relationship
//! between the application's send loop and the peer's drain cadence, the
//! window may shrink to a non-zero floor (no zero-window emitted) or all
//! the way to zero (zero-window emitted). Both are valid steady states and
//! the test would be flaky if it pinned to one.
//!
//! Failure-bundle pattern mirrors `pressure_max_throughput.rs`: a single
//! `PressureBucket` is opened before the workload and `finish_ok`'d on
//! success. Workload + assertions run under `catch_unwind` so any panic
//! is forwarded to `finish_fail`, which dumps a forensic bundle (counter
//! snapshots before/after, engine config, recent events, error text) under
//! `target/pressure-test/pressure-slow-receiver/single_conn_64k_10mbps/<unix_ms>/`.

#![cfg(feature = "pressure-test")]

mod common;

use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "resdtap35";
const OUR_IP: u32 = 0x0a_63_23_02; // 10.99.35.2
const PEER_IP: u32 = 0x0a_63_23_01; // 10.99.35.1
const PEER_IP_STR: &str = "10.99.35.1";
const OUR_IP_STR: &str = "10.99.35.2";
const PEER_PORT: u16 = 5035;

/// Per-write chunk size pushed into `send_bytes` on every workload
/// iteration. 64 KiB matches the suite's targeted throughput shape and
/// lets a partial accept (back-pressure) re-send the same buffer offset
/// next iteration without copying.
const CHUNK_SIZE: usize = 64 * 1024;
/// Wall-clock budget for the bench portion of the workload. 30s gives the
/// peer-side drain throttle enough cycles to repeatedly fill and reopen
/// the receive window so both `send_buf_full` and `tx_window_update`
/// accumulate non-trivial counts.
const DURATION_SECS: u64 = 30;
/// Kernel-side throttle: after every 64 KiB read, sleep this long before
/// the next read. 6.4ms per 64 KiB → ≈10 MB/s steady-state drain — well
/// below what a single lcore can issue, so the engine's send buffer
/// remains back-pressured for the bulk of the workload.
const PEER_DRAIN_SLEEP_US: u64 = 6_400;
/// Max signed drift allowed on either mempool level counter (RX / TX-data).
/// Mirrors `pressure_max_throughput.rs` and `long_soak_stability` —
/// healthy operation lands well under 32 mbufs of residue.
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
    let _ = Command::new("ip").args(["link", "set", iface, "up"]).status();
    let _ = Command::new("ip")
        .args(["addr", "add", "10.99.35.1/24", "dev", iface])
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
fn pressure_slow_receiver_single_conn_64k_10mbps() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-slow-rx",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap35",
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
        // Single conn + small headroom for any in-flight close artifacts.
        max_connections: 4,
        // Short MSL so the active-close TIME_WAIT after the workload
        // finishes well within the test window. The workload itself does
        // not exercise TIME_WAIT — only the post-workload teardown does.
        tcp_msl_ms: 50,
        recv_buffer_bytes: 256 * 1024,
        ..Default::default()
    };
    let engine = Engine::new(cfg.clone()).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // Kernel-side slow-receiver echo server. Single accept, single echo
    // worker — only one connection is opened by the workload. The echo
    // worker reads up to 64 KiB at a time and sleeps `PEER_DRAIN_SLEEP_US`
    // between reads. 64 KiB / 6.4ms ≈ 10 MB/s steady-state drain, which
    // is roughly an order of magnitude below the engine's single-conn
    // peak — the resulting back-pressure fills the receive window and
    // forces `send_bytes` to accept partial chunks.
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
                // Read at most 64 KiB at a time, echo it back, then sleep
                // before the next read. The sleep is what creates the
                // throttle — without it the peer would drain at line rate
                // and the test would degrade to a maxtp-shaped run with
                // no `send_buf_full` activity.
                let mut buf = vec![0u8; CHUNK_SIZE];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).is_err() {
                                break;
                            }
                            thread::sleep(Duration::from_micros(PEER_DRAIN_SLEEP_US));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    // Open the bucket BEFORE the workload starts so the entry-snapshot
    // covers the engine's bring-up state, not the mid-workload state.
    let bucket = PressureBucket::open(
        "pressure-slow-receiver",
        "single_conn_64k_10mbps",
        engine.counters(),
    );
    let baseline = CounterSnapshot::capture(engine.counters());
    eprintln!(
        "[pressure-slow-rx] baseline active_conns={}",
        engine.flow_table().active_conns()
    );

    // Wrap the workload + assertions in catch_unwind so any panic
    // (assertion failure, send error, drain timeout) goes through the
    // failure-bundle path before being re-raised.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_workload(&engine, &baseline);
    }));

    match result {
        Ok(()) => {
            bucket.finish_ok();
        }
        Err(payload) => {
            // Best-effort drain of remaining events for the forensic
            // bundle. The library-side queue is bounded; we cap at 1024.
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
            let bundle_dir =
                bucket.finish_fail(engine.counters(), &cfg, events, err_msg.clone());
            std::panic::resume_unwind(Box::new(format!(
                "pressure_slow_receiver_single_conn_64k_10mbps panicked: {err_msg}; \
                 forensic bundle at {bundle_dir:?}"
            )));
        }
    }
}

/// The workload loop, factored out so `catch_unwind` has a single clean
/// closure boundary. Panics on assertion failure; the caller turns the
/// panic into a forensic bundle.
fn run_workload(engine: &Engine, baseline: &CounterSnapshot) {
    // Single connection. local_port_hint=0 lets the engine pick an
    // ephemeral port.
    let conn: ConnHandle = engine
        .connect(PEER_IP, PEER_PORT, 0)
        .expect("connect");

    // Wait for the Connected event. Drain everything else into /dev/null
    // — the workload below tracks Readable / Closed itself.
    let connect_deadline = Instant::now() + Duration::from_secs(15);
    let mut connected = false;
    while !connected {
        if Instant::now() >= connect_deadline {
            panic!("connect timeout: connection did not reach Connected");
        }
        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            if let InternalEvent::Connected { conn: c, .. } = ev {
                if *c == conn {
                    connected = true;
                }
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    eprintln!("[pressure-slow-rx] connected");

    // Workload: tight send loop with partial-accept handling.
    //
    // We keep a single 64 KiB chunk and an offset into it. Each iteration
    // tries to push the unsent tail (`chunk[offset..]`) into `send_bytes`.
    // When the engine accepts fewer bytes than requested it bumps
    // `tcp.send_buf_full` and we advance `offset` by only the accepted
    // count — the next iteration retries the unsent portion. When the
    // chunk is fully accepted we reset to the start and start the next
    // 64 KiB write. Total bytes sent / received are accumulated for the
    // post-workload progress log; the assertions key off counters not
    // raw byte totals.
    let chunk = vec![0xCDu8; CHUNK_SIZE];
    let mut offset: usize = 0;
    let mut total_sent: u64 = 0;
    let mut total_recvd: u64 = 0;
    let mut sends_attempted: u64 = 0;
    let mut partial_accepts: u64 = 0;

    let workload_deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
    while Instant::now() < workload_deadline {
        let want = CHUNK_SIZE - offset;
        if want > 0 {
            sends_attempted = sends_attempted.saturating_add(1);
            match engine.send_bytes(conn, &chunk[offset..]) {
                Ok(n) => {
                    let n = n as usize;
                    if n < want {
                        // Back-pressure: the engine accepted only part of
                        // the buffer (and bumped `tcp.send_buf_full`
                        // exactly when n < want). Hold the remainder and
                        // retry next iteration.
                        partial_accepts = partial_accepts.saturating_add(1);
                    }
                    offset += n;
                    total_sent = total_sent.saturating_add(n as u64);
                    if offset >= CHUNK_SIZE {
                        offset = 0;
                    }
                }
                Err(_e) => {
                    // Transient error path (e.g. ConnFull during a
                    // momentary stall). Treat as a no-op and let the next
                    // poll/drain cycle clear it. A persistent error would
                    // surface in counters or in the post-workload
                    // active_conns check.
                }
            }
        }

        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            if let InternalEvent::Readable {
                conn: c, total_len, ..
            } = ev
            {
                if *c == conn {
                    total_recvd = total_recvd.saturating_add(*total_len as u64);
                }
            }
        });
    }

    eprintln!(
        "[pressure-slow-rx] workload complete: sends_attempted={sends_attempted} \
         partial_accepts={partial_accepts} total_sent={total_sent} total_recvd={total_recvd}"
    );

    // Active-close. close_conn returns an error only on an unknown
    // handle; it tolerates already-closing FSM states with a successful
    // no-op (matches the engine.close_conn doc-comment).
    let _ = engine.close_conn(conn);

    // Drain until the conn emits Closed. Use a generous deadline —
    // active-close walks through FIN_WAIT_1 → FIN_WAIT_2 → TIME_WAIT and
    // depending on RTT can take a few hundred ms.
    let mut closed = false;
    let close_deadline = Instant::now() + Duration::from_secs(20);
    while !closed {
        if Instant::now() >= close_deadline {
            panic!("close drain timeout: connection did not reach Closed");
        }
        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            if let InternalEvent::Closed { conn: c, .. } = ev {
                if *c == conn {
                    closed = true;
                }
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    eprintln!("[pressure-slow-rx] closed");

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

    // Soft-info: zero-window emit/observe. These are timing-dependent —
    // the window may shrink to a non-zero floor (no zero-window) or all
    // the way to zero. Log for diagnostic visibility but do not assert.
    let rx_zw = delta.delta.get("tcp.rx_zero_window").copied().unwrap_or(0);
    let tx_zw = delta.delta.get("tcp.tx_zero_window").copied().unwrap_or(0);
    eprintln!(
        "[pressure-slow-rx] rx_zero_window={rx_zw} tx_zero_window={tx_zw} (informational)"
    );

    // Back-pressure must have actually fired — without this the test
    // silently degrades to a "fast peer" run and tells us nothing about
    // slow-receiver behavior.
    assert_delta(&delta, "tcp.send_buf_full", Relation::Gt(0));

    // Window-update transmission must have fired — otherwise the stream
    // would have wedged on the first window-fill and the back-pressure
    // signal would not have unblocked. The fact that the workload made
    // progress for 30s implies the peer reopened the window repeatedly,
    // each reopen bumping this counter.
    assert_delta(&delta, "tcp.tx_window_update", Relation::Gt(0));

    // Hard tripwires — back-pressure is a steady-state condition and must
    // not produce any of these.
    assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mempool drift — level counters can move in either direction; the
    // workload should leave both pools within ±POOL_DRIFT_TOLERANCE of
    // their pre-workload level. Back-pressure stresses both pools (RX
    // window-probe / dup-ACK frames; TX retransmit queue residue) so
    // these are the canary for slow-receiver leak classes.
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

    // FSM integrity — the conn we opened must be reaped. This is the
    // direct visibility on the conn-table; derived deltas from
    // tcp.conn_open / tcp.conn_close can mask off-by-one bugs.
    let active_conns_post = engine.flow_table().active_conns();
    assert_eq!(
        active_conns_post, 0,
        "active_conns = {active_conns_post} after close drain — FSM integrity violation"
    );

    // Forensic gate (defensive — same condition as the assert_delta above
    // but reads the atomic directly, surfacing a different panic message
    // shape that the failure-bundle bucket can catch even if the
    // assert_delta wiring is broken).
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
