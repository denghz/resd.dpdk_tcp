//! Pressure-correctness Suite 1: max-throughput cliff curve.
//!
//! Suite goal — drive the engine at the highest sustained throughput a single
//! lcore can issue against a kernel echo peer over TAP, then prove that
//! none of the resource invariants slip even when every available slot in
//! both the conn-table and the mempool fleet is in active use:
//!
//!   * `eth.tx_drop_nomem`               == 0  (hard tripwire — TX-pool exhaustion)
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0  (mbuf accounting integrity)
//!   * `obs.events_dropped`              == 0  (event soft-cap not exceeded)
//!   * `tcp.rx_mempool_avail` drift      ±32   (RX leak class)
//!   * `tcp.tx_data_mempool_avail` drift ±32   (TX-data leak class)
//!   * timer-wheel slot growth post-warmup ≤ 64 (slot-recycle regression)
//!   * FlowTable::active_conns() == 0 after all close (FSM integrity)
//!
//! Per-PR bucket fans 16 concurrent connections across the engine for a
//! 10s wall-clock window. Each round per conn writes 16 KiB and waits for
//! 16 KiB echoed back; the round count is whatever the cliff allows in
//! the budget. Per-round progress is `eprintln!`d so the cliff curve is
//! visible in CI logs for cross-PR regression triage.
//!
//! Failure-bundle pattern: a single `PressureBucket` is opened before
//! the workload and `finish_ok`'d on success. Workload + assertions run
//! under `catch_unwind` so any panic is forwarded to `finish_fail`,
//! which dumps a forensic bundle (counter snapshots before/after,
//! engine config, recent events, error text) under
//! `target/pressure-test/pressure-max-throughput/n16_w16k_10s/<unix_ms>/`.

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
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "resdtap30";
const OUR_IP: u32 = 0x0a_63_1e_02; // 10.99.30.2
const PEER_IP: u32 = 0x0a_63_1e_01; // 10.99.30.1
const PEER_IP_STR: &str = "10.99.30.1";
const OUR_IP_STR: &str = "10.99.30.2";
const PEER_PORT: u16 = 5030;

/// Concurrent connections fanned by the engine during the workload.
const N_CONNS: usize = 16;
/// Bytes the engine sends *and* expects echoed back per conn per round.
const WRITE_SIZE: usize = 16 * 1024;
/// Wall-clock budget for the bench portion of the workload.
const DURATION_SECS: u64 = 10;
/// Max signed drift allowed on either mempool level counter (RX / TX-data).
/// Mirrors the `long_soak_stability` tolerance — under healthy operation,
/// the lcore-cache + NIC ring residue lands well under 32 mbufs.
const POOL_DRIFT_TOLERANCE: i64 = 32;
/// Timer-wheel slots are never deallocated; `slots_len()` reports the
/// all-time peak in-flight depth. Healthy operation reaches a steady-state
/// peak within a few warmup rounds and stays flat. Allow 64 to absorb a
/// transient cascade-driven spike without flagging.
const TIMER_POST_WARMUP_GROWTH: usize = 64;
/// First N completed rounds count as warmup. After round N completes we
/// take the timer-wheel baseline and then assert post-warmup growth
/// against the final reading.
const WARMUP_ROUNDS: u32 = 5;

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
        .args(["addr", "add", "10.99.30.1/24", "dev", iface])
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
fn pressure_max_throughput_n16_w16k_10s() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-maxtp",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap30",
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
        // Headroom above N_CONNS so a slow active-close on round N does
        // not block the workload's per-round connection-set churn.
        max_connections: 32,
        tcp_msl_ms: 100,
        ..Default::default()
    };
    let engine = Engine::new(cfg.clone()).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // Kernel-side echo server. The acceptor thread pulls accepted sockets
    // off the listener in a loop and spawns an echo worker per conn so all
    // N_CONNS connections can run concurrently on the kernel side too.
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
                let mut buf = [0u8; 4096];
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

    // Open the bucket BEFORE the workload starts so the entry-snapshot
    // covers the engine's bring-up state, not the mid-workload state.
    let bucket = PressureBucket::open(
        "pressure-max-throughput",
        "n16_w16k_10s",
        engine.counters(),
    );
    let baseline = CounterSnapshot::capture(engine.counters());
    let baseline_timer_slots = engine.timer_wheel_slots_len();
    eprintln!(
        "[pressure-maxtp] baseline timer_slots={} active_conns={}",
        baseline_timer_slots,
        engine.flow_table().active_conns()
    );

    // Wrap the workload + assertions in catch_unwind so any panic
    // (assertion failure, send error, drain timeout) goes through the
    // failure-bundle path before being re-raised.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_workload(&engine, baseline_timer_slots, &baseline);
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
            let bundle_dir = bucket.finish_fail(
                engine.counters(),
                &cfg,
                events,
                err_msg.clone(),
            );
            std::panic::resume_unwind(Box::new(format!(
                "pressure_max_throughput_n16_w16k_10s panicked: {err_msg}; \
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
    baseline_timer_slots: usize,
    baseline: &CounterSnapshot,
) {
    // Open N_CONNS connections. local_port_hint=0 lets the engine pick
    // distinct ephemeral ports per conn; reusing the same hint would
    // collide on the second connect.
    let mut conns: Vec<ConnHandle> = Vec::with_capacity(N_CONNS);
    for _ in 0..N_CONNS {
        let h = engine
            .connect(PEER_IP, PEER_PORT, 0)
            .expect("connect");
        conns.push(h);
    }

    // Wait for all N_CONNS Connected events. Drain everything else into
    // /dev/null — the workload below tracks Readable/Closed itself.
    let connect_deadline = Instant::now() + Duration::from_secs(15);
    let mut connected = vec![false; N_CONNS];
    let mut connected_count = 0usize;
    while connected_count < N_CONNS {
        if Instant::now() >= connect_deadline {
            panic!(
                "connect timeout: {connected_count}/{N_CONNS} connections established"
            );
        }
        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            if let InternalEvent::Connected { conn: c, .. } = ev {
                if let Some(idx) = conns.iter().position(|h| h == c) {
                    if !connected[idx] {
                        connected[idx] = true;
                        connected_count += 1;
                    }
                }
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    eprintln!("[pressure-maxtp] all {N_CONNS} conns connected");

    // Per-conn round bookkeeping: how many bytes already pushed into
    // send_bytes for the current round, and how many bytes echoed back.
    let mut sent: Vec<usize> = vec![0; N_CONNS];
    let mut recvd: Vec<usize> = vec![0; N_CONNS];
    let payload = vec![0xCDu8; WRITE_SIZE];

    let mut round: u32 = 0;
    let mut warmup_timer_slots: Option<usize> = None;
    let workload_deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
    while Instant::now() < workload_deadline {
        // Round-robin: try to drain remaining bytes for each conn before
        // running poll/drain. send_bytes can return 0 (snd_wnd full) or
        // an error (e.g. ConnFull) — partial progress is fine, the next
        // sweep picks it up.
        for i in 0..N_CONNS {
            if sent[i] < WRITE_SIZE {
                let chunk = &payload[sent[i]..];
                if let Ok(n) = engine.send_bytes(conns[i], chunk) {
                    sent[i] += n as usize;
                }
            }
        }

        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            if let InternalEvent::Readable { conn: c, total_len, .. } = ev {
                if let Some(idx) = conns.iter().position(|h| h == c) {
                    recvd[idx] = recvd[idx].saturating_add(*total_len as usize);
                }
            }
        });

        // Round complete iff every conn has sent + received WRITE_SIZE.
        let round_done = (0..N_CONNS).all(|i| sent[i] >= WRITE_SIZE && recvd[i] >= WRITE_SIZE);
        if round_done {
            round = round.saturating_add(1);
            // Reset per-conn round bookkeeping for the next round.
            for i in 0..N_CONNS {
                sent[i] = 0;
                recvd[i] = 0;
            }
            // Per-round progress so the cliff curve is visible in CI logs.
            let timer_slots = engine.timer_wheel_slots_len();
            let active = engine.flow_table().active_conns();
            eprintln!(
                "[pressure-maxtp] round={round} timer_slots={timer_slots} active_conns={active}"
            );
            if round == WARMUP_ROUNDS {
                warmup_timer_slots = Some(timer_slots);
                eprintln!(
                    "[pressure-maxtp] warmup baseline (post round {round}): timer_slots={timer_slots}"
                );
            }
        }
    }
    let total_rounds = round;
    eprintln!(
        "[pressure-maxtp] workload complete: rounds={total_rounds} duration={DURATION_SECS}s"
    );
    assert!(
        total_rounds >= 1,
        "no complete round in {DURATION_SECS}s — engine or echo-server not making progress"
    );

    // Active-close every conn. close_conn returns an error only on an
    // unknown handle; it tolerates already-closing FSM states with a
    // successful no-op (matches the engine.close_conn doc-comment).
    for &h in &conns {
        let _ = engine.close_conn(h);
    }

    // Drain until every conn emits Closed. Use a generous deadline —
    // active-close walks through FIN_WAIT_1 → FIN_WAIT_2 → TIME_WAIT and
    // depending on RTT can take a few hundred ms per conn.
    let mut closed = vec![false; N_CONNS];
    let mut closed_count = 0usize;
    let close_deadline = Instant::now() + Duration::from_secs(20);
    while closed_count < N_CONNS {
        if Instant::now() >= close_deadline {
            panic!(
                "close drain timeout: {closed_count}/{N_CONNS} connections closed"
            );
        }
        engine.poll_once();
        engine.drain_events(64, |ev, _| {
            if let InternalEvent::Closed { conn: c, .. } = ev {
                if let Some(idx) = conns.iter().position(|h| h == c) {
                    if !closed[idx] {
                        closed[idx] = true;
                        closed_count += 1;
                    }
                }
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    eprintln!("[pressure-maxtp] all {N_CONNS} conns closed");

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

    // Hard tripwires — any of these firing means the workload pushed the
    // engine past a documented invariant.
    assert_delta(&delta, "eth.tx_drop_nomem", Relation::Eq(0));
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mempool drift — level counters can move in either direction; the
    // workload should leave both pools within ±POOL_DRIFT_TOLERANCE of
    // their pre-workload level.
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

    // FSM integrity — every conn we opened must be reaped. This is the
    // direct visibility on the conn-table; derived deltas from
    // tcp.conn_open / tcp.conn_close can mask off-by-one bugs.
    let active_conns_post = engine.flow_table().active_conns();
    assert_eq!(
        active_conns_post, 0,
        "active_conns = {active_conns_post} after close drain — FSM integrity violation"
    );

    // Timer-wheel slot growth post-warmup. Slots are never freed; the
    // wheel reports all-time peak depth. Healthy operation reaches a
    // steady-state peak within WARMUP_ROUNDS rounds and stays flat.
    let post_timer_slots = engine.timer_wheel_slots_len();
    let warmup_slots = warmup_timer_slots.unwrap_or(baseline_timer_slots);
    let timer_growth = (post_timer_slots as i64) - (warmup_slots as i64);
    eprintln!(
        "[pressure-maxtp] timer_slots: baseline={baseline_timer_slots} \
         warmup={warmup_slots} post={post_timer_slots} growth={timer_growth}"
    );
    assert!(
        timer_growth <= TIMER_POST_WARMUP_GROWTH as i64,
        "timer_wheel slots grew {timer_growth} after the {WARMUP_ROUNDS}-round \
         warmup (warmup {warmup_slots}, post {post_timer_slots}) — exceeds \
         tolerance {TIMER_POST_WARMUP_GROWTH}; likely slot-recycle regression"
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
