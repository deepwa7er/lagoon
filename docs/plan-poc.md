# Buoy — Proof of Concept Plan

## Goal

Prove the **Rust core + native UI** architecture works end-to-end on **iOS, macOS, and Linux**. Nothing else.

The POC does almost nothing useful. Its only purpose is to validate the architecture and toolchain before we build features on top of it. Code quality must still be production-grade — the POC becomes the foundation.

## Scope

The POC implements exactly two operations:

1. **Create a thought** — type text, press save, persist to SQLite.
2. **List thoughts** — show all saved thoughts, newest first.

That's it. No search, no embeddings, no sync, no edit history, no styling beyond defaults.

## Architecture

```
            ┌─────────────────────────────────────────────┐
            │              Rust core (crate)              │
            │  - Thought struct                           │
            │  - ThoughtStore (SQLite via rusqlite)       │
            │  - Unit tests                               │
            └──────┬──────────────┬──────────────┬────────┘
                   │              │              │
            UniFFI bindings    UniFFI         direct call
                   │              │              │
            ┌──────▼──┐    ┌──────▼──┐    ┌─────▼─────┐
            │  iOS    │    │  macOS  │    │   Linux   │
            │ SwiftUI │    │ SwiftUI │    │ gtk4-rs   │
            └─────────┘    └─────────┘    └───────────┘
```

The Rust core takes a SQLite **file path as a parameter** — it never decides storage location itself. Each platform passes its own appropriate path (iOS sandbox dir, macOS Application Support, Linux XDG_DATA_HOME).

## Tech choices (locked in for POC)

| Concern | Choice | Why |
|---|---|---|
| Language for core | Rust | Per architecture discussion |
| Storage | SQLite via `rusqlite` with bundled feature | Cross-platform, no system deps, FTS-ready for Phase 2 |
| FFI bridge | UniFFI (Mozilla) | Best Swift output; well-supported |
| iOS UI | SwiftUI | Modern, native, shares heavily with macOS |
| macOS UI | SwiftUI (same Xcode project, separate target) | Maximum code sharing with iOS |
| Linux UI | gtk4-rs | Native Rust, no FFI needed on this platform |
| Build orchestration | `justfile` (or shell) | Keep it simple at POC stage |

## Subphases

### POC-0: Project scaffolding

- Cargo workspace at repo root
  - `crates/core/` — the Rust library
  - `crates/linux/` — the GTK binary
- `apple/Buoy.xcodeproj` — Xcode project with iOS + macOS targets
- `justfile` with `just build-all`, `just build-ios`, `just build-mac`, `just build-linux`
- `.gitignore`, basic CI placeholder (no CI yet — defer)

### POC-1: Rust core

Build in isolation, fully tested before any platform integration.

- `Thought { id: Uuid, text: String, created_at: i64 }`
- `ThoughtStore` with `open(path: &Path) -> Result<Self>`, `create(text: &str) -> Result<Thought>`, `list() -> Result<Vec<Thought>>`
- SQLite schema with a single `thoughts` table
- Migration handling (manual or `refinery` — manual is fine for one table)
- Unit tests using in-memory SQLite

**Done when:** `cargo test -p buoy-core` passes; calling `create` then `list` returns the inserted row.

### POC-2: Cross-compilation

Stand up the toolchain for every target before touching any UI.

- Install Rust targets: `aarch64-apple-ios`, `aarch64-apple-ios-sim`, `x86_64-apple-ios`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` (or appropriate native target if on Linux dev box)
- Produce a **xcframework** combining iOS device + iOS simulator + macOS (universal binary for macOS, separate slices for iOS)
- Produce a Linux build (either static lib for FFI or just compile the GTK binary that depends on core directly)
- `just build-core` produces all artifacts in `dist/`

**Done when:** all targets build clean. No platform shell exists yet — this is pure toolchain work.

### POC-3: UniFFI bridge

- `crates/core/src/buoy.udl` defines the Swift-facing interface (or use the procedural macro variant — pick one approach, document choice)
- Build step generates `Buoy.swift` (the bindings)
- xcframework includes the generated bindings alongside the static lib

**Done when:** the xcframework imports cleanly into an empty Xcode project and `ThoughtStore` is callable from Swift.

### POC-4, POC-5, POC-6: Platform shells (parallel)

These three subphases run **in parallel** — same week, ideally same day, switching between platforms.

**POC-4: iOS shell**
- Xcode project, iOS target, SwiftUI app
- Add xcframework
- Single view: `TextField`, `Button("Save")`, `List` of thoughts
- Tap save → calls `ThoughtStore.create` → list refreshes
- Test on simulator and on a real device

**POC-5: macOS shell**
- Same Xcode project, macOS target
- Share SwiftUI views via a multi-platform Swift module (`apple/Shared/`)
- Same single view, runs as a macOS window
- Test on actual Mac

**POC-6: Linux shell**
- `crates/linux/` binary using `gtk4-rs`
- Depends on `buoy-core` directly (Rust → Rust, no FFI)
- Single `gtk::ApplicationWindow` with `Entry`, `Button`, `ListBox`
- Click save → calls core → list refreshes
- Test on Linux (or in VM/container during macOS development)

### POC-7: Verification

- All three apps run, create thoughts, restart, thoughts persist
- All three back the same Rust core code with no platform-specific business logic
- Document any toolchain pain points encountered (for future-us)
- Audit for hacks: per the project quality bar, none should remain in the codebase. If any exist, fix or document explicitly as known issues.

## Risks (flag and watch)

1. **Rust → iOS toolchain quirks.** The hardest part of the POC. Known pain points: code signing the xcframework, bitcode warnings (now deprecated but Xcode may still complain), Mach-O symbol stripping. Mitigation: follow UniFFI's published iOS guide; budget extra time here.
2. **SQLite path handling.** iOS sandboxing makes this tricky. The platform shell must obtain the correct directory and pass it in. Don't let the core "figure it out" from environment.
3. **macOS sandboxing.** If shipping via the App Store later, sandboxed file access matters. For POC, run unsandboxed and revisit before release.
4. **GTK4 on macOS for testing.** If developing on macOS, install GTK4 via Homebrew to test the Linux app locally, or use a Linux VM. Don't skip Linux testing during POC.

## Acceptance criteria

- [ ] `cargo test` passes in the core crate
- [ ] iOS app installs and runs on simulator; thoughts persist across restarts
- [ ] iOS app runs on a real device
- [ ] macOS app runs natively; thoughts persist
- [ ] Linux app runs natively; thoughts persist
- [ ] All three platforms share the same Rust core code for business logic
- [ ] No hacks or workarounds in the codebase
- [ ] `just build-all` succeeds on a clean checkout

## Estimated effort

- **Smooth path:** 1–2 weeks. Rust toolchain cooperates, UniFFI works first try.
- **Realistic path:** 2–3 weeks. Toolchain friction on iOS, some GTK learning curve.

Do not move to the buildout plan until every acceptance criterion is met.

## What the POC explicitly does NOT include

- Embeddings or semantic search
- Full-text search
- Edit history or update operations
- Sync
- Tags
- Styled UI (default widgets only)
- Error handling beyond `Result` propagation
- Localization
- Onboarding

These belong to the [buildout plan](./plan-buildout.md).
