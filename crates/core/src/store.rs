use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::thought::{EditEntry, Thought};

const SCHEMA_VERSION: i32 = 2;

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
}

impl ThoughtStore {
    /// Open or create the store at `path`, applying any required schema setup.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|source| Error::OpenDatabase {
            path: path.to_path_buf(),
            source,
        })?;
        let store = Self { conn };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory store. Intended for tests and ephemeral use.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    /// Capture a new thought with the current wall-clock timestamp.
    pub fn create(&self, text: &str) -> Result<Thought> {
        let now = now_unix_millis();
        let id = Uuid::new_v4();
        self.conn.execute(
            "INSERT INTO thoughts (id, text, created_at, updated_at, settled_at)
             VALUES (?1, ?2, ?3, ?3, NULL)",
            params![id.as_bytes().as_slice(), text, now],
        )?;
        Ok(Thought {
            id,
            text: text.to_owned(),
            created_at: now,
            updated_at: now,
            is_settled: false,
        })
    }

    /// Replace the text of `id`. If the thought is currently settled, the
    /// prior text is archived into `edit_history` and the thought is
    /// returned to live state (its `settled_at` is cleared).
    pub fn update_thought(&self, id: Uuid, new_text: &str) -> Result<Thought> {
        let now = now_unix_millis();
        let tx = self.conn.unchecked_transaction()?;

        let current: Option<(String, i64, i64, Option<i64>)> = tx
            .query_row(
                "SELECT text, created_at, updated_at, settled_at
                 FROM thoughts WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        let (prior_text, created_at, prior_updated_at, prior_settled_at) =
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
             SET text = ?1, updated_at = ?2, settled_at = NULL
             WHERE id = ?3",
            params![new_text, now, id.as_bytes().as_slice()],
        )?;

        tx.commit()?;

        Ok(Thought {
            id,
            text: new_text.to_owned(),
            created_at,
            updated_at: now,
            is_settled: false,
        })
    }

    /// Delete the thought with the given id. Any associated edit-history
    /// rows are removed by the foreign-key cascade.
    pub fn delete_thought(&self, id: Uuid) -> Result<()> {
        let affected = self.conn.execute(
            "DELETE FROM thoughts WHERE id = ?1",
            params![id.as_bytes().as_slice()],
        )?;
        if affected == 0 {
            return Err(Error::NotFound { id });
        }
        Ok(())
    }

    /// Force every currently-live thought into the settled state. Called
    /// by the platform layer when the app goes to the background, so a
    /// returning user's next edit is always treated as a deliberate
    /// modification rather than a continuation.
    pub fn settle_all_live(&self) -> Result<usize> {
        let now = now_unix_millis();
        let n = self.conn.execute(
            "UPDATE thoughts SET settled_at = ?1 WHERE settled_at IS NULL",
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
            "SELECT id, text, created_at, updated_at, settled_at
             FROM thoughts
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
                    "SELECT id, text, created_at, updated_at, settled_at
                     FROM thoughts
                     ORDER BY created_at DESC, id DESC
                     LIMIT ?1",
                )?;
                let mut rows = stmt.query(params![fetch])?;
                collect_thought_rows(&mut rows)?
            }
            Some(cursor) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, text, created_at, updated_at, settled_at
                     FROM thoughts
                     WHERE created_at < ?1
                        OR (created_at = ?1 AND id < ?2)
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
                "SELECT 1 FROM thoughts WHERE id = ?1",
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

    fn configure(&self) -> Result<()> {
        // Foreign-key enforcement is off by default in SQLite; we need it
        // on for the edit_history -> thoughts CASCADE delete.
        self.conn.execute_batch("PRAGMA foreign_keys = ON;")?;
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

        // Stamp the schema version. `user_version` is a PRAGMA, so we cannot
        // bind it as a parameter; building the statement with an integer
        // literal is safe.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        Ok(())
    }
}

type RawThoughtRow = (Uuid, String, i64, i64, Option<i64>);

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
    Ok((id, text, created_at, updated_at, settled_at))
}

fn into_thought(
    (id, text, created_at, updated_at, settled_at): RawThoughtRow,
    now: i64,
) -> Thought {
    Thought {
        id,
        text,
        created_at,
        updated_at,
        is_settled: is_settled_now(updated_at, settled_at, now),
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
    fn delete_removes_thought_and_history() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let t = store.create("doomed").unwrap();
        store.settle_all_live().unwrap();
        store.update_thought(t.id, "still doomed").unwrap();
        assert_eq!(store.edit_history(t.id).unwrap().len(), 1);

        store.delete_thought(t.id).unwrap();
        assert!(store.list().unwrap().is_empty());
        // FK cascade should have removed the history rows too. We can't
        // call edit_history on the deleted id (it returns NotFound), so
        // verify directly via SQL.
        let history_count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM edit_history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(history_count, 0);
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
}
