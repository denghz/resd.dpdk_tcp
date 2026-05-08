//! bench-vs-mtcp build script — links libfstack.a when the `fstack`
//! cargo feature is enabled. No-op otherwise so default workspace
//! builds don't require F-Stack on the dev host.
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
    // Re-run if either the cargo feature flag or the env override
    // changes.
    println!("cargo:rerun-if-env-changed=FF_PATH");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_FSTACK").is_none() {
        // Default build path — no F-Stack linkage.
        return;
    }

    // F-Stack feature on — link libfstack.a only.
    // DPDK symbols are provided by dpdk-net-sys (via dpdk-net-core dep);
    // do NOT add a second DPDK whole-archive block here.
    let ff_path = std::env::var("FF_PATH").unwrap_or_else(|_| "/opt/f-stack".to_string());
    println!("cargo:rustc-link-search=native={ff_path}/lib");
    // libfstack.a — whole-archive so static-link discards aren't
    // applied to F-Stack's per-cpu init path.
    // -z nostart-stop-gc: lld ≥ 14 may GC __start_set_sysctl_set /
    // __stop_set_sysctl_set encapsulation symbols from libfstack.a;
    // this flag keeps them.
    println!("cargo:rustc-link-arg=-Wl,-z,nostart-stop-gc");
    println!("cargo:rustc-link-arg=-Wl,--whole-archive");
    println!("cargo:rustc-link-arg=-lfstack");
    println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");

    // dpdk-net-sys links DPDK via pkg-config before this build.rs runs.
    // With --as-needed those libs are excluded from DT_NEEDED when
    // processed (no Rust caller references rte_ring_create / rte_timer_*
    // etc. directly). libfstack.a references them; re-link them here with
    // --no-as-needed so they land in DT_NEEDED and satisfy libfstack.a's
    // undefined references at runtime.
    println!("cargo:rustc-link-arg=-Wl,--push-state,--no-as-needed");
    for lib in [
        "rte_ring", "rte_mempool", "rte_mbuf", "rte_eal",
        "rte_ethdev", "rte_net", "rte_pci", "rte_timer",
        "rte_kvargs", "rte_telemetry", "rte_log",
        // DPDK bonding PMD — libfstack.a references rte_eth_bond_*
        // even on configs that don't use bonding (the symbols are
        // resolved at load time, not call time).
        "rte_net_bond",
        // OpenSSL — libfstack.a calls RAND_bytes for arc4random shim.
        "crypto",
    ] {
        println!("cargo:rustc-link-arg=-l{lib}");
    }
    println!("cargo:rustc-link-arg=-Wl,--pop-state");

    // F-Stack uses these system libs at link time per the upstream
    // example/Makefile recipe.
    for lib in ["rt", "m", "dl", "crypto", "pthread", "numa"] {
        println!("cargo:rustc-link-lib={lib}");
    }
    // dpdk-net-sys's DPDK whole-archive block pulls in librte_telemetry.a
    // which references atexit(). Rust places libc before build.rs
    // cargo:rustc-link-arg output in the link order, so the telemetry
    // reference isn't satisfied. Re-emit libc here to ensure it appears
    // after the DPDK block.
    println!("cargo:rustc-link-arg=-lc");
}
