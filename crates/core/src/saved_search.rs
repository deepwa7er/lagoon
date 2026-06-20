use uuid::Uuid;

/// A named, re-runnable query the user has pinned.
///
/// `query` is the raw query text the user saved — free text (run through
/// combined search) or a `#tag` (run as a tag filter); the client routes it the
/// same way the search box does. Saved searches are **local to each store**
/// (not part of the thought sync feed), so a device's pinned queries stay on
/// that device for now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSearch {
    pub id: Uuid,
    pub name: String,
    pub query: String,
    /// Milliseconds since the epoch at which the search was saved.
    pub created_at: i64,
}
