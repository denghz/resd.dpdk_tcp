//! Pressure-correctness Suite: loss-recovery cliff curve.
//!
//! Suite goal — drive the engine through a range of kernel-side netem
//! loss / delay / reorder scenarios while sustaining N=4 concurrent
//! connections, and prove that loss-recovery (RTO / TLP / RACK fast
//! retransmit) fires *and* the resource invariants hold under it:
//!
//!   * `tcp.tx_retrans`                  >  0   (proves loss-recovery fired)
//!   * `tcp.tx_rto + tcp.tx_tlp`         >  0   (RTO or TLP must engage)
//!   * `obs.events_dropped`              == 0   (event soft-cap not exceeded)
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0   (mbuf accounting integrity)
//!   * `tcp.tx_rst`                      == 0   (no abortive close under loss)
//!   * void-retransmit oracle: `TcpRetrans` event count
//!     >= `tcp.tx_retrans` counter delta
//!     (the counter must never overcount the per-packet event stream,
//!     which would indicate double-bumping on the retrans path).
//!
//! Six baseline buckets sweep the loss / delay / reorder grid against the
//! engine's default mempool sizing. A seventh ENOMEM bucket clamps the
//! TX-data mempool to 32 mbufs and re-runs the most adverse netem profile
//! to drive `eth.tx_drop_nomem` deltas off zero, confirming that the
//! retransmit path treats ENOMEM as a soft failure (counter bumps,
//! workload still progresses, void-retransmit oracle still holds).
//!
//! Per-bucket: 60s sustained 16 KiB writes per conn against a kernel TCP
//! echo peer over the TAP iface. Failure-bundle pattern matches the
//! existing pressure suites — open a `PressureBucket` before the workload,
//! `finish_ok` on success, `finish_fail` (which dumps a forensic bundle)
//! on a caught panic.

#![cfg(feature = "pressure-test")]

mod common;

use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};
use dpdk_net_core::flow_table::ConnHandle;
use dpdk_net_core::tcp_events::InternalEvent;

const TAP_IFACE: &str = "resdtap34";
const OUR_IP: u32 = 0x0a_63_22_02; // 10.99.34.2
const PEER_IP: u32 = 0x0a_63_22_01; // 10.99.34.1
const PEER_IP_STR: &str = "10.99.34.1";
const OUR_IP_STR: &str = "10.99.34.2";
const PEER_PORT: u16 = 5034;

/// Concurrent connections fanned per bucket.
const N_CONNS: usize = 4;
/// Sustained per-conn write size — large enough for fragmentation across
/// multiple MSS-sized segments so RACK fast-retransmit and TLP can engage
/// alongside RTO under loss.
const WRITE_SIZE: usize = 16 * 1024;
/// Wall-clock budget per bucket (nightly duration).
const BUCKET_DURATION_SECS: u64 = 60;

const SUITE_NAME: &str = "pressure-loss-recovery";

/// Baseline netem buckets — six profiles trimmed for practical runtime
/// (the original 12-profile grid would clock at 12 minutes; this trim
/// keeps the suite under ~7 minutes while preserving coverage of loss,
/// loss+delay, and loss+delay+reorder regimes).
const BASELINE_BUCKETS: &[(&str, &str)] = &[
    ("loss_05pct", "loss 0.5%"),
    ("loss_1pct", "loss 1%"),
    ("loss_3pct", "loss 3%"),
    ("loss_1pct_delay_5ms", "loss 1% delay 5ms"),
    ("loss_1pct_reorder_3", "loss 1% delay 5ms reorder 50% gap 3"),
    ("loss_3pct_delay_5ms", "loss 3% delay 5ms"),
];

/// ENOMEM bucket — same netem profile as the most adverse baseline, but
/// the engine is constructed with `tx_data_mempool_size: 32` so the
/// retransmit path occasionally fails its mbuf alloc and bumps
/// `eth.tx_drop_nomem`. The void-retransmit oracle must still hold.
const ENOMEM_BUCKET: (&str, &str) = (
    "loss_1pct_reorder_3_enomem",
    "loss 1% delay 5ms reorder 50% gap 3",
);
const ENOMEM_TX_DATA_MEMPOOL_SIZE: u32 = 32;

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
        .args(["addr", "add", "10.99.34.1/24", "dev", iface])
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

/// RAII guard for `tc qdisc add dev <iface> root netem <spec>` on the
/// kernel TAP iface. Drop best-effort issues `tc qdisc del dev <iface>
/// root` so a panic mid-bucket leaves no stale netem qdisc to corrupt the
/// next bucket. Modeled on `tx_mempool_no_leak_under_retrans.rs`.
struct NetemGuard {
    iface: &'static str,
}

impl NetemGuard {
    fn apply(iface: &'static str, spec: &str) -> Self {
        // Best-effort: pre-clean any leftover qdisc from a prior bucket
        // or a previous run that crashed before its Drop fired.
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
        eprintln!("[loss-recovery] applied netem on {iface}: {spec}");
        Self { iface }
    }
}

impl Drop for NetemGuard {
    fn drop(&mut self) {
        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", self.iface, "root"])
            .stderr(std::process::Stdio::null())
            .status();
        eprintln!("[loss-recovery] removed netem on {}", self.iface);
    }
}

fn extract_panic_msg(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&'static str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_string())
}

/// Spawn a kernel-side TCP echo server. The acceptor pulls accepted
/// sockets in a loop and spawns a per-conn echo worker so all N_CONNS
/// connections can run concurrently on the kernel side. The listener
/// thread is detached — it lives for the duration of the test process.
fn spawn_echo_server() {
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
}

/// Build the EngineConfig used by every bucket in this suite. The single
/// override point is `tx_data_mempool_size` — the ENOMEM bucket clamps it
/// to 32 mbufs to drive `eth.tx_drop_nomem` deltas off zero; baseline
/// buckets pass `0` to use the formula default.
fn make_engine_config(kernel_mac: [u8; 6], tx_data_mempool_size: u32) -> EngineConfig {
    EngineConfig {
        port_id: 0,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: kernel_mac,
        tcp_mss: 1460,
        max_connections: 16,
        tcp_msl_ms: 50,
        tcp_per_packet_events: true,
        tx_data_mempool_size,
        ..Default::default()
    }
}

/// EAL init is process-wide and idempotent — both `#[test]` entry points
/// call this. The first call performs the actual init; subsequent calls
/// in the same process accept the "already initialized" return without
/// panicking. (Cargo's default `--test-threads=N` runs tests in the same
/// binary serially when they share a TAP iface; either ordering works.)
fn ensure_eal() {
    let args = [
        "dpdk-net-pressure-loss-recovery",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap34",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    // Best-effort: a second call returns Err but does not corrupt state.
    let _ = eal_init(&args);
}

/// Drive a single bucket: bring the engine to steady-state, sustain
/// N_CONNS × WRITE_SIZE writes for `BUCKET_DURATION_SECS`, count
/// `TcpRetrans` events, close, drain, assert.
///
/// `is_enomem` selects the ENOMEM-specific assertion suite (also requires
/// `eth.tx_drop_nomem > 0` to prove the regime triggered).
fn run_bucket(
    bucket_name: &str,
    netem_spec: &str,
    tx_data_mempool_size: u32,
    is_enomem: bool,
) {
    bring_up_tap(TAP_IFACE);
    thread::sleep(Duration::from_millis(500));

    let kernel_mac = read_kernel_tap_mac(TAP_IFACE);
    let cfg = make_engine_config(kernel_mac, tx_data_mempool_size);
    let engine = Engine::new(cfg.clone()).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // Apply netem AFTER engine bring-up. The TAP iface is created by the
    // `net_tap0` vdev during `eal_init`; netem must wait until the iface
    // exists. Bringing it up after `Engine::new` also avoids the engine's
    // first GARP/ARP probes being delayed/dropped by the netem policy.
    let _netem_guard = NetemGuard::apply(TAP_IFACE, netem_spec);

    let bucket = PressureBucket::open(SUITE_NAME, bucket_name, engine.counters());
    let baseline = CounterSnapshot::capture(engine.counters());
    eprintln!(
        "[loss-recovery] bucket={bucket_name} netem=`{netem_spec}` \
         tx_data_mempool_size={tx_data_mempool_size} duration={BUCKET_DURATION_SECS}s \
         conns={N_CONNS}"
    );

    // `Arc<AtomicU64>` so the `drain_events` closure can capture it
    // immutably while still mutating the count. Using a plain `&AtomicU64`
    // with closures runs into borrow-checker conflicts when the same
    // closure is used in multiple drain phases (connect, workload, close).
    let events_count = Arc::new(AtomicU64::new(0));

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_workload(&engine, &events_count);

        // Final settle — pump for 50 polls so any in-flight mbufs
        // (NIC RX ring residue, lcore cache, FIN/ACK in flight,
        // TIME_WAIT reaper tick) drain before the post-snapshot.
        for _ in 0..50 {
            engine.poll_once();
            let ec = events_count.clone();
            engine.drain_events(64, |ev, _| {
                if matches!(ev, InternalEvent::TcpRetrans { .. }) {
                    ec.fetch_add(1, Ordering::Relaxed);
                }
            });
            thread::sleep(Duration::from_millis(2));
        }

        let post = CounterSnapshot::capture(engine.counters());
        let delta = post.delta_since(&baseline);
        let final_events = events_count.load(Ordering::Relaxed);

        eprintln!(
            "[loss-recovery] bucket={bucket_name} post: \
             tx_retrans={} tx_rto={} tx_tlp={} \
             tx_drop_nomem={} tx_rst={} \
             events_dropped={} mbuf_refcnt_drop_unexpected={} \
             TcpRetrans_events={final_events}",
            delta.delta.get("tcp.tx_retrans").copied().unwrap_or(0),
            delta.delta.get("tcp.tx_rto").copied().unwrap_or(0),
            delta.delta.get("tcp.tx_tlp").copied().unwrap_or(0),
            delta.delta.get("eth.tx_drop_nomem").copied().unwrap_or(0),
            delta.delta.get("tcp.tx_rst").copied().unwrap_or(0),
            delta.delta.get("obs.events_dropped").copied().unwrap_or(0),
            delta
                .delta
                .get("tcp.mbuf_refcnt_drop_unexpected")
                .copied()
                .unwrap_or(0),
        );

        // ─── Per-bucket assertions ─────────────────────────────────────
        // Loss-recovery actually fired: with sustained loss + 60s of
        // sustained writes, `tx_retrans` MUST be > 0. A regression that
        // turns netem loss into a no-op or breaks the retransmit path
        // would zero this out and surface as a vacuous pass on the
        // resource-invariant assertions below.
        assert_delta(&delta, "tcp.tx_retrans", Relation::Gt(0));

        // At least one of RTO or TLP must engage. Under pure RACK
        // recovery with no tail-segment losses, TLP wouldn't fire and
        // RTO might also stay quiet — but with the loss-rate budget in
        // these buckets (≥ 0.5%) at least one of the two timer-driven
        // paths runs across 60s of N=4 conns. (`tx_retrans` includes
        // RACK retransmits too, so the prior assert doesn't subsume
        // this one.)
        let rto = delta.delta.get("tcp.tx_rto").copied().unwrap_or(0);
        let tlp = delta.delta.get("tcp.tx_tlp").copied().unwrap_or(0);
        assert!(
            rto + tlp > 0,
            "neither RTO nor TLP fired under loss (bucket={bucket_name})"
        );

        // Hard tripwires — invariants that must hold even under loss.
        assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));
        assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
        // No abortive close: the workload runs to natural FIN-close;
        // any RST means an FSM error or the engine gave up under loss.
        assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));

        // ─── Void-retransmit oracle ────────────────────────────────────
        // Per-packet `TcpRetrans` events are emitted from each fire
        // handler in lock-step with the `tx_retrans` counter bump.
        // Either the counter and event stream agree, OR the event
        // stream is *strictly larger* (the counter could undercount if
        // a retransmit emits an event but skips the counter — tolerable
        // direction). The counter must NEVER overcount the events:
        // that direction would mean a double-bump on the retrans path
        // (e.g. fire handler bumps + downstream tx_pacer also bumps),
        // which is the regression class this oracle catches.
        let retrans_counter = delta.delta.get("tcp.tx_retrans").copied().unwrap_or(0);
        assert!(
            final_events as i64 >= retrans_counter,
            "void-retransmit oracle violated (bucket={bucket_name}): \
             events={final_events} < counter={retrans_counter}"
        );

        // ─── ENOMEM bucket extra assertions ────────────────────────────
        // The clamped tx_data_mempool drives the retransmit path into
        // ENOMEM regularly. Confirm `eth.tx_drop_nomem` actually
        // ticked — without this, the ENOMEM bucket would silently
        // degrade to a non-ENOMEM run (e.g. if a future formula change
        // raised the floor above our override).
        if is_enomem {
            assert_delta(&delta, "eth.tx_drop_nomem", Relation::Gt(0));
            // Re-check the void-retransmit oracle with a bucket-tagged
            // panic message so a failure here clearly identifies the
            // ENOMEM regime as the bucket of interest. (Same condition
            // as above — defense in depth for the regression class
            // that's the primary motivation for the ENOMEM bucket.)
            assert!(
                final_events as i64 >= retrans_counter,
                "ENOMEM: counter overcounts (bucket={bucket_name}): \
                 events={final_events} < counter={retrans_counter}"
            );
        }

        // ─── Pool drift ────────────────────────────────────────────────
        // Under sustained retransmit + loss both mempools must return to
        // within ±32 mbufs of baseline — the leading structural leak
        // signal in addition to `mbuf_refcnt_drop_unexpected`.
        assert_delta(
            &delta,
            "tcp.rx_mempool_avail",
            Relation::Range(-32, 32),
        );
        assert_delta(
            &delta,
            "tcp.tx_data_mempool_avail",
            Relation::Range(-32, 32),
        );

        // ─── FSM integrity ─────────────────────────────────────────────
        // Every connection opened in the workload must be reaped before
        // the snapshot. This catches stuck FSM states that the counter
        // deltas can mask.
        let active_post = engine.flow_table().active_conns();
        assert_eq!(
            active_post, 0,
            "active_conns = {active_post} after close drain (bucket={bucket_name}) \
             — FSM integrity violation"
        );
    }));

    match result {
        Ok(()) => {
            bucket.finish_ok();
            eprintln!("[loss-recovery] bucket={bucket_name} OK");
        }
        Err(payload) => {
            // Best-effort drain of any remaining events into the bundle.
            let mut events: Vec<InternalEvent> = Vec::with_capacity(1024);
            engine.drain_events(1024, |ev, _| {
                events.push(ev.clone());
            });
            let msg = extract_panic_msg(payload);
            let bundle_dir =
                bucket.finish_fail(engine.counters(), &cfg, events, msg.clone());
            panic!(
                "bucket {bucket_name} failed (bundle: {bundle_dir:?}): {msg}"
            );
        }
    }
}

/// Sustained-write workload for one bucket: open N_CONNS conns, drive
/// 16 KiB writes per conn for `BUCKET_DURATION_SECS`, then close + drain.
/// Counts `TcpRetrans` events into `events_count` for the void-retransmit
/// oracle. Panics on connect / close timeout — caller wraps in
/// `catch_unwind` for the failure-bundle path.
fn run_workload(engine: &Engine, events_count: &Arc<AtomicU64>) {
    // Open N_CONNS connections. local_port_hint=0 lets the engine pick
    // distinct ephemeral ports per conn; reusing the same hint would
    // collide on the second connect.
    let mut conns: Vec<ConnHandle> = Vec::with_capacity(N_CONNS);
    for _ in 0..N_CONNS {
        let h = engine.connect(PEER_IP, PEER_PORT, 0).expect("connect");
        conns.push(h);
    }

    // Wait for all N_CONNS Connected events. Drain TcpRetrans into the
    // shared counter; everything else is pumped to /dev/null. Under
    // heavy loss the SYN handshake itself may need a retrans, so the
    // connect deadline is generous.
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
        let ec = events_count.clone();
        let conns_ref = &conns;
        engine.drain_events(64, |ev, _| {
            match ev {
                InternalEvent::Connected { conn: c, .. } => {
                    if let Some(idx) = conns_ref.iter().position(|h| h == c) {
                        if !connected[idx] {
                            connected[idx] = true;
                            connected_count += 1;
                        }
                    }
                }
                InternalEvent::TcpRetrans { .. } => {
                    ec.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    eprintln!("[loss-recovery] all {N_CONNS} conns connected");

    // Per-conn round bookkeeping mirrors pressure_max_throughput's
    // pattern: each round writes WRITE_SIZE bytes per conn and waits
    // for WRITE_SIZE bytes echoed back, then resets. Under loss,
    // round time stretches out — that's expected; we just keep
    // pushing for the full bucket budget.
    let mut sent: Vec<usize> = vec![0; N_CONNS];
    let mut recvd: Vec<usize> = vec![0; N_CONNS];
    let payload = vec![0xCDu8; WRITE_SIZE];

    let mut round: u32 = 0;
    let workload_deadline = Instant::now() + Duration::from_secs(BUCKET_DURATION_SECS);
    while Instant::now() < workload_deadline {
        // Round-robin: try to drain remaining bytes for each conn. A
        // partial `send_bytes` (returns 0 on snd_wnd full, Err on
        // ConnFull / SendBufferFull) is fine — the next sweep retries.
        for i in 0..N_CONNS {
            if sent[i] < WRITE_SIZE {
                let chunk = &payload[sent[i]..];
                if let Ok(n) = engine.send_bytes(conns[i], chunk) {
                    sent[i] += n as usize;
                }
            }
        }

        engine.poll_once();
        let ec = events_count.clone();
        let conns_ref = &conns;
        engine.drain_events(64, |ev, _| {
            match ev {
                InternalEvent::Readable {
                    conn: c, total_len, ..
                } => {
                    if let Some(idx) = conns_ref.iter().position(|h| h == c) {
                        recvd[idx] = recvd[idx].saturating_add(*total_len as usize);
                    }
                }
                InternalEvent::TcpRetrans { .. } => {
                    ec.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        });

        // Round complete iff every conn has sent + received WRITE_SIZE.
        let round_done = (0..N_CONNS).all(|i| sent[i] >= WRITE_SIZE && recvd[i] >= WRITE_SIZE);
        if round_done {
            round = round.saturating_add(1);
            for i in 0..N_CONNS {
                sent[i] = 0;
                recvd[i] = 0;
            }
        }
    }
    eprintln!(
        "[loss-recovery] workload complete: rounds={round} duration={BUCKET_DURATION_SECS}s"
    );

    // Active-close every conn. close_conn returns an error only on an
    // unknown handle; it tolerates already-closing FSM states with a
    // successful no-op (matches the engine.close_conn doc-comment).
    for &h in &conns {
        let _ = engine.close_conn(h);
    }

    // Drain until every conn emits Closed. Under loss the FIN handshake
    // itself can take many RTOs, so the deadline is generous.
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
        let ec = events_count.clone();
        let conns_ref = &conns;
        engine.drain_events(64, |ev, _| {
            match ev {
                InternalEvent::Closed { conn: c, .. } => {
                    if let Some(idx) = conns_ref.iter().position(|h| h == c) {
                        if !closed[idx] {
                            closed[idx] = true;
                            closed_count += 1;
                        }
                    }
                }
                InternalEvent::TcpRetrans { .. } => {
                    ec.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        });
        thread::sleep(Duration::from_millis(2));
    }
    eprintln!("[loss-recovery] all {N_CONNS} conns closed");
}

/// Suite entry: 6 baseline buckets with the engine's default mempool
/// sizing. Each bucket gets its own engine + netem qdisc; they share the
/// kernel TCP echo server (which is spawned once per test process).
#[test]
fn pressure_loss_recovery_baseline() {
    if skip_if_not_tap() {
        return;
    }
    ensure_eal();
    spawn_echo_server();
    // Give the echo listener a moment to bind before the first bucket
    // tries to connect — under heavy loss a missed accept window would
    // surface as a connect timeout that has nothing to do with the
    // engine's own retransmit path.
    thread::sleep(Duration::from_millis(200));

    for &(bucket_name, netem_spec) in BASELINE_BUCKETS {
        run_bucket(bucket_name, netem_spec, 0, false);
    }
}

/// Suite entry: 1 ENOMEM bucket with the TX-data mempool clamped to 32
/// mbufs. Same netem profile as the most adverse baseline bucket
/// (`loss_1pct_reorder_3`); the small mempool drives the retransmit
/// path into `eth.tx_drop_nomem` regularly. The void-retransmit oracle
/// must still hold under ENOMEM.
#[test]
fn pressure_loss_recovery_enomem() {
    if skip_if_not_tap() {
        return;
    }
    ensure_eal();
    spawn_echo_server();
    thread::sleep(Duration::from_millis(200));

    let (bucket_name, netem_spec) = ENOMEM_BUCKET;
    run_bucket(bucket_name, netem_spec, ENOMEM_TX_DATA_MEMPOOL_SIZE, true);
}
