//! The eframe application: window state, theme/font installation, file opening, and the per-frame
//! update loop that lays out the menu bar, toolbar, table, and status bar.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui;
use encoding_rs::Encoding;
use tableizer_core::remote::{CachedListing, ListingCache, ReadAheadLimits};
use tableizer_core::{
    CancellationToken, ColumnId, ExportScope, RowCount, Schema, ViewportSource, parse::Dialect,
};

use crate::model::{
    CloudConfig, FindJob, Format, GridLayout, LoadedTable, RowSpan, SavedView, SharedTable, View,
    ViewControls, delimiter_display, detect_format, highlight_matcher, next_match, open_table,
    sniff_file,
};
use crate::persist::{cloud, prefs, recent, views};
use crate::ui::{
    ExportKind, ExportRequest, ListRows, columns_tab, empty_view, fmt_bytes, fmt_count, grid,
    menu_bar, parsing_tab, settings_tab, status_bar, toolbar,
};
use crate::{complete, fonts, theme};

/// A running (or just-finished) export, driven on a background thread so a multi-GB write never
/// blocks the UI (the Tier-C contract: async, progress within ~100 ms, cancellable). The UI polls
/// `progress`/`outcome` each frame and offers a Cancel button (see [`TableizerApp::show_export`]).
struct ExportJob {
    /// File name being written, for the progress dialog.
    file_name: String,
    /// Rows written so far (published by the engine's export loop).
    progress: Arc<AtomicU64>,
    /// Total rows to write (known up front — export is gated on a complete index).
    total: u64,
    /// Cancels the export thread when the user hits Cancel (or a new export starts).
    cancel: CancellationToken,
    /// `None` while running; `Some(Ok)` on success, `Some(Err)` with a message on failure.
    outcome: Arc<Mutex<Option<Result<(), String>>>>,
}

pub(crate) struct TableizerApp {
    pub(crate) view: View,
    pub(crate) recent: Vec<PathBuf>,
    theme: theme::Settings,
    /// `(settings, system_dark)` last pushed to egui — restyle only when this changes.
    applied_theme: Option<(theme::Settings, bool)>,
    /// The window title last set — sent to the window only when it changes.
    applied_title: Option<String>,
    /// System font database (for the chrome font + the table-font picker).
    fonts_db: std::sync::Arc<fontdb::Database>,
    /// Installed font families + a monospaced flag (cached for the picker).
    font_families: Vec<(String, bool)>,
    /// Receives the background-measured family list (full monospace flags); `None` once applied.
    font_rx: Option<std::sync::mpsc::Receiver<Vec<(String, bool)>>>,
    /// Table font last pushed to egui — rebuild the font atlas only when this changes.
    applied_table_font: Option<String>,
    /// Filter text in the table-font picker (inside the Settings tab).
    font_search: String,
    /// Whether the picker is filtered to monospaced fonts.
    font_mono_only: bool,
    /// Whether the right-side panel (Columns / Parsing / Settings tabs) is expanded.
    pub(crate) panel_open: bool,
    /// Which tab the right-side panel shows.
    pub(crate) panel_tab: PanelTab,
    /// The in-flight (or just-finished) export, if any — driven on a background thread.
    export_job: Option<ExportJob>,
    /// The in-flight (or just-finished) remote download, if any — driven on a background thread.
    download_job: Option<DownloadJob>,
    /// The in-flight (or just-finished) gzip decompression, if any — driven on a background thread.
    decompress_job: Option<DecompressJob>,
    /// A target (path or URL) queued at startup (CLI arg) to open on the first frame, once a `Context`
    /// exists to drive a remote download's progress UI.
    pending_target: Option<String>,
    /// S3 credentials/config (from Settings) applied when opening `s3://` URLs.
    cloud: CloudConfig,
    /// Whether the start-screen browser shows remote (cloud) or local files.
    browse_mode: BrowseMode,
    /// The **remote** browse tree's root (the buckets). Cached so revisiting never re-lists.
    browse_root: ChildState,
    /// The **local** browse tree's root (Home, Desktop, Downloads, Documents + filesystem root).
    /// Cached, like the remote tree.
    local_root: ChildState,
    /// In-flight remote listings (one per expanding folder, plus the root), keyed by location.
    browse_jobs: Vec<BrowseJob>,
    /// The "go to" field: a URL (remote) or path (local) to add to the tree and expand.
    browse_goto: String,
    /// Folder suggestions for the "go to" field as the user types.
    goto_complete: GotoCompletion,
    /// The "go to" text, shared with the field's read-ahead to steer it toward what's being typed.
    goto_focus: Arc<Mutex<String>>,
    /// Remote folder listings shared by the tree and the "go to" suggestions, filled by read-ahead.
    /// Cleared by Refresh.
    listings: Arc<ListingCache>,
    /// Read-aheads from tree folders being expanded, until each finishes.
    tree_read_aheads: Vec<ReadAheadJob>,
}

/// A running (or just-finished) remote download to the local cache, on a background thread so a
/// multi-GB object fetch never blocks the UI (Tier-C: async, progress, cancellable). The UI polls
/// `progress`/`total`/`outcome` each frame; on success the cached file is opened like a local one.
struct DownloadJob {
    /// The source URL — shown in the progress dialog and used as the opened table's `origin`.
    target: String,
    /// Bytes downloaded so far (published by the engine's fetch loop).
    progress: Arc<AtomicU64>,
    /// Total bytes to download (from the object's `head`; 0 until known).
    total: Arc<AtomicU64>,
    /// Cancels the download thread when the user hits Cancel.
    cancel: CancellationToken,
    /// `None` while running; `Some(Ok(local_path))` on success, `Some(Err)` with a message on failure.
    outcome: Arc<Mutex<Option<Result<PathBuf, String>>>>,
}

/// A running (or just-finished) gzip decompression to the local cache, on a background thread so a
/// multi-GB decompress never blocks the UI. On success the decompressed file is opened.
struct DecompressJob {
    /// The source label (path or URL) — shown in the dialog and used as the opened table's `origin`.
    origin: String,
    /// Compressed bytes consumed so far.
    progress: Arc<AtomicU64>,
    /// Total compressed bytes (the source size).
    total: Arc<AtomicU64>,
    cancel: CancellationToken,
    /// `None` while running; `Some(Ok(local_path))` on success, `Some(Err)` with a message on failure.
    outcome: Arc<Mutex<Option<Result<PathBuf, String>>>>,
}

/// The user's cloud-auth choice, captured so a worker thread (download or browse) can resolve the
/// final `object_store` options off the UI thread (the AWS chain does a network round-trip).
struct CloudAuth {
    /// Static form options (region/endpoint/allow-http, plus keys in static mode).
    form_options: Vec<(String, String)>,
    /// Whether to resolve the AWS chain (env / profiles / SSO / role) — S3 + chosen, on this target.
    use_chain: bool,
    profile: Option<String>,
    region: Option<String>,
}

impl CloudAuth {
    /// Resolve to final `object_store` options on a worker thread: AWS-chain credentials first (when
    /// applicable), then the form options layered on top so an explicit value wins.
    fn resolve(self) -> Result<Vec<(String, String)>, String> {
        let mut options = if self.use_chain {
            tableizer_core::remote::aws_credentials(self.profile.as_deref(), self.region.as_deref())
                .map_err(|e| e.to_string())?
        } else {
            Vec::new()
        };
        options.extend(self.form_options);
        Ok(options)
    }
}

/// A node in the cloud browse **tree**: a bucket/prefix (folder, lazily expandable) or a file.
struct BrowseNode {
    /// Full URL — a folder's ends in `/` (navigable), a file's is the object URL (openable).
    url: String,
    name: String,
    is_dir: bool,
    /// File size (files only).
    size: Option<u64>,
    /// Whether this folder is expanded.
    expanded: bool,
    /// The folder's children once listed — **cached**, so collapse/re-expand never re-fetches.
    children: ChildState,
}

/// Load state of one tree level (the bucket root, or a folder's children). Kept across re-opens of the
/// browser, so a previously listed subtree is never re-fetched.
#[derive(Default)]
enum ChildState {
    /// Not listed yet.
    #[default]
    Unloaded,
    /// A listing is in flight.
    Loading,
    /// Listed — the children (folders first, then files).
    Loaded(Vec<BrowseNode>),
    /// Listing failed; the message shows in the tree, and expanding again retries.
    Failed(String),
}

/// A running directory/bucket listing. Several may run at once (expanding multiple folders); each is
/// keyed by `location` (`""` = the bucket root) so its result lands on the right node.
struct BrowseJob {
    location: String,
    cancel: CancellationToken,
    outcome: Arc<Mutex<Option<Result<tableizer_core::remote::DirListing, String>>>>,
}

/// Folder completion for the "go to" field (see [`crate::complete`]): the folder the suggestions come
/// from (the typed text up to its last separator), its subfolders once listed, and the popup's state.
#[derive(Default)]
struct GotoCompletion {
    /// The folder listed for suggestions; `None` until the text contains a separator.
    parent: Option<String>,
    /// `parent`'s subfolder names; `None` while listing, or when it can't be listed (completion is
    /// best-effort, so a failure just means no suggestions).
    dirs: Option<Vec<String>>,
    /// Whether `dirs` is final. A remote folder's suggestions keep updating from the shared listing
    /// cache until its complete listing lands (a read-ahead may have read only its first page).
    dirs_complete: bool,
    /// The **local** listing in flight for `parent`.
    job: Option<CompletionOutcome>,
    /// Stops the read-ahead from a **remote** `parent` once the text moves on to another folder.
    read_ahead: Option<CancellationToken>,
    /// The suggestion highlighted with ↑/↓, if any. Cleared whenever the text changes.
    highlighted: Option<usize>,
    /// Escape hid the popup; it returns when the text changes.
    dismissed: bool,
    /// Whether the popup showed last frame — kept open while a click on it completes.
    open: bool,
}

/// A local folder listing for [`GotoCompletion`], run on a background thread so a slow (network)
/// mount never blocks the UI: `None` while running; the subfolder names, or an error message.
type CompletionOutcome = Arc<Mutex<Option<Result<Vec<String>, String>>>>;

/// A read-ahead from a tree folder being expanded, on a background thread. The folder's own listing is
/// installed from the shared cache as soon as it lands ([`install_cached`]); the read-ahead goes on
/// below it.
struct ReadAheadJob {
    root: String,
    cancel: CancellationToken,
    /// `None` while running; then whether the folder itself could be listed.
    outcome: Arc<Mutex<Option<Result<(), String>>>>,
}

/// Which filesystem the start-screen browser is showing.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum BrowseMode {
    /// The local filesystem (Home and the usual user folders + the filesystem root), via `std::fs`. The default — it needs no
    /// credentials and is instant.
    #[default]
    Local,
    /// Cloud object storage (buckets / prefixes), via `object_store` + the AWS chain.
    Remote,
}

/// What the rendered browse tree asks [`TableizerApp::show_landing`] to do afterwards.
enum BrowseAction {
    None,
    /// List a remote folder's children (its URL; `""` = root) on a background thread.
    Load(String),
    /// Open the chosen file (URL or local path).
    Open(String),
    /// Re-list the current root (re-discover buckets / re-read the local places).
    Refresh,
    /// Add a typed bucket/prefix URL (remote) or path (local) as a top-level node and expand it.
    Goto(String),
    /// Switch between the local and remote filesystems.
    ToggleMode,
}

/// Which tab the right-side panel shows. Columns and Parsing need a loaded file (Parsing only for
/// delimited text); Settings is always available.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PanelTab {
    #[default]
    Columns,
    Parsing,
    Settings,
}

impl PanelTab {
    fn label(self) -> &'static str {
        match self {
            PanelTab::Columns => "Columns",
            PanelTab::Parsing => "Parsing",
            PanelTab::Settings => "Settings",
        }
    }
}

impl TableizerApp {
    pub(crate) fn new(target: Option<String>) -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // Fast family list now (metadata only); refine monospace flags off-thread (parsing fonts to
        // measure advances is slow, and would otherwise stall startup).
        let font_families = fonts::installed_families(&db, false);
        let fonts_db = std::sync::Arc::new(db);
        let (font_tx, font_rx) = std::sync::mpsc::channel();
        {
            let fonts_db = std::sync::Arc::clone(&fonts_db);
            std::thread::spawn(move || {
                let _ = font_tx.send(fonts::installed_families(&fonts_db, true));
            });
        }
        // Defer opening the CLI target to the first frame (a remote URL needs a `Context` for its
        // download-progress UI); a local path opens effectively instantly there too.
        Self {
            view: View::Empty,
            recent: recent::load(),
            theme: prefs::load(),
            applied_theme: None,
            applied_title: None,
            fonts_db,
            font_families,
            font_rx: Some(font_rx),
            applied_table_font: None,
            font_search: String::new(),
            font_mono_only: false,
            panel_open: false,
            panel_tab: PanelTab::default(),
            export_job: None,
            download_job: None,
            decompress_job: None,
            pending_target: target,
            cloud: cloud::load(),
            browse_mode: BrowseMode::default(),
            browse_root: ChildState::Unloaded,
            local_root: ChildState::Unloaded,
            browse_jobs: Vec::new(),
            browse_goto: String::new(),
            goto_complete: GotoCompletion::default(),
            goto_focus: Arc::default(),
            listings: Arc::default(),
            tree_read_aheads: Vec::new(),
        }
    }

    /// Rebuild and install the font atlas (chrome + table fonts) for the current settings.
    pub(crate) fn install_fonts(&mut self, ctx: &egui::Context) {
        let definitions = fonts::definitions(&self.fonts_db, self.theme.table_font.as_deref());
        ctx.set_fonts(definitions);
        self.applied_table_font = self.theme.table_font.clone();
    }

    /// Open a target chosen anywhere (CLI arg, Open dialog, recent, macOS Open-With): a remote URL is
    /// downloaded to the local cache on a background thread (progress UI), then opened; a local path
    /// opens directly. Always resets the grid's stored column widths so the new file auto-fits.
    fn open_target(&mut self, target: String, ctx: &egui::Context) {
        if tableizer_core::remote::is_remote(&target) {
            self.start_download(target, ctx);
        } else {
            self.open_prepared(PathBuf::from(&target), target, ctx);
        }
    }

    /// A local file is in hand (opened directly, or just downloaded): decompress it first if it's
    /// gzipped (on a background thread), otherwise open it. `origin` is the user-facing label (URL or
    /// path) for the status bar / recent / saved views.
    fn open_prepared(&mut self, local: PathBuf, origin: String, ctx: &egui::Context) {
        if tableizer_core::gzip::is_gzip(&local) {
            self.start_decompress(local, origin, ctx);
        } else {
            self.open_resolved(local, origin, ctx);
        }
    }

    /// Open a resolved **local, uncompressed** file (`engine_path` — the file itself, a downloaded
    /// copy, or a decompressed copy) under the given `origin` label. `origin` keys recent files and
    /// saved views so they survive a re-download/re-decompress to a different cache filename. Resets
    /// the grid's stored column widths so the new file auto-fits.
    fn open_resolved(&mut self, engine_path: PathBuf, origin: String, ctx: &egui::Context) {
        ctx.data_mut(|d| d.remove_by_type::<egui_table::TableState>());
        let path = engine_path;
        let format = detect_format(&path);
        // The saved view (column layout/sort/filter) applies to every format; only the delimiter
        // override within it is delimited-specific. Keyed by `origin`, not the local cache path.
        let saved = views::load(Path::new(&origin)).unwrap_or_default();
        // Delimiter sniffing + override only make sense for delimited text; the other formats carry
        // their own schema, so they use a default dialect (header on → exports include column names).
        let (dialect, detected_delimiter, delimiter_auto) = match format {
            Format::Delimited => {
                let mut dialect = sniff_file(&path);
                let detected_delimiter = dialect.delimiter;
                // A persisted delimiter override must be applied *before* opening (it changes the
                // column structure); the rest of the saved view is applied after.
                let delimiter_auto = match saved.delimiter {
                    Some(byte) => {
                        dialect.delimiter = byte;
                        false
                    }
                    None => true,
                };
                (dialect, detected_delimiter, delimiter_auto)
            }
            Format::Json(_) | Format::Parquet => (Dialect::default(), b',', true),
        };
        // UTF-16 is transcoded to UTF-8 by the engine; single-byte encodings default to UTF-8 here and
        // can be switched to Windows-1252 via the Parsing tab.
        let encoding: &'static Encoding = encoding_rs::UTF_8;
        self.view = match open_table(&path, format, dialect) {
            Ok(table) => {
                let mut layout = GridLayout::new(table.schema().columns.len());
                let mut view = ViewControls::default();
                saved.apply(&mut layout, &mut view);
                recent::add(&mut self.recent, Path::new(&origin));
                View::Loaded(Box::new(LoadedTable {
                    delimiter_input: delimiter_display(dialect.delimiter),
                    detected_delimiter,
                    delimiter_auto,
                    format,
                    path,
                    origin,
                    table,
                    layout,
                    dialect,
                    encoding,
                    view,
                    saved,
                    find_nav: None,
                }))
            }
            Err(error) => View::Failed { path, error },
        };
    }

    /// The panel tabs available right now. Columns + (delimited-only) Parsing need a loaded file;
    /// Settings is always present.
    fn available_tabs(&self) -> Vec<PanelTab> {
        let mut tabs = Vec::with_capacity(3);
        if let View::Loaded(loaded) = &self.view {
            tabs.push(PanelTab::Columns);
            if loaded.format == Format::Delimited {
                tabs.push(PanelTab::Parsing);
            }
        }
        tabs.push(PanelTab::Settings);
        tabs
    }

    /// Keep `panel_tab` on a currently-available tab (the file may have closed or changed format).
    fn fix_panel_tab(&mut self) {
        if !self.available_tabs().contains(&self.panel_tab) {
            self.panel_tab = if matches!(self.view, View::Loaded(_)) {
                PanelTab::Columns
            } else {
                PanelTab::Settings
            };
        }
    }

    /// Render the right panel's tab strip (with a close button) and the active tab's contents.
    fn side_panel_contents(&mut self, ui: &mut egui::Ui) {
        let tabs = self.available_tabs();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for tab in &tabs {
                if ui
                    .selectable_label(self.panel_tab == *tab, tab.label())
                    .clicked()
                {
                    self.panel_tab = *tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if close_button(ui).clicked() {
                    self.panel_open = false;
                }
            });
        });
        ui.separator();
        match self.panel_tab {
            PanelTab::Columns => {
                if let View::Loaded(loaded) = &mut self.view {
                    columns_tab(ui, loaded);
                }
            }
            PanelTab::Parsing => {
                if let View::Loaded(loaded) = &mut self.view {
                    parsing_tab(ui, loaded);
                }
            }
            PanelTab::Settings => settings_tab(
                ui,
                &mut self.theme,
                &self.font_families,
                &mut self.font_search,
                &mut self.font_mono_only,
                &mut self.cloud,
            ),
        }
    }

    /// Whether an export thread is still running (its outcome isn't in yet).
    fn export_running(&self) -> bool {
        self.export_job
            .as_ref()
            .is_some_and(|j| j.outcome.lock().expect("export outcome lock").is_none())
    }

    /// Begin exporting the loaded table to a user-chosen file, on a background thread. The columns +
    /// names are gathered here (they need the loaded table); the native save dialog runs on the UI
    /// thread; then the actual write — potentially minutes on a huge file — happens off-thread,
    /// reporting progress and cancellable, per the Tier-C contract.
    fn start_export(&mut self, scope: ExportScope, kind: ExportKind, ctx: &egui::Context) {
        if self.export_running() {
            return; // one export at a time
        }
        let View::Loaded(loaded) = &self.view else {
            return;
        };
        let schema = loaded.table.schema();
        let columns: Vec<ColumnId> = match scope {
            ExportScope::CurrentView => loaded.layout.displayed(),
            ExportScope::Source => (0..schema.columns.len() as u32).map(ColumnId).collect(),
        };
        // Names (CSV header / NDJSON keys / Parquet columns) are the *raw* source bytes, so they
        // match the exported cells (also raw bytes) regardless of the display encoding.
        let names: Vec<Vec<u8>> = columns.iter().map(|&c| raw_name(schema, c)).collect();
        let has_header = loaded.dialect.has_header;
        let total = match loaded.table.row_count() {
            RowCount::Exact(n) | RowCount::AtLeast(n) => n,
        };
        let table: SharedTable = Arc::clone(&loaded.table);

        let Some(path) = rfd::FileDialog::new()
            .set_file_name(format!("export.{}", kind.extension()))
            .save_file()
        else {
            return; // user cancelled the save dialog
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        let cancel = CancellationToken::new();
        let progress = Arc::new(AtomicU64::new(0));
        let outcome: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
        self.export_job = Some(ExportJob {
            file_name,
            progress: Arc::clone(&progress),
            total,
            cancel: cancel.clone(),
            outcome: Arc::clone(&outcome),
        });

        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = run_export(
                table.as_ref(),
                &path,
                kind,
                scope,
                &columns,
                &names,
                has_header,
                &cancel,
                &progress,
            );
            *outcome.lock().expect("export outcome lock") = Some(result);
            ctx.request_repaint(); // wake the idle UI to show the result
        });
    }

    /// Render the export dialog (progress + Cancel while running; result + dismiss when done).
    fn show_export(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.export_job else {
            return;
        };
        let outcome = job.outcome.lock().expect("export outcome lock").clone();
        let mut dismiss = false;
        egui::Window::new("Export")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(280.0);
                match &outcome {
                    None => {
                        let done = job.progress.load(Ordering::Relaxed);
                        ui.label(format!("Exporting {}…", job.file_name));
                        ui.add_space(4.0);
                        let frac = if job.total > 0 {
                            done as f32 / job.total as f32
                        } else {
                            0.0
                        };
                        ui.add(egui::ProgressBar::new(frac).show_percentage());
                        ui.add_space(2.0);
                        ui.label(format!(
                            "{} / {} rows",
                            fmt_count(done),
                            fmt_count(job.total)
                        ));
                        ui.add_space(6.0);
                        if ui.button("Cancel").clicked() {
                            job.cancel.cancel();
                        }
                        ctx.request_repaint(); // keep the progress bar moving
                    }
                    Some(Ok(())) => {
                        ui.label(format!("Exported {}.", job.file_name));
                        ui.add_space(6.0);
                        dismiss = ui.button("Done").clicked();
                    }
                    Some(Err(error)) => {
                        if job.cancel.is_cancelled() {
                            ui.label("Export cancelled.");
                        } else {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!("Export failed: {error}"),
                            );
                        }
                        ui.add_space(6.0);
                        dismiss = ui.button("Close").clicked();
                    }
                }
            });
        if dismiss {
            self.export_job = None;
        }
    }

    /// Capture the user's cloud-auth choice for a worker thread acting on `target` (download/list).
    fn cloud_auth(&self, target: &str) -> CloudAuth {
        CloudAuth {
            form_options: self.cloud.s3_options(),
            use_chain: self.cloud.uses_aws_chain() && tableizer_core::remote::is_s3(target),
            profile: self.cloud.profile().map(str::to_owned),
            region: self.cloud.region().map(str::to_owned),
        }
    }

    /// Begin downloading a remote URL to the local cache on a background thread (Tier-C: async,
    /// progress within ~100 ms, cancellable) via the engine's `object_store` seam. On completion
    /// [`poll_download`](Self::poll_download) opens the cached file. One download at a time.
    fn start_download(&mut self, target: String, ctx: &egui::Context) {
        if self.download_job.is_some() {
            return; // one download at a time
        }
        let Some(cache_root) = tableizer_core::remote::cache_dir() else {
            self.view = View::Failed {
                path: PathBuf::from(&target),
                error: "no cache directory is available for downloads".to_string(),
            };
            return;
        };
        let cancel = CancellationToken::new();
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let outcome: Arc<Mutex<Option<Result<PathBuf, String>>>> = Arc::new(Mutex::new(None));
        self.download_job = Some(DownloadJob {
            target: target.clone(),
            progress: Arc::clone(&progress),
            total: Arc::clone(&total),
            cancel: cancel.clone(),
            outcome: Arc::clone(&outcome),
        });

        let auth = self.cloud_auth(&target);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<PathBuf, String> {
                let options = auth.resolve()?; // env / SSO / profile / static, resolved off the UI thread
                tableizer_core::remote::fetch_to_cache(
                    &target,
                    &cache_root,
                    &options,
                    &progress,
                    &total,
                    &cancel,
                )
                .map_err(|e| e.to_string())
            })();
            *outcome.lock().expect("download outcome lock") = Some(result);
            ctx.request_repaint(); // wake the idle UI to apply the result
        });
    }

    /// Poll a running download: on success open the cached file (decompressing first if it's gzipped);
    /// a failure is left in `download_job` for [`show_download`] to surface.
    fn poll_download(&mut self, ctx: &egui::Context) {
        let outcome = self
            .download_job
            .as_ref()
            .and_then(|j| j.outcome.lock().expect("download outcome lock").clone());
        match outcome {
            Some(Ok(local)) => {
                let origin = self
                    .download_job
                    .take()
                    .map(|j| j.target)
                    .unwrap_or_default();
                self.open_prepared(local, origin, ctx);
            }
            Some(Err(_)) => {} // leave the job; show_download renders the error + Close
            None => {
                if self.download_job.is_some() {
                    ctx.request_repaint(); // keep the progress bar moving
                }
            }
        }
    }

    /// Begin decompressing a gzipped local file to the cache on a background thread (progress +
    /// cancel). On completion [`poll_decompress`](Self::poll_decompress) opens the result.
    fn start_decompress(&mut self, gz_path: PathBuf, origin: String, ctx: &egui::Context) {
        if self.decompress_job.is_some() {
            return; // one at a time
        }
        let Some(cache_root) = tableizer_core::gzip::cache_dir() else {
            self.view = View::Failed {
                path: gz_path,
                error: "no cache directory is available for decompression".to_string(),
            };
            return;
        };
        let cancel = CancellationToken::new();
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let outcome: Arc<Mutex<Option<Result<PathBuf, String>>>> = Arc::new(Mutex::new(None));
        self.decompress_job = Some(DecompressJob {
            origin,
            progress: Arc::clone(&progress),
            total: Arc::clone(&total),
            cancel: cancel.clone(),
            outcome: Arc::clone(&outcome),
        });
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = tableizer_core::gzip::decompress_to_cache(
                &gz_path,
                &cache_root,
                &progress,
                &total,
                &cancel,
            )
            .map_err(|e| e.to_string());
            *outcome.lock().expect("decompress outcome lock") = Some(result);
            ctx.request_repaint();
        });
    }

    /// Poll a running decompression: on success open the decompressed file; a failure is left in
    /// `decompress_job` for [`show_decompress`] to surface.
    fn poll_decompress(&mut self, ctx: &egui::Context) {
        let outcome = self
            .decompress_job
            .as_ref()
            .and_then(|j| j.outcome.lock().expect("decompress outcome lock").clone());
        match outcome {
            Some(Ok(decompressed)) => {
                let origin = self
                    .decompress_job
                    .take()
                    .map(|j| j.origin)
                    .unwrap_or_default();
                self.open_resolved(decompressed, origin, ctx);
            }
            Some(Err(_)) => {} // leave the job; show_decompress renders the error + Close
            None => {
                if self.decompress_job.is_some() {
                    ctx.request_repaint();
                }
            }
        }
    }

    /// Render the decompression dialog: progress + Cancel while running, an error + Close on failure
    /// (a success is opened by [`poll_decompress`], so the dialog just closes next frame).
    fn show_decompress(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.decompress_job else {
            return;
        };
        let outcome = job.outcome.lock().expect("decompress outcome lock").clone();
        let mut dismiss = false;
        egui::Window::new("Decompressing")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                match &outcome {
                    None => {
                        let done = job.progress.load(Ordering::Relaxed);
                        let total = job.total.load(Ordering::Relaxed);
                        ui.label(format!("Decompressing {}…", job.origin));
                        ui.add_space(4.0);
                        let frac = if total > 0 {
                            done as f32 / total as f32
                        } else {
                            0.0
                        };
                        ui.add(egui::ProgressBar::new(frac).show_percentage());
                        ui.add_space(6.0);
                        if ui.button("Cancel").clicked() {
                            job.cancel.cancel();
                        }
                        ctx.request_repaint();
                    }
                    Some(Ok(_)) => {} // opened by poll_decompress; closes next frame
                    Some(Err(error)) => {
                        if job.cancel.is_cancelled() {
                            ui.label("Decompression cancelled.");
                        } else {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!("Decompression failed: {error}"),
                            );
                        }
                        ui.add_space(6.0);
                        dismiss = ui.button("Close").clicked();
                    }
                }
            });
        if dismiss {
            self.decompress_job = None;
        }
    }

    /// Render the download dialog: progress + Cancel while running, an error + Close on failure (a
    /// success is opened by [`poll_download`], so the dialog just closes next frame).
    fn show_download(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.download_job else {
            return;
        };
        let outcome = job.outcome.lock().expect("download outcome lock").clone();
        let mut dismiss = false;
        egui::Window::new("Downloading")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                match &outcome {
                    None => {
                        let done = job.progress.load(Ordering::Relaxed);
                        let total = job.total.load(Ordering::Relaxed);
                        ui.label(format!("Downloading {}…", job.target));
                        ui.add_space(4.0);
                        let frac = if total > 0 {
                            done as f32 / total as f32
                        } else {
                            0.0
                        };
                        ui.add(egui::ProgressBar::new(frac).show_percentage());
                        ui.add_space(2.0);
                        ui.label(if total > 0 {
                            format!("{} / {}", fmt_bytes(done), fmt_bytes(total))
                        } else {
                            format!("{} downloaded", fmt_bytes(done))
                        });
                        ui.add_space(6.0);
                        if ui.button("Cancel").clicked() {
                            job.cancel.cancel();
                        }
                        ctx.request_repaint();
                    }
                    Some(Ok(_)) => {} // opened by poll_download; this dialog closes next frame
                    Some(Err(error)) => {
                        if job.cancel.is_cancelled() {
                            ui.label("Download cancelled.");
                        } else {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!("Download failed: {error}"),
                            );
                        }
                        ui.add_space(6.0);
                        dismiss = ui.button("Close").clicked();
                    }
                }
            });
        if dismiss {
            self.download_job = None;
        }
    }

    /// The S3 connection parameters for bucket discovery, from the Settings cloud config.
    fn s3_auth(&self) -> tableizer_core::remote::S3Auth {
        let c = &self.cloud;
        // Static keys only apply in the static-keys mode; otherwise the AWS chain (incl. SSO) is used.
        let field =
            |value: &str| (!c.uses_aws_chain() && !value.is_empty()).then(|| value.to_owned());
        tableizer_core::remote::S3Auth {
            profile: c.profile().map(str::to_owned),
            region: c.region().map(str::to_owned),
            access_key_id: field(&c.access_key_id),
            secret_access_key: field(&c.secret_access_key),
            session_token: field(&c.session_token),
            endpoint: field(&c.endpoint),
        }
    }

    /// List a tree location (its node already marked `Loading`): `""` discovers the buckets on a
    /// background thread; a folder is **read ahead** from, and its listing installed from the shared
    /// cache as soon as it's there — at once, if an earlier read-ahead already listed it. Either way
    /// [`poll_browse`](Self::poll_browse) installs the result. Several may run at once.
    fn load_children(&mut self, location: String, ctx: &egui::Context) {
        if !location.is_empty() {
            install_cached(&mut self.browse_root, &self.listings);
            let focus = Arc::new(Mutex::new(location.clone()));
            let job = self.spawn_read_ahead(location, focus, ctx);
            self.tree_read_aheads.push(job);
            return;
        }
        let cancel = CancellationToken::new();
        let outcome: Arc<Mutex<Option<Result<tableizer_core::remote::DirListing, String>>>> =
            Arc::new(Mutex::new(None));
        self.browse_jobs.push(BrowseJob {
            location: location.clone(),
            cancel: cancel.clone(),
            outcome: Arc::clone(&outcome),
        });
        // Root: enumerate buckets reachable with the configured credentials.
        let ctx = ctx.clone();
        let auth = self.s3_auth();
        std::thread::spawn(move || {
            let result = tableizer_core::remote::list_s3_buckets(&auth).map_err(|e| e.to_string());
            *outcome.lock().expect("browse outcome lock") = Some(result);
            ctx.request_repaint();
        });
    }

    /// Install any finished listings onto their tree node (root for `""`, else the folder by URL). A
    /// result for a node no longer in the tree (e.g. after Refresh) is dropped.
    fn poll_browse(&mut self, ctx: &egui::Context) {
        let mut finished = Vec::new();
        let mut running = false;
        self.browse_jobs.retain(|job| {
            match job.outcome.lock().expect("browse outcome lock").take() {
                Some(result) => {
                    finished.push((job.location.clone(), result));
                    false
                }
                None => {
                    running = true;
                    true
                }
            }
        });
        for (location, result) in finished {
            if let (true, Ok(listing)) = (location.is_empty(), &result) {
                // The buckets also feed the "go to" field's suggestions at `s3://`.
                let listing = listing.clone();
                self.listings.insert(
                    "s3://",
                    CachedListing {
                        listing,
                        complete: true,
                    },
                );
            }
            let state = match result {
                Ok(listing) => ChildState::Loaded(nodes_from(listing)),
                Err(error) => ChildState::Failed(error),
            };
            if location.is_empty() {
                self.browse_root = state;
            } else if let Some(node) = find_node_mut(&mut self.browse_root, &location) {
                node.children = state;
            }
        }
        // Read-ahead tree folders: install each listing as it lands (the read-ahead repaints), and
        // mark the folders that could not be listed.
        install_cached(&mut self.browse_root, &self.listings);
        let mut failed = Vec::new();
        self.tree_read_aheads.retain(|job| {
            match job.outcome.lock().expect("read-ahead outcome lock").take() {
                Some(Err(error)) => {
                    failed.push((job.root.clone(), error));
                    false
                }
                Some(Ok(())) => false,
                None => true,
            }
        });
        for (root, error) in failed {
            if let Some(node) = find_node_mut(&mut self.browse_root, &root)
                && matches!(node.children, ChildState::Loading)
            {
                node.children = ChildState::Failed(error);
            }
        }
        if running {
            ctx.request_repaint();
        }
    }

    /// The active browse tree's root, by mode.
    fn active_root_mut(&mut self) -> &mut ChildState {
        match self.browse_mode {
            BrowseMode::Remote => &mut self.browse_root,
            BrowseMode::Local => &mut self.local_root,
        }
    }

    /// Add a typed target as a top-level tree node and expand it: a non-discovered bucket / deep prefix
    /// (remote), or any directory path (local). Already-present nodes are just expanded — reusing the
    /// cached subtree. Local listing happens inline; remote is dispatched to a background thread.
    fn goto_browse(&mut self, target: String, ctx: &egui::Context) {
        if target.is_empty() {
            return;
        }
        let mode = self.browse_mode;
        let root = self.active_root_mut();
        if !matches!(root, ChildState::Loaded(_)) {
            *root = ChildState::Loaded(Vec::new());
        }
        let mut remote_load = false;
        if let ChildState::Loaded(nodes) = root {
            let node = match nodes.iter_mut().position(|n| n.url == target) {
                Some(i) => &mut nodes[i],
                None => {
                    nodes.push(BrowseNode {
                        name: browse_label(&target),
                        url: target.clone(),
                        is_dir: true,
                        size: None,
                        expanded: false,
                        children: ChildState::Unloaded,
                    });
                    nodes.last_mut().expect("just pushed")
                }
            };
            node.expanded = true;
            if matches!(node.children, ChildState::Unloaded | ChildState::Failed(_)) {
                match mode {
                    BrowseMode::Local => {
                        node.children = match list_local_dir(&target) {
                            Ok(children) => ChildState::Loaded(children),
                            Err(error) => ChildState::Failed(error),
                        };
                    }
                    BrowseMode::Remote => {
                        node.children = ChildState::Loading;
                        remote_load = true;
                    }
                }
            }
        }
        if remote_load {
            self.load_children(target, ctx);
        }
    }

    /// Keep the "go to" suggestions in step with the typed text: when its folder part changes, list
    /// that folder afresh (a remote one by reading ahead from it). Local suggestions arrive with their
    /// listing; remote ones are taken from the shared listing cache as the read-ahead fills it.
    fn update_goto_completion(&mut self, ctx: &egui::Context) {
        let mode = self.browse_mode;
        {
            let mut focus = self.goto_focus.lock().expect("goto focus lock");
            if *focus != self.browse_goto {
                focus.clone_from(&self.browse_goto); // steers the read-ahead toward what's typed
            }
        }
        let parent = complete::split(&self.browse_goto, goto_separator(mode))
            .map(|(parent, _)| parent.to_owned());
        if parent != self.goto_complete.parent {
            let completion = &mut self.goto_complete;
            completion.job = None; // a superseded local listing just goes unread
            if let Some(read_ahead) = completion.read_ahead.take() {
                read_ahead.cancel(); // the old folder's read-ahead is no longer wanted
            }
            completion.dirs = None;
            completion.dirs_complete = false;
            completion.parent = parent.clone();
            if let Some(parent) = parent {
                self.start_completion_listing(parent, ctx);
            }
        }
        let completion = &mut self.goto_complete;
        let finished = completion
            .job
            .as_ref()
            .and_then(|outcome| outcome.lock().expect("completion outcome lock").take());
        if let Some(result) = finished {
            completion.job = None;
            completion.dirs = result.ok();
            completion.dirs_complete = true;
        }
        if mode == BrowseMode::Remote
            && !completion.dirs_complete
            && let Some(parent) = &completion.parent
            && let Some((dirs, complete)) = cached_dirs(&self.listings, parent)
        {
            completion.dirs = Some(dirs);
            completion.dirs_complete = complete;
        }
    }

    /// Start listing `parent`'s subfolders for the "go to" suggestions, off the UI thread: a local
    /// directory directly; a remote bucket/prefix by reading ahead from it into the shared cache; the
    /// S3 buckets (at `s3://`) into the cache, unless already there.
    fn start_completion_listing(&mut self, parent: String, ctx: &egui::Context) {
        match self.browse_mode {
            BrowseMode::Local => {
                let outcome: CompletionOutcome = Arc::new(Mutex::new(None));
                self.goto_complete.job = Some(Arc::clone(&outcome));
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let result = complete::list_local_subdirs(Path::new(&parent));
                    *outcome.lock().expect("completion outcome lock") = Some(result);
                    ctx.request_repaint();
                });
            }
            BrowseMode::Remote => match complete::remote_source(&parent) {
                None => {}
                Some(complete::RemoteSource::Buckets) => {
                    if self.listings.get(&parent).is_some() {
                        return;
                    }
                    let auth = self.s3_auth();
                    let listings = Arc::clone(&self.listings);
                    let ctx = ctx.clone();
                    std::thread::spawn(move || {
                        // Best-effort: without credentials there are simply no bucket suggestions.
                        if let Ok(listing) = tableizer_core::remote::list_s3_buckets(&auth) {
                            let complete = true;
                            listings.insert(&parent, CachedListing { listing, complete });
                            ctx.request_repaint();
                        }
                    });
                }
                Some(complete::RemoteSource::Prefix) => {
                    let focus = Arc::clone(&self.goto_focus);
                    let job = self.spawn_read_ahead(parent, focus, ctx);
                    self.goto_complete.read_ahead = Some(job.cancel);
                }
            },
        }
    }

    /// Start a read-ahead from remote folder `root` on a worker thread (see
    /// [`tableizer_core::remote::read_ahead`]), filling the shared listing cache and steered toward
    /// `focus`; each listing that lands repaints, so it shows straight away.
    fn spawn_read_ahead(
        &self,
        root: String,
        focus: Arc<Mutex<String>>,
        ctx: &egui::Context,
    ) -> ReadAheadJob {
        let job = ReadAheadJob {
            root: root.clone(),
            cancel: CancellationToken::new(),
            outcome: Arc::new(Mutex::new(None)),
        };
        let auth = self.cloud_auth(&root);
        let listings = Arc::clone(&self.listings);
        let cancel = job.cancel.clone();
        let outcome = Arc::clone(&job.outcome);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let resolve = || auth.resolve().map_err(tableizer_core::Error::Remote);
            let focus = || focus.lock().expect("read-ahead focus lock").clone();
            let repaint = || ctx.request_repaint();
            let result = tableizer_core::remote::read_ahead(
                &root,
                resolve,
                ReadAheadLimits::default(),
                &focus,
                &listings,
                &cancel,
                &repaint,
            )
            .map_err(|e| e.to_string());
            *outcome.lock().expect("read-ahead outcome lock") = Some(result);
            ctx.request_repaint();
        });
        job
    }

    /// The start screen (shown when no file is open): a left column with recent files and, on the
    /// right, the inline **browser** — a Local/Remote toggle, a jump-to field + Refresh, and the lazy
    /// tree (cloud buckets/prefixes, or the local filesystem). Opening anything here transitions to the
    /// grid; both trees stay cached.
    fn show_landing(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, palette: &theme::Palette) {
        self.update_goto_completion(ctx);
        let mut to_open: Option<PathBuf> = None;
        let mut clear_recent = false;
        let mut action = BrowseAction::None;
        {
            // Split-borrow: the controls column reads `recent`; the browser column mutates the active
            // tree and the jump field. Actions are collected and applied after the borrow ends.
            let TableizerApp {
                recent,
                browse_mode,
                browse_root,
                local_root,
                browse_goto,
                goto_complete,
                ..
            } = self;
            let mode = *browse_mode;
            let root: &mut ChildState = match mode {
                BrowseMode::Remote => browse_root,
                BrowseMode::Local => local_root,
            };
            egui::Panel::left("landing_controls")
                .resizable(true)
                .default_size(300.0)
                .min_size(240.0)
                .max_size(480.0)
                .show(ui, |ui| {
                    empty_view(
                        ui,
                        recent.as_slice(),
                        palette,
                        &mut to_open,
                        &mut clear_recent,
                    );
                });
            egui::CentralPanel::default().show(ui, |ui| {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // The toggle shows the mode it switches *to*.
                    let toggle = match mode {
                        BrowseMode::Remote => "Browse Local",
                        BrowseMode::Local => "Browse Remote",
                    };
                    if ui.button(toggle).clicked() {
                        action = BrowseAction::ToggleMode;
                    }
                    if ui.button("Refresh").clicked() {
                        action = BrowseAction::Refresh;
                    }
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let hint = match mode {
                        BrowseMode::Remote => "s3://bucket/prefix/ — jump to",
                        BrowseMode::Local => "/path/to/folder — jump to",
                    };
                    let width = ui.available_width() - 56.0;
                    let submit = goto_field(ui, browse_goto, goto_complete, mode, hint, width);
                    let ready = !browse_goto.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Go")).clicked() || (submit && ready)
                    {
                        action = BrowseAction::Goto(browse_goto.trim().to_string());
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        let mut rows = ListRows::new(Some(palette.stripe), palette.row_hover);
                        show_browse_children(ui, root, 0, mode, &mut action, &mut rows);
                    });
            });
        }

        if let Some(path) = to_open {
            self.open_target(path.to_string_lossy().into_owned(), ctx);
        }
        if clear_recent {
            recent::clear(&mut self.recent);
        }
        match action {
            BrowseAction::Load(location) => self.load_children(location, ctx),
            BrowseAction::Open(url) => self.open_target(url, ctx),
            BrowseAction::Refresh => {
                // Re-list the folder behind the "go to" suggestions too.
                if let Some(read_ahead) = self.goto_complete.read_ahead.take() {
                    read_ahead.cancel();
                }
                self.goto_complete = GotoCompletion::default();
                ctx.request_repaint();
                match self.browse_mode {
                    BrowseMode::Remote => {
                        for job in self.browse_jobs.drain(..) {
                            job.cancel.cancel(); // abandon in-flight listings of the old tree
                        }
                        for job in self.tree_read_aheads.drain(..) {
                            job.cancel.cancel();
                        }
                        self.listings.clear(); // everything is listed afresh
                        self.browse_root = ChildState::Loading;
                        self.load_children(String::new(), ctx);
                    }
                    BrowseMode::Local => {
                        self.local_root = ChildState::Loaded(local_places());
                    }
                }
            }
            BrowseAction::Goto(target) => self.goto_browse(target, ctx),
            BrowseAction::ToggleMode => {
                self.browse_mode = match self.browse_mode {
                    BrowseMode::Remote => BrowseMode::Local,
                    BrowseMode::Local => BrowseMode::Remote,
                };
                self.browse_goto.clear();
            }
            BrowseAction::None => {}
        }
    }
}

/// The path separators of the "go to" field: the platform's for local paths, `/` for URLs.
fn goto_separator(mode: BrowseMode) -> fn(char) -> bool {
    match mode {
        BrowseMode::Local => std::path::is_separator,
        BrowseMode::Remote => |c| c == '/',
    }
}

/// The folder names in a remote listing (its files dropped), for the "go to" suggestions.
fn folder_names(listing: tableizer_core::remote::DirListing) -> Vec<String> {
    listing
        .entries
        .into_iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.name)
        .collect()
}

/// The "go to" field with its folder-suggestion popup (see [`GotoCompletion`]). ↑/↓ highlight a
/// suggestion; Tab or a click completes to the highlighted one (else the first) and keeps the field
/// focused, so the next level's suggestions follow; Enter completes a highlighted suggestion, and
/// otherwise submits; Escape hides the popup. Returns `true` on submit.
fn goto_field(
    ui: &mut egui::Ui,
    text: &mut String,
    completion: &mut GotoCompletion,
    mode: BrowseMode,
    hint: &str,
    width: f32,
) -> bool {
    let id = goto_field_id();
    let (parent, partial) = complete::split(text, goto_separator(mode)).unwrap_or_default();
    let parent = parent.to_owned();
    // The listing tracks the text as it stood at the start of this frame, as do these suggestions.
    let suggestions: Vec<String> = match &completion.dirs {
        Some(dirs) if completion.parent.as_deref() == Some(parent.as_str()) => {
            complete::matches(dirs, partial)
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
        _ => Vec::new(),
    };
    completion.highlighted = completion.highlighted.filter(|&i| i < suggestions.len());
    let focused = ui.memory(|mem| mem.has_focus(id));
    // Clicking a suggestion takes focus from the field before the click registers, so the popup stays
    // up while a click on it is in progress.
    let clicking =
        completion.open && ui.input(|i| i.pointer.any_down() || i.pointer.any_released());
    let open = !suggestions.is_empty() && !completion.dismissed && (focused || clicking);

    // The popup's keys, taken before the field sees them.
    let mut accepted = None;
    let mut moved = false;
    if open && focused {
        ui.input_mut(|i| {
            let len = suggestions.len();
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                completion.highlighted =
                    complete::move_highlight(completion.highlighted, len, true);
                moved = true;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                completion.highlighted =
                    complete::move_highlight(completion.highlighted, len, false);
                moved = true;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
                accepted = Some(completion.highlighted.unwrap_or(0));
            }
            if completion.highlighted.is_some()
                && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            {
                accepted = completion.highlighted;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                completion.dismissed = true;
            }
        });
    }
    if let Some(i) = accepted {
        complete_to(ui.ctx(), id, text, completion, &parent, &suggestions[i]);
    }

    let response = ui.add(
        egui::TextEdit::singleline(text)
            .id(id)
            .hint_text(hint)
            .desired_width(width)
            // While the popup is up, Tab and Escape act on it instead of moving or dropping focus.
            .event_filter(egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                tab: open,
                escape: open,
            }),
    );
    if response.changed() {
        completion.highlighted = None;
        completion.dismissed = false;
        // Re-evaluate the suggestions (and the keys the field captures) before the next key arrives.
        ui.ctx().request_repaint();
    }
    let submit = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

    completion.open = open;
    let mut clicked = None;
    egui::Popup::from_response(&response)
        .open(open)
        .width(response.rect.width())
        .show(|ui| {
            egui::ScrollArea::vertical()
                .max_height(240.0)
                .show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        for (i, name) in suggestions.iter().enumerate() {
                            let highlighted = completion.highlighted == Some(i);
                            let item = ui.selectable_label(highlighted, name);
                            if highlighted && moved {
                                item.scroll_to_me(None);
                            }
                            if item.clicked() {
                                clicked = Some(i);
                            }
                        }
                    });
                });
        });
    if let Some(i) = clicked {
        complete_to(ui.ctx(), id, text, completion, &parent, &suggestions[i]);
        response.request_focus();
    }
    submit
}

/// The "go to" field's widget id — fixed, as there is only ever one.
fn goto_field_id() -> egui::Id {
    egui::Id::new("browse_goto")
}

/// Complete the "go to" field to folder `name` in `parent`, cursor at the end, ready to type on.
fn complete_to(
    ctx: &egui::Context,
    id: egui::Id,
    text: &mut String,
    completion: &mut GotoCompletion,
    parent: &str,
    name: &str,
) {
    *text = complete::accept(parent, name);
    completion.highlighted = None;
    completion.dismissed = false;
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let end = egui::text::CCursor::new(text.chars().count());
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(end)));
        egui::TextEdit::store_state(ctx, id, state);
    }
    ctx.request_repaint(); // list the new folder straight away
}

/// Build tree nodes (each initially unexpanded/unloaded) from a directory listing.
fn nodes_from(listing: tableizer_core::remote::DirListing) -> Vec<BrowseNode> {
    listing
        .entries
        .into_iter()
        .map(|e| BrowseNode {
            url: e.url,
            name: e.name,
            is_dir: e.is_dir,
            size: e.size,
            expanded: false,
            children: ChildState::Unloaded,
        })
        .collect()
}

/// Fill every remote tree folder still waiting on its listing (`Loading`) whose complete listing has
/// reached the shared cache — read ahead earlier, or just landed from its own read-ahead.
fn install_cached(state: &mut ChildState, listings: &ListingCache) {
    let ChildState::Loaded(nodes) = state else {
        return;
    };
    for node in nodes.iter_mut() {
        if matches!(node.children, ChildState::Loading)
            && let Some(cached) = listings.get(&node.url).filter(|c| c.complete)
        {
            node.children = ChildState::Loaded(nodes_from(cached.listing));
        }
        install_cached(&mut node.children, listings);
    }
}

/// The "go to" suggestions for remote folder `parent` from the shared cache: its subfolder names,
/// and whether that listing is complete (a read-ahead may have read only its first page).
fn cached_dirs(listings: &ListingCache, parent: &str) -> Option<(Vec<String>, bool)> {
    let cached = listings.get(parent)?;
    Some((folder_names(cached.listing), cached.complete))
}

/// Find the folder node with URL `url` anywhere in the tree, to install a finished listing onto it.
fn find_node_mut<'a>(state: &'a mut ChildState, url: &str) -> Option<&'a mut BrowseNode> {
    let ChildState::Loaded(nodes) = state else {
        return None;
    };
    for node in nodes.iter_mut() {
        if node.url == url {
            return Some(node);
        }
        if let Some(found) = find_node_mut(&mut node.children, url) {
            return Some(found);
        }
    }
    None
}

/// A short display name for a typed URL (`s3://bucket/a/b/` → `b`, `s3://bucket/` → `bucket`).
fn browse_label(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(url)
        .to_string()
}

/// Render one tree level (a folder's children, or the bucket root) at `depth`, collecting any action.
fn show_browse_children(
    ui: &mut egui::Ui,
    state: &mut ChildState,
    depth: usize,
    mode: BrowseMode,
    action: &mut BrowseAction,
    rows: &mut ListRows,
) {
    let indent = depth as f32 * 16.0 + 18.0;
    match state {
        ChildState::Unloaded => {}
        ChildState::Loading => {
            rows.status(ui, |ui| {
                ui.add_space(indent);
                ui.weak("Listing…");
            });
        }
        ChildState::Failed(error) => {
            rows.status(ui, |ui| {
                ui.add_space(indent);
                ui.colored_label(ui.visuals().error_fg_color, error.as_str());
            });
        }
        ChildState::Loaded(nodes) if nodes.is_empty() => {
            rows.status(ui, |ui| {
                ui.add_space(indent);
                ui.weak("(empty)");
            });
        }
        ChildState::Loaded(nodes) => {
            for node in nodes.iter_mut() {
                show_browse_node(ui, node, depth, mode, action, rows);
            }
        }
    }
}

/// Render one tree node (folder = expandable disclosure; file = clickable row with size) and recurse.
fn show_browse_node(
    ui: &mut egui::Ui,
    node: &mut BrowseNode,
    depth: usize,
    mode: BrowseMode,
    action: &mut BrowseAction,
    rows: &mut ListRows,
) {
    let indent = depth as f32 * 16.0;
    if node.is_dir {
        let row = rows.item(ui, ui.id().with(&node.url), |ui| {
            ui.add_space(indent);
            ui.spacing_mut().item_spacing.x = 2.0;
            disclosure(ui, node.expanded);
            ui.add(egui::Label::new(node.name.as_str()).selectable(false));
        });
        if row.clicked() {
            node.expanded = !node.expanded;
            // Expanding an unlisted (or previously failed) folder lists it once. Local listing is fast,
            // so it's done inline here (mutating the node directly); a remote listing is dispatched to
            // a background thread via a `Load` action.
            if node.expanded
                && matches!(node.children, ChildState::Unloaded | ChildState::Failed(_))
            {
                match mode {
                    BrowseMode::Local => {
                        node.children = match list_local_dir(&node.url) {
                            Ok(children) => ChildState::Loaded(children),
                            Err(error) => ChildState::Failed(error),
                        };
                    }
                    BrowseMode::Remote => {
                        node.children = ChildState::Loading;
                        *action = BrowseAction::Load(node.url.clone());
                    }
                }
            }
        }
        if node.expanded {
            show_browse_children(ui, &mut node.children, depth + 1, mode, action, rows);
        }
    } else {
        let row = rows.item(ui, ui.id().with(&node.url), |ui| {
            ui.add_space(indent + 18.0); // align past the disclosure column
            ui.add(egui::Label::new(node.name.as_str()).selectable(false));
            if let Some(size) = node.size {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(fmt_bytes(size)).weak())
                            .selectable(false),
                    );
                });
            }
        });
        if row.clicked() {
            *action = BrowseAction::Open(node.url.clone());
        }
    }
}

/// A small disclosure triangle (▶ collapsed / ▼ expanded) drawn as a shape (font-independent, like the
/// grid's painted arrows). Just an indicator: the whole folder row takes the click.
fn disclosure(ui: &mut egui::Ui, open: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    let color = ui.visuals().weak_text_color();
    let c = rect.center();
    let points = if open {
        vec![
            egui::pos2(c.x - 4.0, c.y - 2.0),
            egui::pos2(c.x + 4.0, c.y - 2.0),
            egui::pos2(c.x, c.y + 3.0),
        ]
    } else {
        vec![
            egui::pos2(c.x - 2.0, c.y - 4.0),
            egui::pos2(c.x - 2.0, c.y + 4.0),
            egui::pos2(c.x + 3.0, c.y),
        ]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        color,
        egui::Stroke::NONE,
    ));
}

/// The window's title: the open file's name — the last segment of its `origin` (local path or URL),
/// never the full path — else the app's name.
fn window_title(origin: Option<&str>) -> String {
    origin
        .and_then(|origin| Path::new(origin).file_name())
        .map_or_else(
            || "Tableizer".to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
}

/// The top-level entries of the **local** browse tree: Home, the Desktop, Downloads and Documents
/// folders (wherever the platform keeps them, when they exist), and the filesystem root. The user
/// expands down from these (or jumps via "go to").
fn local_places() -> Vec<BrowseNode> {
    let dirs = directories::UserDirs::new();
    let dirs = dirs.as_ref();
    // The filesystem root (Unix `/`; on Windows this is the root of the home drive — multi-drive
    // listing is a future refinement).
    let root = dirs
        .and_then(|d| d.home_dir().ancestors().last().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/"));
    let named = [
        ("Home", dirs.map(|d| d.home_dir())),
        ("Desktop", dirs.and_then(|d| d.desktop_dir())),
        ("Downloads", dirs.and_then(|d| d.download_dir())),
        ("Documents", dirs.and_then(|d| d.document_dir())),
    ];
    places(&named, &root)
}

/// The local tree's top-level folders: the `named` ones in order — skipping any the platform doesn't
/// have, or that don't exist — then the filesystem `root`, named by its path. Each is a folder node
/// whose `url` is its local path.
fn places(named: &[(&str, Option<&Path>)], root: &Path) -> Vec<BrowseNode> {
    named
        .iter()
        .filter_map(|&(name, path)| path.filter(|p| p.is_dir()).map(|p| place(name, p)))
        .chain([place(&root.to_string_lossy(), root)])
        .collect()
}

/// A top-level folder node of the local tree.
fn place(name: &str, path: &Path) -> BrowseNode {
    BrowseNode {
        url: path.to_string_lossy().into_owned(),
        name: name.to_string(),
        is_dir: true,
        size: None,
        expanded: false,
        children: ChildState::Unloaded,
    }
}

/// List a **local** directory into tree nodes (folders first, then files, each by name). Hidden
/// entries (dot-files) are skipped. Returns an error message (e.g. permission denied) on failure.
fn list_local_dir(path: &str) -> Result<Vec<BrowseNode>, String> {
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // skip hidden entries
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = (!is_dir)
            .then(|| entry.metadata().ok().map(|m| m.len()))
            .flatten();
        let node = BrowseNode {
            url: entry.path().to_string_lossy().into_owned(),
            name,
            is_dir,
            size,
            expanded: false,
            children: ChildState::Unloaded,
        };
        if is_dir { &mut folders } else { &mut files }.push(node);
    }
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    folders.append(&mut files);
    Ok(folders)
}

impl eframe::App for TableizerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Pick up the background-measured font-family list (full monospace flags) when it's ready.
        let measured = self.font_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(families) = measured {
            self.font_families = families;
            self.font_rx = None;
        }

        // A running/finished export shows its own progress dialog (cancellable, off the UI thread).
        self.show_export(&ctx);

        // Open a CLI target queued at startup, now that a `Context` exists to drive a download's UI.
        if let Some(target) = self.pending_target.take() {
            self.open_target(target, &ctx);
        }
        // A running/finished remote download: poll it (opening the cached file on success) and show
        // its progress dialog.
        self.poll_download(&ctx);
        self.show_download(&ctx);
        // A gzipped file (local or just-downloaded) is decompressed to the cache before opening.
        self.poll_decompress(&ctx);
        self.show_decompress(&ctx);
        // The start-screen browser: populate the active root on first landing (each tree is cached
        // thereafter) and install any finished remote listings; rendering + actions are in `show_landing`.
        if matches!(self.view, View::Empty) {
            match self.browse_mode {
                BrowseMode::Remote if matches!(self.browse_root, ChildState::Unloaded) => {
                    self.browse_root = ChildState::Loading;
                    self.load_children(String::new(), &ctx);
                }
                BrowseMode::Local if matches!(self.local_root, ChildState::Unloaded) => {
                    self.local_root = ChildState::Loaded(local_places());
                }
                _ => {}
            }
        }
        self.poll_browse(&ctx);

        // Title the window after the open file (its name only), else the app.
        let origin = match &self.view {
            View::Loaded(loaded) => Some(loaded.origin.as_str()),
            _ => None,
        };
        let title = window_title(origin);
        if self.applied_title.as_ref() != Some(&title) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.applied_title = Some(title);
        }

        // Resolve the theme (following the OS for `Auto`) and restyle only when it changes.
        let system_dark = ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark);
        let (style, palette) = theme::build(&self.theme, system_dark);
        if self.applied_theme.as_ref() != Some(&(self.theme.clone(), system_dark)) {
            ctx.set_global_style(style.clone());
            self.applied_theme = Some((self.theme.clone(), system_dark));
            // `set_global_style` only reaches uis created afterwards (and ctx-level popups), but this
            // frame's root `ui` already exists with the old style. Apply directly so the named text
            // styles resolve this frame too — otherwise the first frame panics on lookup.
            ui.set_style(style);
            // Match the OS window chrome (title bar) to the resolved theme, so it flips along with
            // the app colors instead of staying on the OS default.
            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(
                if theme::is_dark(&self.theme, system_dark) {
                    egui::SystemTheme::Dark
                } else {
                    egui::SystemTheme::Light
                },
            ));
        }
        // Rebuild the font atlas only when the chosen table font changes.
        if self.applied_table_font != self.theme.table_font {
            self.install_fonts(&ctx);
        }

        let mut to_open: Option<PathBuf> = None;
        let mut to_export: Option<ExportRequest> = None;
        let theme_before = self.theme.clone();
        let cloud_before = self.cloud.clone();
        let dialect_before = match &self.view {
            View::Loaded(loaded) => Some(loaded.dialect),
            _ => None,
        };
        // ⌘/Ctrl+F focuses the Find field.
        let focus_find = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F));
        // ⌘Q / Ctrl+Q quits the app.
        if ctx.input_mut(|i| i.consume_shortcut(&QUIT_SHORTCUT)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // ⌘W / Ctrl+W closes the current file (no-op when none is open).
        if ctx.input_mut(|i| i.consume_shortcut(&CLOSE_SHORTCUT))
            && matches!(self.view, View::Loaded(_))
        {
            self.view = View::Empty;
        }
        // ⌘, / Ctrl+, opens the right panel on the Settings tab (toggles it closed if already there).
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS_SHORTCUT)) {
            if self.panel_open && self.panel_tab == PanelTab::Settings {
                self.panel_open = false;
            } else {
                self.panel_open = true;
                self.panel_tab = PanelTab::Settings;
            }
        }
        // Esc closes the panel when nothing else holds keyboard focus (e.g. not mid-edit in a field).
        if self.panel_open
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
            && ctx.memory(|m| m.focused().is_none())
        {
            self.panel_open = false;
        }

        egui::Panel::top("menu_bar").show(ui, |ui| {
            // `wide_menu` gives the bar buttons *and* every dropdown popup roomier horizontal item
            // padding than egui's default `menu_style` (which hugs the text at 2px). `.config(..)`
            // carries it into the submenus, which inherit the bar's menu config.
            egui::MenuBar::new()
                .style(crate::ui::wide_menu)
                .config(egui::containers::menu::MenuConfig::new().style(crate::ui::wide_menu))
                .ui(ui, |ui| menu_bar(ui, self, &mut to_open, &mut to_export));
        });

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::top("toolbar").show(ui, |ui| {
                if let View::Loaded(loaded) = &mut self.view {
                    toolbar(ui, loaded, focus_find);
                }
            });
        }

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::bottom("status_bar").show(ui, |ui| {
                if let View::Loaded(loaded) = &self.view {
                    status_bar(ui, loaded, &palette);
                }
            });
        }

        // Right-side tabbed panel (Columns / Parsing / Settings): resizable, slides in/out when
        // toggled. Shown before the central panel so the grid takes the remaining width; the default
        // width suits the Settings tab (its font picker is the widest content).
        self.fix_panel_tab();
        // egui may flip `panel_open` (drag the edge past min size to close, drag the collapsed
        // handle to reopen); copied out and written back since the contents closure borrows `self`.
        let mut panel_open = self.panel_open;
        egui::Panel::right("side_panel")
            .resizable(true)
            .default_size(280.0)
            .min_size(280.0)
            .max_size(500.0)
            .show_collapsible(ui, &mut panel_open, |ui| self.side_panel_contents(ui));
        self.panel_open = panel_open;

        // React to edits from the toolbar (filter/sort) and the side panel (Parsing → dialect,
        // Columns → layout): a dialect change re-opens the file; otherwise apply the view and persist
        // the per-file saved view. This must run *after* the panel renders — the Parsing tab lives
        // there, so an earlier check would miss the change (and next frame's snapshot would hide it).
        if let View::Loaded(loaded) = &mut self.view {
            if Some(loaded.dialect) != dialect_before {
                if let Ok(reopened) = open_table(&loaded.path, loaded.format, loaded.dialect) {
                    // Keep the user's column order/visibility across a re-open when the column count
                    // is unchanged (e.g. toggling the header row). Only reset the layout when the new
                    // dialect actually changed the column structure (e.g. a different delimiter).
                    let new_count = reopened.schema().columns.len();
                    if new_count != loaded.layout.order.len() {
                        loaded.layout = GridLayout::new(new_count);
                    }
                    loaded.table = reopened;
                }
            } else {
                let desired = loaded.view.desired();
                if desired != loaded.view.applied {
                    loaded.view.applied = desired.clone();
                    loaded.view.error = match loaded.table.set_view(&desired) {
                        Ok(()) => None,
                        Err(error) => Some(error.to_string()),
                    };
                }
                let delimiter = (!loaded.delimiter_auto).then_some(loaded.dialect.delimiter);
                let current = SavedView::snapshot(&loaded.layout, &loaded.view, delimiter);
                if current != loaded.saved {
                    views::save(Path::new(&loaded.origin), &current);
                    loaded.saved = current;
                }
            }

            // Find navigation (toolbar Prev/Next): start a requested scan, then poll the running one.
            if let Some(forward) = loaded.view.find_request.take() {
                start_find_nav(loaded, forward, &ctx);
            }
            if loaded.find_nav.is_some() {
                // Read (and clear) the result under the lock, then release it before mutating state.
                let done = loaded
                    .find_nav
                    .as_ref()
                    .and_then(|job| job.result.lock().expect("find result lock").take());
                match done {
                    Some(found) => {
                        loaded.find_nav = None;
                        // Select + scroll to the hit; a `None` (no match / cancelled) leaves the view.
                        if let Some(row) = found {
                            loaded.view.selected = Some(RowSpan::single(row));
                            loaded.view.pending_scroll = Some(row);
                        }
                    }
                    None => ctx.request_repaint(), // keep polling while the scan runs
                }
            }
        }

        // No inner margin: the table fills the central area edge-to-edge (the empty/failed views
        // center their own content, so they're unaffected).
        let central_frame = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO);
        egui::CentralPanel::default()
            .frame(central_frame)
            .show(ui, |ui| {
                if matches!(self.view, View::Empty) {
                    self.show_landing(ui, &ctx, &palette);
                } else if let View::Loaded(loaded) = &mut self.view {
                    grid(ui, loaded, &palette);
                } else if let View::Failed { path, error } = &self.view {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("Could not open file");
                        ui.label(format!("{}: {error}", path.display()));
                    });
                }
            });

        if self.theme != theme_before {
            prefs::save(&self.theme);
        }
        if self.cloud != cloud_before {
            cloud::save(&self.cloud);
        }
        // Files handed to us by macOS "Open With" / double-click arrive via an Apple Event, not argv
        // (see macos_open.rs); open whatever has been queued since the last frame.
        #[cfg(target_os = "macos")]
        for path in crate::macos_open::take_pending() {
            self.open_target(path.to_string_lossy().into_owned(), &ctx);
        }
        // `open_target` resolves local vs remote and (for local) drops egui_table's stored column
        // widths so the new file's columns auto-fit — column order/visibility live in our own
        // `GridLayout`, so they're unaffected. A recent entry may be a URL, handled the same way.
        if let Some(path) = to_open {
            self.open_target(path.to_string_lossy().into_owned(), &ctx);
        }
        if let Some((scope, kind)) = to_export {
            self.start_export(scope, kind, &ctx);
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Window edges match the panel background (set via the theme `Style`).
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

/// A small ✕ close button drawn as two strokes (shapes, not a glyph — font-independent, per the `ui`
/// module's hand-painted-text invariant). Returns its click response.
fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let c = rect.center();
    let r = 4.0;
    let stroke = egui::Stroke::new(1.5, color);
    ui.painter()
        .line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
    ui.painter()
        .line_segment([c + egui::vec2(-r, r), c + egui::vec2(r, -r)], stroke);
    response.on_hover_text("Close panel")
}

/// Create the export file and run the chosen writer (off the UI thread), reporting `progress` and
/// honouring `cancel`. On any failure — including cancellation — the partial file is removed, so a
/// cancelled or failed export never leaves a truncated file behind.
#[allow(clippy::too_many_arguments)]
fn run_export(
    table: &dyn ViewportSource,
    path: &Path,
    kind: ExportKind,
    scope: ExportScope,
    columns: &[ColumnId],
    names: &[Vec<u8>],
    has_header: bool,
    cancel: &CancellationToken,
    progress: &AtomicU64,
) -> Result<(), String> {
    use tableizer_core::export;
    let result = std::fs::File::create(path)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let writer = std::io::BufWriter::new(file);
            match kind {
                ExportKind::Csv | ExportKind::Tsv => {
                    let delimiter = if matches!(kind, ExportKind::Tsv) {
                        b'\t'
                    } else {
                        b','
                    };
                    // Write a header row only if the source had one (else names are synthetic).
                    let header = has_header.then(|| names.to_vec());
                    export::export_csv(
                        table,
                        writer,
                        delimiter,
                        scope,
                        columns,
                        header.as_deref(),
                        cancel,
                        progress,
                    )
                }
                ExportKind::Ndjson => {
                    export::export_ndjson(table, writer, scope, columns, names, cancel, progress)
                }
                ExportKind::Parquet => {
                    export::export_parquet(table, writer, scope, columns, names, cancel, progress)
                }
            }
            .map_err(|e| e.to_string())
        });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Spawn a background find-navigation scan from the current selection toward `forward`'s boundary,
/// superseding any in-flight scan. The matching display-row (or `None`) is published to
/// `loaded.find_nav`, which the update loop polls and then scrolls + selects. Off the UI thread so a
/// match far from the cursor — or a query that matches nothing — never freezes the grid (Tier C).
fn start_find_nav(loaded: &mut LoadedTable, forward: bool, ctx: &egui::Context) {
    // The non-inverting highlight matcher backs the on-screen highlights, so Prev/Next jump between
    // exactly the cells shown marked. An empty or mid-typed (invalid) query yields nothing to do.
    let Some(matcher) = highlight_matcher(&loaded.view) else {
        return;
    };
    let total = match loaded.table.row_count() {
        RowCount::Exact(n) | RowCount::AtLeast(n) => n,
    };
    let columns = loaded.layout.displayed();
    if total == 0 || columns.is_empty() {
        return;
    }
    // Continue from the current selection; with none, start at the near edge for the direction.
    let first = match (loaded.view.selected.map(|s| s.lead), forward) {
        (Some(r), true) => r.saturating_add(1),
        (Some(0), false) => return, // already at the top — no earlier match to seek
        (Some(r), false) => r - 1,
        (None, true) => 0,
        (None, false) => total - 1,
    };

    // Supersede any running scan, then publish the new job for the poll loop.
    if let Some(prev) = loaded.find_nav.take() {
        prev.cancel.cancel();
    }
    let cancel = CancellationToken::new();
    let result: Arc<Mutex<Option<Option<u64>>>> = Arc::new(Mutex::new(None));
    loaded.find_nav = Some(FindJob {
        cancel: cancel.clone(),
        result: Arc::clone(&result),
    });

    let table: SharedTable = Arc::clone(&loaded.table);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let found = next_match(
            table.as_ref(),
            &columns,
            &matcher,
            first,
            forward,
            total,
            &cancel,
        );
        *result.lock().expect("find result lock") = Some(found);
        ctx.request_repaint(); // wake the idle UI to apply the result
    });
}

/// The raw source bytes of column `id`'s name, with a leading UTF-8 BOM stripped (the BOM is a file
/// marker, not part of the name). Empty for an unknown column — `export.rs` falls back to `colN`.
fn raw_name(schema: &Schema, id: ColumnId) -> Vec<u8> {
    let Some(col) = schema.columns.get(id.0 as usize) else {
        return Vec::new();
    };
    let bytes = col.name.as_ref();
    bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(bytes)
        .to_vec()
}

/// Quit — the standard per-OS shortcut (⌘Q on macOS, Ctrl+Q elsewhere).
pub(crate) const QUIT_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Q);
/// Open the right panel on the Settings tab (⌘, / Ctrl+,).
pub(crate) const SETTINGS_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Comma);
/// Close the current file (⌘W / Ctrl+W).
pub(crate) const CLOSE_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Key;
    use tableizer_core::remote::{DirEntry, DirListing};

    /// Drives [`goto_field`] headlessly through real egui frames and input events.
    struct GotoHarness {
        ctx: egui::Context,
        text: String,
        completion: GotoCompletion,
        submitted: bool,
    }

    impl GotoHarness {
        /// The field focused and showing `text`, with `parent`'s subfolders `dirs` already listed.
        fn new(text: &str, parent: &str, dirs: &[&str]) -> Self {
            let mut harness = Self {
                ctx: egui::Context::default(),
                text: text.to_string(),
                completion: GotoCompletion {
                    parent: Some(parent.to_string()),
                    dirs: Some(dirs.iter().map(|d| d.to_string()).collect()),
                    ..GotoCompletion::default()
                },
                submitted: false,
            };
            harness
                .ctx
                .memory_mut(|mem| mem.request_focus(goto_field_id()));
            harness.frame(Vec::new()); // focus lands
            harness.frame(Vec::new()); // the popup's keys are captured from here on
            harness
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = egui::RawInput {
                events,
                ..egui::RawInput::default()
            };
            let Self {
                ctx,
                text,
                completion,
                submitted,
            } = self;
            let mut output = ctx.run_ui(input, |ui| {
                *submitted |= goto_field(ui, text, completion, BrowseMode::Local, "", 300.0);
            });
            output.textures_delta.clear(); // no renderer to upload the font atlas to
        }

        fn press(&mut self, key: Key) {
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
        }

        fn type_text(&mut self, text: &str) {
            self.frame(vec![egui::Event::Text(text.to_string())]);
        }

        fn focused(&self) -> bool {
            self.ctx.memory(|mem| mem.has_focus(goto_field_id()))
        }
    }

    fn folder(url: &str, children: ChildState) -> BrowseNode {
        BrowseNode {
            url: url.to_string(),
            name: browse_label(url),
            is_dir: true,
            size: None,
            expanded: true,
            children,
        }
    }

    fn cached(entries: &[(&str, bool)], complete: bool) -> CachedListing {
        CachedListing {
            listing: DirListing {
                entries: entries
                    .iter()
                    .map(|&(url, is_dir)| DirEntry {
                        url: url.to_string(),
                        name: browse_label(url),
                        is_dir,
                        size: (!is_dir).then_some(1),
                    })
                    .collect(),
            },
            complete,
        }
    }

    #[test]
    fn install_cached_fills_waiting_folders_with_complete_listings() {
        let listings = ListingCache::default();
        listings.insert("s3://b/a/", cached(&[("s3://b/a/x.csv", false)], true));
        listings.insert("s3://b/p/", cached(&[("s3://b/p/y.csv", false)], false));
        listings.insert("s3://b/c/d/", cached(&[("s3://b/c/d/z/", true)], true));
        let mut root = ChildState::Loaded(vec![
            folder("s3://b/a/", ChildState::Loading),
            folder("s3://b/p/", ChildState::Loading),
            folder(
                "s3://b/c/",
                ChildState::Loaded(vec![folder("s3://b/c/d/", ChildState::Loading)]),
            ),
        ]);
        install_cached(&mut root, &listings);
        let ChildState::Loaded(nodes) = &root else {
            panic!("root stays loaded")
        };
        assert!(
            matches!(&nodes[0].children, ChildState::Loaded(n) if n[0].name == "x.csv"),
            "a complete cached listing is installed"
        );
        assert!(
            matches!(nodes[1].children, ChildState::Loading),
            "a partial listing is not: the folder would look incomplete"
        );
        let ChildState::Loaded(c) = &nodes[2].children else {
            panic!("c stays loaded")
        };
        assert!(
            matches!(&c[0].children, ChildState::Loaded(n) if n[0].name == "z"),
            "nested folders are filled too"
        );
    }

    #[test]
    fn cached_dirs_lists_only_folders_and_says_whether_complete() {
        let listings = ListingCache::default();
        listings.insert(
            "s3://b/",
            cached(&[("s3://b/data/", true), ("s3://b/x.csv", false)], false),
        );
        assert_eq!(
            cached_dirs(&listings, "s3://b/"),
            Some((vec!["data".to_string()], false))
        );
        assert_eq!(cached_dirs(&listings, "s3://b/data/"), None);
    }

    const STRIPE: egui::Color32 = egui::Color32::from_rgb(1, 2, 3);
    const HOVER: egui::Color32 = egui::Color32::from_rgb(4, 5, 6);

    /// Renders a browse tree headlessly, frame by frame, as the start screen does.
    struct TreeHarness {
        ctx: egui::Context,
        tree: ChildState,
        /// The tree's full width.
        width: f32,
    }

    impl TreeHarness {
        fn new(nodes: Vec<BrowseNode>) -> Self {
            let mut harness = Self {
                ctx: egui::Context::default(),
                tree: ChildState::Loaded(nodes),
                width: 0.0,
            };
            harness.frame(Vec::new()); // lay out once, so input finds the rows
            harness
        }

        /// One frame with `events`: the shapes painted (in paint order), and what the tree asked for.
        fn frame(&mut self, events: Vec<egui::Event>) -> (Vec<egui::Shape>, BrowseAction) {
            let input = egui::RawInput {
                events,
                ..egui::RawInput::default()
            };
            let mut action = BrowseAction::None;
            let Self { ctx, tree, width } = self;
            let mut output = ctx.run_ui(input, |ui| {
                *width = ui.max_rect().width();
                let mut rows = ListRows::new(Some(STRIPE), HOVER);
                show_browse_children(ui, tree, 0, BrowseMode::Local, &mut action, &mut rows);
            });
            output.textures_delta.clear(); // no renderer to upload the font atlas to
            (output.shapes.into_iter().map(|c| c.shape).collect(), action)
        }

        /// Where `text` is painted.
        fn text_rect(&mut self, text: &str) -> egui::Rect {
            let (shapes, _) = self.frame(Vec::new());
            shapes
                .iter()
                .find_map(|shape| match shape {
                    egui::Shape::Text(t) if t.galley.text() == text => {
                        Some(t.galley.rect.translate(t.pos.to_vec2()))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{text:?} is not on screen"))
        }

        /// Move the pointer to `pos`; the next frame's shapes.
        fn hover(&mut self, pos: egui::Pos2) -> Vec<egui::Shape> {
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(Vec::new()).0
        }

        /// Click at `pos`; what the tree asked for.
        fn click(&mut self, pos: egui::Pos2) -> BrowseAction {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]).1
        }
    }

    /// The rects filled with `color` in `shapes`, in paint order.
    fn filled(shapes: &[egui::Shape], color: egui::Color32) -> Vec<egui::Rect> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Rect(r) if r.fill == color => Some(r.rect),
                _ => None,
            })
            .collect()
    }

    /// The position in `shapes` of the text `text`.
    fn text_index(shapes: &[egui::Shape], text: &str) -> usize {
        shapes
            .iter()
            .position(|s| matches!(s, egui::Shape::Text(t) if t.galley.text() == text))
            .unwrap()
    }

    fn file(url: &str) -> BrowseNode {
        BrowseNode {
            is_dir: false,
            size: Some(1),
            ..folder(url, ChildState::Unloaded)
        }
    }

    #[test]
    fn window_title_is_the_open_files_name_only() {
        assert_eq!(window_title(Some("/data/2026/sales.csv")), "sales.csv");
        assert_eq!(window_title(Some("s3://bucket/logs/x.log.gz")), "x.log.gz");
    }

    #[test]
    fn window_title_without_a_file_is_the_apps_name() {
        assert_eq!(window_title(None), "Tableizer");
    }

    #[test]
    fn places_lists_the_named_folders_that_exist_then_the_root() {
        let home = tempfile::tempdir().unwrap();
        for dir in ["Desktop", "Downloads"] {
            std::fs::create_dir(home.path().join(dir)).unwrap();
        }
        let desktop = home.path().join("Desktop");
        let downloads = home.path().join("Downloads");
        let missing = home.path().join("Documents"); // not created
        let named = [
            ("Home", Some(home.path())),
            ("Desktop", Some(desktop.as_path())),
            ("Downloads", Some(downloads.as_path())),
            ("Documents", Some(missing.as_path())),
            ("Music", None), // a folder the platform doesn't have
        ];
        let listed: Vec<(String, String)> = places(&named, Path::new("/"))
            .into_iter()
            .map(|node| (node.name, node.url))
            .collect();
        let path = |p: &Path| p.to_string_lossy().into_owned();
        assert_eq!(
            listed,
            [
                ("Home".to_string(), path(home.path())),
                ("Desktop".to_string(), path(&desktop)),
                ("Downloads".to_string(), path(&downloads)),
                ("/".to_string(), "/".to_string()),
            ]
        );
    }

    #[test]
    fn browse_tree_stripes_every_other_row_across_levels() {
        // Rows in display order: a/ (0), a/1 (1), a/2 (2), b (3), c (4) → stripes on 1 and 3.
        let mut h = TreeHarness::new(vec![
            folder(
                "/a/",
                ChildState::Loaded(vec![file("/a/1.csv"), file("/a/2.csv")]),
            ),
            file("/b.csv"),
            file("/c.csv"),
        ]);
        let bands = filled(&h.frame(Vec::new()).0, STRIPE);
        assert_eq!(bands.len(), 2);
        assert!(
            bands.iter().all(|band| band.width() == h.width),
            "full width"
        );
        // Bands for rows 1 and 3 are separated by row 2, never touching.
        assert!(bands[0].bottom() < bands[1].top());
    }

    #[test]
    fn browse_tree_stripe_sits_behind_the_row_text() {
        let mut h = TreeHarness::new(vec![file("/a.csv"), file("/b.csv")]);
        let (shapes, _) = h.frame(Vec::new());
        let band = shapes
            .iter()
            .position(|s| matches!(s, egui::Shape::Rect(r) if r.fill == STRIPE))
            .unwrap();
        assert!(
            band < text_index(&shapes, "b.csv"),
            "the stripe is painted first, so the text sits on top"
        );
    }

    #[test]
    fn browse_tree_stripes_status_rows_too() {
        // a/ (0), its "Listing…" row (1).
        let mut h = TreeHarness::new(vec![folder("/a/", ChildState::Loading)]);
        assert_eq!(filled(&h.frame(Vec::new()).0, STRIPE).len(), 1);
    }

    #[test]
    fn browse_tree_hover_highlights_the_whole_row_like_a_table() {
        let mut h = TreeHarness::new(vec![file("/a.csv"), file("/b.csv"), file("/c.csv")]);
        let row = h.text_rect("a.csv");
        let shapes = h.hover(row.center());
        let bands = filled(&shapes, HOVER);
        assert_eq!(bands.len(), 1, "one hovered row");
        assert_eq!(bands[0].width(), h.width, "full width");
        assert!(bands[0].contains(row.center()));
        let band = shapes
            .iter()
            .position(|s| matches!(s, egui::Shape::Rect(r) if r.fill == HOVER))
            .unwrap();
        assert!(band < text_index(&shapes, "a.csv"), "behind the text");
        // The table's highlight is the only one: no per-label hover box besides it.
        let others: Vec<_> = shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Rect(r) if ![STRIPE, HOVER].contains(&r.fill) => Some(r.rect),
                _ => None,
            })
            .filter(|r| r.is_positive())
            .collect();
        assert_eq!(others, [], "no other highlight");
    }

    #[test]
    fn browse_tree_highlights_nothing_without_hover() {
        let mut h = TreeHarness::new(vec![file("/a.csv"), file("/b.csv")]);
        assert_eq!(filled(&h.frame(Vec::new()).0, HOVER), []);
    }

    #[test]
    fn browse_tree_status_rows_do_not_highlight() {
        let mut h = TreeHarness::new(vec![folder("/a/", ChildState::Loading)]);
        let status = h.text_rect("Listing…");
        assert_eq!(filled(&h.hover(status.center()), HOVER), []);
    }

    #[test]
    fn browse_tree_click_anywhere_on_a_file_row_opens_it() {
        let mut h = TreeHarness::new(vec![file("/a.csv"), file("/b.csv")]);
        let name = h.text_rect("b.csv");
        // Well right of the name, in the row's empty middle.
        let action = h.click(egui::pos2(h.width / 2.0, name.center().y));
        assert!(
            matches!(&action, BrowseAction::Open(url) if url == "/b.csv"),
            "the row opens its file"
        );
        // ...and so does clicking the name itself.
        let action = h.click(name.center());
        assert!(matches!(&action, BrowseAction::Open(url) if url == "/b.csv"));
    }

    #[test]
    fn browse_tree_click_anywhere_on_a_folder_row_toggles_it() {
        let mut collapsed = folder("/a/", ChildState::Loaded(Vec::new()));
        collapsed.expanded = false;
        let mut h = TreeHarness::new(vec![collapsed]);
        let name = h.text_rect("a");
        h.click(egui::pos2(h.width / 2.0, name.center().y));
        let ChildState::Loaded(nodes) = &h.tree else {
            unreachable!()
        };
        assert!(nodes[0].expanded);
    }

    #[test]
    fn goto_tab_completes_the_first_matching_folder() {
        let mut h = GotoHarness::new("/Us", "/", &["Library", "Users", "usr"]);
        assert!(h.completion.open);
        h.press(Key::Tab);
        assert_eq!(h.text, "/Users/");
    }

    #[test]
    fn goto_tab_keeps_focus_with_the_cursor_at_the_end() {
        let mut h = GotoHarness::new("/Us", "/", &["Users"]);
        h.press(Key::Tab);
        assert!(h.focused());
        h.type_text("j");
        assert_eq!(h.text, "/Users/j");
    }

    #[test]
    fn goto_arrows_and_enter_complete_the_highlighted_folder() {
        let mut h = GotoHarness::new("/", "/", &["Library", "Users"]);
        h.press(Key::ArrowDown);
        h.press(Key::ArrowDown);
        h.press(Key::Enter);
        assert_eq!(h.text, "/Users/");
        assert!(!h.submitted);
        assert!(h.focused());
    }

    #[test]
    fn goto_enter_without_a_highlight_submits() {
        let mut h = GotoHarness::new("/Users/", "/Users/", &["john"]);
        h.press(Key::Enter);
        assert!(h.submitted);
        assert_eq!(h.text, "/Users/");
    }

    #[test]
    fn goto_escape_hides_the_suggestions_until_the_text_changes() {
        let mut h = GotoHarness::new("/U", "/", &["Users"]);
        h.press(Key::Escape);
        h.frame(Vec::new());
        assert!(!h.completion.open);
        assert!(h.focused());
        h.type_text("s");
        h.frame(Vec::new());
        assert!(h.completion.open);
    }

    #[test]
    fn goto_shows_no_suggestions_from_another_folders_listing() {
        // The listing is still for `/` while the text has moved on to `/Users/`.
        let h = GotoHarness::new("/Users/", "/", &["Users"]);
        assert!(!h.completion.open);
    }
}
