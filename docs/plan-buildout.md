# Buoy — Buildout Plan

This plan picks up after the [POC](./plan-poc.md) is complete and accepted. It builds the full app in phases.

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
- [ ] One-tap mark-as-actioned on all platforms
- [ ] Filter toggles persist
- [ ] Stale surface is opt-in and not annoying

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
