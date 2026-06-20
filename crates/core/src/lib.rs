//! Buoy core library.
//!
//! Shared business logic for the Buoy note-taking app, consumed by the iOS,
//! macOS, and web clients. See `docs/plan-poc.md` and `docs/plan-buildout.md`
//! at the repository root for design and roadmap.

mod embed;
mod error;
mod saved_search;
mod search;
mod store;
mod sync;
mod tags;
mod thought;

pub use embed::{EMBEDDING_DIM, MiniLmEmbedder, TextEmbedder};
pub use error::{Error, Result};
pub use saved_search::SavedSearch;
pub use search::{MatchRange, ThoughtMatch};
pub use store::{Cursor, DEFAULT_PAGE_SIZE, Page, ThoughtStore};
pub use sync::{SyncCursor, ThoughtChange};
pub use tags::parse_tags;
pub use thought::{EditEntry, Thought};
