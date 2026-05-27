// Smoke test for the macOS slice of BuoyCore.xcframework.
//
// Exercises the full FFI chain: Swift code calls UniFFI-generated bindings,
// which call into the Rust core, which opens SQLite, persists a thought, and
// reads it back. Crashes (non-zero exit) if anything is wrong.
//
// Run via `just smoke-xcframework` after `just build-xcframework`.

import Foundation

@main
struct SmokeTest {
    static func main() throws {
        let path = NSTemporaryDirectory() + "buoy-smoke-\(UUID().uuidString).sqlite"
        defer { try? FileManager.default.removeItem(atPath: path) }

        let store = try ThoughtStore.open(path: path)
        let captured = try store.create(text: "smoke test thought")
        let listed = try store.list()

        precondition(listed.count == 1, "expected 1 thought, got \(listed.count)")
        precondition(listed[0].id == captured.id, "id mismatch")
        precondition(listed[0].text == "smoke test thought", "text mismatch")
        precondition(listed[0].createdAt == captured.createdAt, "createdAt mismatch")

        print("smoke test OK — id=\(captured.id) created_at=\(captured.createdAt)")
    }
}
