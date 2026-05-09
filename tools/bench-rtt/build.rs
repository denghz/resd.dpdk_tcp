//! bench-rtt build script — links libfstack.a when the `fstack`
//! cargo feature is enabled. No-op otherwise so default workspace
//! builds don't require F-Stack on the dev host.
//!
//! Mirrors `tools/bench-vs-mtcp/build.rs` verbatim. The duplication
//! exists for the same reason `src/fstack_ffi.rs` is duplicated:
//! bench-vs-mtcp depends on bench-rtt for the workload helpers, so
//! bench-rtt cannot depend back on bench-vs-mtcp without a cycle.
//! Phase 5 of the bench-suite overhaul lifts both this build script
//! and the FFI bindings into a shared crate.
//!
//! DPDK is intentionally NOT re-linked here. dpdk-net-sys (a transitive
//! dep via dpdk-net-core) already links DPDK via pkg-config. Adding a
//! second DPDK whole-archive block duplicates the tailq constructor
//! objects and triggers a fatal `RTE_MBUF_DYNFIELD tailq is already
//! registered` PANIC at binary startup.
//!
//! F-Stack install layout (image-builder component
//! 04b-install-f-stack.yaml installs at):
//!   /opt/f-stack/lib/libfstack.a
//!   /opt/f-stack/include/  (ff_api.h, ff_config.h, ...)
//!
//! Operators with F-Stack installed elsewhere can override via the
//! `FF_PATH` env var (matches F-Stack's upstream Makefile convention).

fn main() {
    println!("cargo:rerun-if-env-changed=FF_PATH");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_FSTACK").is_none() {
        // Default build path — no F-Stack linkage.
        return;
    }

    // F-Stack feature on — link libfstack.a only.
    let ff_path = std::env::var("FF_PATH").unwrap_or_else(|_| "/opt/f-stack".to_string());
    println!("cargo:rustc-link-search=native={ff_path}/lib");
    println!("cargo:rustc-link-arg=-Wl,-z,nostart-stop-gc");
    println!("cargo:rustc-link-arg=-Wl,--whole-archive");
    println!("cargo:rustc-link-arg=-lfstack");
    println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");

    // dpdk-net-sys links DPDK via pkg-config before this build.rs runs.
    // With --as-needed those libs are excluded from DT_NEEDED when
    // processed; re-link them with --no-as-needed so they satisfy
    // libfstack.a's undefined references at runtime.
    println!("cargo:rustc-link-arg=-Wl,--push-state,--no-as-needed");
    for lib in [
        "rte_ring", "rte_mempool", "rte_mbuf", "rte_eal",
        "rte_ethdev", "rte_net", "rte_pci", "rte_timer",
        "rte_kvargs", "rte_telemetry", "rte_log",
        "rte_net_bond",
        "crypto",
    ] {
        println!("cargo:rustc-link-arg=-l{lib}");
    }
    println!("cargo:rustc-link-arg=-Wl,--pop-state");

    for lib in ["rt", "m", "dl", "crypto", "pthread", "numa"] {
        println!("cargo:rustc-link-lib={lib}");
    }
    // libc tail-link, see bench-vs-mtcp/build.rs for rationale.
    println!("cargo:rustc-link-arg=-lc");
}
