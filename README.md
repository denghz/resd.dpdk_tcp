# resd.dpdk_tcp

DPDK-based userspace TCP stack in Rust with a C ABI for C++ consumers.

See `docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md` for the design.

## Build

Requires DPDK 23.11 installed (`pkg-config --exists libdpdk` must succeed).

Use `cargo build --locked` to avoid lockfile drift; some transitive deps (notably
`home` / `windows-sys`) publish newer releases that require `edition2024` and
break on our Cargo 1.75 toolchain.

```sh
cargo build --release --locked
cmake -S examples/cpp-consumer -B examples/cpp-consumer/build
cmake --build examples/cpp-consumer/build
```

## Test

```sh
cargo test
```
