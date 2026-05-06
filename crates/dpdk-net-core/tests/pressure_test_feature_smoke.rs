//! Smoke test for the `pressure-test` cargo feature (A11.0 step 2 / T1).
//!
//! Exercises every accessor introduced by T1:
//!   * `Counters::read_level_counter_u32` for `tcp.tx_data_mempool_avail`
//!     and `tcp.rx_mempool_avail` (`AtomicU32` level fields, NOT delta
//!     counters; the generic `lookup_counter` cannot read these because
//!     it is `&AtomicU64`-typed).
//!   * `FlowTable::active_conns` (count of currently-occupied slots).
//!   * `FlowTable::states` (FSM-state iterator for FSM-trajectory tests).
//!   * `FlowTable::reassembly_byte_occupancy` (global sum of OOO reorder
//!     queue bytes across all conns).
//!
//! No EAL or DPDK port is required — all four accessors operate on
//! Rust-side state structures (`Counters`, `FlowTable`) that can be
//! instantiated directly. This keeps the smoke test viable on every CI
//! tier (it does NOT require `DPDK_NET_TEST_TAP=1` or hugepages).
//!
//! Gated by `pressure-test` feature; the test binary is empty in default
//! builds, matching the `test-inject` smoke-test pattern.
#![cfg(feature = "pressure-test")]

use dpdk_net_core::counters::Counters;
use dpdk_net_core::flow_table::FlowTable;

#[test]
fn read_level_counter_u32_known_names() {
    let c = Counters::new();
    // Fresh counters: both samples default to 0 (the engine bumps them
    // inside `poll_once`'s once-per-second sampler; a Counters not yet
    // attached to an Engine never observes a sample).
    assert_eq!(c.read_level_counter_u32("tcp.tx_data_mempool_avail"), Some(0));
    assert_eq!(c.read_level_counter_u32("tcp.rx_mempool_avail"), Some(0));

    // Write a non-zero sample via the public AtomicU32 field and confirm
    // the typed accessor reflects it.
    use std::sync::atomic::Ordering;
    c.tcp.tx_data_mempool_avail.store(12345, Ordering::Relaxed);
    c.tcp.rx_mempool_avail.store(9876, Ordering::Relaxed);
    assert_eq!(c.read_level_counter_u32("tcp.tx_data_mempool_avail"), Some(12345));
    assert_eq!(c.read_level_counter_u32("tcp.rx_mempool_avail"), Some(9876));
}

#[test]
fn read_level_counter_u32_unknown_name_returns_none() {
    let c = Counters::new();
    assert!(c.read_level_counter_u32("nonexistent.path").is_none());
    // A delta counter is u64 and must NOT be reachable via the level-typed
    // accessor (callers that want delta counters use `lookup_counter`).
    assert!(c.read_level_counter_u32("tcp.tx_data").is_none());
}

#[test]
fn flow_table_active_conns_starts_at_zero() {
    let ft = FlowTable::new(8);
    assert_eq!(ft.active_conns(), 0);
}

#[test]
fn flow_table_states_iterator_empty_on_fresh_table() {
    let ft = FlowTable::new(8);
    let collected: Vec<_> = ft.states().collect();
    assert!(collected.is_empty());
}

#[test]
fn flow_table_reassembly_byte_occupancy_zero_on_fresh_table() {
    let ft = FlowTable::new(8);
    assert_eq!(ft.reassembly_byte_occupancy(), 0);
}
