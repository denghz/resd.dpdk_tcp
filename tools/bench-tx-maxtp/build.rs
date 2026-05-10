//! bench-tx-maxtp build script — delegates the libfstack.a + DPDK
//! link-arg sequence to `bench-build-helpers` (T51 deferred-work
//! item 2 closure). DPDK is intentionally NOT re-whole-archived here
//! — `dpdk-net-sys` already links it via pkg-config; the helper's
//! `--no-as-needed` block just satisfies libfstack.a's undefined
//! references. See `tools/bench-build-helpers/src/lib.rs` for the
//! full link-arg recipe + rationale.

fn main() {
    println!("cargo:rerun-if-env-changed=FF_PATH");
    println!("cargo:rerun-if-changed=build.rs");
    bench_build_helpers::emit_fstack_link_args_if_enabled();
}
