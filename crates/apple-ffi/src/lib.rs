//! Apple-platform FFI surface for Buoy.
//!
//! Wraps `buoy-core` types with `UniFFI` proc macros so that the iOS and
//! macOS apps can call into the core through generated Swift bindings.
//! The Linux GTK app does not depend on this crate — it uses `buoy-core`
//! directly without any FFI.
//!
//! `UniFFI`'s generated scaffolding uses raw pointers and unsafe FFI; we
//! relax the workspace `unsafe_code` lint locally because FFI inherently
//! requires it.

#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use buoy_core::{
    Cursor as CoreCursor, Error as CoreError, MatchRange as CoreMatchRange, MiniLmEmbedder,
    Page as CorePage, Thought as CoreThought, ThoughtMatch as CoreThoughtMatch,
    ThoughtStore as CoreStore,
};
use uuid::Uuid;

uniffi::setup_scaffolding!();

/// Page size the platform UIs use when they have no more specific need.
/// Mirrors `buoy_core::DEFAULT_PAGE_SIZE` so Swift and Rust callers agree.
#[uniffi::export]
#[must_use]
pub fn default_page_size() -> u32 {
    u32::try_from(buoy_core::DEFAULT_PAGE_SIZE).expect("page size fits in u32")
}

/// Swift-facing thought record. `id` is the UUID as a lowercase hyphenated
/// string; timestamps are milliseconds since the epoch (1970-01-01 UTC).
#[derive(uniffi::Record)]
pub struct Thought {
    pub id: String,
    pub text: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// True when this thought has settled — subsequent edits will create
    /// edit-history entries rather than silently overwriting.
    pub is_settled: bool,
}

impl From<CoreThought> for Thought {
    fn from(value: CoreThought) -> Self {
        Self {
            id: value.id.to_string(),
            text: value.text,
            created_at: value.created_at,
            updated_at: value.updated_at,
            is_settled: value.is_settled,
        }
    }
}

/// Keyset pagination cursor pointing just past one specific thought. Opaque
/// to Swift: callers receive it from one page and hand it back unchanged to
/// fetch the next.
#[derive(uniffi::Record)]
pub struct Cursor {
    pub created_at: i64,
    pub id: String,
}

impl From<CoreCursor> for Cursor {
    fn from(value: CoreCursor) -> Self {
        Self {
            created_at: value.created_at,
            id: value.id.to_string(),
        }
    }
}

/// One page of thoughts, newest first. `next_cursor` is present when more
/// older thoughts exist after this page.
#[derive(uniffi::Record)]
pub struct Page {
    pub thoughts: Vec<Thought>,
    pub next_cursor: Option<Cursor>,
}

impl From<CorePage> for Page {
    fn from(value: CorePage) -> Self {
        Self {
            thoughts: value.thoughts.into_iter().map(Into::into).collect(),
            next_cursor: value.next_cursor.map(Into::into),
        }
    }
}

/// A half-open byte range (UTF-8 offsets) into a [`ThoughtMatch`] snippet
/// covering one matched term.
#[derive(uniffi::Record)]
pub struct MatchRange {
    pub start: u64,
    pub len: u64,
}

impl From<CoreMatchRange> for MatchRange {
    fn from(value: CoreMatchRange) -> Self {
        Self {
            start: u64::try_from(value.start).expect("offset fits in u64"),
            len: u64::try_from(value.len).expect("length fits in u64"),
        }
    }
}

/// One ranked full-text search result: the thought, a contextual snippet
/// of its text, and highlight ranges into that snippet.
#[derive(uniffi::Record)]
pub struct ThoughtMatch {
    pub thought: Thought,
    pub snippet: String,
    pub ranges: Vec<MatchRange>,
}

impl From<CoreThoughtMatch> for ThoughtMatch {
    fn from(value: CoreThoughtMatch) -> Self {
        Self {
            thought: value.thought.into(),
            snippet: value.snippet,
            ranges: value.ranges.into_iter().map(Into::into).collect(),
        }
    }
}

/// Errors surfaced to Swift. `UniFFI` maps each variant to a case on a Swift
/// `Error` enum so callers can pattern-match in `catch` blocks.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("storage error: {message}")]
    Storage { message: String },
    #[error("invalid id: {message}")]
    InvalidId { message: String },
    #[error("not found")]
    NotFound,
    #[error("embedding error: {message}")]
    Embedding { message: String },
}

impl From<CoreError> for FfiError {
    fn from(value: CoreError) -> Self {
        match value {
            CoreError::NotFound { .. } => Self::NotFound,
            embedding @ (CoreError::ModelLoad { .. }
            | CoreError::Embedding { .. }
            | CoreError::NoEmbedder) => Self::Embedding {
                message: embedding.to_string(),
            },
            other => Self::Storage {
                message: other.to_string(),
            },
        }
    }
}

fn parse_id(id: &str) -> Result<Uuid, FfiError> {
    Uuid::parse_str(id).map_err(|err| FfiError::InvalidId {
        message: err.to_string(),
    })
}

/// Swift-facing wrapper around the core `ThoughtStore`.
///
/// The core store holds a single `rusqlite::Connection`, which is `Send` but
/// not `Sync`. `UniFFI` requires exported objects to be both, so we serialise
/// access through a `Mutex`. This is the right boundary — the core stays
/// lean for the Linux GTK app, and the FFI layer pays the synchronisation
/// cost only where it is actually needed.
#[derive(uniffi::Object)]
pub struct ThoughtStore {
    inner: Mutex<CoreStore>,
}

#[uniffi::export]
impl ThoughtStore {
    /// Open or create the store at `path`. The platform layer supplies the
    /// path (iOS sandbox dir, macOS Application Support, etc.).
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Self>, FfiError> {
        let inner = CoreStore::open(&PathBuf::from(path))?;
        Ok(Arc::new(Self {
            inner: Mutex::new(inner),
        }))
    }

    /// Capture a new thought.
    pub fn create(&self, text: &str) -> Result<Thought, FfiError> {
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard.create(text)?.into())
    }

    /// Replace the text of an existing thought. If the thought is settled
    /// at the moment of the edit, the prior text is captured into the
    /// edit history before the update lands.
    pub fn update(&self, id: &str, text: &str) -> Result<Thought, FfiError> {
        let uuid = parse_id(id)?;
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard.update_thought(uuid, text)?.into())
    }

    /// Delete a thought and its edit history.
    pub fn delete(&self, id: &str) -> Result<(), FfiError> {
        let uuid = parse_id(id)?;
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        guard.delete_thought(uuid)?;
        Ok(())
    }

    /// Force every currently-live thought into the settled state. The
    /// platform layer calls this when the app moves to the background,
    /// so a returning user's next edit is treated as a deliberate
    /// modification rather than a continuation of the live session.
    pub fn settle_all_live(&self) -> Result<(), FfiError> {
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        guard.settle_all_live()?;
        Ok(())
    }

    /// Return every stored thought, newest first.
    pub fn list(&self) -> Result<Vec<Thought>, FfiError> {
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard.list()?.into_iter().map(Into::into).collect())
    }

    /// Return one page of thoughts, newest first. `before` is the cursor
    /// returned by the previous page, or nil to start at the newest.
    pub fn list_paginated(&self, before: Option<Cursor>, limit: u32) -> Result<Page, FfiError> {
        let before = before
            .map(|cursor| {
                Ok::<_, FfiError>(CoreCursor {
                    created_at: cursor.created_at,
                    id: parse_id(&cursor.id)?,
                })
            })
            .transpose()?;
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard.list_paginated(before, limit as usize)?.into())
    }

    /// Combined keyword + semantic search, best matches first. `query` is
    /// raw user input; the final word matches as a prefix on the keyword
    /// side so results stay useful while the user is still typing.
    /// Degrades to keyword-only when no embedder is attached.
    pub fn search_combined(&self, query: &str, limit: u32) -> Result<Vec<ThoughtMatch>, FfiError> {
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard
            .search_combined(query, limit as usize)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Related thoughts for an in-progress draft (the suggestion strip).
    /// `exclude_id` drops the thought currently being edited. Returns
    /// empty with a blank draft or no attached embedder — suggestions are
    /// an enhancement, never an error the composer must handle. Callers
    /// debounce ~200ms after the last keystroke.
    pub fn find_related(
        &self,
        draft_text: &str,
        top_k: u32,
        exclude_id: Option<String>,
    ) -> Result<Vec<ThoughtMatch>, FfiError> {
        let exclude = exclude_id.map(|id| parse_id(&id)).transpose()?;
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard
            .find_related(draft_text, top_k as usize, exclude)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Related thoughts for an existing thought, ranked by its stored
    /// vector. Empty when the thought has no vector yet.
    pub fn find_related_to(&self, id: &str, top_k: u32) -> Result<Vec<ThoughtMatch>, FfiError> {
        let uuid = parse_id(id)?;
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard
            .find_related_to(uuid, top_k as usize)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Load the embedding model from `model_dir` and attach it to the
    /// store. Loading takes a few hundred milliseconds — call from a
    /// background task, then drain `embed_missing`.
    pub fn attach_embedder(&self, model_dir: &str) -> Result<(), FfiError> {
        let embedder = MiniLmEmbedder::load(Path::new(model_dir))?;
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        guard.set_embedder(Box::new(embedder));
        Ok(())
    }

    /// Embed up to `limit` thoughts that have no stored vector yet.
    /// Returns how many were embedded; call repeatedly until it returns 0.
    /// Keep `limit` small — the store lock is held while embedding.
    pub fn embed_missing(&self, limit: u32) -> Result<u32, FfiError> {
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        let count = guard.embed_missing(limit as usize)?;
        Ok(u32::try_from(count).expect("count bounded by u32 limit"))
    }
}
