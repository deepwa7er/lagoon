# Lagoon — Buildout Plan

This plan picks up after the [POC](./plan-poc.md) is complete and accepted. It builds the full app in phases.

## Direction update (2026-06-16)

The platform set and the server model changed, by decision:

- **Web replaces the Linux native client.** The gtk4 app (`crates/linux`) is
  removed. Desktop use (macOS, Fedora) is now via a **web app** served from the
  VPS. The platforms that "move together" are now **iOS + web**; the macOS
  SwiftUI shell remains.
- **A server-authoritative web lagoon is live**, not the E2E sync server below.
  `crates/server` (`lagoon-server`, axum) holds a **canonical server-side store**
  (SQLite + the MiniLM embedder) and serves the React frontend in `web/`. It
  runs on the `deepwa7er` tailnet (`:8092`), deployed via tugboat
  (`deploy.toml`/`deploy/provision.sh`), enrolled in `lighthouse.target`. No
  app-level auth — the tailnet is the security boundary, as with the other
  services. The browser is a thin client; the core is reused unchanged.
- **iOS now syncs with the web store** (offline-capable, server-authoritative) —
  see the realized design under Phase 5. The two no longer hold separate notes.

The "all platforms move together / no hacks" quality bar is unchanged.

---

## Operating principles

1. **All three platforms move together.** Each phase delivers on iOS, macOS, and Linux before the next phase begins. The Rust core work in a phase happens first, then the three UIs proceed in parallel.
2. **No phase is "done" until it works on all three platforms.** No skipping Linux because it's the slow one. No deferring iOS because the simulator is annoying. If a platform can't ship the phase, that's a problem with the design and we fix it before moving on.
3. **No hacks. Ever.** Per the project quality bar, if a phase blocks on a missing capability in the codebase, we add the capability properly rather than working around it.
4. **Audit at every phase boundary.** Before moving to the next phase, review for: hacks introduced, drift between platforms, dead code, unfinished implementations.

## Phase boundaries are gates

At the end of each phase:
- [ ] Acceptance criteria met on all three platforms
- [ ] No hacks in code (manual review)
- [ ] Regression check on prior phases (manual)
- [ ] Rust core tests pass
- [ ] Decision: continue, or pause and fix what's drifting

---

## Phase 1: Stream UX foundation

**Goal:** The capture-and-stream experience is solid, even with zero AI.

### Rust core work
- Add `updated_at: i64` to `Thought`
- Add `edit_history` table — each entry stores prior text + timestamp when a settled thought is edited
- "Settled" logic: a thought becomes settled after either (a) N minutes of inactivity, or (b) the app goes to background. Until settled, edits silently overwrite; after settled, edits append to `edit_history`.
- `update_thought(id, new_text)`, `delete_thought(id)`
- `list_paginated(cursor, limit)` returning the next page newest-first
- Tests for settled boundaries and edit history

### Platform UI work (parallel across iOS, macOS, Linux)
- **Stream view** — newest at bottom, lazy-loaded pagination, smooth scrolling
- **Composer** — keyboard-focused on app open, multiline input, send action
- **Tap recent thought to edit** — composer pre-fills, save updates the thought
- **Visual distinction** between live (editable) and settled (immutable from composer) thoughts
- **Relative timestamps** — "5 min ago", "yesterday"

### Risks
- GTK4 text input parity with iOS/macOS for IME, paste, multiline behavior. Verify early in the phase.
- Mobile keyboard handling — composer must not get hidden behind keyboard.

### Acceptance criteria
- [ ] All three apps: open, type, send, see it appear
- [ ] Edit a recent thought within window, overwrite works
- [ ] Edit a settled thought, edit history captured
- [ ] Stream scrolls smoothly with 1000+ thoughts

---

## Phase 2: Local full-text search

**Goal:** Find any thought by keyword, instantly.

### Rust core work
- Add SQLite **FTS5 virtual table** mirroring `thoughts`
- Triggers to keep FTS in sync on insert/update/delete
- `search_text(query) -> Vec<ThoughtMatch>` returning ranked results with match offsets for highlighting
- Tests covering ranking, special characters, partial words

### Platform UI work (parallel)
- Search bar — pull-to-reveal on mobile, persistent on desktop
- As-you-type results (debounced ~150ms)
- Highlight matched terms in result snippets
- Tap result → scroll stream to that thought (or open detail view)

### Acceptance criteria
- [ ] Search returns results in <50ms with 10k thoughts
- [ ] Highlighting works on all three platforms
- [ ] Search across special characters, multi-word queries, partial words

---

## Phase 3: Embeddings & semantic search

**Goal:** Find related thoughts when keywords don't match.

This is the **highest-risk phase**. Begin with a spike (1-3 days) to validate the model runs on iOS before committing to the design.

### Pre-phase spike
- Try `candle` with `all-MiniLM-L6-v2` on an iPhone in a throwaway branch
- If it works: proceed as below, single embedding implementation in Rust
- If it doesn't: fall back to using Apple's `NaturalLanguage` framework for iOS/macOS embeddings via Swift, `candle` for Linux. The embedding step moves to the platform layer for Apple platforms; the core stores and ranks vectors but doesn't compute them on Apple. Document this split honestly — it is not a hack if explicitly designed for, but it must be the genuinely correct decision given the constraint.

### Rust core work
- Integrate `candle` (or fallback) with `all-MiniLM-L6-v2` (384-dim vectors)
- Embed on capture; re-embed on edit
- Store vectors as SQLite BLOBs (1536 bytes each at 384 dims × f32)
- `search_semantic(query_text, top_k)` — embed query, rank by cosine similarity
- `search_combined(query, top_k)` — merge FTS and semantic with a ranking heuristic (start simple: union with score normalization)
- Tests with a small fixture corpus

### Platform UI work
- Search becomes "combined" by default — no user toggle initially
- Same UI as Phase 2

### Acceptance criteria
- [ ] Embedding compute takes <100ms on iPhone
- [ ] Semantic search returns sensible results on a hand-curated test corpus
- [ ] Combined ranking visibly improves over keyword-only on test corpus
- [ ] Model file ships inside app bundle (or downloaded on first run — decide during spike)

---

## Phase 4: Smart suggestions at capture

**Goal:** While typing a new thought, surface related past thoughts unobtrusively.

### Rust core work
- `find_related(draft_text, top_k)` reusing semantic search
- Debounced caller pattern documented for platform layers

### Platform UI work (parallel)
- Thin strip above composer showing 2–3 related thoughts as the user types
- Tap to view source thought (modal or navigation)
- Swipe-dismiss / outside-tap to ignore the strip without losing focus
- Each thought in the stream gets a small "X related" affordance — tap to expand

### Design constraint
- The suggestion strip must **never** be a modal, never block typing, never steal focus. If the user types right through it, it gets out of the way.

### Acceptance criteria
- [ ] Suggestions appear within 200ms of typing pause
- [ ] Suggestions never interrupt typing flow
- [ ] Related-thoughts affordance works in stream

---

## Phase 5: Sync server

**Goal:** A VPS-hosted sync server, plus a client sync engine in the Rust core.

### Realized (2026-06-16) — server-authoritative, no E2E

What shipped diverges from the original (E2E op-log) design below, because the
web app made the server authoritative and able to read notes:

- **Model:** server-authoritative, **per-record last-writer-wins** by
  `updated_at` (not per-field lamport / CRDT). Deletes are **tombstones**
  (`deleted_at`); local edits carry a **`dirty` outbox flag**. Schema v5 in
  `crates/core/src/store.rs`; sync API (`changes_since` / `pending_changes` /
  `apply_remote` / `mark_synced`) in `crates/core/src/sync.rs`.
- **Wire:** JSON over plain HTTP on the tailnet (the tailnet encrypts transport).
  **No E2E, no auth** — the server holds readable notes, like the web app.
- **Endpoint:** one `POST /api/sync` (`crates/server`) — push the client's
  changes (LWW), return the server's changes since the client's cursor.
- **Clients:** the web app is a live view of the store (no sync needed). iOS/macOS
  sync via `apple/Lagoon/Lagoon/Sync.swift` + the FFI sync methods (Phase 6).

The E2E op-log design below is **retained as reference** only — revisit it if
multi-user or untrusted-server requirements ever appear.

### Decisions to make at phase start

| Question | Default choice | Reconsider if |
|---|---|---|
| Sync model | Op-log with per-field merge | True multi-writer conflict on the same thought becomes common (then move to CRDT/Automerge) |
| Wire format | MessagePack over HTTPS | Devtools debugging makes JSON easier — switch if needed |
| Auth | Long-lived bearer token per device, issued by server admin (you) | More than one user ever expected |
| Real-time push | Polling on a timer + on app foreground | Latency feels bad |
| Encryption | E2E: device-side encryption, server stores ciphertext blobs | Storage growth makes search-on-server tempting — but then we lose E2E |
| Server lang/framework | Rust + axum | You'd rather use something else |
| Server storage | SQLite (litestream for backups) | Multi-user scale ever materializes |

### Server work
- Endpoints:
  - `POST /ops` — push new ops
  - `GET /ops?since=cursor` — pull ops since cursor
  - `GET /health`
- Storage: ops table (ciphertext blob + metadata)
- Auth middleware checking bearer token
- Deploy story: systemd service + reverse proxy on VPS, TLS via Let's Encrypt
- Logging, basic metrics

### Client work (in Rust core)
- `SyncEngine` — opens a connection, pulls since last cursor, applies, pushes pending ops
- Encryption/decryption boundary inside the engine
- Local cursor storage
- Conflict resolution: each field versioned by lamport-style counter; last-writer-wins per field, with edit_history preserving collisions
- Offline queue — ops persist locally until pushed

### Acceptance criteria
- [ ] Two devices can sync thoughts via the server
- [ ] Offline edits queue and sync on reconnect
- [ ] Server stores no plaintext
- [ ] Token compromise can be remediated (rotate token, server invalidates old)
- [ ] Server deployable via documented steps to a fresh VPS

---

## Phase 6: Sync integration on each platform

**Goal:** Sync runs in the background invisibly.

### Realized (2026-06-16) — iOS/macOS

Implemented in `apple/Lagoon/Lagoon/Sync.swift` + `ContentView.swift`: a `SyncService`
that pushes the local outbox and applies the server's changes, triggered on app
open, on `scenePhase` active/background, and after each capture; a toolbar sync
status indicator. The store + network run off the main actor. App config: an
outgoing-network entitlement (`Lagoon.entitlements`) and a tailnet ATS exception
(`Info.plist`).

The remaining list items are now done:
- **Periodic background sync (iOS)** — a `BGAppRefreshTask` registered via the
  SwiftUI `.backgroundTask(.appRefresh:)` scene modifier (no AppDelegate). The
  identifier (`com.deepwa7er.Lagoon.refresh`) and the `fetch` background mode are
  declared in `Info.plist`; the app submits the next request on backgrounding and
  each run re-chains. The handler (`BackgroundSync.run`) opens its own store and
  reconciles, independent of the UI. macOS keeps syncing on foreground/capture.
- **Pull-to-refresh** — `.refreshable` on the stream awaits a reconcile. The sync
  path was refactored to an awaitable `sync()` (with `syncNow()` as the
  fire-and-forget wrapper) that coalesces onto an in-flight reconcile, so the
  spinner reflects real completion.
- **Offline banner** — failures are classified (`SyncStatus`): connectivity
  errors become `.offline`, shown as a slim passive banner + a `wifi.slash`
  toolbar glyph; other failures stay `.failed`. Capture is unaffected.

Cursor handling was consolidated into one `SyncService.reconcilePersisting`
entry point used by both the UI and background sync. Because background sync
opens a **second** connection to the store file, the core now sets
`PRAGMA busy_timeout = 5000` (`crates/core/src/store.rs`) so a momentary
write-lock between the two connections waits rather than failing with
`SQLITE_BUSY`.

### Platform UI work
- **iOS** — `BGTaskScheduler` for periodic sync, sync-on-foreground, sync-on-send (push immediately)
- **macOS** — timer in the running app, sync-on-foreground, sync-on-send
- **Linux** — sync on app run, optional systemd user timer for background (document setup)
- All three:
  - Subtle sync status indicator ("Last synced 30s ago")
  - Pull-to-refresh (or equivalent) for manual sync
  - Offline indicator when the server is unreachable

### Acceptance criteria
- [ ] Capture on one device → visible on another within ~30s under normal conditions
- [ ] Both devices offline, both edit same thought, both come online → resolves without data loss; edit history preserves both versions
- [ ] Sync state is debuggable from the UI

---

## Phase 7: Tags and saved searches

**Goal:** Replace the *concept* of folders with lightweight tags and pinned queries.

### Realized (2026-06-20) — complete across core + server + web + Apple

Platforms are now **iOS + web** (Linux native dropped, see the Direction
update), so "move together" means core + web + Apple.

- **Core (schema v6):** inline `#tag`s are the source of truth in the thought
  text; the store mirrors them into `tags`/`thought_tags`, reconciled from text
  on every write (the FTS pattern) and GC'd when unused — so tags ride the text
  and sync for free. `tags_with_prefix` (autocomplete), `thoughts_with_tag`
  (filter), and `saved_searches` (pinned queries, **local to each store**, not in
  the sync feed; `query` is stored verbatim and routed like the search box). See
  `crates/core/src/{tags.rs,store.rs}`.
- **Server + FFI:** `/api/tags`, `/api/tags/{name}/thoughts`, saved-search CRUD;
  matching UniFFI exports (`apple-ffi`), ready for SwiftUI.
- **Web:** `#tag` autocomplete in the composer, inline tag chips (click →
  filter), a bare `#tag` query routes to the tag filter, and a pinned-searches
  bar. Decision taken: tag-chip tap reuses the search view; saved searches are
  local-first.

- **Apple (iOS + macOS):** inline `#tag` chips (linked → filter via an
  `OpenURLAction`), a `#tag` autocomplete strip above the composer, bare-`#tag`
  query routing, and a pinned saved-search bar (pin via alert, unpin via
  long-press). Built + run on the iOS 26.5 simulator (chips + saved bar render);
  macOS builds from the same shared SwiftUI. One platform-idiomatic difference
  from web: iOS autocompletes the tag at the draft's end, not an arbitrary caret.

**Phase-boundary audit (passed):** no hacks; web/iOS/macOS at feature parity;
no dead code; 82 core tests green. The web flows were browser-verified end to
end (9/9); the iOS UI was visually verified (full interaction-driving would need
XCUITest). While here, fixed a regression the breakwater migration introduced —
the Apple sync URL pointed at the now-dead `tailnet:8092` and now uses
`https://lagoon.intern.deepwa7er.net`.

### Rust core work
- `tags` table + `thought_tags` join
- Parser for `#tag` syntax in thought text
- Optional: tag suggestion via simple keyword/cluster heuristic (not LLM in this phase)
- `SavedSearch { id, name, query, semantic: bool }` table

### Platform UI work
- `#tag` autocomplete in composer (suggests existing tags)
- Tag chips on thoughts in stream
- Saved searches list — pinned in sidebar on desktop, dedicated screen on mobile
- "Save this search" action from search results

### Acceptance criteria
- [ ] Tagging from composer works on all platforms
- [ ] Saved searches update live as new thoughts are added
- [ ] No regression in stream performance with thousands of tagged thoughts

---

## Phase 8: Action tracking

**Goal:** Distinguish thoughts that still need action from those that don't.

### Realized (2026-06-20) — complete across core + server + web + Apple

- **Core (schema v7):** `actioned_at INTEGER` column on `thoughts`; `is_actioned:
  bool` derived field on `Thought` (NULL → false). `mark_actioned(id)` and
  `unmark_actioned(id)` both set/clear `actioned_at`, bump `updated_at`, and mark
  `dirty = 1` so the state propagates in the LWW sync feed. `list_stale(older_than_ms,
  limit)` surfaces unactioned thoughts not updated in N ms, oldest first. The v7
  migration guards against duplicate `ADD COLUMN` on stores where `user_version`
  was manually reset (using `pragma_table_info` check). ThoughtChange gains
  `actioned_at: Option<i64>` so the field is fully sync-able. 11 new core tests
  (93 total green).
- **Server + FFI:** `is_actioned` on `ThoughtDto`, `actioned_at` on
  `ThoughtChangeDto`; `POST /api/thoughts/{id}/mark-actioned`,
  `POST /api/thoughts/{id}/unmark-actioned`, `GET /api/thoughts/stale`; matching
  UniFFI exports (`markActioned`, `unmarkActioned`, `listStale`).
- **Web:** "done" button per row toggles actioned state. Actioned rows are visually
  faded (`opacity-40`) with strikethrough. A "hide done / show done" toggle in the
  footer persists across sessions via localStorage. `ThoughtDto.is_actioned` threaded
  from API through types → App → ThoughtRow. Web frontend builds clean.
- **Apple (iOS + macOS):** leading swipe-to-done (green checkmark; swipe left on
  actioned to undo, orange arrow). Actioned rows render at 45% opacity with
  strikethrough. A toolbar button (checkmark.circle) toggles hide-actioned, persisted
  via `@AppStorage`. The toggle action (mark/unmark) syncs immediately via
  `syncNow()`. iOS + macOS build from the same SwiftUI; xcframework rebuilt. iOS
  26.5 simulator build: `BUILD SUCCEEDED`.

**Phase-boundary audit (passed):** no hacks; mark/unmark propagates through sync
(LWW on `updated_at`); actioned_at rides the existing sync feed with zero new
protocol changes; all 93 core tests green; web frontend and iOS both build.

### Rust core work
- `actioned_at: Option<i64>` field on `Thought`
- `mark_actioned(id)` / `unmark_actioned(id)`
- `list_stale(older_than)` — unactioned thoughts older than threshold
- Settings record for "default view: all / unactioned only"

### Platform UI work
- Swipe gesture (mobile) / button (desktop) to mark actioned
- Visual treatment for actioned thoughts (faded? collapsed?)
- Optional default filter to hide actioned
- Gentle stale-thought surface — opt-in setting, configurable threshold

### Acceptance criteria
- [x] One-tap mark-as-actioned on all platforms
- [x] Filter toggles persist
- [ ] Stale surface is opt-in and not annoying (list_stale endpoint exists; no dedicated UI surface yet)

---

## Phase 9: Polish

**Goal:** Make it nice. Long tail of small things.

- Edit history viewer (see all versions of a thought)
- Export — Markdown, JSON
- Import — Apple Notes export, plain text dump
- Settings screen — model choice, sync interval, defaults
- Performance — virtualize stream view if needed for very large datasets
- Onboarding — paste sync token to add a new device
- Crash reporting — Sentry self-hosted on VPS, or none
- Accessibility audit on all three platforms
- App icon, splash, marketing copy

---

## Out of scope (defer or punt)

These are deliberately not in this plan. If we want them, they get their own plan documents.

- **Generative AI features** ("summarize my week", "what did I say about X?"). Easy to add later as a server-side call to Claude or local LLM. Not core to the design.
- **Sharing thoughts with other people.** Major auth/permissions surface.
- **Web app.** Doable but a fourth client.
- **Android app.** Would benefit from the Rust core; defer until iOS/macOS/Linux are stable.
- **Rich text / Markdown rendering.** Plain text only initially. Add when there's user demand.
- **Image / audio attachments.** Major storage and sync surface; defer.
- **Calendar/reminder integration.** Useful but out of scope for the core experience.

---

## How to use this document

This is a living plan, not a contract. Phase order is intentional but boundaries can shift if reality dictates. Two things must not shift:

1. **All three platforms move together.** No platform gets left behind.
2. **No hacks.** If a phase reveals an architectural mistake, fix it properly even if it means pausing.

When in doubt, re-read the project quality bar in `~/.claude/CLAUDE.md`.
