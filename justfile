# Buoy build orchestration.
# Real build targets land in POC-2 (cross-compilation) and POC-3 (UniFFI).
# For now: just the core crate.

# Show available recipes
default:
    @just --list

# Run all core tests
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
