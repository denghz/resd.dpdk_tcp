//! Integration test that brings up an engine against a TAP virtual device
//! (no real NIC needed). Runs only when DPDK_NET_TEST_TAP=1 in env.

#[test]
fn engine_lifecycle_on_tap() {
    if std::env::var("DPDK_NET_TEST_TAP").ok().as_deref() != Some("1") {
        eprintln!("skipping; set DPDK_NET_TEST_TAP=1 to run");
        return;
    }

    // EAL args: in-memory, use vdev TAP so no real NIC is required.
    let args = [
        "dpdk-net-test",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=dpdktap0",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    dpdk_net_core::engine::eal_init(&args).expect("EAL init");

    let cfg = dpdk_net_core::engine::EngineConfig {
        port_id: 0,
        ..Default::default()
    };
    let engine = dpdk_net_core::engine::Engine::new(cfg).expect("engine new");

    // T1 review I-1, I-2: confirm `Engine::tx_hdr_mempool_size()` reports
    // the production default (2048) when the config carries the `0`
    // sentinel. Mirrors the `rx_mempool_size` / `tx_data_mempool_size`
    // diagnostic getters; pressure-test consumers rely on this to verify
    // the engine resolved their override correctly.
    assert_eq!(
        engine.tx_hdr_mempool_size(),
        2048,
        "tx_hdr_mempool_size() must resolve `0` sentinel → 2048 default"
    );

    // Poll a few times on an idle link; expect 0 packets.
    for _ in 0..10 {
        engine.poll_once();
    }
    let c = engine.counters();
    assert!(c.poll.iters.load(std::sync::atomic::Ordering::Relaxed) >= 10);
    // rx_pkts may be >0 if stray ARP arrived; that's fine, we just don't assert 0.
    drop(engine);
}
