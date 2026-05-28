import BuoyCore
import SwiftUI

#if os(macOS)
import AppKit
#endif

@MainActor
@Observable
final class ThoughtListModel {
    var thoughts: [Thought] = []
    var draft: String = ""
    var errorMessage: String?
    /// When non-nil, `save` will update the existing thought instead of
    /// creating a new one. Cleared on save or cancel.
    var editingId: String?

    private var store: ThoughtStore?

    var isEditing: Bool { editingId != nil }

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

    func startEditing(_ thought: Thought) {
        draft = thought.text
        editingId = thought.id
    }

    func cancelEditing() {
        draft = ""
        editingId = nil
    }

    func save() async {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, let store else { return }
        do {
            if let id = editingId {
                _ = try store.update(id: id, text: text)
            } else {
                _ = try store.create(text: text)
            }
            draft = ""
            editingId = nil
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
    #if os(macOS)
    @State private var keyMonitor: Any?
    #endif

    var body: some View {
        VStack(spacing: 0) {
            List {
                ForEach(Array(model.thoughts.reversed()), id: \.id) { thought in
                    ThoughtRow(thought: thought)
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.startEditing(thought)
                            composerFocused = true
                        }
                }
            }
            .listStyle(.plain)
            .defaultScrollAnchor(.bottom)

            Divider()

            if model.isEditing {
                EditingBanner(onCancel: { model.cancelEditing() })
            }

            HStack(alignment: .bottom, spacing: 8) {
                ZStack(alignment: .topLeading) {
                    TextEditor(text: $model.draft)
                        .scrollContentBackground(.hidden)
                        .focused($composerFocused)
                        .frame(minHeight: 15, maxHeight: 80)
                        .modifier(BareReturnSubmits {
                            Task { await model.save() }
                        })

                    if model.draft.isEmpty {
                        Text(model.isEditing ? "" : "What's on your mind?")
                            .foregroundStyle(.tertiary)
                            .padding(.leading, 5)
                            .padding(.top, 8)
                            .allowsHitTesting(false)
                    }
                }

                Button(model.isEditing ? "Update" : "Save") {
                    Task { await model.save() }
                }
                .disabled(model.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
            .padding(12)
        }
        .task {
            await model.open()
            composerFocused = true
        }
        #if os(macOS)
        .onAppear { installKeyMonitor() }
        .onDisappear { removeKeyMonitor() }
        #endif
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

    #if os(macOS)
    private func installKeyMonitor() {
        // SwiftUI's `.onKeyPress` does not let an `.ignored` Return fall
        // through to the multi-line text editor's newline insertion, so on
        // macOS we intercept keystrokes at the AppKit level instead. Bare
        // Return saves; Shift+Return passes through so NSTextView inserts
        // a literal newline. Escape cancels an in-progress edit.
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            // 53 = Escape
            if event.keyCode == 53 && model.isEditing {
                model.cancelEditing()
                return nil
            }
            // 36 = Return, 76 = numeric keypad Enter
            guard event.keyCode == 36 || event.keyCode == 76 else { return event }
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            if flags.contains(.shift) {
                return event
            }
            Task { await model.save() }
            return nil
        }
    }

    private func removeKeyMonitor() {
        if let monitor = keyMonitor {
            NSEvent.removeMonitor(monitor)
            keyMonitor = nil
        }
    }
    #endif
}

/// Bare Return submits; Shift+Return inserts a newline. On macOS this is
/// handled at the AppKit level (see `installKeyMonitor`), so the modifier
/// is a no-op there to avoid SwiftUI's surprising `.onKeyPress` behavior.
/// iOS has no Shift modifier on the on-screen keyboard, so the
/// `.onKeyPress` form is sufficient.
private struct BareReturnSubmits: ViewModifier {
    let action: () -> Void

    func body(content: Content) -> some View {
        #if os(macOS)
        content
        #else
        content.onKeyPress(keys: [.return]) { keyPress in
            if keyPress.modifiers.contains(.shift) {
                return .ignored
            }
            action()
            return .handled
        }
        #endif
    }
}

private struct EditingBanner: View {
    let onCancel: () -> Void

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "pencil")
                .imageScale(.small)
                .foregroundStyle(.secondary)
            Text("Editing thought")
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
            Button("Cancel", action: onCancel)
                .buttonStyle(.plain)
                .font(.caption)
                .foregroundStyle(.tint)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(Color.accentColor.opacity(0.08))
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
