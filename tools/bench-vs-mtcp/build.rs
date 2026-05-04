//! bench-vs-mtcp build script — links libfstack.a + DPDK 23.11
//! when the `fstack` cargo feature is enabled. No-op otherwise so
//! default workspace builds don't require F-Stack on the dev host.
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

    // F-Stack feature on — link libfstack.a + DPDK 23.11 statics.
    let ff_path = std::env::var("FF_PATH").unwrap_or_else(|_| "/opt/f-stack".to_string());
    println!("cargo:rustc-link-search=native={ff_path}/lib");
    // libfstack.a — whole-archive so static-link discards aren't
    // applied to F-Stack's per-cpu init path.
    println!("cargo:rustc-link-arg=-Wl,--whole-archive");
    println!("cargo:rustc-link-arg=-lfstack");
    println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");

    // DPDK 23.11 — pkg-config produces the right -L/-l flags.
    let dpdk_libs_out = std::process::Command::new("pkg-config")
        .args(["--static", "--libs", "libdpdk"])
        .output()
        .expect("pkg-config --static --libs libdpdk failed (DPDK 23.11 not installed?)");
    if !dpdk_libs_out.status.success() {
        panic!(
            "pkg-config --static --libs libdpdk exited {}: {}",
            dpdk_libs_out.status,
            String::from_utf8_lossy(&dpdk_libs_out.stderr)
        );
    }
    let dpdk_libs = String::from_utf8_lossy(&dpdk_libs_out.stdout);
    for tok in dpdk_libs.split_whitespace() {
        // Pass through everything pkg-config emits — `-L`, `-l`,
        // `-Wl,...` — the `cargo:rustc-link-arg` channel honours them.
        println!("cargo:rustc-link-arg={tok}");
    }

    // F-Stack uses these system libs at link time per the upstream
    // example/Makefile recipe.
    for lib in ["rt", "m", "dl", "crypto", "pthread", "numa"] {
        println!("cargo:rustc-link-lib={lib}");
    }
}
