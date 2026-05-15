# PO17-A Final Review — Cycle 5 (2026-05-14)

## Verdict
SAFE — no production TimeWait entry/exit or removal bypass was found in HEAD `2c1e53f`; recommend landing with one debug invariant to catch future counter drift.

## Bugs Found
No correctness bugs found in HEAD behavior.

Non-blocking hardening finding:

- LOW, `crates/dpdk-net-core/src/engine.rs:5558`: `TimeWait` exit decrements with `saturating_sub(1)`. I did not find a current undercount path, but if a future bypass decrements from zero this masks the drift and leaves `tw_count == 0`, which would make `reap_time_wait` return early at `crates/dpdk-net-core/src/engine.rs:4316` while real `TimeWait` conns could exist. Repro sketch: introduce any direct state/remove path that exits `TimeWait` without a matching counter increment, then call an exit through `transition_conn`. Mitigation: add `debug_assert!(self.tw_count.get() > 0)` before the decrement, or compute exact count under debug before the early-return.

## Counter-Desync Path Analysis

### TimeWait Entry Sites

| file:line | routes through transition_conn? | notes |
|---|---|---|
| `crates/dpdk-net-core/src/tcp_input.rs:1585` | Yes | `FinWait1` + our FIN ACKed + peer FIN returns `Some(TimeWait)`; engine applies `outcome.new_state` via `transition_conn` at `crates/dpdk-net-core/src/engine.rs:5327`. |
| `crates/dpdk-net-core/src/tcp_input.rs:1589` | Yes | `FinWait2` + peer FIN returns `Some(TimeWait)`; same engine path at `crates/dpdk-net-core/src/engine.rs:5327`. |
| `crates/dpdk-net-core/src/tcp_input.rs:1591` | Yes | `Closing` + our FIN ACKed returns `Some(TimeWait)`; same engine path at `crates/dpdk-net-core/src/engine.rs:5327`. |
| `crates/dpdk-net-core/src/engine.rs:9458` | No, test only | Unit test seeds a standalone `FlowTable`, not an `Engine`; it only replicates the reaper predicate at `crates/dpdk-net-core/src/engine.rs:9467`. |
| `crates/dpdk-net-core/src/engine.rs:9463` | No, test only | Same standalone predicate test. |
| `crates/dpdk-net-core/src/tcp_input.rs:2933` | No, test only | Unit test seeds a local `TcpConn` to assert TimeWait ACK replay behavior. |

No other `.state = TcpState::TimeWait` assignments were found after grepping source and tests.

### TimeWait Exit Sites

| file:line | routes through transition_conn? | notes |
|---|---|---|
| `crates/dpdk-net-core/src/engine.rs:4369` | Yes | `reap_time_wait` moves expired/skipped `TimeWait` conns to `Closed`, then removes the slot at `crates/dpdk-net-core/src/engine.rs:4429`; decrement happens in `transition_conn` at `crates/dpdk-net-core/src/engine.rs:5558`. |
| `crates/dpdk-net-core/src/tcp_input.rs:1511` | Yes | RST in any close state, including `TimeWait` because dispatch includes it at `crates/dpdk-net-core/src/tcp_input.rs:407`, returns `Some(Closed)`; engine applies it at `crates/dpdk-net-core/src/engine.rs:5327` and removes only after verifying `Closed` at `crates/dpdk-net-core/src/engine.rs:5456`. |
| `crates/dpdk-net-core/src/engine.rs:4271` | Yes | `force_close_etimedout` / `abort_conn` can close an arbitrary handle, including hypothetical `TimeWait`; it transitions through `transition_conn` before removing at `crates/dpdk-net-core/src/engine.rs:4303`. |
| `crates/dpdk-net-core/src/engine.rs:7951` | No, destructor only | `Engine::drop` clears the whole flow table. This can discard `TimeWait` without decrement, but the engine is being destroyed and `tw_count` is never read again. |
| `crates/dpdk-net-core/src/engine.rs:2987` | No, public mutable escape hatch | `Engine::flow_table()` exposes `RefMut<FlowTable>`. Current grep found no caller that seeds/removes `TimeWait` through it, but a future Rust-side test/tool could bypass `transition_conn`; this is not used by the C ABI. |

Other `Closed`/remove paths checked:

- `crates/dpdk-net-core/src/engine.rs:5512`: normal `outcome.closed` removal only runs after state is `Closed` at `crates/dpdk-net-core/src/engine.rs:5456`; a `TimeWait` RST reaches that state through `transition_conn`.
- `crates/dpdk-net-core/src/engine.rs:6231`: connect failure removes a newly inserted client before `SynSent`; not `TimeWait`.
- `crates/dpdk-net-core/src/engine.rs:8505`: passive-open relisten teardown removes a passive `SynReceived` conn after synthetic `SynReceived -> Listen` accounting at `crates/dpdk-net-core/src/engine.rs:8476`; not `TimeWait`.

### Bypass Risk Summary

No production bypass paths found.

Observed non-production/lifecycle bypasses:

- Direct `TimeWait` fixture assignment in unit tests: `crates/dpdk-net-core/src/engine.rs:9458`, `crates/dpdk-net-core/src/engine.rs:9463`, `crates/dpdk-net-core/src/tcp_input.rs:2933`.
- Engine destruction clears all slots at `crates/dpdk-net-core/src/engine.rs:7951`.
- Public Rust `flow_table()` exposes direct mutation at `crates/dpdk-net-core/src/engine.rs:2987`; no observed current caller creates a `TimeWait` counter bypass.

## Test Coverage Analysis

Relevant coverage:

- `crates/dpdk-net-core/tests/counter-coverage.rs:1791`, `:1805`, `:1833`, `:1851` cover the `FinWait1/FinWait2/Closing -> TimeWait` rows and `TimeWait -> Closed` row.
- `crates/dpdk-net-core/tests/counter-coverage.rs:2038` reaches `TimeWait`, advances virtual time, and calls `test_reap_time_wait`, whose engine shim is at `crates/dpdk-net-core/src/engine.rs:8574`.
- `crates/dpdk-net-core/tests/obs_smoke.rs:159` expects `FinWait1 -> TimeWait -> Closed`, and `crates/dpdk-net-core/tests/obs_smoke.rs:264` asserts the ordered StateChange chain including `TimeWait -> Closed`.
- `crates/dpdk-net-core/tests/tcp_basic_tap.rs:293` asserts the `TimeWait -> Closed` transition counter becomes non-zero.
- `crates/dpdk-net-core/tests/test_server_active_close.rs:56` verifies active close reaches `TimeWait`.

Coverage gap: I did not see a test that intentionally desynchronizes `tw_count` against real flow-table `TimeWait` population, nor a debug invariant test. Existing real-path tests should fail if normal entry does not increment or normal reap does not decrement.

## Soundness / Concurrency Notes

`Cell<u32>` does not introduce a new thread-safety model. `Engine` already contains `Cell` fields at `crates/dpdk-net-core/src/engine.rs:1202` and `RefCell` fields at `crates/dpdk-net-core/src/engine.rs:1219`, and comments explicitly describe it as single-lcore / `!Sync` at `crates/dpdk-net-core/src/engine.rs:1350`. I found no `unsafe impl Send` or `unsafe impl Sync` for `Engine`.

The C ABI obtains `&Engine` from a raw pointer at `crates/dpdk-net/src/lib.rs:58`; concurrent calls on that raw pointer were already outside Rust's normal aliasing guarantees and would already contend with `RefCell`-mutated engine state. PO17-A does not add a new unsafe block or wire-visible behavior.

Placement is safe under the current single-lcore model: `transition_conn` writes `conn.state` at `crates/dpdk-net-core/src/engine.rs:5537`, sets the TimeWait deadline at `crates/dpdk-net-core/src/engine.rs:5539`, drops the flow-table borrow at `crates/dpdk-net-core/src/engine.rs:5543`, then updates `tw_count` at `crates/dpdk-net-core/src/engine.rs:5556`. There is no callback or reentrant call between the state write and counter update.

## Recommendation
LAND WITH DEBUG_ASSERT.

Suggested assertion at the decrement arm:

```rust
} else if from == TcpState::TimeWait && to != TcpState::TimeWait {
    debug_assert!(self.tw_count.get() > 0, "tw_count underflow on TimeWait exit");
    self.tw_count.set(self.tw_count.get().saturating_sub(1));
}
```

Reasoning: current production entry/exit paths route through `transition_conn`, and the only observed bypasses are tests, public direct-mutation escape hatch, or destructor cleanup. The debug assertion is cheap and catches the exact failure mode that `saturating_sub` would otherwise hide.
