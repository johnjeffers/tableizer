//! Transparent gzip support (`docs/architecture.md` § I/O).
//!
//! Gzip is a *streaming* format with no random access, which is incompatible with the offset index's
//! seek-and-parse-forward model. So — exactly like a remote object (`crate::remote`) — a gzipped file
//! is **decompressed once to a local cache file** in the OS state dir, and that seekable file is what
//! the engine opens: index, view, sort, search, and export all work unchanged. Byte fidelity holds on
//! the decompressed content (gzip decode is lossless and deterministic; the wrapper is transport, not
//! part of the data's identity).
//!
//! The decompressed copy is keyed by the source's `{path, size, mtime}` and reused on reopen; a
//! changed source re-decompresses. Multi-member streams (concatenated gzip, e.g. from `pigz`/`bgzip`)
//! are read in full via [`MultiGzDecoder`].

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::read::MultiGzDecoder;

use crate::{CancellationToken, Error, Result};

/// The gzip magic number (`1f 8b`).
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Whether `path` is gzip-compressed, by its leading magic bytes (authoritative — an extension may
/// lie, e.g. a gzipped file named `.csv`, or a `.gz` that isn't).
pub fn is_gzip(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok_and(|()| magic == GZIP_MAGIC)
}

/// Directory holding decompressed copies: `$TABLEIZER_CACHE_DIR/decompressed` if set, else the OS
/// *state* dir (Linux) / local-data equivalent under `tableizer/decompressed`. Never beside the
/// source file.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TABLEIZER_CACHE_DIR") {
        return Some(PathBuf::from(dir).join("decompressed"));
    }
    let base = directories::BaseDirs::new()?;
    let root = base.state_dir().unwrap_or_else(|| base.data_local_dir());
    Some(root.join("tableizer").join("decompressed"))
}

/// Decompress the gzipped `src` into `cache_root`, returning the local path of the decompressed file.
/// A still-valid decompressed copy (same source size + mtime) is reused. `progress`/`total` report
/// **compressed** bytes consumed (so the bar reaches 100% regardless of the unknown output size), and
/// `cancel` aborts cleanly, leaving no partial file behind.
pub fn decompress_to_cache(
    src: &Path,
    cache_root: &Path,
    progress: &AtomicU64,
    total: &AtomicU64,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let meta = std::fs::metadata(src)?;
    total.store(meta.len(), Ordering::Relaxed);
    let dest = cache_root.join(cache_name(src, &meta));
    if dest.exists() {
        progress.store(meta.len(), Ordering::Relaxed); // already decompressed
        return Ok(dest);
    }
    std::fs::create_dir_all(cache_root)?;

    let reader = CountingReader {
        inner: BufReader::new(File::open(src)?),
        read: progress,
    };
    let mut decoder = MultiGzDecoder::new(reader);
    // Decompress to a temp file, then atomically rename in — a cancelled/failed run never leaves a
    // half-written file that a later open would treat as complete.
    let tmp = dest.with_extension("partial");
    let mut out = BufWriter::new(File::create(&tmp)?);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.is_cancelled() {
            drop(out);
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Cancelled);
        }
        let n = decoder.read(&mut buf).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("gzip decode failed: {e}"),
            ))
        })?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    out.flush()?;
    drop(out);
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Total size of the decompressed cache, in bytes (for the cache-management UI).
pub fn total_size() -> u64 {
    let Some(dir) = cache_dir() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Delete all decompressed copies.
pub fn clear() {
    if let Some(dir) = cache_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The cache filename for `src`: a stable hash of the path plus size + mtime (so a changed source
/// invalidates), keeping the *inner* extension (`data.csv.gz` → `…​.csv`) so format detection on the
/// decompressed copy still works.
fn cache_name(src: &Path, meta: &std::fs::Metadata) -> String {
    let hash = crate::stable_hash(src);
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    match inner_extension(src) {
        Some(ext) => format!("{hash:016x}-{size}-{mtime}.{ext}"),
        None => format!("{hash:016x}-{size}-{mtime}"),
    }
}

/// The extension beneath the `.gz` suffix (`logs.tsv.gz` → `tsv`), if any.
fn inner_extension(src: &Path) -> Option<String> {
    let name = src.file_name()?.to_str()?;
    let inner = name
        .strip_suffix(".gz")
        .or_else(|| name.strip_suffix(".GZ"))
        .unwrap_or(name);
    let ext = Path::new(inner).extension()?.to_str()?;
    (!ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Counts bytes pulled from the underlying reader (compressed bytes consumed), for progress.
struct CountingReader<'a, R> {
    inner: R,
    read: &'a AtomicU64,
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    /// Write `data` gzipped to a temp file with the given suffix; return the file (kept alive).
    fn gzip_temp(suffix: &str, data: &[u8]) -> tempfile::NamedTempFile {
        let file = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
        let mut encoder =
            GzEncoder::new(File::create(file.path()).unwrap(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap();
        file
    }

    #[test]
    fn is_gzip_detects_by_magic_not_extension() {
        // Gzipped content in a `.csv`-named file is still recognised.
        let gz = gzip_temp(".csv", b"a,b\n1,2\n");
        assert!(is_gzip(gz.path()));
        // Plain content is not gzip, whatever the name.
        let plain = tempfile::Builder::new().suffix(".gz").tempfile().unwrap();
        std::fs::write(plain.path(), b"name,age\nbob,30\n").unwrap();
        assert!(!is_gzip(plain.path()));
    }

    #[test]
    fn decompress_round_trips_and_reuses_the_cache() {
        let original = b"id,name\n1,alice\n2,bob\n3,carol\n";
        let gz = gzip_temp(".csv.gz", original);
        let cache = tempfile::tempdir().unwrap();
        let progress = AtomicU64::new(0);
        let total = AtomicU64::new(0);
        let cancel = CancellationToken::new();

        let out = decompress_to_cache(gz.path(), cache.path(), &progress, &total, &cancel).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), original);
        assert!(out.starts_with(cache.path()));
        // The inner `.csv` extension is preserved so format detection still works.
        assert_eq!(out.extension().and_then(|e| e.to_str()), Some("csv"));
        // Progress reached the compressed size.
        assert_eq!(
            progress.load(Ordering::Relaxed),
            total.load(Ordering::Relaxed)
        );
        assert!(total.load(Ordering::Relaxed) > 0);

        // A second call reuses the same cached file.
        let again =
            decompress_to_cache(gz.path(), cache.path(), &progress, &total, &cancel).unwrap();
        assert_eq!(again, out);
    }

    #[test]
    fn inner_extension_strips_the_gz_suffix() {
        assert_eq!(
            inner_extension(Path::new("/d/logs.tsv.gz")).as_deref(),
            Some("tsv")
        );
        assert_eq!(
            inner_extension(Path::new("/d/data.csv.GZ")).as_deref(),
            Some("csv")
        );
        assert_eq!(inner_extension(Path::new("/d/archive.gz")), None);
    }
}
