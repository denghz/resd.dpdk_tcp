//! Pressure-correctness Suite — RX-mempool exhaustion under flooder.
//!
//! Suite goal — pressurize the engine's RX path with a tiny `rx_mempool_size`
//! (256 mbufs) while a kernel-side flooder writes 64 KiB chunks in a tight
//! loop with no read-side back-pressure. The engine's `recv_buffer_bytes` is
//! held at 32 KiB so its receive window collapses quickly, the kernel TCP
//! stack hits zero-window and stops sending — but the NIC RX ring already
//! has frames in flight that the engine cannot allocate fresh mbufs to
//! receive. That is the exhaustion regime: `eth.rx_drop_nomem` MUST fire,
//! while the integrity invariants — zero `mbuf_refcnt_drop_unexpected`, zero
//! `obs.events_dropped`, and FlowTable settle to active_conns=0 — must hold:
//!
//!   * `eth.rx_drop_nomem`               > 0  (regime tripwire — the test's
//!                                              raison d'être; if this
//!                                              didn't fire the engine
//!                                              wasn't actually pressured)
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0 (UAF / double-free guard)
//!   * `obs.events_dropped`              == 0 (event soft-cap not exceeded)
//!   * `rx_mempool_avail` recovery       post-pause > pre-pause (the engine
//!                                              must release mbufs back to
//!                                              the pool when the flooder
//!                                              quiets)
//!   * FlowTable::active_conns() == 0 after final settle (FSM integrity)
//!
//! Timing: 30s window with a mid-test 5s pause at t=15s. The flooder thread
//! observes an `AtomicBool`; clearing it stops the writes for 5s, then the
//! flag is restored and the flood resumes. The pre/post-pause readings of
//! `tcp.rx_mempool_avail` give the recovery signal.
//!
//! Failure-bundle pattern: a single `PressureBucket` is opened before the
//! workload and `finish_ok`'d on success. Workload + assertions run under
//! `catch_unwind` so any panic is forwarded to `finish_fail`, which dumps a
//! forensic bundle (counter snapshots before/after, engine config, recent
//! events, error text) under
//! `target/pressure-test/pressure-mempool-exhaustion/rx_256mbufs_flood_30s/<unix_ms>/`.

#![cfg(feature = "pressure-test")]

mod common;

use std::io::Write as IoWrite;
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::pressure::{
    assert_delta, CounterSnapshot, PressureBucket, Relation,
};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "resdtap36";
const OUR_IP: u32 = 0x0a_63_24_02; // 10.99.36.2
const PEER_IP: u32 = 0x0a_63_24_01; // 10.99.36.1
const PEER_IP_STR: &str = "10.99.36.1";
const OUR_IP_STR: &str = "10.99.36.2";
const PEER_PORT: u16 = 5036;

/// Total wall-clock budget for the workload — 30s window covering the
/// pre-pause flood, the 5s mid-test pause, and the post-pause flood.
const DURATION_SECS: u64 = 30;
/// Mid-test pause duration (5s of flooder quiescence at t=15s) — long
/// enough for the NIC RX ring to drain and the engine to release mbufs
/// back to the pool, so `rx_mempool_avail` rises off the floor.
const PAUSE_SECS: u64 = 5;
/// Time-into-window at which the mid-test pause begins.
const PAUSE_START_SECS: u64 = 15;
/// Flooder chunk size — 64 KiB matches the spec; large enough to keep the
/// kernel TCP stack producing back-to-back full-MSS segments at line rate.
const FLOOD_CHUNK: usize = 64 * 1024;

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
        .args(["addr", "add", "10.99.36.1/24", "dev", iface])
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
fn pressure_mempool_exhaustion_rx_256mbufs_flood_30s() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-mempool-exhaust",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap36",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    eal_init(&args).expect("EAL init");

    bring_up_tap(TAP_IFACE);
    thread::sleep(Duration::from_millis(500));

    let kernel_mac = read_kernel_tap_mac(TAP_IFACE);

    // SMALL RX mempool to force exhaustion under the flooder. SMALL
    // recv_buffer to make the receive window collapse quickly so the
    // kernel TCP stack engages flow control — the regime under test is
    // "NIC RX ring outpaces engine drain", not "kernel keeps writing
    // forever past zero-window" (which the kernel would refuse anyway).
    let cfg = EngineConfig {
        port_id: 0,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: kernel_mac,
        tcp_mss: 1460,
        max_connections: 4,
        tcp_msl_ms: 50,
        rx_mempool_size: 256,
        recv_buffer_bytes: 32 * 1024,
        ..Default::default()
    };
    let engine = Engine::new(cfg.clone()).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // Kernel-side flooder. The kernel binds a TCP listener on PEER_IP:PEER_PORT,
    // accepts one connection (the engine's active open), then writes 64 KiB
    // chunks in a tight loop until the pause flag flips back. While the
    // flag is asserted the writer parks on a 10ms tick — long enough for
    // the NIC RX ring to drain to the engine and the engine to return
    // mbufs to the pool, so the post-pause `rx_mempool_avail` reading
    // recovers off the exhaustion floor.
    //
    // pause_flag semantics: `false` = flood; `true` = pause.
    let pause_flag = Arc::new(AtomicBool::new(false));
    let pause_flag_writer = Arc::clone(&pause_flag);
    let listener = TcpListener::bind((PEER_IP_STR, PEER_PORT)).expect("bind flood");
    listener.set_nonblocking(false).ok();
    let flooder = thread::spawn(move || {
        // Single-conn workload — accept the engine's active open, then
        // flood until the writer's `write_all` returns an error (engine
        // closed, conn reset, or test deadline reached and engine torn
        // down). We never break out on a clean read — the kernel side
        // is purely a writer here.
        let mut sock = match listener.accept() {
            Ok((s, _)) => s,
            Err(_) => return,
        };
        sock.set_nodelay(true).ok();
        let buf = vec![0xABu8; FLOOD_CHUNK];
        loop {
            if pause_flag_writer.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            // write_all blocks under kernel-side back-pressure (zero
            // window from the engine). Errors here mean the engine
            // tore the conn down — the flooder thread exits cleanly.
            if sock.write_all(&buf).is_err() {
                return;
            }
        }
    });

    // Open the bucket BEFORE the workload starts so the entry-snapshot
    // covers the engine's bring-up state, not the mid-workload state.
    let bucket = PressureBucket::open(
        "pressure-mempool-exhaustion",
        "rx_256mbufs_flood_30s",
        engine.counters(),
    );
    let baseline = CounterSnapshot::capture(engine.counters());

    // Wrap workload + assertions in catch_unwind so any panic
    // (assertion failure, drain timeout, connect refusal) routes through
    // the failure-bundle path before being re-raised.
    let pause_flag_main = Arc::clone(&pause_flag);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_workload(&engine, &pause_flag_main, &baseline);
    }));

    // Halt the flooder regardless of test outcome so the thread can join
    // cleanly when the engine drops at end-of-test.
    pause_flag.store(true, Ordering::Relaxed);

    match result {
        Ok(()) => {
            bucket.finish_ok();
            // Best-effort: let the flooder thread exit. If the engine has
            // torn down the conn, the next `write_all` errors and the
            // thread returns. We don't block on join — a well-behaved
            // workload returns within DURATION_SECS + a little settle.
            let _ = flooder.join();
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
            // Defensive: drop the flooder thread before re-raising so the
            // unwind doesn't race on the listener still being bound.
            let _ = flooder.join();
            std::panic::resume_unwind(Box::new(format!(
                "pressure_mempool_exhaustion_rx_256mbufs_flood_30s panicked: {err_msg}; \
                 forensic bundle at {bundle_dir:?}"
            )));
        }
    }
}

/// The workload loop, factored out so `catch_unwind` has a single clean
/// closure boundary. Panics on assertion failure; the caller turns the
/// panic into a forensic bundle.
fn run_workload(
    engine: &Engine,
    pause_flag: &Arc<AtomicBool>,
    baseline: &CounterSnapshot,
) {
    // Single conn — the engine actively opens to the kernel listener.
    // local_port_hint=0 lets the engine pick an ephemeral port.
    let conn: ConnHandle = engine
        .connect(PEER_IP, PEER_PORT, 0)
        .expect("connect");

    // Wait for the Connected event so the kernel-side accept has
    // returned and the flooder thread is positioned to start writing.
    let connect_deadline = Instant::now() + Duration::from_secs(15);
    let mut connected = false;
    while !connected {
        if Instant::now() >= connect_deadline {
            panic!("connect timeout: Connected event not seen within 15s");
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
    eprintln!("[pressure-mempool-exhaust] conn established; flooder is writing");

    // Track total Readable bytes for visibility — the data is acked as
    // the engine drains the per-conn recv buffer, but we don't do
    // anything special with the bytes. The combination of
    // recv_buffer_bytes=32K and rx_mempool_size=256 is what creates the
    // pressure regime.
    let workload_start = Instant::now();
    let workload_deadline = workload_start + Duration::from_secs(DURATION_SECS);
    let pause_start = workload_start + Duration::from_secs(PAUSE_START_SECS);
    let pause_end = pause_start + Duration::from_secs(PAUSE_SECS);

    let mut total_readable: u64 = 0;
    let mut paused = false;
    let mut resumed = false;
    let mut pre_pause_avail: Option<u32> = None;
    let mut post_pause_avail: Option<u32> = None;

    while Instant::now() < workload_deadline {
        let now = Instant::now();
        // Mid-test pause window: at t=PAUSE_START_SECS, snapshot the
        // current `rx_mempool_avail` (the exhaustion floor reading) and
        // raise the pause flag. At t=PAUSE_START_SECS+PAUSE_SECS, snapshot
        // again (the recovery reading) and clear the flag.
        if !paused && now >= pause_start {
            pre_pause_avail = Some(
                engine
                    .counters()
                    .tcp
                    .rx_mempool_avail
                    .load(Ordering::Relaxed),
            );
            pause_flag.store(true, Ordering::Relaxed);
            paused = true;
            eprintln!(
                "[pressure-mempool-exhaust] pause begin: rx_mempool_avail={}",
                pre_pause_avail.unwrap_or(0)
            );
        }
        if paused && !resumed && now >= pause_end {
            post_pause_avail = Some(
                engine
                    .counters()
                    .tcp
                    .rx_mempool_avail
                    .load(Ordering::Relaxed),
            );
            pause_flag.store(false, Ordering::Relaxed);
            resumed = true;
            eprintln!(
                "[pressure-mempool-exhaust] pause end: rx_mempool_avail={} (was {} at pause start)",
                post_pause_avail.unwrap_or(0),
                pre_pause_avail.unwrap_or(0)
            );
        }

        engine.poll_once();
        engine.drain_events(128, |ev, _| {
            if let InternalEvent::Readable { conn: c, total_len, .. } = ev {
                if *c == conn {
                    total_readable = total_readable.saturating_add(*total_len as u64);
                }
            }
        });
    }
    eprintln!(
        "[pressure-mempool-exhaust] workload complete: \
         duration={DURATION_SECS}s readable_bytes={total_readable}"
    );

    // Sanity: the pause-window readings must have been captured. If
    // either is None the workload ran for less than PAUSE_START+PAUSE
    // seconds, which would mean DURATION_SECS got mis-tuned.
    let pre = pre_pause_avail
        .expect("pre-pause rx_mempool_avail not captured — workload too short");
    let post = post_pause_avail
        .expect("post-pause rx_mempool_avail not captured — workload too short");

    // Halt the flooder before active-close so the kernel side observes
    // EOF cleanly rather than a half-mid-write reset.
    pause_flag.store(true, Ordering::Relaxed);

    // Active-close. close_conn returns Err only on an unknown handle;
    // already-closing FSM states are tolerated as a no-op.
    let _ = engine.close_conn(conn);

    // Drain until Closed for our conn. Generous deadline — under
    // residual flooder pressure the FIN handshake can take a few hundred
    // ms because the kernel side's send queue must drain past in-flight
    // data first.
    let close_deadline = Instant::now() + Duration::from_secs(20);
    let mut closed = false;
    while !closed {
        if Instant::now() >= close_deadline {
            panic!("close drain timeout: Closed event not seen within 20s");
        }
        engine.poll_once();
        engine.drain_events(128, |ev, _| {
            if let InternalEvent::Closed { conn: c, .. } = ev {
                if *c == conn {
                    closed = true;
                }
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    eprintln!("[pressure-mempool-exhaust] conn closed");

    // Final settle — pump for 50 polls so any in-flight mbufs (NIC RX
    // ring residue, lcore cache, FIN/ACK in flight, TIME_WAIT reaper
    // tick) drain before the post-snapshot.
    for _ in 0..50 {
        engine.poll_once();
        engine.drain_events(128, |_ev, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    // ─── Assertions ────────────────────────────────────────────────────
    let after = CounterSnapshot::capture(engine.counters());
    let delta = after.delta_since(baseline);

    // Regime tripwire: the workload MUST have driven the engine into
    // the exhaustion regime. If `rx_drop_nomem` did not fire, the test
    // is no longer testing what it claims to test (e.g. the small pool
    // got large enough to absorb the flood, or the flooder never wrote).
    assert_delta(&delta, "eth.rx_drop_nomem", Relation::Gt(0));

    // Integrity invariants under exhaustion: zero refcount accounting
    // failures, zero dropped events. The whole point of the suite is
    // to assert these stay clean WHILE the regime tripwire fires.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mid-test recovery: post-pause `rx_mempool_avail` must be strictly
    // greater than the pre-pause reading. With the flooder paused for
    // PAUSE_SECS, the engine drains the NIC RX ring and returns mbufs
    // to the pool — `rx_mempool_avail` rises off the exhaustion floor.
    // If this doesn't recover, the engine is leaking mbufs (they aren't
    // making it back to the pool even when nothing is consuming them).
    assert!(
        post > pre,
        "rx_mempool_avail did not recover during mid-test pause: \
         pre_pause={pre} post_pause={post} (expected post > pre); \
         engine is not returning mbufs to the pool when flooder quiets"
    );

    // TX-data pool drift: the TX-data mempool must also return to within
    // ±32 mbufs of baseline — the canonical TX-side leak signal.
    assert_delta(
        &delta,
        "tcp.tx_data_mempool_avail",
        Relation::Range(-32, 32),
    );

    // Post-pause liveness: engine continues receiving after the pause.
    // `eth.rx_pkts` is a monotone counter; its delta over the full 30s
    // window must be > 0 even under exhaustion (the engine accepted at
    // least one packet before the pool hit bottom).
    assert_delta(&delta, "eth.rx_pkts", Relation::Gt(0));

    // FSM integrity: the conn we opened must be reaped. A single stuck
    // connection here would mean the close handshake stalled under the
    // residual exhaustion pressure — a regression we want to catch.
    let active_post = engine.flow_table().active_conns();
    assert_eq!(
        active_post, 0,
        "active_conns = {active_post} after close drain — FSM integrity violation"
    );

    // Forensic gate (defensive — same condition as the assert_delta
    // above but reads the atomic directly so the failure-bundle bucket
    // can catch a different panic-message shape if the assert_delta
    // wiring is broken).
    let drop_unexpected = engine
        .counters()
        .tcp
        .mbuf_refcnt_drop_unexpected
        .load(Ordering::Relaxed);
    assert_eq!(
        drop_unexpected, 0,
        "mbuf_refcnt_drop_unexpected fired {drop_unexpected}× during exhaustion workload — leak signal"
    );
}
