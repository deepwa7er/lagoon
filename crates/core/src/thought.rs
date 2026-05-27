use uuid::Uuid;

/// A single thought captured by the user.
///
/// Thoughts are the only first-class entity in the Buoy data model. They
/// are append-only at capture time; later phases will introduce edit
/// history, tags, and sync metadata, but the identity (`id`) and creation
/// time (`created_at`) never change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thought {
    pub id: Uuid,
    pub text: String,
    /// Milliseconds since the epoch (1970-01-01 UTC) at which the thought
    /// was first captured.
    pub created_at: i64,
}
