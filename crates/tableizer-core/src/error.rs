//! Engine error type.
//!
//! Malformed *data* is deliberately NOT an error here: it is parsed leniently and flagged for the
//! UI to surface (see [`crate::parse`]). These variants are I/O and structural failures only.

/// Errors surfaced by the engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An underlying I/O failure (read, seek, mmap, sidecar write).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A persisted sidecar artifact (offset index, sort permutation) is stale or corrupt relative
    /// to its source file and must be rebuilt.
    #[error("stale or invalid cache artifact: {0}")]
    StaleArtifact(String),

    /// The operation was cancelled via its [`crate::CancellationToken`].
    #[error("operation cancelled")]
    Cancelled,

    /// A user-supplied search pattern (regex) failed to compile.
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
}

/// Convenience alias for engine results.
pub type Result<T> = std::result::Result<T, Error>;
