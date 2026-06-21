use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::embed::{TextEmbedder, blob_to_vector, dot, vector_to_blob};
use crate::error::{Error, Result};
use crate::saved_search::SavedSearch;
use crate::search::{
    MATCH_MARK_END, MATCH_MARK_START, SNIPPET_TOKENS, ThoughtMatch, build_match_query,
    extract_ranges,
};
use crate::sync::{SyncCursor, ThoughtChange};
use crate::tags::parse_tags;
use crate::thought::{EditEntry, Thought};

const SCHEMA_VERSION: i32 = 7;

/// Semantic results below this cosine similarity are dropped: they read as
/// noise to the user. Initial heuristic calibrated against the Phase 3
/// spike numbers (unrelated pairs scored 0.05–0.12, related 0.48–0.53).
const MIN_SEMANTIC_SIMILARITY: f32 = 0.25;

/// Constant in reciprocal-rank-fusion scoring (`1 / (K + rank)`), the
/// standard value from the RRF literature. Higher K flattens the
/// difference between rank positions.
const RRF_K: f32 = 60.0;

/// How long a thought stays "live" since its last edit. Edits within this
/// window silently overwrite; later edits archive the prior text into
/// `edit_history`.
const SETTLE_WINDOW_MS: i64 = 60 * 60 * 1000; // 60 minutes

/// Default page size for the stream view. Callers may pass any size to
/// `list_paginated`; this is just what the platform UIs ask for when they
/// don't have a more specific need.
pub const DEFAULT_PAGE_SIZE: usize = 64;

/// A keyset pagination cursor pointing just past one specific thought.
///
/// The pair `(created_at, id)` is unique and total-orderable in the same
/// way the stream's `ORDER BY created_at DESC, id DESC` is, so we can
/// resume a scan after this exact row without skips or duplicates even
/// when multiple thoughts share a millisecond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub created_at: i64,
    pub id: Uuid,
}

/// One page of thoughts. `next_cursor` is `Some` when more older thoughts
/// exist after this page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub thoughts: Vec<Thought>,
    pub next_cursor: Option<Cursor>,
}

/// Persistent store for thoughts.
///
/// Each platform supplies the database file path; the store does not pick a
/// location on its own. Sandboxing rules differ across iOS, macOS, and Linux,
/// so path discovery belongs in the platform layer.
///
/// `ThoughtStore` is not `Sync`; pass it across threads only through a guard
/// such as a `Mutex`.
pub struct ThoughtStore {
    conn: Connection,
    /// When attached, new and edited thoughts are embedded on write and
    /// semantic search becomes available. Optional because the model file
    /// may not be present on a device; everything else degrades cleanly.
    embedder: RefCell<Option<Box<dyn TextEmbedder>>>,
}

impl ThoughtStore {
    /// Open or create the store at `path`, applying any required schema setup.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|source| Error::OpenDatabase {
            path: path.to_path_buf(),
            source,
        })?;
        let store = Self {
            conn,
            embedder: RefCell::new(None),
        };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory store. Intended for tests and ephemeral use.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn,
            embedder: RefCell::new(None),
        };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    /// Attach an embedder. From here on, captures and edits are embedded
    /// on write, and `search_semantic` / the semantic half of
    /// `search_combined` are available. Call `embed_missing` afterwards to
    /// backfill thoughts captured while no embedder was attached.
    pub fn set_embedder(&self, embedder: Box<dyn TextEmbedder>) {
        *self.embedder.borrow_mut() = Some(embedder);
    }

    /// Whether an embedder is currently attached.
    pub fn has_embedder(&self) -> bool {
        self.embedder.borrow().is_some()
    }

    /// Capture a new thought with the current wall-clock timestamp.
    pub fn create(&self, text: &str) -> Result<Thought> {
        let now = now_unix_millis();
        let id = Uuid::new_v4();
        // Computed before the transaction so the model doesn't run inside it.
        let vector = self.try_embed(text);
        let tx = self.conn.unchecked_transaction()?;
        // dirty = 1 marks this as a local change to push on the next sync.
        tx.execute(
            "INSERT INTO thoughts (id, text, created_at, updated_at, settled_at, dirty)
             VALUES (?1, ?2, ?3, ?3, NULL, 1)",
            params![id.as_bytes().as_slice(), text, now],
        )?;
        if let Some(vector) = vector {
            tx.execute(
                "INSERT INTO embeddings (thought_id, vector) VALUES (?1, ?2)",
                params![id.as_bytes().as_slice(), vector_to_blob(&vector)],
            )?;
        }
        reconcile_tags(&tx, id, text)?;
        tx.commit()?;
        Ok(Thought {
            id,
            text: text.to_owned(),
            created_at: now,
            updated_at: now,
            is_settled: false,
            is_actioned: false,
        })
    }

    /// Embed `text` if an embedder is attached. Embedding *errors* are
    /// deliberately swallowed here: capture and edit must never fail
    /// because the model hiccuped. The missing vector is picked up later
    /// by `embed_missing`, which does surface errors.
    fn try_embed(&self, text: &str) -> Option<Vec<f32>> {
        self.embedder.borrow().as_ref()?.embed(text).ok()
    }

    /// Replace the text of `id`. If the thought is currently settled, the
    /// prior text is archived into `edit_history` and the thought is
    /// returned to live state (its `settled_at` is cleared).
    pub fn update_thought(&self, id: Uuid, new_text: &str) -> Result<Thought> {
        let now = now_unix_millis();
        // Computed before the transaction so the model doesn't run inside it.
        let vector = self.try_embed(new_text);
        let tx = self.conn.unchecked_transaction()?;

        let current: Option<(String, i64, i64, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT text, created_at, updated_at, settled_at, actioned_at
                 FROM thoughts WHERE id = ?1 AND deleted_at IS NULL",
                params![id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;

        let (prior_text, created_at, prior_updated_at, prior_settled_at, actioned_at) =
            current.ok_or(Error::NotFound { id })?;

        if is_settled_now(prior_updated_at, prior_settled_at, now) {
            tx.execute(
                "INSERT INTO edit_history (thought_id, text, archived_at)
                 VALUES (?1, ?2, ?3)",
                params![id.as_bytes().as_slice(), prior_text, now],
            )?;
        }

        tx.execute(
            "UPDATE thoughts
             SET text = ?1, updated_at = ?2, settled_at = NULL, dirty = 1
             WHERE id = ?3",
            params![new_text, now, id.as_bytes().as_slice()],
        )?;

        match vector {
            // The text changed, so a re-embed is required either way:
            // replace the vector when we have one, drop the stale one
            // when we don't (embed_missing will recompute it).
            Some(vector) => {
                tx.execute(
                    "INSERT OR REPLACE INTO embeddings (thought_id, vector) VALUES (?1, ?2)",
                    params![id.as_bytes().as_slice(), vector_to_blob(&vector)],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM embeddings WHERE thought_id = ?1",
                    params![id.as_bytes().as_slice()],
                )?;
            }
        }

        reconcile_tags(&tx, id, new_text)?;
        tx.commit()?;

        Ok(Thought {
            id,
            text: new_text.to_owned(),
            created_at,
            updated_at: now,
            is_settled: false,
            is_actioned: actioned_at.is_some(),
        })
    }

    /// Delete the thought with the given id.
    ///
    /// This is a *soft* delete: the row is kept as a tombstone (`deleted_at`
    /// set) so the deletion can propagate to other devices on sync. It is
    /// excluded from every read and search path. The stored embedding is
    /// dropped (a tombstone is never a semantic-search candidate); the
    /// `edit_history` rows are retained. `dirty = 1` queues the tombstone to
    /// push on the next sync.
    pub fn delete_thought(&self, id: Uuid) -> Result<()> {
        let now = now_unix_millis();
        let tx = self.conn.unchecked_transaction()?;
        let affected = tx.execute(
            "UPDATE thoughts SET deleted_at = ?1, updated_at = ?1, dirty = 1
             WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id.as_bytes().as_slice()],
        )?;
        if affected == 0 {
            return Err(Error::NotFound { id });
        }
        tx.execute(
            "DELETE FROM embeddings WHERE thought_id = ?1",
            params![id.as_bytes().as_slice()],
        )?;
        // A tombstone carries no tags; clearing them also GCs any now-orphan tag.
        reconcile_tags(&tx, id, "")?;
        tx.commit()?;
        Ok(())
    }

    /// Force every currently-live thought into the settled state. Called
    /// by the platform layer when the app goes to the background, so a
    /// returning user's next edit is always treated as a deliberate
    /// modification rather than a continuation.
    pub fn settle_all_live(&self) -> Result<usize> {
        let now = now_unix_millis();
        let n = self.conn.execute(
            "UPDATE thoughts SET settled_at = ?1
             WHERE settled_at IS NULL AND deleted_at IS NULL",
            params![now],
        )?;
        Ok(n)
    }

    /// Return every stored thought, newest first. For datasets large
    /// enough that this matters (thousands of rows), callers should use
    /// `list_paginated` instead.
    pub fn list(&self) -> Result<Vec<Thought>> {
        let now = now_unix_millis();
        let mut stmt = self.conn.prepare(
            "SELECT id, text, created_at, updated_at, settled_at, actioned_at
             FROM thoughts
             WHERE deleted_at IS NULL
             ORDER BY created_at DESC, id DESC",
        )?;
        let mut rows = stmt.query([])?;
        let raw = collect_thought_rows(&mut rows)?;
        Ok(raw.into_iter().map(|r| into_thought(r, now)).collect())
    }

    /// Return one page of thoughts, newest first. `before` is the cursor
    /// returned by the previous page, or `None` to start at the newest.
    pub fn list_paginated(&self, before: Option<Cursor>, limit: usize) -> Result<Page> {
        let now = now_unix_millis();
        // Fetch one extra row so we can tell whether another page follows.
        let fetch = limit.saturating_add(1);

        let raw = match before {
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, text, created_at, updated_at, settled_at, actioned_at
                     FROM thoughts
                     WHERE deleted_at IS NULL
                     ORDER BY created_at DESC, id DESC
                     LIMIT ?1",
                )?;
                let mut rows = stmt.query(params![fetch])?;
                collect_thought_rows(&mut rows)?
            }
            Some(cursor) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, text, created_at, updated_at, settled_at, actioned_at
                     FROM thoughts
                     WHERE deleted_at IS NULL
                       AND (created_at < ?1 OR (created_at = ?1 AND id < ?2))
                     ORDER BY created_at DESC, id DESC
                     LIMIT ?3",
                )?;
                let mut rows = stmt.query(params![
                    cursor.created_at,
                    cursor.id.as_bytes().as_slice(),
                    fetch
                ])?;
                collect_thought_rows(&mut rows)?
            }
        };

        let has_more = raw.len() > limit;
        let thoughts: Vec<Thought> = raw
            .into_iter()
            .take(limit)
            .map(|r| into_thought(r, now))
            .collect();

        let next_cursor = if has_more {
            thoughts.last().map(|t| Cursor {
                created_at: t.created_at,
                id: t.id,
            })
        } else {
            None
        };

        Ok(Page {
            thoughts,
            next_cursor,
        })
    }

    /// Return the edit history for a thought, oldest entry first.
    /// Returns an empty vector if the thought has never been edited after
    /// settling; returns `NotFound` if the thought itself doesn't exist.
    pub fn edit_history(&self, id: Uuid) -> Result<Vec<EditEntry>> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM thoughts WHERE id = ?1 AND deleted_at IS NULL",
                params![id.as_bytes().as_slice()],
                |row| row.get::<_, i32>(0),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(Error::NotFound { id });
        }

        let mut stmt = self.conn.prepare(
            "SELECT text, archived_at FROM edit_history
             WHERE thought_id = ?1
             ORDER BY archived_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![id.as_bytes().as_slice()], |row| {
            Ok(EditEntry {
                text: row.get(0)?,
                archived_at: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Full-text search over thought text, best matches first (BM25).
    ///
    /// `query` is raw user input — it is treated as literal words, with the
    /// final word matched as a prefix so results stay useful mid-keystroke.
    /// Input with nothing searchable in it returns an empty result set.
    pub fn search_text(&self, query: &str, limit: usize) -> Result<Vec<ThoughtMatch>> {
        let now = now_unix_millis();
        let Some(match_query) = build_match_query(query) else {
            return Ok(Vec::new());
        };

        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.text, t.created_at, t.updated_at, t.settled_at, t.actioned_at,
                    snippet(thoughts_fts, 0, ?2, ?3, '…', ?4)
             FROM thoughts_fts
             JOIN thoughts t ON t.rowid = thoughts_fts.rowid
             WHERE thoughts_fts MATCH ?1
               AND t.deleted_at IS NULL
             ORDER BY rank
             LIMIT ?5",
        )?;
        let mut rows = stmt.query(params![
            match_query,
            MATCH_MARK_START.to_string(),
            MATCH_MARK_END.to_string(),
            SNIPPET_TOKENS,
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;

        let mut matches = Vec::new();
        while let Some(row) = rows.next()? {
            let raw = parse_thought_row(row)?;
            let marked_snippet: String = row.get(6)?;
            let (snippet, ranges) = extract_ranges(&marked_snippet);
            matches.push(ThoughtMatch {
                thought: into_thought(raw, now),
                snippet,
                ranges,
            });
        }
        Ok(matches)
    }

    /// Embed up to `limit` thoughts that have no stored vector, newest
    /// first. Backfills thoughts captured while no embedder was attached
    /// (including everything that predates the embeddings schema) and
    /// retries writes whose inline embedding failed. Returns how many
    /// thoughts were embedded; call repeatedly until it returns 0.
    pub fn embed_missing(&self, limit: usize) -> Result<usize> {
        let embedder_ref = self.embedder.borrow();
        let Some(embedder) = embedder_ref.as_ref() else {
            return Err(Error::NoEmbedder);
        };

        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.text
             FROM thoughts t
             LEFT JOIN embeddings e ON e.thought_id = t.id
             WHERE e.thought_id IS NULL AND t.deleted_at IS NULL
             ORDER BY t.created_at DESC
             LIMIT ?1",
        )?;
        let pending = stmt
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let mut count = 0;
        for (id_blob, text) in pending {
            let id = uuid_from_blob(&id_blob)?;
            let vector = embedder.embed(&text)?;
            self.conn.execute(
                "INSERT OR REPLACE INTO embeddings (thought_id, vector) VALUES (?1, ?2)",
                params![id.as_bytes().as_slice(), vector_to_blob(&vector)],
            )?;
            count += 1;
        }
        Ok(count)
    }

    /// Semantic search: embed `query` and rank stored thoughts by cosine
    /// similarity, best first. Results below `MIN_SEMANTIC_SIMILARITY`
    /// are dropped. Semantic matches carry the whole thought text as their
    /// snippet and no highlight ranges — there are no matched terms to
    /// point at. Errors with `NoEmbedder` when no embedder is attached.
    pub fn search_semantic(&self, query: &str, top_k: usize) -> Result<Vec<ThoughtMatch>> {
        let embedder_ref = self.embedder.borrow();
        let Some(embedder) = embedder_ref.as_ref() else {
            return Err(Error::NoEmbedder);
        };
        let query_vector = embedder.embed(query)?;
        self.rank_by_vector(&query_vector, top_k, None)
    }

    /// Related thoughts for a draft the user is currently typing — the
    /// suggestion strip's query. Reuses semantic ranking; `exclude` drops
    /// the thought being edited so it doesn't suggest itself. Returns
    /// empty (rather than erroring) with no embedder or a blank draft:
    /// suggestions are an enhancement, never a failure the composer has
    /// to handle.
    ///
    /// Callers should debounce: invoke ~200ms after the last keystroke,
    /// cancelling the pending call on each new one.
    pub fn find_related(
        &self,
        draft_text: &str,
        top_k: usize,
        exclude: Option<Uuid>,
    ) -> Result<Vec<ThoughtMatch>> {
        if draft_text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let embedder_ref = self.embedder.borrow();
        let Some(embedder) = embedder_ref.as_ref() else {
            return Ok(Vec::new());
        };
        let query_vector = embedder.embed(draft_text)?;
        self.rank_by_vector(&query_vector, top_k, exclude)
    }

    /// Related thoughts for an existing thought, ranked by its *stored*
    /// vector (no embedding computed, so this works without an embedder).
    /// Returns empty when the thought has no vector yet; `NotFound` when
    /// the thought doesn't exist.
    pub fn find_related_to(&self, id: Uuid, top_k: usize) -> Result<Vec<ThoughtMatch>> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM thoughts WHERE id = ?1 AND deleted_at IS NULL",
                params![id.as_bytes().as_slice()],
                |row| row.get::<_, i32>(0),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(Error::NotFound { id });
        }

        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT vector FROM embeddings WHERE thought_id = ?1",
                params![id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(blob) = blob else {
            return Ok(Vec::new());
        };
        let query_vector = blob_to_vector(&blob)?;
        self.rank_by_vector(&query_vector, top_k, Some(id))
    }

    /// Rank stored vectors against `query_vector` by cosine similarity,
    /// best first, dropping results under `MIN_SEMANTIC_SIMILARITY` and
    /// the `exclude`d thought (a thought is always most similar to
    /// itself).
    fn rank_by_vector(
        &self,
        query_vector: &[f32],
        top_k: usize,
        exclude: Option<Uuid>,
    ) -> Result<Vec<ThoughtMatch>> {
        let now = now_unix_millis();

        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.text, t.created_at, t.updated_at, t.settled_at, t.actioned_at,
                    e.vector
             FROM embeddings e
             JOIN thoughts t ON t.id = e.thought_id
             WHERE t.deleted_at IS NULL",
        )?;
        let mut rows = stmt.query([])?;
        let mut scored = Vec::new();
        while let Some(row) = rows.next()? {
            let raw = parse_thought_row(row)?;
            if Some(raw.0) == exclude {
                continue;
            }
            let blob: Vec<u8> = row.get(6)?;
            let vector = blob_to_vector(&blob)?;
            if vector.len() != query_vector.len() {
                return Err(Error::CorruptRow {
                    table: "embeddings",
                    detail: format!(
                        "stored vector has {} dims, query has {}",
                        vector.len(),
                        query_vector.len()
                    ),
                });
            }
            let similarity = dot(query_vector, &vector);
            if similarity >= MIN_SEMANTIC_SIMILARITY {
                scored.push((similarity, raw));
            }
        }

        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(scored
            .into_iter()
            .take(top_k)
            .map(|(_, raw)| {
                let thought = into_thought(raw, now);
                ThoughtMatch {
                    snippet: thought.text.clone(),
                    ranges: Vec::new(),
                    thought,
                }
            })
            .collect())
    }

    /// Combined search: keyword (FTS5) and semantic results merged with
    /// reciprocal-rank fusion. Degrades to keyword-only when no embedder
    /// is attached, so the platform UIs can call this unconditionally.
    /// When a thought appears in both lists, the keyword version wins the
    /// representation (it carries snippet highlights) and the fused score
    /// ranks it higher than either list alone would.
    pub fn search_combined(&self, query: &str, top_k: usize) -> Result<Vec<ThoughtMatch>> {
        let keyword = self.search_text(query, top_k)?;
        let semantic = if self.has_embedder() {
            self.search_semantic(query, top_k)?
        } else {
            Vec::new()
        };

        let mut fused: Vec<(f32, ThoughtMatch)> = Vec::new();
        let mut index_of: HashMap<Uuid, usize> = HashMap::new();
        for list in [keyword, semantic] {
            for (rank, item) in list.into_iter().enumerate() {
                let rank_f = f32::from(u16::try_from(rank).unwrap_or(u16::MAX));
                let score = 1.0 / (RRF_K + rank_f + 1.0);
                if let Some(&i) = index_of.get(&item.thought.id) {
                    fused[i].0 += score;
                    if fused[i].1.ranges.is_empty() && !item.ranges.is_empty() {
                        fused[i].1 = item;
                    }
                } else {
                    index_of.insert(item.thought.id, fused.len());
                    fused.push((score, item));
                }
            }
        }

        fused.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| b.1.thought.created_at.cmp(&a.1.thought.created_at))
        });
        fused.truncate(top_k);
        Ok(fused.into_iter().map(|(_, item)| item).collect())
    }

    // ── sync ────────────────────────────────────────────────────────────────

    /// Local changes not yet pushed to the server — the client's outbox.
    /// Returns up to `limit` `dirty` rows (including tombstones), oldest change
    /// first, so a push proceeds in change order.
    pub fn pending_changes(&self, limit: usize) -> Result<Vec<ThoughtChange>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, created_at, updated_at, settled_at, deleted_at, actioned_at
             FROM thoughts
             WHERE dirty = 1
             ORDER BY updated_at ASC, id ASC
             LIMIT ?1",
        )?;
        let mut rows = stmt.query(params![i64::try_from(limit).unwrap_or(i64::MAX)])?;
        collect_change_rows(&mut rows)
    }

    /// The change feed: every thought (including tombstones) whose
    /// `(updated_at, id)` is strictly after `since`, oldest first, up to
    /// `limit`. This is what a client pulls from the server, and how the server
    /// answers a pull. When exactly `limit` rows return, more may remain — the
    /// caller resumes from [`SyncCursor::after`] the last one.
    pub fn changes_since(
        &self,
        since: Option<SyncCursor>,
        limit: usize,
    ) -> Result<Vec<ThoughtChange>> {
        let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
        match since {
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, text, created_at, updated_at, settled_at, deleted_at, actioned_at
                     FROM thoughts
                     ORDER BY updated_at ASC, id ASC
                     LIMIT ?1",
                )?;
                let mut rows = stmt.query(params![limit_i])?;
                collect_change_rows(&mut rows)
            }
            Some(cursor) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, text, created_at, updated_at, settled_at, deleted_at, actioned_at
                     FROM thoughts
                     WHERE updated_at > ?1 OR (updated_at = ?1 AND id > ?2)
                     ORDER BY updated_at ASC, id ASC
                     LIMIT ?3",
                )?;
                let mut rows = stmt.query(params![
                    cursor.updated_at,
                    cursor.id.as_bytes().as_slice(),
                    limit_i
                ])?;
                collect_change_rows(&mut rows)
            }
        }
    }

    /// Apply a change received from another device, last-writer-wins by
    /// `updated_at`. Inserts the thought when absent; otherwise overwrites the
    /// local row only when `change.updated_at` is strictly newer than the local
    /// one (so re-applying an old or identical change is a no-op). The applied
    /// row is marked clean (`dirty = 0`) — it came from the peer, not a local
    /// edit. Embeddings are recomputed from the new text (dropped for a
    /// tombstone); the FTS index follows via its triggers. Returns whether the
    /// change was applied.
    pub fn apply_remote(&self, change: &ThoughtChange) -> Result<bool> {
        let id_blob = change.id.as_bytes().as_slice();
        let local_updated: Option<i64> = self
            .conn
            .query_row(
                "SELECT updated_at FROM thoughts WHERE id = ?1",
                params![id_blob],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(local) = local_updated {
            if local >= change.updated_at {
                return Ok(false);
            }
        }

        // Embed live changes so semantic search works; never embed a tombstone.
        // Errors are swallowed like create/update — embed_missing retries later.
        let vector = if change.deleted_at.is_none() {
            self.try_embed(&change.text)
        } else {
            None
        };

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO thoughts
                 (id, text, created_at, updated_at, settled_at, deleted_at, actioned_at, dirty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
             ON CONFLICT(id) DO UPDATE SET
                 text        = excluded.text,
                 created_at  = excluded.created_at,
                 updated_at  = excluded.updated_at,
                 settled_at  = excluded.settled_at,
                 deleted_at  = excluded.deleted_at,
                 actioned_at = excluded.actioned_at,
                 dirty       = 0",
            params![
                id_blob,
                change.text,
                change.created_at,
                change.updated_at,
                change.settled_at,
                change.deleted_at,
                change.actioned_at,
            ],
        )?;
        match vector {
            Some(vector) => {
                tx.execute(
                    "INSERT OR REPLACE INTO embeddings (thought_id, vector) VALUES (?1, ?2)",
                    params![id_blob, vector_to_blob(&vector)],
                )?;
            }
            None => {
                // Tombstone, or text changed with no embedder: drop any stale
                // vector. embed_missing recomputes live ones later.
                tx.execute(
                    "DELETE FROM embeddings WHERE thought_id = ?1",
                    params![id_blob],
                )?;
            }
        }
        // Mirror tags from the incoming text (none for a tombstone) so a synced
        // thought's tags match its text on this device too.
        let tag_text = if change.deleted_at.is_some() {
            ""
        } else {
            change.text.as_str()
        };
        reconcile_tags(&tx, change.id, tag_text)?;
        tx.commit()?;
        Ok(true)
    }

    /// Clear the `dirty` flag for rows that were successfully pushed, but only
    /// when the row is unchanged since it was read (matched on `updated_at`).
    /// A concurrent local edit between push and ack bumps `updated_at`, so that
    /// row stays dirty and is pushed again on the next sync — no lost write.
    pub fn mark_synced(&self, pushed: &[(Uuid, i64)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (id, updated_at) in pushed {
            tx.execute(
                "UPDATE thoughts SET dirty = 0 WHERE id = ?1 AND updated_at = ?2",
                params![id.as_bytes().as_slice(), updated_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ── tags & saved searches ────────────────────────────────────────────────

    /// Tag names beginning with `prefix` (ASCII case-insensitive), most-used
    /// first — for `#tag` autocomplete. An empty prefix returns the most-used
    /// tags overall. Only tags currently applied to a live thought appear.
    pub fn tags_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let pattern = format!("{}%", escape_like(prefix));
        let mut stmt = self.conn.prepare(
            "SELECT t.name
             FROM tags t
             JOIN thought_tags tt ON tt.tag_id = t.id
             JOIN thoughts th ON th.id = tt.thought_id AND th.deleted_at IS NULL
             WHERE t.name LIKE ?1 ESCAPE '\\'
             GROUP BY t.id
             ORDER BY COUNT(*) DESC, t.name COLLATE NOCASE ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![pattern, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get::<_, String>(0),
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Live thoughts carrying the tag `name` (case-insensitive), newest first —
    /// the "tap a tag to filter" path.
    pub fn thoughts_with_tag(&self, name: &str, limit: usize) -> Result<Vec<Thought>> {
        let now = now_unix_millis();
        let mut stmt = self.conn.prepare(
            "SELECT th.id, th.text, th.created_at, th.updated_at, th.settled_at, th.actioned_at
             FROM thoughts th
             JOIN thought_tags tt ON tt.thought_id = th.id
             JOIN tags t ON t.id = tt.tag_id
             WHERE t.name = ?1 AND th.deleted_at IS NULL
             ORDER BY th.created_at DESC, th.id DESC
             LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![name, i64::try_from(limit).unwrap_or(i64::MAX)])?;
        let raw = collect_thought_rows(&mut rows)?;
        Ok(raw.into_iter().map(|r| into_thought(r, now)).collect())
    }

    /// Save a named query (a pinned search). `query` is stored verbatim — the
    /// caller routes it the same way the search box does (a `#tag` filters by
    /// tag; anything else runs combined search).
    pub fn create_saved_search(&self, name: &str, query: &str) -> Result<SavedSearch> {
        let id = Uuid::new_v4();
        let now = now_unix_millis();
        self.conn.execute(
            "INSERT INTO saved_searches (id, name, query, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id.as_bytes().as_slice(), name, query, now],
        )?;
        Ok(SavedSearch {
            id,
            name: name.to_owned(),
            query: query.to_owned(),
            created_at: now,
        })
    }

    /// Every saved search, oldest first.
    pub fn list_saved_searches(&self) -> Result<Vec<SavedSearch>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, query, created_at FROM saved_searches
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id_blob, name, query, created_at) = row?;
            out.push(SavedSearch {
                id: uuid_from_blob(&id_blob)?,
                name,
                query,
                created_at,
            });
        }
        Ok(out)
    }

    /// Delete a saved search. Removing one that doesn't exist is a no-op.
    pub fn delete_saved_search(&self, id: Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM saved_searches WHERE id = ?1",
            params![id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    // ── action tracking ──────────────────────────────────────────────────────

    /// Mark the thought as actioned. Sets `actioned_at = now`, bumps
    /// `updated_at`, marks dirty so the state propagates on sync. Returns the
    /// updated thought. `NotFound` if the thought doesn't exist or is deleted.
    pub fn mark_actioned(&self, id: Uuid) -> Result<Thought> {
        let now = now_unix_millis();
        let affected = self.conn.execute(
            "UPDATE thoughts SET actioned_at = ?1, updated_at = ?1, dirty = 1
             WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id.as_bytes().as_slice()],
        )?;
        if affected == 0 {
            return Err(Error::NotFound { id });
        }
        self.get_thought(id, now)
    }

    /// Clear the actioned state. Sets `actioned_at = NULL`, bumps `updated_at`,
    /// marks dirty. Returns the updated thought. `NotFound` if the thought
    /// doesn't exist or is deleted.
    pub fn unmark_actioned(&self, id: Uuid) -> Result<Thought> {
        let now = now_unix_millis();
        let affected = self.conn.execute(
            "UPDATE thoughts SET actioned_at = NULL, updated_at = ?1, dirty = 1
             WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id.as_bytes().as_slice()],
        )?;
        if affected == 0 {
            return Err(Error::NotFound { id });
        }
        self.get_thought(id, now)
    }

    /// Unactioned thoughts that haven't been updated in `older_than_ms`
    /// milliseconds, oldest first — the "stale inbox" view. A threshold of
    /// `7 * 24 * 60 * 60 * 1000` (7 days) is a reasonable default for callers
    /// that don't have a more specific need.
    pub fn list_stale(&self, older_than_ms: i64, limit: usize) -> Result<Vec<Thought>> {
        let now = now_unix_millis();
        let threshold = now.saturating_sub(older_than_ms);
        let mut stmt = self.conn.prepare(
            "SELECT id, text, created_at, updated_at, settled_at, actioned_at
             FROM thoughts
             WHERE deleted_at IS NULL
               AND actioned_at IS NULL
               AND updated_at < ?1
             ORDER BY updated_at ASC, id ASC
             LIMIT ?2",
        )?;
        let mut rows =
            stmt.query(params![threshold, i64::try_from(limit).unwrap_or(i64::MAX)])?;
        let raw = collect_thought_rows(&mut rows)?;
        Ok(raw.into_iter().map(|r| into_thought(r, now)).collect())
    }

    fn get_thought(&self, id: Uuid, now: i64) -> Result<Thought> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, created_at, updated_at, settled_at, actioned_at
             FROM thoughts WHERE id = ?1 AND deleted_at IS NULL",
        )?;
        let mut rows = stmt.query(params![id.as_bytes().as_slice()])?;
        let raw = collect_thought_rows(&mut rows)?;
        raw.into_iter()
            .next()
            .map(|r| into_thought(r, now))
            .ok_or(Error::NotFound { id })
    }

    fn configure(&self) -> Result<()> {
        // Foreign-key enforcement is off by default in SQLite; we need it
        // on for the edit_history -> thoughts CASCADE delete.
        //
        // busy_timeout: the same store file is opened by more than one
        // connection — the foreground UI and, on iOS, a periodic background-sync
        // task. Without a timeout, a connection that finds the file momentarily
        // write-locked by the other fails instantly with SQLITE_BUSY. Five
        // seconds lets it wait out the (sub-millisecond, infrequent) lock
        // instead. Harmless for the single-connection in-memory store.
        self.conn
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let current: i32 = self
            .conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |row| {
                row.get(0)
            })
            .optional()?
            .unwrap_or(0);

        if current < 1 {
            self.conn.execute_batch(
                "CREATE TABLE thoughts (
                    id         BLOB    PRIMARY KEY NOT NULL,
                    text       TEXT    NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX thoughts_created_at_idx ON thoughts (created_at DESC);",
            )?;
        }

        if current < 2 {
            // SQLite's ALTER TABLE cannot reference another column in
            // DEFAULT, so we add the column with a placeholder default
            // and then backfill from `created_at` in a separate step.
            self.conn.execute_batch(
                "ALTER TABLE thoughts ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
                ALTER TABLE thoughts ADD COLUMN settled_at INTEGER;
                UPDATE thoughts SET updated_at = created_at WHERE updated_at = 0;
                CREATE TABLE edit_history (
                    id          INTEGER PRIMARY KEY,
                    thought_id  BLOB    NOT NULL,
                    text        TEXT    NOT NULL,
                    archived_at INTEGER NOT NULL,
                    FOREIGN KEY (thought_id) REFERENCES thoughts(id) ON DELETE CASCADE
                );
                CREATE INDEX edit_history_thought_idx
                    ON edit_history (thought_id, archived_at);",
            )?;
        }

        if current < 3 {
            // External-content FTS5 index over `thoughts.text`. The
            // triggers keep it in sync; the UPDATE trigger is scoped to
            // `text` so settle/timestamp updates don't churn the index.
            // The backfill covers rows that existed before this migration.
            self.conn.execute_batch(
                "CREATE VIRTUAL TABLE thoughts_fts USING fts5(
                    text,
                    content='thoughts',
                    content_rowid='rowid',
                    tokenize='unicode61 remove_diacritics 2'
                );
                INSERT INTO thoughts_fts (rowid, text)
                    SELECT rowid, text FROM thoughts;
                CREATE TRIGGER thoughts_fts_after_insert
                AFTER INSERT ON thoughts BEGIN
                    INSERT INTO thoughts_fts (rowid, text)
                    VALUES (new.rowid, new.text);
                END;
                CREATE TRIGGER thoughts_fts_after_delete
                AFTER DELETE ON thoughts BEGIN
                    INSERT INTO thoughts_fts (thoughts_fts, rowid, text)
                    VALUES ('delete', old.rowid, old.text);
                END;
                CREATE TRIGGER thoughts_fts_after_update
                AFTER UPDATE OF text ON thoughts BEGIN
                    INSERT INTO thoughts_fts (thoughts_fts, rowid, text)
                    VALUES ('delete', old.rowid, old.text);
                    INSERT INTO thoughts_fts (rowid, text)
                    VALUES (new.rowid, new.text);
                END;",
            )?;
        }

        if current < 4 {
            // 384-dim f32 vectors as little-endian BLOBs (1536 bytes), one
            // per embedded thought. Rows are absent (not NULL) for thoughts
            // that haven't been embedded yet; `embed_missing` backfills.
            self.conn.execute_batch(
                "CREATE TABLE embeddings (
                    thought_id BLOB PRIMARY KEY NOT NULL
                        REFERENCES thoughts(id) ON DELETE CASCADE,
                    vector     BLOB NOT NULL
                );",
            )?;
        }

        if current < 5 {
            // Cross-device sync: `deleted_at` tombstones (so deletes can
            // propagate) and a `dirty` outbox flag (rows modified locally and
            // not yet pushed). The index supports the `(updated_at, id)` keyset
            // the change feed scans on. Existing rows are marked dirty so a
            // device with pre-sync data pushes it all up on its first sync.
            self.conn.execute_batch(
                "ALTER TABLE thoughts ADD COLUMN deleted_at INTEGER;
                ALTER TABLE thoughts ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0;
                UPDATE thoughts SET dirty = 1;
                CREATE INDEX thoughts_updated_at_idx ON thoughts (updated_at, id);",
            )?;
        }

        if current < 6 {
            self.migrate_to_v6()?;
        }

        if current < 7 {
            self.migrate_to_v7()?;
        }

        // Stamp the schema version. `user_version` is a PRAGMA, so we cannot
        // bind it as a parameter; building the statement with an integer
        // literal is safe.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        Ok(())
    }

    /// v7: action tracking — `actioned_at INTEGER` column on `thoughts`. NULL
    /// means unactioned; a timestamp means the user marked it done. Existing
    /// rows start NULL (unactioned).
    ///
    /// Guards against re-applying when the column is already on disk (can happen
    /// when a test resets `user_version` on a schema that already includes v7 to
    /// exercise an earlier migration path). SQLite doesn't support
    /// `ADD COLUMN IF NOT EXISTS`, so we check via `pragma_table_info`.
    fn migrate_to_v7(&self) -> Result<()> {
        let already_present: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('thoughts') WHERE name = 'actioned_at'",
                [],
                |row| row.get::<_, i32>(0),
            )
            .optional()?
            .unwrap_or(0)
            > 0;
        if !already_present {
            self.conn
                .execute_batch("ALTER TABLE thoughts ADD COLUMN actioned_at INTEGER;")?;
        }
        Ok(())
    }

    /// v6: the inline-`#tag` mirror tables (`tags`/`thought_tags`, reconciled
    /// from thought text on every write) and `saved_searches` (pinned queries,
    /// local to this store). Backfills tag links from existing thought text —
    /// `SQLite` can't parse `#tags`, so it reuses the write-path reconciler.
    fn migrate_to_v6(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE tags (
                id   INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE COLLATE NOCASE
            );
            CREATE TABLE thought_tags (
                thought_id BLOB    NOT NULL REFERENCES thoughts(id) ON DELETE CASCADE,
                tag_id     INTEGER NOT NULL REFERENCES tags(id)     ON DELETE CASCADE,
                PRIMARY KEY (thought_id, tag_id)
            );
            CREATE INDEX thought_tags_tag_idx ON thought_tags (tag_id);
            CREATE TABLE saved_searches (
                id         BLOB    PRIMARY KEY NOT NULL,
                name       TEXT    NOT NULL,
                query      TEXT    NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )?;
        let existing: Vec<(Vec<u8>, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, text FROM thoughts WHERE deleted_at IS NULL")?;
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id_blob, text) in existing {
            reconcile_tags(&self.conn, uuid_from_blob(&id_blob)?, &text)?;
        }
        Ok(())
    }
}

// (id, text, created_at, updated_at, settled_at, actioned_at)
type RawThoughtRow = (Uuid, String, i64, i64, Option<i64>, Option<i64>);

fn collect_thought_rows(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<RawThoughtRow>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(parse_thought_row(row)?);
    }
    Ok(out)
}

fn parse_thought_row(row: &rusqlite::Row<'_>) -> Result<RawThoughtRow> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let id = uuid_from_blob(&id_bytes)?;
    let text: String = row.get(1)?;
    let created_at: i64 = row.get(2)?;
    let updated_at: i64 = row.get(3)?;
    let settled_at: Option<i64> = row.get(4)?;
    let actioned_at: Option<i64> = row.get(5)?;
    Ok((id, text, created_at, updated_at, settled_at, actioned_at))
}

/// Collect `ThoughtChange` rows from a query selecting
/// `(id, text, created_at, updated_at, settled_at, deleted_at, actioned_at)`.
fn collect_change_rows(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<ThoughtChange>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let id_bytes: Vec<u8> = row.get(0)?;
        out.push(ThoughtChange {
            id: uuid_from_blob(&id_bytes)?,
            text: row.get(1)?,
            created_at: row.get(2)?,
            updated_at: row.get(3)?,
            settled_at: row.get(4)?,
            deleted_at: row.get(5)?,
            actioned_at: row.get(6)?,
        });
    }
    Ok(out)
}

fn into_thought(
    (id, text, created_at, updated_at, settled_at, actioned_at): RawThoughtRow,
    now: i64,
) -> Thought {
    Thought {
        id,
        text,
        created_at,
        updated_at,
        is_settled: is_settled_now(updated_at, settled_at, now),
        is_actioned: actioned_at.is_some(),
    }
}

fn uuid_from_blob(bytes: &[u8]) -> Result<Uuid> {
    <[u8; 16]>::try_from(bytes)
        .map(Uuid::from_bytes)
        .map_err(|_| Error::CorruptRow {
            table: "thoughts",
            detail: format!("id column had {} bytes, expected 16", bytes.len()),
        })
}

fn is_settled_now(updated_at: i64, settled_at: Option<i64>, now: i64) -> bool {
    settled_at.is_some() || (now - updated_at > SETTLE_WINDOW_MS)
}

fn now_unix_millis() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before UNIX epoch");
    i64::try_from(duration.as_millis()).expect("system clock is past year 292,278,994")
}

/// Reconcile the `tags`/`thought_tags` mirror for one thought from its text.
///
/// Sets the thought's tag links to exactly the `#tag`s in `text` (de-duplicated,
/// and case-folded across thoughts via the `NOCASE` unique on `tags.name`), then
/// drops any tag no thought references anymore, so autocomplete only ever offers
/// tags in use. Passing `""` clears a thought's tags (deletes / tombstones).
/// Runs inside the caller's transaction.
fn reconcile_tags(conn: &Connection, thought_id: Uuid, text: &str) -> Result<()> {
    let id_blob = thought_id.as_bytes().as_slice();
    conn.execute(
        "DELETE FROM thought_tags WHERE thought_id = ?1",
        params![id_blob],
    )?;
    for name in parse_tags(text) {
        conn.execute(
            "INSERT INTO tags (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            params![name],
        )?;
        let tag_id: i64 = conn.query_row(
            "SELECT id FROM tags WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO thought_tags (thought_id, tag_id) VALUES (?1, ?2)",
            params![id_blob, tag_id],
        )?;
    }
    conn.execute(
        "DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM thought_tags)",
        [],
    )?;
    Ok(())
}

/// Escape `LIKE` wildcards (`%`, `_`) and the escape char so a literal prefix —
/// a tag may legitimately contain `_` — matches literally. Pair with
/// `ESCAPE '\'` in the query.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn create_then_list_returns_the_thought() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let thought = store.create("hello buoy").unwrap();
        assert_eq!(store.list().unwrap(), vec![thought]);
    }

    #[test]
    fn list_returns_newest_first() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let first = store.create("one").unwrap();
        sleep(Duration::from_millis(2));
        let second = store.create("two").unwrap();
        sleep(Duration::from_millis(2));
        let third = store.create("three").unwrap();
        assert_eq!(store.list().unwrap(), vec![third, second, first]);
    }

    #[test]
    fn created_thought_is_live_with_matching_timestamps() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("x").unwrap();
        assert!(!t.is_settled);
        assert_eq!(t.created_at, t.updated_at);
    }

    #[test]
    fn unicode_text_round_trips() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let captured = store.create("水 🌊 思考 — émoji ok").unwrap();
        assert_eq!(store.list().unwrap(), vec![captured]);
    }

    #[test]
    fn ids_are_unique_across_creates() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let a = store.create("a").unwrap();
        let b = store.create("b").unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn thoughts_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buoy.sqlite");
        let original = {
            let store = ThoughtStore::open(&path).unwrap();
            store.create("survives a restart").unwrap()
        };
        let store = ThoughtStore::open(&path).unwrap();
        assert_eq!(store.list().unwrap(), vec![original]);
    }

    #[test]
    fn migrate_is_idempotent_and_upgrades_v1_to_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buoy.sqlite");
        {
            let _ = ThoughtStore::open(&path).unwrap();
        }
        // Re-opening must not error or duplicate the schema; the v1->v2
        // ALTER TABLE statements in particular would fail noisily if run
        // a second time.
        let store = ThoughtStore::open(&path).unwrap();
        store.create("after second open").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn upgrading_existing_v1_database_backfills_updated_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buoy.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE thoughts (
                    id         BLOB    PRIMARY KEY NOT NULL,
                    text       TEXT    NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX thoughts_created_at_idx ON thoughts (created_at DESC);
                PRAGMA user_version = 1;",
            )
            .unwrap();
            let id = Uuid::new_v4();
            conn.execute(
                "INSERT INTO thoughts (id, text, created_at) VALUES (?1, ?2, ?3)",
                params![id.as_bytes().as_slice(), "legacy", 1_700_000_000_000_i64],
            )
            .unwrap();
        }

        let store = ThoughtStore::open(&path).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].text, "legacy");
        assert_eq!(listed[0].created_at, 1_700_000_000_000);
        assert_eq!(listed[0].updated_at, 1_700_000_000_000);
        // A row that's already old by the time we open it must come back
        // as settled — that's the whole point of the time-based rule.
        assert!(listed[0].is_settled);
    }

    #[test]
    fn update_within_window_overwrites_without_history() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let original = store.create("first draft").unwrap();
        let updated = store.update_thought(original.id, "second draft").unwrap();

        assert_eq!(updated.text, "second draft");
        assert_eq!(updated.created_at, original.created_at);
        assert!(updated.updated_at >= original.updated_at);
        assert!(!updated.is_settled);
        assert!(store.edit_history(original.id).unwrap().is_empty());
    }

    #[test]
    fn update_after_force_settle_archives_prior_text() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("original").unwrap();
        let settled_count = store.settle_all_live().unwrap();
        assert_eq!(settled_count, 1);

        let updated = store.update_thought(t.id, "revised").unwrap();
        assert_eq!(updated.text, "revised");
        // The edit revived the thought, so it must be live again.
        assert!(!updated.is_settled);

        let history = store.edit_history(t.id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, "original");
    }

    #[test]
    fn multiple_settled_edits_accumulate_history() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("v1").unwrap();
        store.settle_all_live().unwrap();
        store.update_thought(t.id, "v2").unwrap();
        store.settle_all_live().unwrap();
        store.update_thought(t.id, "v3").unwrap();

        let history = store.edit_history(t.id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text, "v1");
        assert_eq!(history[1].text, "v2");
        assert!(history[0].archived_at <= history[1].archived_at);
    }

    #[test]
    fn settle_all_live_only_touches_live_rows() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("a").unwrap();
        store.create("b").unwrap();
        store.settle_all_live().unwrap();
        // Running it a second time should settle zero rows; everything's
        // already settled.
        assert_eq!(store.settle_all_live().unwrap(), 0);
    }

    #[test]
    fn update_unknown_id_returns_not_found() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let err = store
            .update_thought(Uuid::new_v4(), "nope")
            .expect_err("should fail");
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn delete_is_a_tombstone_excluded_from_reads() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("doomed").unwrap();
        store.settle_all_live().unwrap();
        store.update_thought(t.id, "still doomed").unwrap();

        store.delete_thought(t.id).unwrap();
        // Excluded from every read/search path, and not addressable by id.
        assert!(store.list().unwrap().is_empty());
        assert!(store.list_paginated(None, 10).unwrap().thoughts.is_empty());
        assert!(store.search_text("doomed", 10).unwrap().is_empty());
        assert!(matches!(
            store.edit_history(t.id),
            Err(Error::NotFound { .. })
        ));
        assert!(matches!(
            store.update_thought(t.id, "zombie"),
            Err(Error::NotFound { .. })
        ));
        // Its stored embedding is gone (a tombstone is never a search candidate).
        let embeddings: i64 = store
            .conn
            .query_row("SELECT count(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(embeddings, 0);

        // But the row survives as a tombstone so the deletion can propagate.
        let change = store.changes_since(None, 100).unwrap();
        assert_eq!(change.len(), 1);
        assert_eq!(change[0].id, t.id);
        assert!(change[0].deleted_at.is_some());
    }

    #[test]
    fn delete_unknown_id_returns_not_found() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let err = store
            .delete_thought(Uuid::new_v4())
            .expect_err("should fail");
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn list_paginated_walks_through_all_rows() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let mut created = Vec::new();
        for i in 0..7 {
            created.push(store.create(&format!("thought {i}")).unwrap());
            sleep(Duration::from_millis(2));
        }
        // Newest first.
        created.reverse();

        let mut collected = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = store.list_paginated(cursor, 3).unwrap();
            collected.extend(page.thoughts);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        assert_eq!(collected, created);
    }

    #[test]
    fn list_paginated_returns_no_cursor_when_exactly_one_page() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("only one").unwrap();
        let page = store.list_paginated(None, 10).unwrap();
        assert_eq!(page.thoughts.len(), 1);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn list_paginated_cursor_is_stable_across_same_millisecond_creates() {
        let store = ThoughtStore::open_in_memory().unwrap();
        // Insert several rows with identical created_at to exercise the
        // (created_at, id) tiebreaker in the cursor.
        let now = now_unix_millis();
        for _ in 0..5 {
            let id = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO thoughts (id, text, created_at, updated_at, settled_at)
                     VALUES (?1, ?2, ?3, ?3, NULL)",
                    params![id.as_bytes().as_slice(), "tie", now],
                )
                .unwrap();
        }

        let page1 = store.list_paginated(None, 2).unwrap();
        let page2 = store.list_paginated(page1.next_cursor, 2).unwrap();
        let page3 = store.list_paginated(page2.next_cursor, 2).unwrap();

        let total = page1.thoughts.len() + page2.thoughts.len() + page3.thoughts.len();
        assert_eq!(total, 5);

        // No id should appear twice.
        let mut ids: Vec<Uuid> = page1
            .thoughts
            .iter()
            .chain(page2.thoughts.iter())
            .chain(page3.thoughts.iter())
            .map(|t| t.id)
            .collect();
        ids.sort();
        let mut deduped = ids.clone();
        deduped.dedup();
        assert_eq!(ids, deduped);
    }

    #[test]
    fn search_finds_thoughts_by_keyword() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let groceries = store.create("buy milk and eggs").unwrap();
        store.create("call the dentist").unwrap();

        let matches = store.search_text("milk", 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].thought, groceries);
    }

    #[test]
    fn search_requires_all_words() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("rust compiler").unwrap();
        store.create("rust on the bumper").unwrap();

        let matches = store.search_text("rust compiler", 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].thought.text, "rust compiler");
    }

    #[test]
    fn search_matches_last_word_as_prefix() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("banana bread recipe").unwrap();

        assert_eq!(store.search_text("ban", 10).unwrap().len(), 1);
        assert_eq!(store.search_text("banana rec", 10).unwrap().len(), 1);
        // Only the *last* word is a prefix; earlier words must be whole.
        assert!(store.search_text("ban bread", 10).unwrap().is_empty());
    }

    #[test]
    fn search_ranks_higher_term_frequency_first() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("sailing notes from the lake trip").unwrap();
        store
            .create("sailing sailing and more sailing this weekend")
            .unwrap();

        let matches = store.search_text("sailing", 10).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(
            matches[0].thought.text,
            "sailing sailing and more sailing this weekend"
        );
    }

    #[test]
    fn search_special_characters_do_not_error() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("don't forget the (important) thing*").unwrap();

        for query in [
            "don't",
            "\"quoted\"",
            "(important)",
            "thing*",
            "AND",
            "OR",
            "NEAR",
            "milk OR eggs",
            "—",
            "***",
            "🌊",
        ] {
            // Must never surface an FTS5 syntax error, whatever the input.
            let _ = store.search_text(query, 10).unwrap();
        }

        assert_eq!(store.search_text("don't", 10).unwrap().len(), 1);
        assert_eq!(store.search_text("important", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_empty_query_returns_nothing() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("anything").unwrap();
        assert!(store.search_text("", 10).unwrap().is_empty());
        assert!(store.search_text("   ", 10).unwrap().is_empty());
    }

    #[test]
    fn search_respects_limit() {
        let store = ThoughtStore::open_in_memory().unwrap();
        for i in 0..5 {
            store.create(&format!("note number {i}")).unwrap();
        }
        assert_eq!(store.search_text("note", 3).unwrap().len(), 3);
    }

    #[test]
    fn search_index_follows_updates_and_deletes() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("original wording").unwrap();

        store.update_thought(t.id, "revised phrasing").unwrap();
        assert!(store.search_text("wording", 10).unwrap().is_empty());
        assert_eq!(store.search_text("phrasing", 10).unwrap().len(), 1);

        store.delete_thought(t.id).unwrap();
        assert!(store.search_text("phrasing", 10).unwrap().is_empty());
    }

    #[test]
    fn search_snippet_ranges_cover_matched_terms() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store
            .create("a fairly long thought about the moment the harbor buoy light blinked twice before dawn")
            .unwrap();

        let matches = store.search_text("buoy", 10).unwrap();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        // The thought is longer than the snippet window, so it truncates.
        assert!(m.snippet.contains('…'));
        assert_eq!(m.ranges.len(), 1);
        let range = m.ranges[0];
        assert_eq!(&m.snippet[range.start..range.start + range.len], "buoy");
    }

    #[test]
    fn search_matches_ignore_diacritics() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("rendezvous at the café").unwrap();
        assert_eq!(store.search_text("cafe", 10).unwrap().len(), 1);
    }

    #[test]
    fn migrating_pre_fts_database_backfills_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buoy.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE thoughts (
                    id         BLOB    PRIMARY KEY NOT NULL,
                    text       TEXT    NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX thoughts_created_at_idx ON thoughts (created_at DESC);
                PRAGMA user_version = 1;",
            )
            .unwrap();
            let id = Uuid::new_v4();
            conn.execute(
                "INSERT INTO thoughts (id, text, created_at) VALUES (?1, ?2, ?3)",
                params![
                    id.as_bytes().as_slice(),
                    "pre-existing searchable row",
                    1_700_000_000_000_i64
                ],
            )
            .unwrap();
        }

        let store = ThoughtStore::open(&path).unwrap();
        let matches = store.search_text("searchable", 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].thought.text, "pre-existing searchable row");
    }

    /// Deterministic test embedder: one dimension per *concept* (set to 1
    /// when the text contains any of the concept's words) plus a small
    /// constant bias dimension so no vector is ever zero, then
    /// L2-normalized. Synonyms land on the same dimension — "milk" and
    /// "cheese" are semantically close while sharing no keyword — and
    /// texts sharing no concepts score ~0.06, safely under
    /// `MIN_SEMANTIC_SIMILARITY`.
    struct VocabEmbedder;

    const CONCEPTS: [&[&str]; 3] = [
        &["milk", "cheese"],
        &["boat", "sailing"],
        &["rust", "compiler"],
    ];

    impl TextEmbedder for VocabEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let lower = text.to_lowercase();
            let mut v = vec![0.0_f32; CONCEPTS.len() + 1];
            for (i, words) in CONCEPTS.iter().enumerate() {
                if words.iter().any(|word| lower.contains(word)) {
                    v[i] = 1.0;
                }
            }
            v[CONCEPTS.len()] = 0.25;
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            Ok(v.into_iter().map(|x| x / norm).collect())
        }
    }

    /// Embedder that always fails — exercises the capture-must-not-fail
    /// contract.
    struct FailingEmbedder;

    impl TextEmbedder for FailingEmbedder {
        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Err(Error::Embedding {
                detail: "synthetic failure".into(),
            })
        }
    }

    fn embedding_count(store: &ThoughtStore) -> i64 {
        store
            .conn
            .query_row("SELECT count(*) FROM embeddings", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn create_with_embedder_stores_a_vector() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        store.create("milk run").unwrap();
        assert_eq!(embedding_count(&store), 1);
    }

    #[test]
    fn create_without_embedder_stores_no_vector_and_backfill_catches_up() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("milk run").unwrap();
        store.create("boat trip").unwrap();
        assert_eq!(embedding_count(&store), 0);

        store.set_embedder(Box::new(VocabEmbedder));
        assert_eq!(store.embed_missing(1).unwrap(), 1);
        assert_eq!(store.embed_missing(10).unwrap(), 1);
        assert_eq!(store.embed_missing(10).unwrap(), 0);
        assert_eq!(embedding_count(&store), 2);
    }

    #[test]
    fn embed_missing_without_embedder_errors() {
        let store = ThoughtStore::open_in_memory().unwrap();
        assert!(matches!(
            store.embed_missing(10).expect_err("should fail"),
            Error::NoEmbedder
        ));
    }

    #[test]
    fn capture_and_edit_survive_a_failing_embedder() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        let t = store.create("milk").unwrap();
        assert_eq!(embedding_count(&store), 1);

        // The embedder breaks; capture and edit must still work, and the
        // edit must drop the now-stale vector rather than keep it.
        store.set_embedder(Box::new(FailingEmbedder));
        store.create("still captured").unwrap();
        store.update_thought(t.id, "boat instead").unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(embedding_count(&store), 0);

        // A working embedder later picks both up via backfill.
        store.set_embedder(Box::new(VocabEmbedder));
        assert_eq!(store.embed_missing(10).unwrap(), 2);
    }

    #[test]
    fn update_reembeds_the_new_text() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        let t = store.create("milk and yogurt").unwrap();
        store.update_thought(t.id, "boat maintenance").unwrap();

        let results = store.search_semantic("boat", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].thought.id, t.id);
        assert!(store.search_semantic("milk", 10).unwrap().is_empty());
    }

    #[test]
    fn delete_cascades_the_embedding() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        let t = store.create("milk").unwrap();
        assert_eq!(embedding_count(&store), 1);
        store.delete_thought(t.id).unwrap();
        assert_eq!(embedding_count(&store), 0);
    }

    #[test]
    fn search_semantic_ranks_by_similarity_and_respects_top_k() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        // Two concepts vs one vs none for the query "milk sailing".
        let both = store.create("milk for the sailing trip").unwrap();
        let one = store.create("just milk today").unwrap();
        store.create("rust musings").unwrap();

        let results = store.search_semantic("milk sailing", 10).unwrap();
        assert_eq!(
            results.len(),
            2,
            "rust thought is below the similarity floor"
        );
        assert_eq!(results[0].thought.id, both.id);
        assert_eq!(results[1].thought.id, one.id);
        // Semantic matches carry full text and no highlight ranges.
        assert_eq!(results[0].snippet, results[0].thought.text);
        assert!(results[0].ranges.is_empty());

        assert_eq!(store.search_semantic("milk sailing", 1).unwrap().len(), 1);
    }

    #[test]
    fn search_semantic_without_embedder_errors() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("anything").unwrap();
        assert!(matches!(
            store
                .search_semantic("anything", 5)
                .expect_err("should fail"),
            Error::NoEmbedder
        ));
    }

    #[test]
    fn search_combined_merges_keyword_and_semantic() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        // Keyword-and-semantic hit ("milk" matches FTS and the vector).
        let both = store.create("milk for the week").unwrap();
        // Semantic-only hit: "cheese" shares the dairy concept dimension
        // with "milk" but has no keyword overlap, so FTS misses it.
        let semantic_only = store.create("cheese platter craving").unwrap();
        store.create("borrow checker woes").unwrap();

        let results = store.search_combined("milk", 10).unwrap();
        let ids: Vec<Uuid> = results.iter().map(|m| m.thought.id).collect();
        assert!(ids.contains(&both.id));
        assert!(ids.contains(&semantic_only.id));
        // Appearing in both lists outranks appearing in one.
        assert_eq!(results[0].thought.id, both.id);
        // The keyword representation (with highlights) wins for dual hits.
        assert!(!results[0].ranges.is_empty());
    }

    #[test]
    fn search_combined_degrades_to_keyword_only_without_embedder() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("plain keyword milk match").unwrap();
        let results = store.search_combined("milk", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].ranges.is_empty());
    }

    #[test]
    fn find_related_ranks_and_excludes_the_edited_thought() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        let dairy = store.create("cheese for the party").unwrap();
        store.create("compiler error archaeology").unwrap();

        // Drafting a related thought surfaces the dairy one.
        let related = store.find_related("milk run tomorrow", 3, None).unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].thought.id, dairy.id);

        // While *editing* the dairy thought, it must not suggest itself.
        let related = store
            .find_related("cheese for the party", 3, Some(dairy.id))
            .unwrap();
        assert!(related.is_empty());
    }

    #[test]
    fn find_related_is_empty_without_embedder_or_blank_draft() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("anything").unwrap();
        assert!(store.find_related("anything", 3, None).unwrap().is_empty());

        store.set_embedder(Box::new(VocabEmbedder));
        assert!(store.find_related("   ", 3, None).unwrap().is_empty());
    }

    #[test]
    fn find_related_to_uses_the_stored_vector_and_excludes_self() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(VocabEmbedder));
        let milk = store.create("milk for coffee").unwrap();
        let cheese = store.create("cheese board ideas").unwrap();
        store.create("borrow checker woes").unwrap();

        let related = store.find_related_to(milk.id, 3).unwrap();
        assert_eq!(related.len(), 1, "only the dairy-concept thought relates");
        assert_eq!(related[0].thought.id, cheese.id);

        // Works without an embedder — ranking uses stored vectors only.
        let fresh = ThoughtStore::open_in_memory().unwrap();
        fresh.set_embedder(Box::new(VocabEmbedder));
        let a = fresh.create("milk").unwrap();
        fresh.create("cheese").unwrap();
        let fresh_related = fresh.find_related_to(a.id, 3).unwrap();
        assert_eq!(fresh_related.len(), 1);
    }

    #[test]
    fn find_related_to_without_vector_is_empty_and_unknown_id_errors() {
        let store = ThoughtStore::open_in_memory().unwrap();
        // No embedder at creation time -> no stored vector.
        let t = store.create("milk").unwrap();
        assert!(store.find_related_to(t.id, 3).unwrap().is_empty());

        assert!(matches!(
            store
                .find_related_to(Uuid::new_v4(), 3)
                .expect_err("should fail"),
            Error::NotFound { .. }
        ));
    }

    #[test]
    #[ignore = "requires models/all-MiniLM-L6-v2 — run via `just test-semantic`"]
    fn semantic_search_with_real_model_on_fixture_corpus() {
        let model_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/all-MiniLM-L6-v2");
        let embedder = crate::embed::MiniLmEmbedder::load(&model_dir).unwrap();
        let store = ThoughtStore::open_in_memory().unwrap();
        store.set_embedder(Box::new(embedder));

        store
            .create("buy milk, eggs, and bread at the grocery store")
            .unwrap();
        store
            .create("the sailboat needs a new jib before the regatta")
            .unwrap();
        store
            .create("rust lifetimes make sense once you stop fighting them")
            .unwrap();

        // No keyword overlap with the groceries thought at all.
        let results = store
            .search_semantic("what food do I need to pick up", 2)
            .unwrap();
        assert!(!results.is_empty(), "semantic search found nothing");
        assert!(
            results[0].thought.text.contains("grocery"),
            "expected the groceries thought first, got: {}",
            results[0].thought.text
        );

        let combined = store.search_combined("sailboat regatta", 3).unwrap();
        assert_eq!(
            combined[0].thought.text,
            "the sailboat needs a new jib before the regatta"
        );
    }

    #[test]
    fn edit_history_unknown_id_returns_not_found() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let err = store.edit_history(Uuid::new_v4()).expect_err("should fail");
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn empty_text_is_allowed() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let captured = store.create("").unwrap();
        assert_eq!(captured.text, "");
        assert_eq!(store.list().unwrap(), vec![captured]);
    }

    #[test]
    fn corrupt_id_blob_surfaces_as_corrupt_row() {
        let store = ThoughtStore::open_in_memory().unwrap();
        // Insert a row directly with a malformed id (wrong length).
        store
            .conn
            .execute(
                "INSERT INTO thoughts (id, text, created_at, updated_at, settled_at)
                 VALUES (?1, ?2, ?3, ?3, NULL)",
                params![&[0u8, 1, 2][..], "bad", 0_i64],
            )
            .unwrap();
        let err = store.list().expect_err("list should fail on corrupt row");
        assert!(matches!(
            err,
            Error::CorruptRow {
                table: "thoughts",
                ..
            }
        ));
    }

    // ── sync ────────────────────────────────────────────────────────────────

    fn change_of(t: &Thought) -> ThoughtChange {
        ThoughtChange {
            id: t.id,
            text: t.text.clone(),
            created_at: t.created_at,
            updated_at: t.updated_at,
            settled_at: None,
            deleted_at: None,
            actioned_at: None,
        }
    }

    #[test]
    fn create_and_edit_mark_rows_dirty() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let a = store.create("alpha").unwrap();
        let b = store.create("beta").unwrap();
        // Both are local changes awaiting push.
        let pending = store.pending_changes(100).unwrap();
        assert_eq!(pending.len(), 2);

        // Marking them synced clears the outbox.
        store
            .mark_synced(&[(a.id, a.updated_at), (b.id, b.updated_at)])
            .unwrap();
        assert!(store.pending_changes(100).unwrap().is_empty());

        // A later edit re-dirties just that row.
        let a2 = store.update_thought(a.id, "alpha edited").unwrap();
        let pending = store.pending_changes(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, a.id);
        assert_eq!(pending[0].updated_at, a2.updated_at);
    }

    #[test]
    fn apply_remote_inserts_then_respects_last_writer_wins() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        let remote = ThoughtChange {
            id,
            text: "from another device".into(),
            created_at: 1_000,
            updated_at: 1_000,
            settled_at: None,
            deleted_at: None,
            actioned_at: None,
        };
        // Absent locally → inserted, and it lands clean (not in the outbox).
        assert!(store.apply_remote(&remote).unwrap());
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.pending_changes(100).unwrap().is_empty());

        // An older change for the same id is ignored.
        let older = ThoughtChange {
            text: "stale".into(),
            updated_at: 500,
            ..remote.clone()
        };
        assert!(!store.apply_remote(&older).unwrap());
        assert_eq!(store.list().unwrap()[0].text, "from another device");

        // A strictly newer change wins.
        let newer = ThoughtChange {
            text: "newer wins".into(),
            updated_at: 2_000,
            ..remote.clone()
        };
        assert!(store.apply_remote(&newer).unwrap());
        assert_eq!(store.list().unwrap()[0].text, "newer wins");
        // Re-applying it is an idempotent no-op (equal updated_at).
        assert!(!store.apply_remote(&newer).unwrap());
    }

    #[test]
    fn apply_remote_tombstone_deletes_locally() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("here then gone").unwrap();
        let tombstone = ThoughtChange {
            deleted_at: Some(t.updated_at + 1),
            updated_at: t.updated_at + 1,
            ..change_of(&t)
        };
        assert!(store.apply_remote(&tombstone).unwrap());
        assert!(store.list().unwrap().is_empty());
        // The tombstone is now clean locally (came from the peer).
        assert!(store.pending_changes(100).unwrap().is_empty());
    }

    #[test]
    fn changes_since_is_a_gapless_keyset_feed() {
        let store = ThoughtStore::open_in_memory().unwrap();
        for i in 0..5 {
            store.create(&format!("t{i}")).unwrap();
        }
        // Page through the whole feed two at a time using the keyset cursor.
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.changes_since(cursor, 2).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(SyncCursor::after(page.last().unwrap()));
            seen.extend(page.iter().map(|c| c.id));
            if seen.len() >= 5 {
                break;
            }
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 5, "every row seen exactly once, no gaps/dupes");
    }

    #[test]
    fn mark_synced_keeps_a_concurrently_edited_row_dirty() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("v1").unwrap();
        // The edit must land in a later millisecond than the create so its
        // updated_at differs — that's the whole point of the ack check below.
        sleep(Duration::from_millis(2));
        // Simulate: read the row for push, then it's edited before the ack.
        let edited = store.update_thought(t.id, "v2").unwrap();
        // Ack the OLD version — must not clear the (now newer) dirty row.
        store.mark_synced(&[(t.id, t.updated_at)]).unwrap();
        let pending = store.pending_changes(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].updated_at, edited.updated_at);
    }

    // ── tags & saved searches ────────────────────────────────────────────────

    fn tag_count(store: &ThoughtStore) -> i64 {
        store
            .conn
            .query_row("SELECT count(*) FROM tags", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn create_mirrors_tags_from_text() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("ship the #idea and the #plan").unwrap();
        let mut tags = store.tags_with_prefix("", 10).unwrap();
        tags.sort();
        assert_eq!(tags, vec!["idea", "plan"]);
    }

    #[test]
    fn editing_text_adds_and_removes_tags_and_gcs_orphans() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("first #alpha").unwrap();
        assert_eq!(store.tags_with_prefix("", 10).unwrap(), vec!["alpha"]);

        store.update_thought(t.id, "now #beta only").unwrap();
        // alpha is orphaned and GC'd; beta is present.
        assert_eq!(store.tags_with_prefix("", 10).unwrap(), vec!["beta"]);
        assert_eq!(tag_count(&store), 1);
    }

    #[test]
    fn tags_are_case_folded_across_thoughts() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let a = store.create("one #Idea").unwrap();
        let b = store.create("two #idea").unwrap();
        // A single tag, first casing kept.
        assert_eq!(store.tags_with_prefix("", 10).unwrap(), vec!["Idea"]);
        // Both thoughts filter under it, case-insensitively.
        let ids: Vec<Uuid> = store
            .thoughts_with_tag("IDEA", 10)
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert!(ids.contains(&a.id) && ids.contains(&b.id));
    }

    #[test]
    fn tags_with_prefix_filters_and_orders_by_use() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("#work a").unwrap();
        store.create("#work b").unwrap();
        store.create("#worry c").unwrap();
        store.create("#home d").unwrap();
        // Prefix "wo" matches work + worry; work is used twice, so it leads.
        assert_eq!(
            store.tags_with_prefix("wo", 10).unwrap(),
            vec!["work", "worry"]
        );
    }

    #[test]
    fn tags_with_prefix_escapes_underscore_wildcard() {
        let store = ThoughtStore::open_in_memory().unwrap();
        store.create("#a_b").unwrap();
        store.create("#axb").unwrap();
        // "a_" must match literally (a, underscore), not "a<any>".
        assert_eq!(store.tags_with_prefix("a_", 10).unwrap(), vec!["a_b"]);
    }

    #[test]
    fn thoughts_with_tag_excludes_deleted() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let keep = store.create("keep #x").unwrap();
        let gone = store.create("drop #x").unwrap();
        store.delete_thought(gone.id).unwrap();
        let hits = store.thoughts_with_tag("x", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, keep.id);
        // The remaining thought keeps the tag alive.
        assert_eq!(store.tags_with_prefix("", 10).unwrap(), vec!["x"]);
    }

    #[test]
    fn deleting_last_tagged_thought_gcs_the_tag() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let only = store.create("solo #unique").unwrap();
        assert_eq!(tag_count(&store), 1);
        store.delete_thought(only.id).unwrap();
        assert_eq!(tag_count(&store), 0);
    }

    #[test]
    fn apply_remote_mirrors_tags() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let remote = ThoughtChange {
            id: Uuid::new_v4(),
            text: "synced #frompeer".into(),
            created_at: 1_000,
            updated_at: 1_000,
            settled_at: None,
            deleted_at: None,
            actioned_at: None,
        };
        store.apply_remote(&remote).unwrap();
        assert_eq!(store.tags_with_prefix("", 10).unwrap(), vec!["frompeer"]);

        // A tombstone clears them.
        let tombstone = ThoughtChange {
            deleted_at: Some(2_000),
            updated_at: 2_000,
            ..remote
        };
        store.apply_remote(&tombstone).unwrap();
        assert!(store.tags_with_prefix("", 10).unwrap().is_empty());
    }

    #[test]
    fn migration_backfills_tags_from_existing_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buoy.sqlite");
        {
            let store = ThoughtStore::open(&path).unwrap();
            store.create("legacy #backfilled note").unwrap();
            // Drop the v6 tables and pretend the file is still at v5, leaving
            // the tagged thought text in place.
            store
                .conn
                .execute_batch(
                    "DROP TABLE thought_tags; DROP TABLE tags; DROP TABLE saved_searches;
                     PRAGMA user_version = 5;",
                )
                .unwrap();
        }
        // Reopening runs the v6 migration, which backfills tags from text.
        let store = ThoughtStore::open(&path).unwrap();
        assert_eq!(
            store.tags_with_prefix("", 10).unwrap(),
            vec!["backfilled"]
        );
    }

    #[test]
    fn saved_search_crud() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let a = store.create_saved_search("Work", "#work").unwrap();
        let b = store.create_saved_search("Milk", "milk OR dairy").unwrap();

        let all = store.list_saved_searches().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|s| *s == a));
        assert!(all.iter().any(|s| s.query == "milk OR dairy"));

        store.delete_saved_search(b.id).unwrap();
        let all = store.list_saved_searches().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, a.id);

        // Deleting a missing one is a no-op.
        store.delete_saved_search(Uuid::new_v4()).unwrap();
        assert_eq!(store.list_saved_searches().unwrap().len(), 1);
    }

    // ── action tracking ──────────────────────────────────────────────────────

    #[test]
    fn new_thought_is_unactioned() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("to do").unwrap();
        assert!(!t.is_actioned);
    }

    #[test]
    fn mark_actioned_sets_flag() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("to do").unwrap();
        assert!(!t.is_actioned);
        let actioned = store.mark_actioned(t.id).unwrap();
        assert!(actioned.is_actioned);
        assert_eq!(actioned.id, t.id);
        // Persists through list().
        assert!(store.list().unwrap()[0].is_actioned);
    }

    #[test]
    fn unmark_actioned_clears_flag() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("done").unwrap();
        store.mark_actioned(t.id).unwrap();
        let unactioned = store.unmark_actioned(t.id).unwrap();
        assert!(!unactioned.is_actioned);
        assert!(!store.list().unwrap()[0].is_actioned);
    }

    #[test]
    fn mark_actioned_unknown_id_returns_not_found() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let err = store.mark_actioned(Uuid::new_v4()).expect_err("should fail");
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn unmark_actioned_unknown_id_returns_not_found() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let err = store.unmark_actioned(Uuid::new_v4()).expect_err("should fail");
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn mark_actioned_marks_row_dirty_for_sync() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("item").unwrap();
        // Simulate a successful push.
        store.mark_synced(&[(t.id, t.updated_at)]).unwrap();
        assert!(store.pending_changes(100).unwrap().is_empty());
        // Marking actioned re-dirties the row.
        store.mark_actioned(t.id).unwrap();
        let pending = store.pending_changes(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].actioned_at.is_some());
    }

    #[test]
    fn actioned_at_propagates_through_sync() {
        let store_a = ThoughtStore::open_in_memory().unwrap();
        let store_b = ThoughtStore::open_in_memory().unwrap();

        let t = store_a.create("shared item").unwrap();
        // Push from A to B.
        let change = store_a.pending_changes(10).unwrap().remove(0);
        store_b.apply_remote(&change).unwrap();
        assert!(!store_b.list().unwrap()[0].is_actioned);

        // A marks it actioned; the change propagates to B.
        store_a.mark_actioned(t.id).unwrap();
        let actioned_change = store_a.pending_changes(10).unwrap().remove(0);
        assert!(actioned_change.actioned_at.is_some());
        store_b.apply_remote(&actioned_change).unwrap();
        assert!(store_b.list().unwrap()[0].is_actioned);
    }

    #[test]
    fn list_stale_returns_old_unactioned_thoughts() {
        let store = ThoughtStore::open_in_memory().unwrap();
        // Insert an "old" row directly to avoid the timing dependency.
        let id = Uuid::new_v4();
        let old_ts = now_unix_millis() - 10_000; // 10 seconds ago
        store
            .conn
            .execute(
                "INSERT INTO thoughts (id, text, created_at, updated_at, settled_at)
                 VALUES (?1, ?2, ?3, ?3, NULL)",
                params![id.as_bytes().as_slice(), "old item", old_ts],
            )
            .unwrap();

        // Threshold of 5 seconds: the 10s-old row qualifies.
        let stale = store.list_stale(5_000, 10).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, id);
        assert!(!stale[0].is_actioned);
    }

    #[test]
    fn list_stale_excludes_actioned_thoughts() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        let old_ts = now_unix_millis() - 10_000;
        store
            .conn
            .execute(
                "INSERT INTO thoughts (id, text, created_at, updated_at, settled_at, actioned_at)
                 VALUES (?1, ?2, ?3, ?3, NULL, ?3)",
                params![id.as_bytes().as_slice(), "done", old_ts],
            )
            .unwrap();

        // Actioned thoughts never appear in the stale list regardless of age.
        let stale = store.list_stale(5_000, 10).unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn list_stale_excludes_recently_updated_thoughts() {
        let store = ThoughtStore::open_in_memory().unwrap();
        // A freshly-created thought should not appear (updated_at is now).
        store.create("fresh").unwrap();
        // Threshold of 5 minutes; the fresh thought was updated now.
        let stale = store.list_stale(5 * 60 * 1000, 10).unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn update_thought_preserves_actioned_state() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("todo").unwrap();
        store.mark_actioned(t.id).unwrap();
        // Editing the text should not clear the actioned flag.
        let updated = store.update_thought(t.id, "todo (revised)").unwrap();
        assert!(updated.is_actioned);
        assert!(store.list().unwrap()[0].is_actioned);
    }
}
