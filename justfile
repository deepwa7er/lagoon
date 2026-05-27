# Buoy build orchestration.
#
# Local development (native macOS / Linux): `cargo` commands work directly.
# Cross-compilation: requires the musl-cross toolchain on macOS hosts —
#   brew tap filosottile/musl-cross
#   brew install filosottile/musl-cross/musl-cross
# (See .cargo/config.toml for details.)

# Apple targets compiled into the iOS/macOS app bundles.
apple_targets := "aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-darwin x86_64-apple-darwin"

# Linux target used as a compile-time guard rail on macOS hosts.
# The actual Linux GTK app is built natively on Linux (POC-6 onward).
linux_targets := "x86_64-unknown-linux-musl"

# Target used to source UniFFI metadata for bindgen. Any Apple target works
# since the metadata is identical; we pick the host arch for speed.
bindgen_target := "aarch64-apple-darwin"

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

# Build the apple-ffi static library for every Apple target.
# This transitively builds buoy-core for each target as well.
build-apple:
    #!/usr/bin/env bash
    set -euo pipefail
    for t in {{apple_targets}}; do
        echo "==> $t"
        cargo build --lib -p buoy-apple-ffi --release --target "$t"
    done

# Cross-compile the core crate to Linux as a guard rail on macOS hosts.
build-linux:
    #!/usr/bin/env bash
    set -euo pipefail
    for t in {{linux_targets}}; do
        echo "==> $t"
        cargo build --lib -p buoy-core --release --target "$t"
    done

# Generate Swift bindings via the workspace-local uniffi-bindgen.
# Depends on having a built apple-ffi static lib for `bindgen_target`.
build-bindings:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --lib -p buoy-apple-ffi --release --target {{bindgen_target}}
    rm -rf generated/swift
    mkdir -p generated/swift
    cargo run --release -p buoy-apple-ffi --bin uniffi-bindgen -- generate \
        --library target/{{bindgen_target}}/release/libbuoy_apple_ffi.a \
        --language swift \
        --out-dir generated/swift

# Build BuoyCore.xcframework and the companion Swift bindings file in dist/.
build-xcframework: build-apple build-bindings
    #!/usr/bin/env bash
    set -euo pipefail

    rm -rf dist/BuoyCore.xcframework dist/staging dist/BuoyCore.swift
    mkdir -p dist/staging/ios-device/Headers dist/staging/ios-sim/Headers dist/staging/macos/Headers

    # iOS device slice — single arch (arm64 device).
    cp target/aarch64-apple-ios/release/libbuoy_apple_ffi.a dist/staging/ios-device/

    # iOS simulator slice — universal (arm64-sim + x86_64-sim).
    lipo -create \
        target/aarch64-apple-ios-sim/release/libbuoy_apple_ffi.a \
        target/x86_64-apple-ios/release/libbuoy_apple_ffi.a \
        -output dist/staging/ios-sim/libbuoy_apple_ffi.a

    # macOS slice — universal (arm64 + x86_64).
    lipo -create \
        target/aarch64-apple-darwin/release/libbuoy_apple_ffi.a \
        target/x86_64-apple-darwin/release/libbuoy_apple_ffi.a \
        -output dist/staging/macos/libbuoy_apple_ffi.a

    # Each slice carries an identical Headers/ directory with the C header
    # plus a module map. The modulemap is renamed to the conventional
    # `module.modulemap` so Xcode picks it up automatically.
    for slice in ios-device ios-sim macos; do
        cp generated/swift/buoy_apple_ffiFFI.h        dist/staging/$slice/Headers/
        cp generated/swift/buoy_apple_ffiFFI.modulemap dist/staging/$slice/Headers/module.modulemap
    done

    xcodebuild -create-xcframework \
        -library dist/staging/ios-device/libbuoy_apple_ffi.a -headers dist/staging/ios-device/Headers \
        -library dist/staging/ios-sim/libbuoy_apple_ffi.a    -headers dist/staging/ios-sim/Headers \
        -library dist/staging/macos/libbuoy_apple_ffi.a      -headers dist/staging/macos/Headers \
        -output dist/BuoyCore.xcframework

    cp generated/swift/buoy_apple_ffi.swift dist/BuoyCore.swift
    rm -rf dist/staging

    echo ""
    echo "==> dist/BuoyCore.xcframework"
    echo "==> dist/BuoyCore.swift (drag into Xcode alongside the xcframework)"

# Build the core crate for every target on every platform.
build-all: build-apple build-linux

# Compile + run a Swift smoke test against the macOS slice of the xcframework.
# Proves the full FFI chain works on macOS before we wire it into a real app.
smoke-xcframework:
    #!/usr/bin/env bash
    set -euo pipefail
    headers="dist/BuoyCore.xcframework/macos-arm64_x86_64/Headers"
    lib_dir="dist/BuoyCore.xcframework/macos-arm64_x86_64"
    if [ ! -d "$headers" ] || [ ! -f "$lib_dir/libbuoy_apple_ffi.a" ]; then
        echo "dist/BuoyCore.xcframework is missing; run \`just build-xcframework\` first." >&2
        exit 1
    fi
    bin="$(mktemp -d)/buoy-smoke"
    swiftc -o "$bin" \
        -Xcc -fmodule-map-file="$headers/module.modulemap" \
        -Xcc -I"$headers" \
        -L "$lib_dir" \
        -lbuoy_apple_ffi \
        dist/BuoyCore.swift scripts/smoke-test.swift
    "$bin"

# Remove build artifacts and generated files.
clean:
    cargo clean
    rm -rf dist generated
