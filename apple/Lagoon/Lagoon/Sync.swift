import LagoonCore
import Foundation

// Two-way sync against the server-authoritative lagoon store on the tailnet.
//
// The flow each tick: read the local outbox (`pendingChanges`), POST it to
// `/api/sync` along with our cursor, apply the server's returned changes
// (last-writer-wins, in the core), mark the pushed rows synced, and persist the
// new cursor. Capture works fully offline; a sync just reconciles when we can
// reach the server. See crates/server/src/api.rs and crates/core/src/sync.rs.

/// Where the lagoon server lives — the breakwater HTTPS front door on the tailnet
/// (a valid wildcard cert, so no ATS exception is needed). lagoon binds loopback
/// on the VPS; breakwater serves it at this name, the same `*.internal` scheme
/// the web app and the rest of the suite use.
let lagoonServerURL = URL(string: "https://lagoon.intern.deepwa7er.net")!

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
    let actionedAt: Int64?

    init(_ c: ThoughtChange) {
        id = c.id
        text = c.text
        createdAt = c.createdAt
        updatedAt = c.updatedAt
        settledAt = c.settledAt
        deletedAt = c.deletedAt
        actionedAt = c.actionedAt
    }

    var core: ThoughtChange {
        ThoughtChange(
            id: id, text: text, createdAt: createdAt, updatedAt: updatedAt,
            settledAt: settledAt, deletedAt: deletedAt, actionedAt: actionedAt)
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

    /// Key under which the opaque server cursor is persisted between launches.
    static let cursorKey = "lagoon.sync.cursor"

    /// Reconcile using the persisted cursor, advancing it only on success. The
    /// single entry point for both the UI model and background sync, so cursor
    /// handling lives in exactly one place. Returns how many remote changes were
    /// applied (so a caller can decide whether to refresh its view).
    static func reconcilePersisting(store: ThoughtStore, baseURL: URL) async throws -> Int {
        let cursor = UserDefaults.standard.string(forKey: cursorKey)
        let result = try await reconcile(store: store, baseURL: baseURL, since: cursor)
        UserDefaults.standard.set(result.cursor, forKey: cursorKey)
        return result.applied
    }
}

// MARK: - Sync status (UI)

/// Live state of the sync subsystem, surfaced in the UI. The time of the last
/// *successful* reconcile is tracked separately (it persists across later
/// failures), so this enum models only the current condition.
enum SyncStatus: Equatable {
    case idle
    case syncing
    /// The last attempt failed because the server was unreachable. An expected,
    /// transient condition — shown plainly, not as an error.
    case offline
    /// The last attempt failed for some other reason; carries a message.
    case failed(String)

    var isOffline: Bool { self == .offline }
}

// MARK: - Store location

/// On-device SQLite store location, shared by the UI and background sync so both
/// open the exact same file. Creates the containing directory if missing.
enum LagoonStore {
    static func url() throws -> URL {
        let fileManager = FileManager.default
        let support = try fileManager.url(
            for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true)
        let dir = support.appendingPathComponent("Lagoon", isDirectory: true)
        try fileManager.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("lagoon.sqlite")
    }
}

#if os(iOS)
import BackgroundTasks

// MARK: - Background sync (iOS)

/// Periodic background reconcile via BGTaskScheduler. The
/// `.backgroundTask(.appRefresh:)` modifier in `LagoonApp` registers the handler
/// that calls `run()`; the app submits the first request when it backgrounds,
/// and each run chains the next — so the cadence is self-sustaining without an
/// AppDelegate. The OS throttles actual timing by usage and power.
enum BackgroundSync {
    /// Must match the value in BGTaskSchedulerPermittedIdentifiers (Info.plist).
    static let taskIdentifier = "com.deepwa7er.Lagoon.refresh"

    /// Floor for when the system may next run the refresh — not a guarantee.
    private static let earliestInterval: TimeInterval = 15 * 60

    /// Ask the system to run a refresh later. Best-effort: submission can fail
    /// (too many pending requests, capability unavailable in Low Power Mode);
    /// the next foreground or pull-to-refresh sync covers any gap, so a failure
    /// here is logged, not surfaced.
    static func schedule() {
        let request = BGAppRefreshTaskRequest(identifier: taskIdentifier)
        request.earliestBeginDate = Date(timeIntervalSinceNow: earliestInterval)
        do {
            try BGTaskScheduler.shared.submit(request)
        } catch {
            print("lagoon: could not schedule background sync: \(error)")
        }
    }

    /// Run one reconcile from a background launch. Opens its own store handle
    /// (the UI may not be alive) and chains the next request *first*, so an error
    /// mid-reconcile never breaks the recurring chain. Errors are swallowed — a
    /// failed background sync just means the next sync reconciles instead.
    static func run() async {
        schedule()
        do {
            let store = try ThoughtStore.open(path: LagoonStore.url().path)
            _ = try await SyncService.reconcilePersisting(store: store, baseURL: lagoonServerURL)
        } catch {
            print("lagoon: background sync failed: \(error)")
        }
    }
}
#endif
