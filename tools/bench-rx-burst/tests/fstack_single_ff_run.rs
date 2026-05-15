//! Regression test for the bench-rx-burst fstack arm hang observed in
//! the T55 fast-iter-suite v2 run on 2026-05-12 06:09.
//!
//! Symptom: `bench-rx-burst --stack fstack` initialized F-Stack
//! successfully (init logs visible — "Successed to register dpdk
//! interface") but never emitted any per-bucket progress markers and
//! got SIGKILLed by the outer 300 s timeout. The empty CSV cascaded
//! the failure into every subsequent fast-iter run because each
//! SIGKILL left an F-Stack ESTAB connection on the peer side, which
//! the peer's single-threaded `burst-echo-server` could never recycle
//! (it was blocked in `write()` to the dead DUT).
//!
//! Root cause was NOT structural (the grid was already wired inside a
//! single `ff_run` — see the module-level doc on
//! `tools/bench-rx-burst/src/fstack.rs`) but a missing forward-progress
//! watchdog: `phase_read_burst` looped on `FF_EAGAIN` forever when the
//! peer was wedged, with no way out short of process-level SIGKILL.
//!
//! This file asserts two invariants:
//!
//! 1. **Single `ff_run` per process** — the entire bucket grid runs
//!    inside one `ff_run` call site (string-grep over `fstack.rs`).
//!    `ff_run` is documented one-shot per process in
//!    `tools/bench-fstack-ffi/src/lib.rs` (calls `rte_eal_cleanup` on
//!    exit); a regression that re-introduces per-bucket `ff_run` would
//!    SIGSEGV instead of hang, mirroring the bench-rtt `8d40aa3`
//!    failure mode.
//!
//! 2. **Stall watchdog wired into the state machine** — both
//!    `phase_send_cmd` and `phase_read_burst` check
//!    `state.last_progress.elapsed() > STALL_TIMEOUT` before EAGAIN-
//!    looping, and `phase_wait_connect` checks `connect_started_at
//!    .elapsed() > CONNECT_TIMEOUT`. Without these checks a wedged
//!    peer hangs the bench until the outer process-level timeout fires.
//!
//! Both invariants live as text-pattern checks against the source so
//! they survive future refactors that rename the helpers — a behaviour
//! test would need a live F-Stack peer (DPDK, libfstack.a, hugepages,
//! a NIC bound to vfio-pci) which `cargo test` doesn't have.

const FSTACK_RS: &str = include_str!("../src/fstack.rs");

/// Count occurrences of `ff_run(` as a substring (the FFI binding
/// `ff_run(callback, arg)`). One occurrence is the legitimate entry
/// in `run_grid`; >1 means a future change re-introduced the
/// bench-rtt-8d40aa3 regression (per-bucket `ff_run`).
fn ff_run_call_sites() -> usize {
    // Count by counting non-overlapping matches of `ff_run(`. We
    // intentionally don't filter unsafe-block context: any
    // `ff_run(...)` mention in the source is a candidate call site.
    // Comment / docstring mentions use `ff_run` (no paren) so they
    // don't match.
    FSTACK_RS.matches("ff_run(").count()
}

#[test]
fn fstack_rs_calls_ff_run_exactly_once() {
    // The bench-rtt `8d40aa3` regression had per-bucket `ff_run`
    // calls. Asserting `==1` here pins the structural invariant so
    // any future change re-introducing the bug fails this test
    // before reaching a live AMI run.
    let n = ff_run_call_sites();
    assert_eq!(
        n, 1,
        "bench-rx-burst::fstack::run_grid must call ff_run exactly \
         once per process; found {n} call sites. ff_run calls \
         rte_eal_cleanup on exit, so multiple call sites SIGSEGV on \
         the second invocation (see tools/bench-fstack-ffi/src/lib.rs \
         lines 113-117)."
    );
}

#[test]
fn fstack_rs_ff_run_lives_inside_run_grid() {
    // Stronger structural check: the lone `ff_run(` call site must
    // be within `pub fn run_grid`. Catches a refactor that moves the
    // FFI call out of run_grid (e.g. into an iter loop in main.rs)
    // even if the total count stays at 1.
    let run_grid_idx = FSTACK_RS
        .find("pub fn run_grid")
        .expect("run_grid public entry not found");
    let ff_run_idx = FSTACK_RS
        .find("ff_run(")
        .expect("ff_run call site not found");
    assert!(
        ff_run_idx > run_grid_idx,
        "ff_run call site appears before `pub fn run_grid` in fstack.rs \
         — the grid driver must own the ff_run call, not a sibling fn"
    );
}

#[test]
fn fstack_rs_has_stall_watchdog_constants() {
    // The 2026-05-12 T55 hang fix added a forward-progress watchdog
    // so a wedged peer surfaces as a bucket-level error instead of
    // an infinite ff_read EAGAIN loop. Pin the constants so a
    // future refactor doesn't silently drop them.
    assert!(
        FSTACK_RS.contains("const STALL_TIMEOUT: Duration"),
        "STALL_TIMEOUT constant missing — phase_send_cmd / phase_read_burst \
         must enforce a forward-progress watchdog (was added 2026-05-12 \
         after the T55 v2 fast-iter hang)"
    );
    assert!(
        FSTACK_RS.contains("const CONNECT_TIMEOUT: Duration"),
        "CONNECT_TIMEOUT constant missing — phase_wait_connect must \
         enforce a connect-handshake ceiling so a backed-up peer accept \
         queue doesn't hang the bucket forever"
    );
}

#[test]
fn fstack_rs_send_cmd_checks_last_progress() {
    // Pin the watchdog check at the top of `phase_send_cmd`.
    let send_cmd_idx = FSTACK_RS
        .find("fn phase_send_cmd")
        .expect("phase_send_cmd not found");
    let read_burst_idx = FSTACK_RS
        .find("fn phase_read_burst")
        .expect("phase_read_burst not found");
    let body = &FSTACK_RS[send_cmd_idx..read_burst_idx];
    assert!(
        body.contains("state.last_progress.elapsed()"),
        "phase_send_cmd must check `state.last_progress.elapsed()` \
         against STALL_TIMEOUT — otherwise a wedged peer hangs the \
         bucket forever. Body excerpt:\n{body}"
    );
    assert!(
        body.contains("STALL_TIMEOUT"),
        "phase_send_cmd must reference STALL_TIMEOUT (the watchdog \
         constant)"
    );
}

#[test]
fn fstack_rs_read_burst_checks_last_progress() {
    // Same check on the read side — this is where the T55 v2 hang
    // actually manifested (ff_read EAGAIN looping for 300 s).
    let read_burst_idx = FSTACK_RS
        .find("fn phase_read_burst")
        .expect("phase_read_burst not found");
    let advance_idx = FSTACK_RS
        .find("fn advance_post_burst")
        .expect("advance_post_burst not found");
    let body = &FSTACK_RS[read_burst_idx..advance_idx];
    assert!(
        body.contains("state.last_progress.elapsed()"),
        "phase_read_burst must check `state.last_progress.elapsed()` \
         against STALL_TIMEOUT — this is the T55 v2 hang site (ff_read \
         EAGAIN'd forever against a wedged peer). Body excerpt:\n{body}"
    );
    assert!(
        body.contains("STALL_TIMEOUT"),
        "phase_read_burst must reference STALL_TIMEOUT"
    );
}

#[test]
fn fstack_rs_wait_connect_checks_handshake_deadline() {
    // The connect-side watchdog uses a separate `connect_started_at`
    // anchor + CONNECT_TIMEOUT because Connect/WaitConnect don't make
    // ff_read/ff_write progress (so they can't use `last_progress`).
    let wait_connect_idx = FSTACK_RS
        .find("fn phase_wait_connect")
        .expect("phase_wait_connect not found");
    let start_first_idx = FSTACK_RS
        .find("fn start_first_burst")
        .expect("start_first_burst not found");
    let body = &FSTACK_RS[wait_connect_idx..start_first_idx];
    assert!(
        body.contains("state.connect_started_at.elapsed()"),
        "phase_wait_connect must check connect_started_at against \
         CONNECT_TIMEOUT — otherwise a backed-up peer accept queue \
         hangs the bucket forever. Body excerpt:\n{body}"
    );
    assert!(
        body.contains("CONNECT_TIMEOUT"),
        "phase_wait_connect must reference CONNECT_TIMEOUT"
    );
}

#[test]
fn fstack_rs_progress_updates_on_ff_write_success() {
    // `state.last_progress` is the engine of the watchdog — it must
    // be updated on every successful ff_write (n > 0) inside
    // phase_send_cmd. Forgetting to bump it would cause the watchdog
    // to fire on healthy-but-slow buckets.
    let send_cmd_idx = FSTACK_RS
        .find("fn phase_send_cmd")
        .expect("phase_send_cmd not found");
    let read_burst_idx = FSTACK_RS
        .find("fn phase_read_burst")
        .expect("phase_read_burst not found");
    let body = &FSTACK_RS[send_cmd_idx..read_burst_idx];
    assert!(
        body.contains("state.last_progress = Instant::now()"),
        "phase_send_cmd must update `state.last_progress` on each \
         successful ff_write (n > 0); otherwise the watchdog fires \
         on slow but healthy buckets. Body excerpt:\n{body}"
    );
}

#[test]
fn fstack_rs_progress_updates_on_ff_read_success() {
    // Same on the read side.
    let read_burst_idx = FSTACK_RS
        .find("fn phase_read_burst")
        .expect("phase_read_burst not found");
    let advance_idx = FSTACK_RS
        .find("fn advance_post_burst")
        .expect("advance_post_burst not found");
    let body = &FSTACK_RS[read_burst_idx..advance_idx];
    assert!(
        body.contains("state.last_progress = Instant::now()"),
        "phase_read_burst must update `state.last_progress` on each \
         successful ff_read (n > 0)."
    );
}
