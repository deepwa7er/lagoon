//! Types for cross-device sync.
//!
//! buoy syncs against a server-authoritative store (the VPS): each client pushes
//! its locally-modified rows and pulls the server's changes since a cursor,
//! reconciling by last-writer-wins on `updated_at`. Deletes propagate as
//! tombstones (`deleted_at = Some`). See `ThoughtStore::changes_since`,
//! `pending_changes`, `apply_remote`, and `mark_synced`.

use uuid::Uuid;

/// A keyset cursor into the change feed, ordered by `(updated_at, id)` — the
/// same total-order trick the stream's pagination cursor uses, so a sync can
/// resume after an exact row even when several share a millisecond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncCursor {
    pub updated_at: i64,
    pub id: Uuid,
}

/// The full state of one thought for sync purposes, including tombstones.
///
/// Unlike [`crate::Thought`] (a read-model with a *derived* `is_settled`), this
/// carries the raw persisted columns — `settled_at` and `deleted_at` — so a
/// remote can be applied byte-for-byte. A tombstone has `deleted_at = Some`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThoughtChange {
    pub id: Uuid,
    pub text: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub settled_at: Option<i64>,
    pub deleted_at: Option<i64>,
    pub actioned_at: Option<i64>,
}

impl SyncCursor {
    /// The cursor positioned just past `change` — i.e. its `(updated_at, id)`.
    #[must_use]
    pub fn after(change: &ThoughtChange) -> Self {
        Self {
            updated_at: change.updated_at,
            id: change.id,
        }
    }
}
