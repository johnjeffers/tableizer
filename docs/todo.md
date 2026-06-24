- Per column filters/functions (min, max, unique)
- CI/CD
  - Package as a standard app
  - Create release artifacts for each OS
- App should be allowed to be set as the default handler for CSV, TSV, etc.
  - macOS: DONE — verified warm + cold (Open With / double-click / set-as-default) on macOS 26.
    Declaration via `CFBundleDocumentTypes` + imported UTIs in `scripts/package-macos.sh`; the
    open-documents receiver is `src/macos_open.rs` (cold launch leans on a run-loop-entry re-assert —
    re-verify if winit/eframe/macOS are upgraded).
  - Windows: per-extension association via a ProgID + `shell\open\command` (installer-written); files
    arrive as argv, so no receiver code needed.
  - Linux: a `.desktop` file with `MimeType=` + `update-desktop-database`, plus a `shared-mime-info`
    XML for Parquet/NDJSON (no standard MIME types); files arrive as argv.
- Export refinements
  - Typed NDJSON/Parquet export (numbers/booleans/nulls) with documented coercion rules — current
    export writes every value as text to preserve byte fidelity (e.g. a `"007"` id keeps its zeros)
  - User-chosen delimiter for CSV/TSV export
- Multiple tabs / files
- Gzip — DONE (`crates/tableizer-core/src/gzip.rs`): a `.gz` (detected by magic, not extension) is
  decompressed once to `tableizer/decompressed` in the state dir (background, progress + cancel,
  source-fingerprint-keyed), then opened like a normal file — so the offset index / random access all
  work. Composes with remote (`s3://…/data.csv.gz` → download → decompress → open) and keeps the inner
  extension for format detection. Follow-ups: other codecs (zstd/bzip2/xz), gz-aware export (re-gzip),
  and a streaming/seekable-gzip path to avoid the full decompress for huge files (bgzip/zran).
- Cloud / remote files (S3, GCS, Azure, HTTP) — `crates/tableizer-core/src/remote.rs`
  - DONE (first cut): open by URL (File ▸ Open URL…, empty-view button, or CLI arg) via `object_store`;
    download-to-cache in the state dir (ETag-keyed, streamed with progress + cancel), then opened like
    a local file. `parse_url` builds a blank store, so credentials are wired explicitly.
  - DONE: cloud file **browser** (File ▸ Browse Cloud…, empty-view button, or the Open-URL dialog's
    Browse…) — opens to a **bucket list discovered from the credentials** (`remote::list_s3_buckets`
    via `aws-sdk-s3` ListBuckets, since object_store is bucket-scoped and can't enumerate); click a
    bucket to enter, then `remote::list_dir` (object_store `list_with_delimiter`) lists folders + files,
    Up ascends (back to the bucket list at a bucket root), click a file to open. Background-listed with
    the same credential resolution as opening (SSO/profile/static). NOTE: `aws-sdk-s3` must keep
    `default-features=false` + `default-https-client` (NOT the default `rustls` feature → legacy
    rustls 0.21, RUSTSEC-2026-0098/0099/0104). Follow-ups: bucket discovery for GCS/Azure (S3 only
    now), breadcrumb trail, recent buckets, type-ahead filter, paging very large prefixes.
  - DONE: S3 credentials (Settings ▸ Cloud storage), two modes — (1) **AWS chain** (default):
    `remote::aws_credentials` via `aws-config` covers env, `~/.aws` profiles, **SSO** (`aws sso login`
    token cache → temp creds), assume-role, EC2/ECS roles; optional profile + region fields. (2)
    **Static keys** for pasted creds / S3-compatible (MinIO/R2: endpoint + allow-HTTP). Secrets saved
    plaintext-0600 in `cloud.json`.
  - Streaming `ReadAt` source — first screen from a head fetch, random access by ranged GET, no full
    download — so multi-TB cloud objects don't require downloading the whole thing. Generalises the
    planned `pread`/SIGBUS seam; download-to-cache is the eager subset of it.
  - Credential follow-ups: in-app SSO login (browser device flow) so an expired token can be refreshed
    without a terminal; GCS/Azure credential forms (only S3 has one — they still use env); move secrets
    to the OS keychain (`keyring`) instead of plaintext-0600 `cloud.json`.
  - Surface egress/read cost and add a remote-cache size cap + eviction (Clear cache already exists).
- Validate the grid holds 60 fps under load (4K, ideally Windows) — the original grid go/no-go bar
