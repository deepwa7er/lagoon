use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::thought::Thought;

const SCHEMA_VERSION: i32 = 1;

/// Persistent store for thoughts.
///
/// Each platform supplies the database file path; the store does not pick a
/// location on its own. Sandboxing rules differ across iOS, macOS, and Linux,
/// so path discovery belongs in the platform layer.
///
/// `ThoughtStore` is not `Sync`; pass it across threads only through a guard
/// such as a `Mutex`. The POC clients run all storage operations on a single
/// thread.
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
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory store. Intended for tests and ephemeral use.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Capture a new thought with the current wall-clock timestamp.
    pub fn create(&self, text: &str) -> Result<Thought> {
        let thought = Thought {
            id: Uuid::new_v4(),
            text: text.to_owned(),
            created_at: now_unix_millis(),
        };
        self.conn.execute(
            "INSERT INTO thoughts (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![
                thought.id.as_bytes().as_slice(),
                thought.text,
                thought.created_at,
            ],
        )?;
        Ok(thought)
    }

    /// Return every stored thought, newest first.
    pub fn list(&self) -> Result<Vec<Thought>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, created_at FROM thoughts ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], row_to_thought)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Result<Vec<_>>>()
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

        // Stamp the schema version. `user_version` is a PRAGMA, so we cannot
        // bind it as a parameter; building the statement with an integer
        // literal is safe.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        Ok(())
    }
}

fn row_to_thought(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Thought>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let text: String = row.get(1)?;
    let created_at: i64 = row.get(2)?;

    let id = match <[u8; 16]>::try_from(id_bytes.as_slice()) {
        Ok(bytes) => Uuid::from_bytes(bytes),
        Err(_) => {
            return Ok(Err(Error::CorruptRow {
                table: "thoughts",
                detail: format!("id column had {} bytes, expected 16", id_bytes.len()),
            }));
        }
    };

    Ok(Ok(Thought {
        id,
        text,
        created_at,
    }))
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

        let listed = store.list().unwrap();
        assert_eq!(listed, vec![thought]);
    }

    #[test]
    fn list_returns_newest_first() {
        let store = ThoughtStore::open_in_memory().unwrap();

        let first = store.create("one").unwrap();
        // The schema sorts by created_at DESC, then id DESC. A sleep here
        // ensures the millisecond timestamps differ so we exercise the
        // primary sort key rather than relying on the tiebreaker.
        sleep(Duration::from_millis(2));
        let second = store.create("two").unwrap();
        sleep(Duration::from_millis(2));
        let third = store.create("three").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed, vec![third, second, first]);
    }

    #[test]
    fn list_is_empty_for_fresh_store() {
        let store = ThoughtStore::open_in_memory().unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn unicode_text_round_trips() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let captured = store.create("水 🌊 思考 — émoji ok").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed, vec![captured]);
    }

    #[test]
    fn empty_text_is_allowed() {
        let store = ThoughtStore::open_in_memory().unwrap();
        let captured = store.create("").unwrap();
        assert_eq!(captured.text, "");
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
    fn created_at_is_recent_unix_ms() {
        let before = now_unix_millis();
        let store = ThoughtStore::open_in_memory().unwrap();
        let thought = store.create("x").unwrap();
        let after = now_unix_millis();

        assert!(thought.created_at >= before);
        assert!(thought.created_at <= after);
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
        let listed = store.list().unwrap();
        assert_eq!(listed, vec![original]);
    }

    #[test]
    fn migrate_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buoy.sqlite");

        {
            let _ = ThoughtStore::open(&path).unwrap();
        }
        // Re-opening must not error or duplicate the schema.
        let store = ThoughtStore::open(&path).unwrap();
        store.create("after second open").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn corrupt_id_blob_surfaces_as_corrupt_row() {
        let store = ThoughtStore::open_in_memory().unwrap();
        // Insert a row directly with a malformed id (wrong length).
        store
            .conn
            .execute(
                "INSERT INTO thoughts (id, text, created_at) VALUES (?1, ?2, ?3)",
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
