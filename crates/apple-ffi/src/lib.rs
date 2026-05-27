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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use buoy_core::{Error as CoreError, Thought as CoreThought, ThoughtStore as CoreStore};

uniffi::setup_scaffolding!();

/// Swift-facing thought record. `id` is the UUID as a lowercase hyphenated
/// string; `created_at` is milliseconds since the epoch (1970-01-01 UTC).
#[derive(uniffi::Record)]
pub struct Thought {
    pub id: String,
    pub text: String,
    pub created_at: i64,
}

impl From<CoreThought> for Thought {
    fn from(value: CoreThought) -> Self {
        Self {
            id: value.id.to_string(),
            text: value.text,
            created_at: value.created_at,
        }
    }
}

/// Errors surfaced to Swift. `UniFFI` maps each variant to a case on a Swift
/// `Error` enum so callers can pattern-match in `catch` blocks.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("storage error: {message}")]
    Storage { message: String },
}

impl From<CoreError> for FfiError {
    fn from(value: CoreError) -> Self {
        Self::Storage {
            message: value.to_string(),
        }
    }
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

    /// Return every stored thought, newest first.
    pub fn list(&self) -> Result<Vec<Thought>, FfiError> {
        let guard = self.inner.lock().expect("ThoughtStore mutex poisoned");
        Ok(guard.list()?.into_iter().map(Into::into).collect())
    }
}
