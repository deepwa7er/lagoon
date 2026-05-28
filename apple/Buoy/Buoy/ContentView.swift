import BuoyCore
import SwiftUI

@MainActor
@Observable
final class ThoughtListModel {
    var thoughts: [Thought] = []
    var draft: String = ""
    var errorMessage: String?

    private var store: ThoughtStore?

    func open() async {
        do {
            let path = try Self.storeURL().path
            store = try ThoughtStore.open(path: path)
            await refresh()
        } catch {
            errorMessage = "Failed to open store: \(error.localizedDescription)"
        }
    }

    func refresh() async {
        guard let store else { return }
        do {
            thoughts = try store.list()
        } catch {
            errorMessage = "Failed to load thoughts: \(error.localizedDescription)"
        }
    }

    func save() async {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, let store else { return }
        do {
            _ = try store.create(text: text)
            draft = ""
            await refresh()
        } catch {
            errorMessage = "Failed to save thought: \(error.localizedDescription)"
        }
    }

    private static func storeURL() throws -> URL {
        let fileManager = FileManager.default
        let support = try fileManager.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let dir = support.appendingPathComponent("Buoy", isDirectory: true)
        try fileManager.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("buoy.sqlite")
    }
}

struct ContentView: View {
    @State private var model = ThoughtListModel()
    @FocusState private var composerFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            List {
                ForEach(Array(model.thoughts.reversed()), id: \.id) { thought in
                    ThoughtRow(thought: thought)
                }
            }
            .listStyle(.plain)
            .defaultScrollAnchor(.bottom)

            Divider()

            HStack(alignment: .bottom, spacing: 8) {
                TextField("What's on your mind?", text: $model.draft, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(1...8)
                    .focused($composerFocused)
                    .onKeyPress(keys: [.return]) { keyPress in
                        // Shift+Return inserts a literal newline; bare Return saves.
                        if keyPress.modifiers.contains(.shift) {
                            return .ignored
                        }
                        Task { await model.save() }
                        return .handled
                    }

                Button("Save") {
                    Task { await model.save() }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(model.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
            .padding(12)
        }
        .task {
            await model.open()
            composerFocused = true
        }
        .alert(
            "Error",
            isPresented: Binding(
                get: { model.errorMessage != nil },
                set: { if !$0 { model.errorMessage = nil } }
            ),
            actions: {
                Button("OK") { model.errorMessage = nil }
            },
            message: {
                Text(model.errorMessage ?? "")
            }
        )
    }
}

private struct ThoughtRow: View {
    let thought: Thought

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(thought.text)
            Text(
                Date(timeIntervalSince1970: Double(thought.createdAt) / 1000),
                style: .relative
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 4)
    }
}

#Preview {
    ContentView()
}
