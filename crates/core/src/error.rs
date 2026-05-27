use std::path::PathBuf;

/// Errors produced by the Buoy core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying `SQLite` operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    /// The database file could not be opened at the requested path.
    #[error("could not open database at {path}: {source}")]
    OpenDatabase {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    /// A stored row contained data the application could not interpret.
    /// Indicates corruption or an out-of-sync schema, not a user error.
    #[error("corrupt row in `{table}`: {detail}")]
    CorruptRow { table: &'static str, detail: String },
}

pub type Result<T> = std::result::Result<T, Error>;
