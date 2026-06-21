use uuid::Uuid;

/// A single thought captured by the user.
///
/// Thoughts are immutable in identity (`id`, `created_at`) but their `text`
/// can be edited. While a thought is "live" — recently captured or recently
/// touched — edits silently overwrite. Once a thought settles (either after
/// `SETTLE_WINDOW_MS` of inactivity or when the app explicitly forces it),
/// subsequent edits archive the previous text into the edit history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thought {
    pub id: Uuid,
    pub text: String,
    /// Milliseconds since the epoch (1970-01-01 UTC) at which the thought
    /// was first captured. Never changes after creation.
    pub created_at: i64,
    /// Milliseconds since the epoch at which `text` was last modified.
    /// Equal to `created_at` for thoughts that have never been edited.
    pub updated_at: i64,
    /// Whether this thought is currently settled — i.e. subsequent edits
    /// will create edit-history entries rather than silently overwriting.
    /// Derived from `settled_at IS NOT NULL OR (now - updated_at > window)`.
    pub is_settled: bool,
    /// Whether this thought has been marked actioned (dealt with). Derived from
    /// `actioned_at IS NOT NULL`.
    pub is_actioned: bool,
}

/// A historical version of a thought captured when a settled thought is
/// edited. The text stored here is the *prior* text — what the thought
/// said before the edit that triggered archiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditEntry {
    pub text: String,
    /// Milliseconds since the epoch at which this prior version was
    /// archived (i.e. when the edit that replaced it landed).
    pub archived_at: i64,
}
