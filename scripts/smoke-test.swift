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

        // Pagination round-trip: walk a multi-page stream through the
        // cursor and confirm every thought comes back exactly once.
        let total = Int(defaultPageSize()) + 3
        for i in 1..<total {
            _ = try store.create(text: "page filler \(i)")
        }
        var walked: [String] = []
        var cursor: Cursor?
        repeat {
            let page = try store.listPaginated(before: cursor, limit: defaultPageSize())
            walked.append(contentsOf: page.thoughts.map(\.id))
            cursor = page.nextCursor
        } while cursor != nil
        precondition(walked.count == total, "expected \(total) thoughts, walked \(walked.count)")
        precondition(Set(walked).count == total, "pagination returned a duplicate id")
        precondition(walked.last == captured.id, "oldest thought should be the last walked")

        print("smoke test OK — id=\(captured.id) created_at=\(captured.createdAt), paginated \(walked.count) thoughts")
    }
}
