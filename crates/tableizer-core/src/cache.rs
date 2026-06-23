//! Persistent index cache (`docs/architecture.md`): the offset index is written to the OS *state* dir —
//! never beside the source file — and reloaded on the next open if it is still valid (matching size +
//! mtime + dialect). A stale index would show *silently-wrong rows*, so validation is strict: any
//! mismatch (missing, stale, different dialect, corrupt) returns `None` and the index is rebuilt.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::index::{OffsetIndex, read_bytes, read_u8, read_u32, read_u64};
use crate::parse::Dialect;

const CACHE_MAGIC: &[u8; 4] = b"TZC1";
const CACHE_VERSION: u32 = 1;

/// The directory holding index caches: `$TABLEIZER_CACHE_DIR` if set, else the OS *state* dir
/// (Linux) or its local-data equivalent (macOS/Windows), under `tableizer/index-cache`. Never the
/// source file's directory.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TABLEIZER_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = directories::BaseDirs::new()?;
    let root = base.state_dir().unwrap_or_else(|| base.data_local_dir());
    Some(root.join("tableizer").join("index-cache"))
}

fn cache_file_in(dir: &Path, source: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    dir.join(format!("{:016x}.idx", hasher.finish()))
}

/// Source-file identity for invalidation: byte size + modification time.
struct Fingerprint {
    size: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
}

fn fingerprint(source: &Path) -> Option<Fingerprint> {
    let meta = fs::metadata(source).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Fingerprint {
        size: meta.len(),
        mtime_secs: mtime.as_secs(),
        mtime_nanos: mtime.subsec_nanos(),
    })
}

/// Load a cached index for `source` built with `dialect`, if one exists and is still valid. Returns
/// `None` on any mismatch — never a wrong index.
pub fn load(source: &Path, dialect: &Dialect) -> Option<OffsetIndex> {
    load_in(&cache_dir()?, source, dialect)
}

/// Save `index` (built from `source` with `dialect`) to the cache. Best-effort: a failed write just
/// means the next open rebuilds.
pub fn save(source: &Path, dialect: &Dialect, index: &OffsetIndex) {
    if let Some(dir) = cache_dir() {
        save_in(&dir, source, dialect, index);
    }
}

/// Total size of the cache directory, in bytes (for the cache-management UI).
pub fn total_size() -> u64 {
    let Some(dir) = cache_dir() else {
        return 0;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Delete all cached indexes.
pub fn clear() {
    if let Some(dir) = cache_dir() {
        let _ = fs::remove_dir_all(dir);
    }
}

fn load_in(dir: &Path, source: &Path, dialect: &Dialect) -> Option<OffsetIndex> {
    let fp = fingerprint(source)?;
    let bytes = fs::read(cache_file_in(dir, source)).ok()?;
    let mut pos = 0;
    if read_bytes(&bytes, &mut pos, 4)? != CACHE_MAGIC {
        return None;
    }
    if read_u32(&bytes, &mut pos)? != CACHE_VERSION {
        return None;
    }
    let size = read_u64(&bytes, &mut pos)?;
    let mtime_secs = read_u64(&bytes, &mut pos)?;
    let mtime_nanos = read_u32(&bytes, &mut pos)?;
    let delimiter = read_u8(&bytes, &mut pos)?;
    let quote = read_u8(&bytes, &mut pos)?;
    // Strict validation: a stale or differently-parsed index must NOT be reused.
    if size != fp.size
        || mtime_secs != fp.mtime_secs
        || mtime_nanos != fp.mtime_nanos
        || delimiter != dialect.delimiter
        || quote != dialect.quote
    {
        return None;
    }
    OffsetIndex::deserialize(&bytes[pos..])
}

fn save_in(dir: &Path, source: &Path, dialect: &Dialect, index: &OffsetIndex) {
    let Some(fp) = fingerprint(source) else {
        return;
    };
    let mut out = Vec::new();
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    out.extend_from_slice(&fp.size.to_le_bytes());
    out.extend_from_slice(&fp.mtime_secs.to_le_bytes());
    out.extend_from_slice(&fp.mtime_nanos.to_le_bytes());
    out.push(dialect.delimiter);
    out.push(dialect.quote);
    out.extend_from_slice(&index.serialize());
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(cache_file_in(dir, source), out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn build_index(data: &[u8], dialect: &Dialect) -> OffsetIndex {
        OffsetIndex::build(data, dialect).unwrap()
    }

    #[test]
    fn saves_and_reloads_a_valid_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = tempfile::NamedTempFile::new().unwrap();
        src.write_all(b"a,b\nc,d\ne,f\n").unwrap();
        let dialect = Dialect::default();
        let index = build_index(b"a,b\nc,d\ne,f\n", &dialect);

        save_in(dir.path(), src.path(), &dialect, &index);
        let loaded = load_in(dir.path(), src.path(), &dialect).expect("cache hit");

        assert_eq!(loaded.row_count(), index.row_count());
    }

    #[test]
    fn rejects_a_cache_when_the_file_changed() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = tempfile::NamedTempFile::new().unwrap();
        src.write_all(b"a,b\nc,d\n").unwrap();
        let dialect = Dialect::default();
        save_in(
            dir.path(),
            src.path(),
            &dialect,
            &build_index(b"a,b\nc,d\n", &dialect),
        );

        // Appending changes the file size (and mtime) → the cached index must be treated as stale.
        src.write_all(b"e,f\n").unwrap();
        src.flush().unwrap();

        assert!(
            load_in(dir.path(), src.path(), &dialect).is_none(),
            "a stale cache must never be reused"
        );
    }

    #[test]
    fn rejects_a_cache_built_with_a_different_dialect() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = tempfile::NamedTempFile::new().unwrap();
        src.write_all(b"a;b\nc;d\n").unwrap();
        let comma = Dialect::default();
        save_in(
            dir.path(),
            src.path(),
            &comma,
            &build_index(b"a;b\nc;d\n", &comma),
        );

        let semicolon = Dialect {
            delimiter: b';',
            ..Dialect::default()
        };
        assert!(
            load_in(dir.path(), src.path(), &semicolon).is_none(),
            "an index built for a different delimiter must miss"
        );
    }
}
