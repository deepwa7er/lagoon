import BuoyCore
import Foundation

// Two-way sync against the server-authoritative buoy store on the tailnet.
//
// The flow each tick: read the local outbox (`pendingChanges`), POST it to
// `/api/sync` along with our cursor, apply the server's returned changes
// (last-writer-wins, in the core), mark the pushed rows synced, and persist the
// new cursor. Capture works fully offline; a sync just reconciles when we can
// reach the server. See crates/server/src/api.rs and crates/core/src/sync.rs.

/// Where the buoy server lives. Plain HTTP over the tailnet, which encrypts
/// transport (the same model the web app and the rest of the suite use). The
/// app carries an ATS exception for this host so iOS/macOS permit it.
let buoyServerURL = URL(string: "http://deepwa7er.tailcfab97.ts.net:8092")!

/// How many local changes to push per sync. A personal store fits in one tick;
/// if there are more, the next tick drains the rest.
private let syncBatchLimit: UInt32 = 500

// MARK: - Wire DTOs (snake_case JSON ⇆ the server)

/// One thought row on the wire. Mirrors the server's `ThoughtChangeDto`. JSON
/// uses snake_case; the coder strategies convert to/from these camelCase names.
struct WireChange: Codable {
    let id: String
    let text: String
    let createdAt: Int64
    let updatedAt: Int64
    let settledAt: Int64?
    let deletedAt: Int64?

    init(_ c: ThoughtChange) {
        id = c.id
        text = c.text
        createdAt = c.createdAt
        updatedAt = c.updatedAt
        settledAt = c.settledAt
        deletedAt = c.deletedAt
    }

    var core: ThoughtChange {
        ThoughtChange(
            id: id, text: text, createdAt: createdAt, updatedAt: updatedAt,
            settledAt: settledAt, deletedAt: deletedAt)
    }
}

private struct SyncRequestBody: Encodable {
    let since: String?
    let changes: [WireChange]
}

private struct SyncResponseBody: Decodable {
    let changes: [WireChange]
    let cursor: String?
}

enum SyncError: Error, CustomStringConvertible {
    case http(Int)

    var description: String {
        switch self {
        case let .http(code): return "server returned HTTP \(code)"
        }
    }
}

// MARK: - Sync service

/// Stateless reconcile against the server. All store access and the network
/// call run off the main actor (the FFI store is thread-safe via its internal
/// mutex, and `applyRemote` may embed — never on the main thread).
enum SyncService {
    /// Run one push+pull reconcile. Returns the new cursor to persist, and how
    /// many remote changes were applied (so the caller can refresh its view).
    static func reconcile(
        store: ThoughtStore, baseURL: URL, since cursor: String?
    ) async throws -> (cursor: String?, applied: Int) {
        let pending = try store.pendingChanges(limit: syncBatchLimit)
        let body = SyncRequestBody(since: cursor, changes: pending.map(WireChange.init))

        var request = URLRequest(url: baseURL.appendingPathComponent("api/sync"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        request.httpBody = try encoder.encode(body)

        let (data, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode)
        else {
            throw SyncError.http((response as? HTTPURLResponse)?.statusCode ?? -1)
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let result = try decoder.decode(SyncResponseBody.self, from: data)

        // Apply the server's changes (last-writer-wins in the core), then clear
        // the dirty flag for what we pushed (skipping rows edited since).
        var applied = 0
        for change in result.changes where try store.applyRemote(change: change.core) {
            applied += 1
        }
        try store.markSynced(
            pushed: pending.map { SyncAck(id: $0.id, updatedAt: $0.updatedAt) })

        return (result.cursor ?? cursor, applied)
    }
}
