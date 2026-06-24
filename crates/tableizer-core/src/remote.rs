//! Opening files from cloud object storage (`docs/architecture.md` § I/O).
//!
//! Multi-cloud by design: every backend (S3 / GCS / Azure / HTTP) is reached through the
//! `object_store` crate, which is *pure I/O* — it sources bytes by ranged GET and is **not** a query
//! or sort engine, so reusing it does not touch the "the external sort is ours" invariant.
//!
//! This first cut is **download-to-cache**: a remote object is fetched once to the OS state dir and
//! then opened exactly like a local file, so the whole engine (index, view, sort, search, export,
//! index persistence) works unchanged. The streaming `ReadAt` source — first screen from a head
//! fetch, random access by ranged GET, no full download — is the documented next step and reuses this
//! same `object_store` seam and cache directory.
//!
//! `object_store` is async; the engine is synchronous, so the async work is confined here behind a
//! per-call current-thread runtime (`block_on`).
//!
//! **Credentials/config.** `object_store::parse_url` builds a *blank* backend (no environment read),
//! so authentication is supplied explicitly via the `options` passed to [`fetch_to_cache`] (merged
//! over the process environment). For S3 these options come from one of two sources: explicit static
//! keys from the in-app form, or — the default — [`aws_credentials`], which runs the full `aws-config`
//! provider chain (environment, `~/.aws` profiles, **SSO**, assume-role, EC2/ECS roles). That is how
//! SSO works: `object_store` has no SSO support, so we resolve the temporary credentials ourselves and
//! hand them in as static options.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt};
use url::Url;

use crate::{CancellationToken, Error, Result};

/// Per-bucket S3 region cache. S3 buckets are region-specific, but one credential set (e.g. an SSO
/// role) can read buckets in many regions — so we discover each bucket's region once and reuse it.
fn region_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The bucket name of an S3 URL (`s3://bucket/key` → `bucket`), else `None`.
fn s3_bucket(url: &Url) -> Option<String> {
    matches!(url.scheme(), "s3" | "s3a")
        .then(|| url.host_str().map(str::to_owned))
        .flatten()
}

/// Discover `bucket`'s region via S3 `HeadBucket` (which the SDK resolves across regions, returning the
/// bucket's real region) and cache it. Credentials come from the already-resolved `options`, so no
/// extra chain/SSO resolution. `None` on any failure — the caller then falls back to the configured
/// region, so a same-region bucket is unaffected. This is the in-app equivalent of passing `--region`.
async fn resolve_bucket_region(bucket: &str, options: &[(String, String)]) -> Option<String> {
    if let Some(region) = region_cache().lock().expect("region cache").get(bucket) {
        return Some(region.clone());
    }
    let opt = |key: &str| {
        options
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    // Reuse aws-config only for the runtime plumbing (HTTP client, sleep); credentials are taken from
    // `options` (static keys, or chain/SSO-resolved upstream) so SSO isn't resolved again here.
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"));
    if let (Some(key), Some(secret)) = (opt("aws_access_key_id"), opt("aws_secret_access_key")) {
        loader = loader.credentials_provider(aws_credential_types::Credentials::new(
            key,
            secret,
            opt("aws_session_token"),
            None,
            "tableizer-resolved",
        ));
    }
    let sdk = loader.load().await;
    let client = aws_sdk_s3::Client::from_conf(aws_sdk_s3::config::Builder::from(&sdk).build());
    // A bucket outside the request region answers HeadBucket with a 301 carrying the real region in
    // the `x-amz-bucket-region` header (NOT a `Location` header) — which the SDK surfaces as an error.
    // Read the region from either the success output (same region) or that redirect error's headers,
    // so cross-region buckets resolve without the SDK having to follow the redirect.
    let region = match client.head_bucket().bucket(bucket).send().await {
        Ok(output) => output.bucket_region().map(str::to_owned),
        Err(error) => error
            .raw_response()
            .and_then(|response| response.headers().get("x-amz-bucket-region"))
            .map(str::to_owned),
    }?;
    region_cache()
        .lock()
        .expect("region cache")
        .insert(bucket.to_string(), region.clone());
    Some(region)
}

/// Build the `object_store` for `url`, first resolving an AWS bucket's actual region and overriding it
/// in the options (a bucket in a non-default region then "just works", as `--region` would on the CLI).
/// Skipped for non-S3 URLs and for S3-compatible custom endpoints (single-region).
async fn build_store(
    url: &Url,
    options: &[(String, String)],
) -> Result<(Box<dyn ObjectStore>, object_store::path::Path)> {
    let mut merged = merge_options(std::env::vars(), options);
    if let Some(bucket) = s3_bucket(url)
        && !options.iter().any(|(k, _)| k == "aws_endpoint")
        && let Some(region) = resolve_bucket_region(&bucket, options).await
    {
        merged.push(("aws_region".to_string(), region)); // applied last → wins over the configured one
    }
    object_store::parse_url_opts(url, merged)
        .map_err(|e| Error::Remote(format!("unsupported location: {e}")))
}

/// One entry in a remote directory listing: a child "folder" (common prefix) or an object (file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Full URL to navigate into (folder) or open (file).
    pub url: String,
    /// Display name — the last path segment.
    pub name: String,
    /// Whether this is a navigable prefix (`true`) or a file (`false`).
    pub is_dir: bool,
    /// Object size in bytes (files only).
    pub size: Option<u64>,
}

/// The immediate children of a remote prefix — folders first, then files (each sorted by name) — for
/// the cloud file browser.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
}

/// URL schemes routed to a cloud object store. Anything else (a bare path, a Windows drive letter) is
/// treated as a local filesystem path by [`is_remote`].
const REMOTE_SCHEMES: &[&str] = &[
    "s3", "s3a", "gs", "gcs", "az", "azure", "adl", "abfs", "abfss", "http", "https",
];

/// Whether `target` names a remote object — a URL with a cloud scheme — rather than a local path.
/// A bare path (`/data/x.csv`, `rel/x.csv`) or a Windows drive path (`C:\..`, scheme `c`) is local.
pub fn is_remote(target: &str) -> bool {
    Url::parse(target).is_ok_and(|u| REMOTE_SCHEMES.contains(&u.scheme()))
}

/// Whether `target` is an S3 (or `s3a`) URL — the scheme that authenticates with AWS credentials, so
/// the only one for which [`aws_credentials`] applies.
pub fn is_s3(target: &str) -> bool {
    Url::parse(target).is_ok_and(|u| matches!(u.scheme(), "s3" | "s3a"))
}

/// Resolve AWS credentials through the full provider chain via `aws-config`: environment variables,
/// `~/.aws` config/credentials **profiles**, **SSO** (the `aws sso login` token cache exchanged for
/// temporary credentials via `GetRoleCredentials`), assume-role, and EC2/ECS roles — returned (with
/// the resolved region, if any) as `object_store` S3 options.
///
/// `object_store` has no SSO support of its own, so this is the bridge: resolve once here, then pass
/// the resulting *temporary* credentials to it as static options. `profile`/`region` override the
/// ambient defaults (e.g. `AWS_PROFILE`). Errors carry an SSO-login hint, since an expired SSO token
/// is the common failure.
pub fn aws_credentials(
    profile: Option<&str>,
    region: Option<&str>,
) -> Result<Vec<(String, String)>> {
    use aws_credential_types::provider::ProvideCredentials;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(profile) = profile {
            loader = loader.profile_name(profile);
        }
        if let Some(region) = region {
            loader = loader.region(aws_config::Region::new(region.to_string()));
        }
        let config = loader.load().await;
        let provider = config.credentials_provider().ok_or_else(|| {
            Error::Remote(
                "no AWS credentials are configured (set up SSO, a profile, or keys)".into(),
            )
        })?;
        let creds = provider.provide_credentials().await.map_err(|e| {
            Error::Remote(format!(
                "AWS credential resolution failed — for SSO, run `aws sso login` first: {e}"
            ))
        })?;
        let mut options = vec![
            (
                "aws_access_key_id".to_string(),
                creds.access_key_id().to_string(),
            ),
            (
                "aws_secret_access_key".to_string(),
                creds.secret_access_key().to_string(),
            ),
        ];
        if let Some(token) = creds.session_token() {
            options.push(("aws_session_token".to_string(), token.to_string()));
        }
        if let Some(region) = config.region() {
            options.push(("aws_region".to_string(), region.to_string()));
        }
        Ok(options)
    })
}

/// S3 connection parameters for **bucket discovery** ([`list_s3_buckets`]). Mirrors the Settings
/// form: an AWS profile/region for the chain (incl. SSO), or static keys + endpoint for an
/// S3-compatible store. When `access_key_id`/`secret_access_key` are set they override the chain.
#[derive(Clone, Debug, Default)]
pub struct S3Auth {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    /// Custom endpoint for S3-compatible stores (MinIO/R2); forces path-style addressing.
    pub endpoint: Option<String>,
}

/// Discover the buckets reachable with the given credentials via the S3 `ListBuckets` API, as a
/// [`DirListing`] of navigable folders (`s3://<bucket>/`). `object_store` is bucket-scoped and cannot
/// enumerate buckets, so this goes through `aws-sdk-s3` — which shares the `aws-config` provider chain,
/// so SSO/profiles/roles work the same as elsewhere. Static keys (when set) override the chain; a
/// custom endpoint targets an S3-compatible store.
pub fn list_s3_buckets(auth: &S3Auth) -> Result<DirListing> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(profile) = &auth.profile {
            loader = loader.profile_name(profile);
        }
        if let Some(region) = &auth.region {
            loader = loader.region(aws_config::Region::new(region.clone()));
        }
        if let (Some(key), Some(secret)) = (&auth.access_key_id, &auth.secret_access_key) {
            let creds = aws_credential_types::Credentials::new(
                key.clone(),
                secret.clone(),
                auth.session_token.clone(),
                None,
                "tableizer-static",
            );
            loader = loader.credentials_provider(creds);
        }
        let sdk = loader.load().await;

        let mut s3 = aws_sdk_s3::config::Builder::from(&sdk);
        // ListBuckets must be signed for a region; fall back to us-east-1 if none was resolved.
        if sdk.region().is_none() {
            s3 = s3.region(aws_config::Region::new("us-east-1"));
        }
        if let Some(endpoint) = auth.endpoint.as_deref().filter(|e| !e.is_empty()) {
            s3 = s3.endpoint_url(endpoint).force_path_style(true);
        }
        let client = aws_sdk_s3::Client::from_conf(s3.build());
        let output = client.list_buckets().send().await.map_err(|e| {
            Error::Remote(format!(
                "could not list buckets — for SSO, run `aws sso login` first: {e}"
            ))
        })?;
        let names = output
            .buckets()
            .iter()
            .filter_map(|b| b.name().map(str::to_owned));
        Ok(buckets_to_listing(names))
    })
}

/// Build a [`DirListing`] of bucket folders (`s3://<name>/`) from bucket names, sorted by name. Pure
/// (so it is unit-tested apart from the live `ListBuckets` call).
fn buckets_to_listing(names: impl IntoIterator<Item = String>) -> DirListing {
    let mut entries: Vec<DirEntry> = names
        .into_iter()
        .map(|name| DirEntry {
            url: format!("s3://{name}/"),
            name,
            is_dir: true,
            size: None,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    DirListing { entries }
}

/// List the immediate children of the remote prefix `location` (a bucket root like `s3://bucket` or a
/// prefix like `s3://bucket/data/`) for the file browser: child prefixes become navigable folders and
/// objects become files, each carrying a full URL. `options` supply credentials/config exactly as in
/// [`fetch_to_cache`]. Folders are listed before files, each group sorted by name.
pub fn list_dir(
    location: &str,
    options: &[(String, String)],
    cancel: &CancellationToken,
) -> Result<DirListing> {
    let url = Url::parse(location).map_err(|e| Error::Remote(format!("invalid URL: {e}")))?;
    // The bucket/host root to rebuild absolute URLs from the store-relative keys the listing returns.
    let base = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let (store, prefix) = build_store(&url, options).await?;
        // `list_with_delimiter` returns the prefix's immediate children: common prefixes (folders) and
        // objects (files), rather than a full recursive listing.
        let prefix = (!prefix.as_ref().is_empty()).then_some(prefix);
        let result = store
            .list_with_delimiter(prefix.as_ref())
            .await
            .map_err(|e| Error::Remote(e.to_string()))?;

        let mut folders: Vec<DirEntry> = result
            .common_prefixes
            .iter()
            .map(|p| {
                let key = p.as_ref();
                DirEntry {
                    name: last_segment(key).to_string(),
                    url: format!("{base}/{key}/"),
                    is_dir: true,
                    size: None,
                }
            })
            .collect();
        let mut files: Vec<DirEntry> = result
            .objects
            .iter()
            .filter_map(|o| {
                let key = o.location.as_ref();
                let name = last_segment(key);
                // Skip the zero-length "directory marker" object some stores return for the prefix.
                (!name.is_empty()).then(|| DirEntry {
                    name: name.to_string(),
                    url: format!("{base}/{key}"),
                    is_dir: false,
                    size: Some(o.size),
                })
            })
            .collect();
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        files.sort_by(|a, b| a.name.cmp(&b.name));
        folders.append(&mut files);
        Ok(DirListing { entries: folders })
    })
}

/// The parent prefix URL of `location` for "up" navigation, or `None` when already at the bucket root.
/// `s3://bucket/a/b/` → `s3://bucket/a/`; `s3://bucket/a/` → `s3://bucket/`; `s3://bucket/` → `None`.
pub fn parent_url(location: &str) -> Option<String> {
    let url = Url::parse(location).ok()?;
    let mut segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    segments.pop()?; // drop the current leaf; `None` (empty) means we were already at the root
    let base = format!("{}://{}", url.scheme(), url.host_str()?);
    if segments.is_empty() {
        Some(format!("{base}/"))
    } else {
        Some(format!("{base}/{}/", segments.join("/")))
    }
}

/// The last non-empty `/`-separated segment of a key (its file/folder name).
fn last_segment(key: &str) -> &str {
    key.rsplit('/').find(|s| !s.is_empty()).unwrap_or(key)
}

/// Directory holding downloaded remote objects: `$TABLEIZER_CACHE_DIR/remote-cache` if set, else the
/// OS *state* dir (Linux) / local-data equivalent under `tableizer/remote-cache`. Never beside any
/// user data, and separate from the index cache.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TABLEIZER_CACHE_DIR") {
        return Some(PathBuf::from(dir).join("remote-cache"));
    }
    let base = directories::BaseDirs::new()?;
    let root = base.state_dir().unwrap_or_else(|| base.data_local_dir());
    Some(root.join("tableizer").join("remote-cache"))
}

/// Total size of the downloaded-objects cache, in bytes (for the cache-management UI).
pub fn cache_size() -> u64 {
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

/// Delete all downloaded remote objects.
pub fn clear_cache() {
    if let Some(dir) = cache_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Download the remote object at `target` into `cache_root`, returning the local file path. An already
/// downloaded copy is reused when the object's identity (ETag, else size + last-modified) is unchanged
/// — so reopening a stable object is instant and offline, while a changed object re-downloads. Total
/// byte size is published to `total` once known and bytes-written to `progress` as they land; `cancel`
/// aborts cleanly, leaving no partial file behind.
///
/// `options` are backend config/credentials (e.g. `aws_region`, `aws_access_key_id`, `aws_endpoint`)
/// from the in-app form; they are layered *over* the process environment so explicit settings win.
pub fn fetch_to_cache(
    target: &str,
    cache_root: &Path,
    options: &[(String, String)],
    progress: &AtomicU64,
    total: &AtomicU64,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let url = Url::parse(target).map_err(|e| Error::Remote(format!("invalid URL: {e}")))?;

    // The engine is sync; drive the async object-store calls on a confined current-thread runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // `object_store` builds a *blank* store, so credentials/region are passed as options; the
        // bucket's real region is resolved and applied so a non-default-region bucket just works.
        let (store, path) = build_store(&url, options).await?;
        let meta = store
            .head(&path)
            .await
            .map_err(|e| Error::Remote(e.to_string()))?;
        let size: u64 = meta.size;
        total.store(size, Ordering::Relaxed);

        // Reuse key: prefer the content-derived ETag; fall back to size + last-modified.
        let identity = meta
            .e_tag
            .clone()
            .unwrap_or_else(|| format!("{size}-{}", meta.last_modified.to_rfc3339()));
        let dest = cache_root.join(cache_name(target, &identity));
        if dest.exists() {
            progress.store(size, Ordering::Relaxed); // already complete
            return Ok(dest);
        }
        std::fs::create_dir_all(cache_root)?;

        // Stream to a temp file, then atomically rename in — a cancelled or failed download must never
        // leave a half-written file that a later open would treat as the complete object.
        let tmp = dest.with_extension("partial");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut stream = store
            .get(&path)
            .await
            .map_err(|e| Error::Remote(e.to_string()))?
            .into_stream();
        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Cancelled);
            }
            let bytes = chunk.map_err(|e| Error::Remote(e.to_string()))?;
            std::io::Write::write_all(&mut file, &bytes)?;
            progress.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        std::io::Write::flush(&mut file)?;
        drop(file);
        std::fs::rename(&tmp, &dest)?;
        Ok(dest)
    })
}

/// Layer `extra` (in-app config) over the process `env` for `object_store`'s builder. The builder
/// applies entries in order and later wins, so `extra` is appended last — letting an explicit form
/// value override the same key from the environment. Unrecognised keys (most env vars) are ignored by
/// the builder.
fn merge_options<I>(env: I, extra: &[(String, String)]) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    env.into_iter().chain(extra.iter().cloned()).collect()
}

/// Cache filename for `target` at content `identity`: stable hashes of the URL and the identity, with
/// the source extension preserved so extension-based format detection still works on the cached copy.
fn cache_name(target: &str, identity: &str) -> String {
    let url_hash = crate::stable_hash(Path::new(target));
    let id_hash = crate::stable_hash(Path::new(identity));
    match url_extension(target) {
        Some(ext) => format!("{url_hash:016x}-{id_hash:016x}.{ext}"),
        None => format!("{url_hash:016x}-{id_hash:016x}"),
    }
}

/// The file extension of a URL's last path segment, if it has a short alphanumeric one (e.g. the
/// `csv` of `s3://bucket/dir/sales.csv`). `None` when there's no usable extension.
fn url_extension(target: &str) -> Option<String> {
    let url = Url::parse(target).ok()?;
    let last = url.path_segments()?.next_back()?;
    let ext = last.rsplit_once('.')?.1;
    (!ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_remote_classifies_schemes() {
        assert!(is_remote("s3://bucket/key.csv"));
        assert!(is_remote("gs://bucket/object"));
        assert!(is_remote("https://example.com/data.csv"));
        // Local paths are not remote.
        assert!(!is_remote("/var/data/local.csv"));
        assert!(!is_remote("relative/path.csv"));
        assert!(!is_remote("C:\\Users\\data.csv")); // Windows drive (scheme `c`)
        assert!(!is_remote("data.csv"));
    }

    #[test]
    fn buckets_to_listing_sorts_and_builds_urls() {
        let listing = buckets_to_listing(["zeta".to_string(), "alpha".to_string()]);
        let entries: Vec<(&str, &str, bool)> = listing
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.url.as_str(), e.is_dir))
            .collect();
        assert_eq!(
            entries,
            vec![("alpha", "s3://alpha/", true), ("zeta", "s3://zeta/", true),]
        );
    }

    #[test]
    fn s3_bucket_extracts_the_host_for_s3_urls_only() {
        assert_eq!(
            s3_bucket(&Url::parse("s3://my-bucket/a/b.csv").unwrap()).as_deref(),
            Some("my-bucket")
        );
        assert_eq!(
            s3_bucket(&Url::parse("s3a://other/x").unwrap()).as_deref(),
            Some("other")
        );
        // Non-S3 schemes have no AWS bucket region to resolve.
        assert_eq!(s3_bucket(&Url::parse("gs://bucket/x").unwrap()), None);
        assert_eq!(s3_bucket(&Url::parse("https://h/x").unwrap()), None);
    }

    #[test]
    fn parent_url_walks_up_to_the_bucket_root() {
        assert_eq!(
            parent_url("s3://bucket/a/b/").as_deref(),
            Some("s3://bucket/a/")
        );
        assert_eq!(
            parent_url("s3://bucket/a/file.csv").as_deref(),
            Some("s3://bucket/a/")
        );
        assert_eq!(
            parent_url("s3://bucket/a/").as_deref(),
            Some("s3://bucket/")
        );
        // Already at the root → no parent.
        assert_eq!(parent_url("s3://bucket/"), None);
        assert_eq!(parent_url("s3://bucket"), None);
    }

    #[test]
    fn list_dir_returns_folders_then_files() {
        // A `file://` directory exercises list_with_delimiter through object_store's local backend.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("nested.csv"), b"x").unwrap();
        std::fs::write(dir.path().join("b.csv"), b"hi").unwrap();
        std::fs::write(dir.path().join("a.csv"), b"hello").unwrap();
        let url = Url::from_directory_path(dir.path()).unwrap();

        let listing = list_dir(url.as_str(), &[], &CancellationToken::new()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        // Folder first, then files sorted by name; the nested file is not listed (non-recursive).
        assert_eq!(names, vec!["sub", "a.csv", "b.csv"]);
        assert!(listing.entries[0].is_dir);
        assert!(!listing.entries[1].is_dir);
        assert_eq!(listing.entries[1].size, Some(5)); // a.csv = "hello"
        // The folder URL is navigable (ends in `/`) and the file URL points at the object.
        assert!(listing.entries[0].url.ends_with("/sub/"));
        assert!(listing.entries[1].url.ends_with("/a.csv"));
    }

    #[test]
    fn is_s3_matches_only_the_s3_schemes() {
        assert!(is_s3("s3://bucket/key.csv"));
        assert!(is_s3("s3a://bucket/key.csv"));
        assert!(!is_s3("gs://bucket/key.csv"));
        assert!(!is_s3("https://example.com/key.csv"));
        assert!(!is_s3("/local/key.csv"));
    }

    #[test]
    fn url_extension_picks_the_last_segment_suffix() {
        assert_eq!(
            url_extension("s3://b/dir/sales.csv").as_deref(),
            Some("csv")
        );
        assert_eq!(
            url_extension("https://h/x.PARQUET").as_deref(),
            Some("parquet")
        );
        assert_eq!(url_extension("s3://b/dir/noext").as_deref(), None);
    }

    #[test]
    fn merge_options_appends_in_app_config_after_the_environment() {
        // object_store's builder applies options in order and lets the last win, so an in-app value
        // must follow the same key from the environment — that's how the credential form overrides it.
        let env = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("aws_region".to_string(), "env-region".to_string()),
        ];
        let merged = merge_options(
            env,
            &[("aws_region".to_string(), "form-region".to_string())],
        );
        let regions: Vec<&str> = merged
            .iter()
            .filter(|(k, _)| k == "aws_region")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(regions, vec!["env-region", "form-region"]);
    }

    #[test]
    fn fetch_downloads_then_reuses_the_cached_copy() {
        // A `file://` URL routes through object_store's local backend — the same `head` + streamed
        // `get` code path as S3, exercised deterministically with no network.
        let src = tempfile::Builder::new().suffix(".csv").tempfile().unwrap();
        std::fs::write(src.path(), b"a,b\n1,2\n3,4\n").unwrap();
        let url = Url::from_file_path(src.path()).unwrap();

        let cache = tempfile::tempdir().unwrap();
        let progress = AtomicU64::new(0);
        let total = AtomicU64::new(0);
        let cancel = CancellationToken::new();

        let dest =
            fetch_to_cache(url.as_str(), cache.path(), &[], &progress, &total, &cancel).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"a,b\n1,2\n3,4\n");
        assert_eq!(total.load(Ordering::Relaxed), 12);
        assert_eq!(progress.load(Ordering::Relaxed), 12);
        assert!(dest.starts_with(cache.path()));
        // The source extension is preserved on the cached file.
        assert_eq!(dest.extension().and_then(|e| e.to_str()), Some("csv"));

        // A second fetch reuses the same cached file (unchanged identity).
        let again =
            fetch_to_cache(url.as_str(), cache.path(), &[], &progress, &total, &cancel).unwrap();
        assert_eq!(again, dest);
    }
}
