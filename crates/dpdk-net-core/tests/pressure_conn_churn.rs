//! Pressure Suite 2 — sequential connection-churn correctness.
//!
//! Goal: open + drive + close N back-to-back TCP connections through the engine
//! and prove that the resource invariants the engine claims under steady-state
//! still hold under churn. Distinct from the long-soak suite (one conn, many
//! RTTs) because the connection-lifecycle path itself is what's being stressed:
//! handshake state, FlowTable slot recycle, TIME_WAIT reaping, timer-wheel
//! arm/cancel pairing, and the close-path mempool drain all fire N times
//! in rapid succession.
//!
//! Per-PR bucket sized at N=64 in a ~10s window so the suite runs cheap on
//! CI; nightly soak buckets at higher N can wrap this same routine if needed.
//!
//! Asserts on the post-settle counter delta:
//!   * `tcp.conn_open` ≥ N        — every connect saw the handshake complete.
//!   * `tcp.conn_close` ≥ N       — every close saw the FSM walk to CLOSED.
//!   * `tcp.conn_table_full` == 0 — table never wedged (max_conns=8 vs N=64).
//!   * `tcp.tx_rst` == 0          — no spurious RSTs in the steady-state churn.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0 — no leaked / double-freed mbufs.
//!   * `obs.events_dropped` == 0   — drain cadence kept up under the churn.
//!   * RX / TX-data mempool drift ≤ ±32 — no per-cycle pool leak.
//!
//! Plus two structural gates that aren't counter-deltas:
//!   * `flow_table().active_conns() == 0` post-settle — every slot reaped.
//!   * Timer-wheel post-warmup growth ≤ 64 — no per-cycle slot accumulation.
//!
//! Gated on `DPDK_NET_TEST_TAP=1` + sudo (TAP vdev requires root) AND the
//! `pressure-test` cargo feature (the failure-bundle helper + level-counter
//! reads are pressure-test gated).

#![cfg(feature = "pressure-test")]

mod common;

use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpListener;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::tcp_events::InternalEvent;

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};

const TAP_IFACE: &str = "resdtap31";
const OUR_IP: u32 = 0x0a_63_1f_02; // 10.99.31.2
const PEER_IP: u32 = 0x0a_63_1f_01; // 10.99.31.1
const PEER_IP_STR: &str = "10.99.31.1";
const OUR_IP_STR: &str = "10.99.31.2";
const PEER_PORT: u16 = 5031;

/// Per-PR bucket — 64 cycles, ~10s wall-clock budget. Each cycle does
/// connect → 5×128B echo round-trip → active close → wait Closed.
const N_CHURN: u32 = 64;
/// Per-conn payload: 5 round-trips of 128 bytes. Small enough that a
/// single-conn round-trip fits in <1ms on TAP, big enough to exercise the
/// data path beyond the SYN/SYN-ACK boundary.
const BYTES_PER_CONN: usize = 5 * 128;

/// Mempool drift tolerance — same budget the long-soak / connect-close-cycle
/// tests use. Absorbs lcore mempool cache + NIC ring residue.
const POOL_DRIFT_TOLERANCE: i64 = 32;

/// Timer-wheel post-warmup growth budget — slots are never freed; this is
/// the gap between the warmup-snapshot peak and the post-settle peak. The
/// long-soak test uses 64 for 100k iters, so 64 is comfortable for 64
/// cycles too — anything over this signals a per-cycle slot-recycle bug.
const TIMER_POST_WARMUP_GROWTH: usize = 64;

/// After how many warmup cycles to capture the timer-wheel baseline.
const WARMUP_CYCLES: u32 = 10;

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
        .args(["addr", "add", "10.99.31.1/24", "dev", iface])
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
fn pressure_conn_churn_n64_10s() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-churn",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap31",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    eal_init(&args).expect("EAL init");

    bring_up_tap(TAP_IFACE);
    thread::sleep(Duration::from_millis(500));

    let kernel_mac = read_kernel_tap_mac(TAP_IFACE);

    // max_connections=8 lets the table absorb at most one in-flight conn
    // plus a TIME_WAIT residue without ever wedging — N=64 sequential
    // cycles only need 1-2 simultaneous slots. tcp_msl_ms=50 → TIME_WAIT
    // window ~100ms so reaping is fast enough not to dominate the budget.
    let cfg = EngineConfig {
        port_id: 0,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: kernel_mac,
        tcp_mss: 1460,
        max_connections: 8,
        tcp_msl_ms: 50,
        ..Default::default()
    };
    let engine = Engine::new(cfg.clone()).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // Multi-accept echo peer. The kernel side stays running for the full
    // suite — each accepted socket spawns its own echo handler that runs
    // until EOF, mirroring whatever bytes our engine pushes.
    let listener = TcpListener::bind((PEER_IP_STR, PEER_PORT)).expect("bind echo");
    listener
        .set_nonblocking(false)
        .expect("listener blocking-mode");
    // SO_REUSEADDR for clean re-binds across repeated test runs in CI;
    // std's TcpListener::bind sets it on Linux by default, but we make
    // the intent explicit via set_ttl-adjacent socket-options where the
    // platform supports it. (std doesn't expose a portable SO_REUSEADDR
    // setter post-bind; the bind-time default is sufficient.)
    let _ = listener.set_ttl(64);
    thread::spawn(move || loop {
        let (mut sock, _) = match listener.accept() {
            Ok(s) => s,
            Err(_) => break,
        };
        sock.set_nodelay(true).ok();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match sock.read(&mut buf) {
                    Ok(0) => break, // EOF — peer closed.
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    });

    // Open the pressure bucket BEFORE the first cycle so the snapshot
    // baseline reflects the engine post-bring-up but pre-workload.
    let bucket = PressureBucket::open(
        "pressure-conn-churn",
        "n64_10s",
        engine.counters(),
    );
    let before = CounterSnapshot::capture(engine.counters());

    eprintln!(
        "[pressure-conn-churn] starting N={} cycles ({} bytes/conn)",
        N_CHURN, BYTES_PER_CONN
    );

    let mut warmup_slots: Option<usize> = None;
    let payload = vec![0xABu8; BYTES_PER_CONN];
    let loop_start = Instant::now();

    for i in 0..N_CHURN {
        if i > 0 && i.is_multiple_of(10) {
            eprintln!(
                "[pressure-conn-churn] cycle {} / {} (elapsed {:?})",
                i,
                N_CHURN,
                loop_start.elapsed()
            );
        }

        // Capture timer-wheel slot count after the warmup window so we can
        // separate steady-state peak from per-cycle creep below.
        if i == WARMUP_CYCLES {
            warmup_slots = Some(engine.timer_wheel_slots_len());
            eprintln!(
                "[pressure-conn-churn] warmup_slots = {}",
                warmup_slots.unwrap()
            );
        }

        // 1. Connect. Third arg is `local_port_hint: u16` — passing 0 lets
        //    the engine pick an ephemeral port, matching the spec intent
        //    of "one fresh conn per cycle" (the spec calls this slot a
        //    "tag" but the engine API names it `local_port_hint`; 0 is
        //    the documented "let the stack pick" sentinel).
        let conn = engine
            .connect(PEER_IP, PEER_PORT, 0)
            .unwrap_or_else(|e| panic!("cycle {i}: connect: {e:?}"));

        // 2. Drive handshake to Connected (5s deadline per cycle).
        let mut connected = false;
        let connect_deadline = Instant::now() + Duration::from_secs(5);
        while !connected && Instant::now() < connect_deadline {
            engine.poll_once();
            engine.drain_events(16, |ev, _| {
                if matches!(ev, InternalEvent::Connected { conn: c, .. } if *c == conn) {
                    connected = true;
                }
            });
            std::hint::spin_loop();
        }
        assert!(connected, "cycle {i}: connect timeout");

        // 3. Send BYTES_PER_CONN; pump send_bytes until accepted, draining
        //    incoming Readable in parallel so we don't backlog the recv
        //    buffer while sending.
        let mut sent: u32 = 0;
        let mut recv_total: u32 = 0;
        let send_deadline = Instant::now() + Duration::from_secs(5);
        while (sent as usize) < BYTES_PER_CONN {
            if let Ok(n) = engine.send_bytes(conn, &payload[sent as usize..]) {
                sent = sent.saturating_add(n);
            }
            engine.poll_once();
            engine.drain_events(32, |ev, _| {
                if let InternalEvent::Readable { total_len, conn: c, .. } = ev {
                    if *c == conn {
                        recv_total = recv_total.saturating_add(*total_len);
                    }
                }
            });
            assert!(
                Instant::now() < send_deadline,
                "cycle {i}: send_bytes timeout (sent={sent})"
            );
        }

        // 4. Drain remaining echo: wait for BYTES_PER_CONN bytes back.
        let recv_deadline = Instant::now() + Duration::from_secs(5);
        while (recv_total as usize) < BYTES_PER_CONN {
            engine.poll_once();
            engine.drain_events(32, |ev, _| {
                if let InternalEvent::Readable { total_len, conn: c, .. } = ev {
                    if *c == conn {
                        recv_total = recv_total.saturating_add(*total_len);
                    }
                }
            });
            assert!(
                Instant::now() < recv_deadline,
                "cycle {i}: drain timeout (recv_total={recv_total})"
            );
        }

        // 5. Initiate active close. We don't use FORCE_TW_SKIP — the spec
        //    deliberately exercises the real TIME_WAIT path with
        //    tcp_msl_ms=50 (≈100ms TIME_WAIT). That's the resource-
        //    integrity cost we want measured.
        engine
            .close_conn(conn)
            .unwrap_or_else(|e| panic!("cycle {i}: close_conn: {e:?}"));

        // 6. Pump until Closed for this handle (5s deadline; TIME_WAIT
        //    accelerated to 100ms). The peer's echo handler will see EOF
        //    after our FIN drains and shut its side cleanly.
        let mut closed = false;
        let close_deadline = Instant::now() + Duration::from_secs(5);
        while !closed && Instant::now() < close_deadline {
            engine.poll_once();
            engine.drain_events(32, |ev, _| {
                if matches!(ev, InternalEvent::Closed { conn: c, .. } if *c == conn) {
                    closed = true;
                }
            });
            std::hint::spin_loop();
        }
        assert!(closed, "cycle {i}: did not observe Closed event");
    }
    let loop_elapsed = loop_start.elapsed();

    // Final settle — pump for ~100ms (50 × 2ms) so any in-flight close-
    // path mbufs (NIC RX ring, lcore cache, TIME_WAIT reaper completion)
    // drain before the post-snapshot.
    for _ in 0..50 {
        engine.poll_once();
        engine.drain_events(32, |_ev, _| {});
        thread::sleep(Duration::from_millis(2));
    }

    eprintln!(
        "[pressure-conn-churn] {} cycles in {:?} ({:.2}ms/cycle)",
        N_CHURN,
        loop_elapsed,
        (loop_elapsed.as_millis() as f64) / (N_CHURN as f64)
    );

    // Capture the post-settle snapshot and the structural state we'll
    // assert directly (not via assert_delta).
    let after = CounterSnapshot::capture(engine.counters());
    let delta = after.delta_since(&before);
    let active_conns_post = engine.flow_table().active_conns();
    let final_slots = engine.timer_wheel_slots_len();

    eprintln!(
        "[pressure-conn-churn] post: active_conns={} timer_slots={}",
        active_conns_post, final_slots
    );

    // Run the asserts inside catch_unwind so a failure dumps a forensic
    // bundle (counters_before/after/delta + config + last_error) to disk
    // before re-raising. The PressureBucket's finish_fail does the dump
    // and returns the bundle path, which we surface in the panic so a
    // CI failure log links straight to the artifacts.
    let counters_for_dump = engine.counters();
    let assert_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Conn accounting: opens must equal closes (exact parity) and
        // both ≥ N. Ge allows retried connects (PeerUnreachable while ARP
        // warms), but the open/close parity check is strict: any asymmetry
        // is an FSM accounting bug regardless of the iteration count.
        assert_delta(&delta, "tcp.conn_open", Relation::Ge(N_CHURN as i64));
        assert_delta(&delta, "tcp.conn_close", Relation::Ge(N_CHURN as i64));
        let opens = delta.delta.get("tcp.conn_open").copied().unwrap_or(0);
        let closes = delta.delta.get("tcp.conn_close").copied().unwrap_or(0);
        assert_eq!(
            opens, closes,
            "conn_open ({opens}) ≠ conn_close ({closes}) — FSM accounting parity error"
        );

        // No overflow of the connection table.
        assert_delta(&delta, "tcp.conn_table_full", Relation::Eq(0));

        // No spurious RSTs — every connection should walk the four-way
        // close. A single tx_rst here means the FSM took an abort path
        // when it shouldn't have.
        assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));

        // Resource integrity: refcount-drop guard never fired and the
        // observability soft-cap never overflowed.
        assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
        assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

        // Pool drift — RX and TX-data mempools both within ±32 mbufs of
        // baseline. Level counters; the Range checks both directions.
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

        // Structural: every flow-table slot reaped.
        assert_eq!(
            active_conns_post, 0,
            "flow_table.active_conns() = {active_conns_post} ≠ 0 \
             after settle — slot(s) didn't reap"
        );

        // Structural: timer-wheel post-warmup growth bounded.
        let warmup_slots =
            warmup_slots.expect("warmup snapshot captured at WARMUP_CYCLES");
        let post_warmup_growth = final_slots.saturating_sub(warmup_slots);
        assert!(
            post_warmup_growth as i64 <= TIMER_POST_WARMUP_GROWTH as i64,
            "timer_wheel slots grew {post_warmup_growth} after the \
             {WARMUP_CYCLES}-cycle warmup (warmup {warmup_slots}, post {final_slots}) \
             — exceeds tolerance {TIMER_POST_WARMUP_GROWTH}; likely \
             slot-recycle regression"
        );
    }));

    match assert_result {
        Ok(()) => {
            bucket.finish_ok();
        }
        Err(panic) => {
            let err_str = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    panic
                        .downcast_ref::<&'static str>()
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            let bundle_dir =
                bucket.finish_fail(counters_for_dump, &cfg, Vec::new(), err_str.clone());
            panic!(
                "pressure-conn-churn assertion failed: {err_str}\n\
                 forensic bundle written to {bundle_dir:?}"
            );
        }
    }
}
