//! **Read-ahead** for the remote browser (`docs/architecture.md` § I/O): list a folder, then — in the
//! background — the folders below it, so expanding one in the tree or completing into one in the
//! jump-to field is answered from a shared [`ListingCache`] instead of a round-trip.
//!
//! Bounded on every axis, since a bucket can be arbitrarily deep or wide:
//! - **depth** — folders at most [`ReadAheadLimits::max_depth`] levels below the root are listed;
//! - **requests** — at most [`ReadAheadLimits::max_requests`] listings per read-ahead (a wide level
//!   spends the budget instead of fanning out without end);
//! - **pages** — only the root is listed in full; a read-ahead folder costs **one page** (where the
//!   backend can page), so a folder holding a million objects costs one request, not a thousand. Its
//!   listing is then cached as incomplete, and listed in full once the user actually visits it.
//!
//! Folders on the way to what the user is typing (the *focus*) are listed first.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use object_store::list::{PaginatedListOptions, PaginatedListStore};
use object_store::{ObjectStore, ObjectStoreScheme};
use url::Url;

use super::{DirListing, listing_from, store_options};
use crate::{CancellationToken, Error, Result};

/// List remote folder `root` in full into `cache`, then read ahead into the folders below it within
/// `limits` (see the module docs), preferring those related to `focus()` — re-read before each
/// request, so typing steers a read-ahead already under way. `on_listing` runs after each listing
/// lands (e.g. to repaint). Blocks until done; run it on a worker thread.
///
/// Credentials come from `resolve`, called only once a request is actually needed — when everything
/// within reach is already cached there is nothing to fetch, and nothing to resolve. One store (one
/// connection pool, one credential set) serves the whole read-ahead.
///
/// Errors only when the root can't be listed (or on cancel); a read-ahead folder that fails is just
/// left to be listed when visited.
pub fn read_ahead(
    root: &str,
    resolve: impl FnOnce() -> Result<Vec<(String, String)>>,
    limits: ReadAheadLimits,
    focus: &dyn Fn() -> String,
    cache: &ListingCache,
    cancel: &CancellationToken,
    on_listing: &dyn Fn(),
) -> Result<()> {
    let mut crawl = Crawl::new(root, limits);
    let Some(first) = crawl.next(&focus(), cached_in(cache)) else {
        return Ok(()); // everything within reach is already cached
    };
    let options = resolve()?;
    let url = Url::parse(root).map_err(|e| Error::Remote(format!("invalid URL: {e}")))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let lister = Lister::connect(&url, &options).await?;
        let hooks = Hooks {
            focus,
            cache,
            cancel,
            on_listing,
        };
        drive(&mut crawl, first, &lister, limits.concurrency, &hooks).await
    })
}

/// What `cache` already knows of a folder, as its subfolder URLs — for the root (depth 0) only a
/// complete listing will do, since the root is the folder the user is looking at.
fn cached_in(cache: &ListingCache) -> impl Fn(&str, usize) -> Option<Vec<String>> + '_ {
    move |url, depth| {
        cache
            .get(url)
            .filter(|cached| depth > 0 || cached.complete)
            .map(|cached| subfolder_urls(&cached.listing))
    }
}

/// The URLs of a listing's folders.
fn subfolder_urls(listing: &DirListing) -> Vec<String> {
    listing
        .entries
        .iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.url.clone())
        .collect()
}

/// Lists one remote folder by URL: in `full`, or only its first page where the backend can page.
pub(crate) trait ListFolder {
    fn list<'a>(&'a self, url: &'a str, full: bool) -> BoxFuture<'a, Result<CachedListing>>;
}

/// How a running read-ahead meets its caller (see [`read_ahead`]): what steers it, where its
/// listings go, and how it is stopped.
struct Hooks<'a> {
    focus: &'a dyn Fn() -> String,
    cache: &'a ListingCache,
    cancel: &'a CancellationToken,
    on_listing: &'a dyn Fn(),
}

/// Run `crawl` against `lister`, starting with `first` (already taken from it), until it is done or
/// cancelled: up to `concurrency` listings in flight; the root (depth 0) listed in full and the rest
/// one page each; each result cached and announced. Only a root failure is an error.
async fn drive(
    crawl: &mut Crawl,
    first: (String, usize),
    lister: &impl ListFolder,
    concurrency: usize,
    hooks: &Hooks<'_>,
) -> Result<()> {
    let Hooks {
        focus,
        cache,
        cancel,
        on_listing,
    } = *hooks;
    let mut in_flight = FuturesUnordered::new();
    let mut first = Some(first);
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        while in_flight.len() < concurrency {
            let Some((url, depth)) = first
                .take()
                .or_else(|| crawl.next(&focus(), cached_in(cache)))
            else {
                break;
            };
            in_flight.push(async move {
                let result = lister.list(&url, depth == 0).await;
                (url, depth, result)
            });
        }
        let Some((url, depth, result)) = in_flight.next().await else {
            return Ok(()); // nothing in flight and nothing left to request
        };
        match result {
            Ok(listed) => {
                crawl.listed(depth, subfolder_urls(&listed.listing));
                cache.insert(&url, listed);
                on_listing();
            }
            Err(error) if depth == 0 => return Err(error),
            Err(_) => {} // a read-ahead miss: the folder is listed when visited
        }
    }
}

/// The `object_store` behind a read-ahead: one store for the bucket (one connection pool and one
/// credential set for every listing), plus its paged-listing face where the backend has one (S3, GCS,
/// Azure — not HTTP), so a read-ahead folder costs one page.
struct Lister {
    /// The bucket/host root (`s3://bucket`) that folder URLs are relative to.
    base: String,
    store: Arc<dyn ObjectStore>,
    paged: Option<Arc<dyn PaginatedListStore>>,
}

impl Lister {
    /// Build the store for `url`'s bucket from resolved `options` (with the bucket's region, as for
    /// downloads).
    async fn connect(url: &Url, options: &[(String, String)]) -> Result<Self> {
        let options = store_options(url, options).await;
        let base = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
        let remote = |e: object_store::Error| Error::Remote(e.to_string());
        let (scheme, _) =
            ObjectStoreScheme::parse(url).map_err(|e| Error::Remote(e.to_string()))?;
        let (store, paged): (Arc<dyn ObjectStore>, Option<Arc<dyn PaginatedListStore>>) =
            match scheme {
                ObjectStoreScheme::AmazonS3 => {
                    let builder = object_store::aws::AmazonS3Builder::new().with_url(url.as_str());
                    let store = Arc::new(
                        configured(builder, &options, |b, k, v| b.with_config(k, v))
                            .build()
                            .map_err(remote)?,
                    );
                    (store.clone(), Some(store))
                }
                ObjectStoreScheme::GoogleCloudStorage => {
                    let builder =
                        object_store::gcp::GoogleCloudStorageBuilder::new().with_url(url.as_str());
                    let store = Arc::new(
                        configured(builder, &options, |b, k, v| b.with_config(k, v))
                            .build()
                            .map_err(remote)?,
                    );
                    (store.clone(), Some(store))
                }
                ObjectStoreScheme::MicrosoftAzure => {
                    let builder =
                        object_store::azure::MicrosoftAzureBuilder::new().with_url(url.as_str());
                    let store = Arc::new(
                        configured(builder, &options, |b, k, v| b.with_config(k, v))
                            .build()
                            .map_err(remote)?,
                    );
                    (store.clone(), Some(store))
                }
                _ => {
                    let (store, _) = object_store::parse_url_opts(url, options).map_err(remote)?;
                    (Arc::from(store), None)
                }
            };
        Ok(Self { base, store, paged })
    }
}

impl ListFolder for Lister {
    fn list<'a>(&'a self, url: &'a str, full: bool) -> BoxFuture<'a, Result<CachedListing>> {
        async move {
            let remote = |e: object_store::Error| Error::Remote(e.to_string());
            // The folder's key prefix within the bucket: `data/2024/`, or empty at the bucket root.
            let key = url
                .strip_prefix(self.base.as_str())
                .unwrap_or_default()
                .trim_start_matches('/');
            if let (Some(paged), false) = (&self.paged, full) {
                let options = PaginatedListOptions {
                    delimiter: Some("/".into()),
                    ..PaginatedListOptions::default()
                };
                let page = paged
                    .list_paginated((!key.is_empty()).then_some(key), options)
                    .await
                    .map_err(remote)?;
                return Ok(CachedListing {
                    listing: listing_from(&self.base, &page.result),
                    complete: page.page_token.is_none(),
                });
            }
            let prefix = (!key.is_empty())
                .then(|| object_store::path::Path::parse(key))
                .transpose()
                .map_err(|e| Error::Remote(e.to_string()))?;
            let result = self
                .store
                .list_with_delimiter(prefix.as_ref())
                .await
                .map_err(remote)?;
            Ok(CachedListing {
                listing: listing_from(&self.base, &result),
                complete: true,
            })
        }
        .boxed()
    }
}

/// Apply `options` (`object_store` config keys, as `parse_url_opts` takes them) to a store builder,
/// skipping keys this backend doesn't recognise.
fn configured<B, K: std::str::FromStr>(
    builder: B,
    options: &[(String, String)],
    with_config: fn(B, K, String) -> B,
) -> B {
    options.iter().fold(builder, |builder, (key, value)| {
        match key.to_ascii_lowercase().parse() {
            Ok(key) => with_config(builder, key, value.clone()),
            Err(_) => builder,
        }
    })
}

/// How far one read-ahead reaches. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadAheadLimits {
    /// Levels below the root to list: 2 = its subfolders and theirs.
    pub max_depth: usize,
    /// Listing requests per read-ahead, the root's included.
    pub max_requests: usize,
    /// Listing requests in flight at once.
    pub concurrency: usize,
}

impl Default for ReadAheadLimits {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_requests: 100,
            concurrency: 8,
        }
    }
}

/// A remote folder's listing as cached: `complete` is `false` when only its first page was read (a
/// read-ahead of a folder with more entries than one page holds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedListing {
    pub listing: DirListing,
    pub complete: bool,
}

/// Remote folder listings by folder URL (`s3://bucket/a/`), shared by read-ahead workers and the UI.
/// Kept until [`clear`](Self::clear)ed (the browser's Refresh), like the browse tree itself.
#[derive(Debug, Default)]
pub struct ListingCache(Mutex<HashMap<String, CachedListing>>);

impl ListingCache {
    /// The cached listing of folder `url`, if any.
    pub fn get(&self, url: &str) -> Option<CachedListing> {
        self.0.lock().expect("listing cache").get(url).cloned()
    }

    /// Cache `listing` for folder `url` — unless it is a partial listing and a complete one is
    /// already cached, which it must not displace.
    pub fn insert(&self, url: &str, listing: CachedListing) {
        let mut cache = self.0.lock().expect("listing cache");
        if listing.complete || !cache.get(url).is_some_and(|cached| cached.complete) {
            cache.insert(url.to_string(), listing);
        }
    }

    /// Forget every listing.
    pub fn clear(&self) {
        self.0.lock().expect("listing cache").clear();
    }
}

/// The order a read-ahead lists folders in: breadth-first from the root, within the depth limit and
/// the request budget, each folder at most once, with folders related to the focus first.
#[derive(Debug)]
pub(crate) struct Crawl {
    max_depth: usize,
    max_requests: usize,
    /// Folders still to list, with their depth below the root, in breadth-first order.
    queue: VecDeque<(String, usize)>,
    /// Every folder ever queued, so each is listed once.
    seen: HashSet<String>,
    /// Listing requests issued so far.
    requests: usize,
}

impl Crawl {
    /// A crawl from folder `root` (depth 0).
    pub(crate) fn new(root: &str, limits: ReadAheadLimits) -> Self {
        Self {
            max_depth: limits.max_depth,
            max_requests: limits.max_requests,
            queue: VecDeque::from([(root.to_string(), 0)]),
            seen: HashSet::from([root.to_string()]),
            requests: 0,
        }
    }

    /// The next folder to request, with its depth: the first queued folder related to `focus` (on
    /// the way to it, or below it — ignoring case, as completion does), else the next breadth-first.
    /// A folder `cached` can stand in for (returning its subfolder URLs) is expanded on the spot,
    /// without a request. `None` when nothing is left or the request budget is spent.
    pub(crate) fn next(
        &mut self,
        focus: &str,
        cached: impl Fn(&str, usize) -> Option<Vec<String>>,
    ) -> Option<(String, usize)> {
        loop {
            let pick = self.focused(focus).unwrap_or(0);
            let (url, depth) = self.queue.remove(pick)?;
            if let Some(subfolders) = cached(&url, depth) {
                self.listed(depth, subfolders);
                continue;
            }
            if self.requests >= self.max_requests {
                return None;
            }
            self.requests += 1;
            return Some((url, depth));
        }
    }

    /// The position of the first queued folder on the way to `focus`, or below it.
    fn focused(&self, focus: &str) -> Option<usize> {
        let focus = focus.to_lowercase();
        self.queue.iter().position(|(url, _)| {
            let url = url.to_lowercase();
            url.starts_with(&focus) || focus.starts_with(&url)
        })
    }

    /// Record that the folder at `depth` has `subfolders` (their URLs): queue those within the depth
    /// limit and not seen before.
    pub(crate) fn listed(&mut self, depth: usize, subfolders: impl IntoIterator<Item = String>) {
        if depth >= self.max_depth {
            return;
        }
        for url in subfolders {
            if self.seen.insert(url.clone()) {
                self.queue.push_back((url, depth + 1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::DirEntry;
    use futures::FutureExt;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    /// A future that is pending once before completing — so listings genuinely overlap.
    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// An in-memory bucket of folders, recording each listing request and its overlap.
    #[derive(Default)]
    struct FakeStore {
        /// Folder URL → its subfolder URLs.
        tree: HashMap<String, Vec<String>>,
        /// Folders whose listing fails.
        failing: HashSet<String>,
        /// Folders with more than one page: a one-page listing of them is incomplete.
        paged: HashSet<String>,
        /// `(url, full)` per request, in order.
        calls: Mutex<Vec<(String, bool)>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl FakeStore {
        fn new(tree: &[(&str, &[&str])]) -> Self {
            Self {
                tree: tree
                    .iter()
                    .map(|(folder, subs)| (folder.to_string(), urls(subs)))
                    .collect(),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<(String, bool)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ListFolder for FakeStore {
        fn list<'a>(&'a self, url: &'a str, full: bool) -> BoxFuture<'a, Result<CachedListing>> {
            async move {
                self.calls.lock().unwrap().push((url.to_string(), full));
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                YieldOnce(false).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                if self.failing.contains(url) {
                    return Err(Error::Remote(format!("cannot list {url}")));
                }
                let entries = self.tree.get(url).cloned().unwrap_or_default();
                Ok(CachedListing {
                    listing: folders(&entries),
                    complete: full || !self.paged.contains(url),
                })
            }
            .boxed()
        }
    }

    /// A listing of the given subfolder URLs.
    fn folders(subfolders: &[String]) -> DirListing {
        DirListing {
            entries: subfolders
                .iter()
                .map(|url| DirEntry {
                    url: url.clone(),
                    name: url.trim_end_matches('/').rsplit('/').next().unwrap().into(),
                    is_dir: true,
                    size: None,
                })
                .collect(),
        }
    }

    /// Run [`drive`] from `root` to completion against `store`.
    fn run_drive(
        root: &str,
        store: &FakeStore,
        limits: ReadAheadLimits,
        cache: &ListingCache,
        cancel: &CancellationToken,
        on_listing: &dyn Fn(),
    ) -> Result<()> {
        let mut crawl = Crawl::new(root, limits);
        let first = crawl.next("", uncached).expect("the root is always first");
        let hooks = Hooks {
            focus: &String::new,
            cache,
            cancel,
            on_listing,
        };
        futures::executor::block_on(drive(&mut crawl, first, store, limits.concurrency, &hooks))
    }

    fn bucket() -> FakeStore {
        FakeStore::new(&[
            ("s3://b/", &["s3://b/x/", "s3://b/y/"]),
            ("s3://b/x/", &["s3://b/x/1/"]),
            ("s3://b/y/", &[]),
            ("s3://b/x/1/", &["s3://b/x/1/deep/"]),
        ])
    }

    fn limits(max_depth: usize, max_requests: usize) -> ReadAheadLimits {
        ReadAheadLimits {
            max_depth,
            max_requests,
            concurrency: 8,
        }
    }

    fn uncached(_: &str, _: usize) -> Option<Vec<String>> {
        None
    }

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Drain a crawl, answering each request from `tree` (folder → subfolders), with no focus.
    fn order(mut crawl: Crawl, tree: &HashMap<&str, Vec<&str>>) -> Vec<(String, usize)> {
        let mut listed = Vec::new();
        while let Some((url, depth)) = crawl.next("", uncached) {
            let subfolders = tree.get(url.as_str()).cloned().unwrap_or_default();
            crawl.listed(depth, urls(&subfolders));
            listed.push((url, depth));
        }
        listed
    }

    fn sample_tree() -> HashMap<&'static str, Vec<&'static str>> {
        HashMap::from([
            ("s3://b/", vec!["s3://b/x/", "s3://b/y/"]),
            ("s3://b/x/", vec!["s3://b/x/1/"]),
            ("s3://b/y/", vec!["s3://b/y/2/"]),
            ("s3://b/x/1/", vec!["s3://b/x/1/deep/"]),
        ])
    }

    fn listing(names: &[&str]) -> CachedListing {
        CachedListing {
            listing: DirListing {
                entries: names
                    .iter()
                    .map(|n| DirEntry {
                        url: format!("s3://b/{n}/"),
                        name: n.to_string(),
                        is_dir: true,
                        size: None,
                    })
                    .collect(),
            },
            complete: true,
        }
    }

    #[test]
    fn crawl_lists_breadth_first_down_to_the_depth_limit() {
        let listed = order(Crawl::new("s3://b/", limits(2, 100)), &sample_tree());
        let expected = [
            ("s3://b/", 0),
            ("s3://b/x/", 1),
            ("s3://b/y/", 1),
            ("s3://b/x/1/", 2),
            ("s3://b/y/2/", 2),
        ];
        assert_eq!(
            listed,
            expected.map(|(u, d)| (u.to_string(), d)).to_vec(),
            "s3://b/x/1/deep/ is at depth 3, past the limit"
        );
    }

    #[test]
    fn crawl_stops_at_the_request_budget() {
        let listed = order(Crawl::new("s3://b/", limits(2, 3)), &sample_tree());
        assert_eq!(listed.len(), 3);
    }

    #[test]
    fn crawl_lists_each_folder_once() {
        let mut crawl = Crawl::new("s3://b/", limits(2, 100));
        assert_eq!(crawl.next("", uncached), Some(("s3://b/".into(), 0)));
        crawl.listed(0, urls(&["s3://b/x/"]));
        crawl.listed(0, urls(&["s3://b/x/", "s3://b/"]));
        assert_eq!(crawl.next("", uncached), Some(("s3://b/x/".into(), 1)));
        assert_eq!(crawl.next("", uncached), None);
    }

    #[test]
    fn crawl_prefers_folders_related_to_the_focus() {
        let mut crawl = Crawl::new("s3://b/", limits(2, 100));
        crawl.next("", uncached);
        crawl.listed(0, urls(&["s3://b/alpha/", "s3://b/data/", "s3://b/zeta/"]));
        // Typing toward a folder (case-insensitively) lists it first...
        assert_eq!(
            crawl.next("s3://b/DA", uncached),
            Some(("s3://b/data/".into(), 1))
        );
        crawl.listed(1, urls(&["s3://b/data/2024/"]));
        // ...and then what lies below it.
        assert_eq!(
            crawl.next("s3://b/data/", uncached),
            Some(("s3://b/data/2024/".into(), 2))
        );
        // Unrelated to the focus: breadth-first again.
        assert_eq!(
            crawl.next("s3://b/q", uncached),
            Some(("s3://b/alpha/".into(), 1))
        );
    }

    #[test]
    fn crawl_expands_cached_folders_without_spending_requests() {
        let cache = HashMap::from([("s3://b/", urls(&["s3://b/x/", "s3://b/y/"]))]);
        let cached = |url: &str, _: usize| cache.get(url).cloned();
        let mut crawl = Crawl::new("s3://b/", limits(2, 1));
        // The root comes from the cache; the one request goes to its first subfolder.
        assert_eq!(crawl.next("", cached), Some(("s3://b/x/".into(), 1)));
        assert_eq!(crawl.next("", cached), None);
    }

    #[test]
    fn drive_lists_the_root_in_full_and_reads_ahead_one_page_each() {
        let store = bucket();
        let cache = ListingCache::default();
        run_drive(
            "s3://b/",
            &store,
            limits(2, 100),
            &cache,
            &CancellationToken::new(),
            &|| {},
        )
        .unwrap();
        assert_eq!(
            store.calls(),
            [
                ("s3://b/".to_string(), true),
                ("s3://b/x/".to_string(), false),
                ("s3://b/y/".to_string(), false),
                ("s3://b/x/1/".to_string(), false),
            ]
        );
    }

    #[test]
    fn drive_caches_every_listing_and_announces_each() {
        let mut store = bucket();
        store.paged.insert("s3://b/x/".into());
        let cache = ListingCache::default();
        let announced = AtomicUsize::new(0);
        let on_listing = || {
            announced.fetch_add(1, Ordering::SeqCst);
        };
        run_drive(
            "s3://b/",
            &store,
            limits(2, 100),
            &cache,
            &CancellationToken::new(),
            &on_listing,
        )
        .unwrap();
        assert_eq!(announced.load(Ordering::SeqCst), 4);
        let root = cache.get("s3://b/").unwrap();
        assert!(root.complete);
        assert_eq!(root.listing, folders(&urls(&["s3://b/x/", "s3://b/y/"])));
        // A folder with more than one page, read ahead as one page, is cached as incomplete.
        assert!(!cache.get("s3://b/x/").unwrap().complete);
        assert!(cache.get("s3://b/x/1/").is_some());
    }

    #[test]
    fn drive_keeps_at_most_the_concurrency_limit_in_flight() {
        let subfolders: Vec<String> = (0..10).map(|i| format!("s3://b/{i}/")).collect();
        let subs: Vec<&str> = subfolders.iter().map(String::as_str).collect();
        let store = FakeStore::new(&[("s3://b/", &subs)]);
        let limits = ReadAheadLimits {
            concurrency: 3,
            ..limits(1, 100)
        };
        run_drive(
            "s3://b/",
            &store,
            limits,
            &ListingCache::default(),
            &CancellationToken::new(),
            &|| {},
        )
        .unwrap();
        assert_eq!(store.calls().len(), 11);
        assert_eq!(store.max_in_flight.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn drive_fails_when_the_root_cannot_be_listed() {
        let mut store = bucket();
        store.failing.insert("s3://b/".into());
        let cache = ListingCache::default();
        let result = run_drive(
            "s3://b/",
            &store,
            limits(2, 100),
            &cache,
            &CancellationToken::new(),
            &|| {},
        );
        assert!(matches!(result, Err(Error::Remote(_))));
        assert_eq!(cache.get("s3://b/"), None);
    }

    #[test]
    fn drive_skips_a_read_ahead_folder_that_fails() {
        let mut store = bucket();
        store.failing.insert("s3://b/x/".into());
        let cache = ListingCache::default();
        run_drive(
            "s3://b/",
            &store,
            limits(2, 100),
            &cache,
            &CancellationToken::new(),
            &|| {},
        )
        .unwrap();
        assert_eq!(cache.get("s3://b/x/"), None);
        assert!(cache.get("s3://b/y/").is_some());
    }

    #[test]
    fn drive_stops_when_cancelled() {
        let store = bucket();
        let cancel = CancellationToken::new();
        let result = run_drive(
            "s3://b/",
            &store,
            limits(2, 100),
            &ListingCache::default(),
            &cancel,
            &|| cancel.cancel(),
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(store.calls().len(), 1);
    }

    #[test]
    fn read_ahead_lists_a_real_store_root_first_then_below() {
        // A `file://` directory exercises the real `object_store` path (its local backend, which
        // can't page, so the read-ahead folder is listed whole).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("nested.csv"), b"x").unwrap();
        std::fs::write(dir.path().join("b.csv"), b"hi").unwrap();
        std::fs::write(dir.path().join("a.csv"), b"hello").unwrap();
        let root = Url::from_directory_path(dir.path()).unwrap().to_string();
        let cache = ListingCache::default();

        read_ahead(
            &root,
            || Ok(Vec::new()),
            limits(1, 100),
            &String::new,
            &cache,
            &CancellationToken::new(),
            &|| {},
        )
        .unwrap();

        let listed = cache.get(&root).unwrap();
        assert!(listed.complete);
        let entries = &listed.listing.entries;
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // Folder first, then files sorted by name; the nested file is not listed (non-recursive).
        assert_eq!(names, ["sub", "a.csv", "b.csv"]);
        assert_eq!(entries[1].size, Some(5)); // a.csv = "hello"
        // The folder URL is navigable (ends in `/`) and the file URL points at the object.
        assert_eq!(entries[0].url, format!("{root}sub/"));
        assert_eq!(entries[1].url, format!("{root}a.csv"));
        // ...and the folder below was read ahead, under that same URL.
        let sub = cache.get(&entries[0].url).unwrap();
        assert_eq!(sub.listing.entries[0].name, "nested.csv");
    }

    #[test]
    fn read_ahead_resolves_no_credentials_when_everything_is_cached() {
        let cache = ListingCache::default();
        cache.insert(
            "s3://b/",
            CachedListing {
                listing: folders(&urls(&["s3://b/x/"])),
                complete: true,
            },
        );
        cache.insert(
            "s3://b/x/",
            CachedListing {
                listing: folders(&[]),
                complete: true,
            },
        );
        let resolve = || -> Result<Vec<(String, String)>> { panic!("nothing to fetch") };
        let result = read_ahead(
            "s3://b/",
            resolve,
            limits(2, 100),
            &String::new,
            &cache,
            &CancellationToken::new(),
            &|| {},
        );
        assert!(result.is_ok());
    }

    #[test]
    fn read_ahead_relists_a_root_cached_only_in_part() {
        let cache = ListingCache::default();
        cache.insert(
            "s3://b/",
            CachedListing {
                listing: folders(&[]),
                complete: false,
            },
        );
        let resolve = || -> Result<Vec<(String, String)>> {
            Err(Error::Remote("resolution attempted".into()))
        };
        let result = read_ahead(
            "s3://b/",
            resolve,
            limits(2, 100),
            &String::new,
            &cache,
            &CancellationToken::new(),
            &|| {},
        );
        assert!(
            matches!(&result, Err(Error::Remote(m)) if m == "resolution attempted"),
            "{result:?}"
        );
    }

    #[test]
    fn cache_returns_what_was_inserted() {
        let cache = ListingCache::default();
        cache.insert("s3://b/", listing(&["x"]));
        assert_eq!(cache.get("s3://b/"), Some(listing(&["x"])));
        assert_eq!(cache.get("s3://b/x/"), None);
    }

    #[test]
    fn cache_keeps_a_complete_listing_over_a_partial_one() {
        let cache = ListingCache::default();
        cache.insert("s3://b/", listing(&["x", "y"]));
        let partial = CachedListing {
            complete: false,
            ..listing(&["x"])
        };
        cache.insert("s3://b/", partial.clone());
        assert_eq!(cache.get("s3://b/"), Some(listing(&["x", "y"])));
        // A partial listing does replace an older partial one, and a complete one replaces it.
        cache.insert("s3://b/a/", partial.clone());
        cache.insert("s3://b/a/", listing(&["z"]));
        assert_eq!(cache.get("s3://b/a/"), Some(listing(&["z"])));
    }

    #[test]
    fn cache_clear_forgets_everything() {
        let cache = ListingCache::default();
        cache.insert("s3://b/", listing(&["x"]));
        cache.clear();
        assert_eq!(cache.get("s3://b/"), None);
    }
}
