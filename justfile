# Buoy build orchestration.
#
# Local development (native macOS / Linux): `cargo` commands work directly.
# Cross-compilation: requires the musl-cross toolchain on macOS hosts —
#   brew tap filosottile/musl-cross
#   brew install filosottile/musl-cross/musl-cross
# (See .cargo/config.toml for details.)
#
# Xcframework packaging arrives in POC-3 once UniFFI bindings exist.

# Apple targets compiled into the iOS/macOS app bundles.
apple_targets := "aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-darwin x86_64-apple-darwin"

# Linux target used as a compile-time guard rail on macOS hosts.
# The actual Linux GTK app is built natively on Linux (POC-6 onward).
linux_targets := "x86_64-unknown-linux-musl"

# Show available recipes
default:
    @just --list

# Run all core tests on the host
test:
    cargo test --workspace

# Lint with clippy (warnings as errors)
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Apply formatting
fmt:
    cargo fmt --all

# Run lint + fmt-check + test (CI-style local check)
check: lint fmt-check test

# Build the core crate (release) for one specific target.
build-core target:
    cargo build -p buoy-core --release --target {{target}}

# Build the core crate for every Apple target.
build-apple:
    #!/usr/bin/env bash
    set -euo pipefail
    for t in {{apple_targets}}; do
        echo "==> $t"
        cargo build -p buoy-core --release --target "$t"
    done

# Build the core crate for every Linux target.
build-linux:
    #!/usr/bin/env bash
    set -euo pipefail
    for t in {{linux_targets}}; do
        echo "==> $t"
        cargo build -p buoy-core --release --target "$t"
    done

# Build the core crate for every target on every platform.
build-all: build-apple build-linux

# Remove build artifacts
clean:
    cargo clean
