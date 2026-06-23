//! The byte source backing a file-based [`crate::ViewportSource`]: either owned in-memory bytes or a
//! memory-mapped file. Both deref to `&[u8]`, so the readers (CSV, NDJSON, Parquet) stay agnostic to
//! which it is. Cheap to clone — a background index builder holds its own `Arc` to keep the bytes
//! alive while it runs.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

use crate::Result;

/// Owned bytes or a memory-mapped file. Shared by every file-backed table.
#[derive(Clone)]
pub(crate) enum Source {
    Bytes(Arc<[u8]>),
    Mmap(Arc<Mmap>),
}

impl Source {
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Source::Bytes(b) => b,
            Source::Mmap(m) => m,
        }
    }
}

/// Memory-map a file read-only.
#[allow(unsafe_code)] // SAFETY justified at the `Mmap::map` call below.
pub(crate) fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path)?;
    // SAFETY: we map a read-only view of `file` and never mutate the mapping. Documented risk:
    // another process truncating the file could cause SIGBUS on later access (spec §4.2); Phase 0
    // accepts this — a SIGBUS guard / positioned-read fallback is planned.
    Ok(unsafe { Mmap::map(&file)? })
}
