//! The eframe application: window state, theme/font installation, file opening, and the per-frame
//! update loop that lays out the menu bar, toolbar, table, and status bar.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui;
use encoding_rs::Encoding;
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
    ExportKind, ExportRequest, columns_tab, empty_view, fmt_bytes, fmt_count, grid, menu_bar,
    parsing_tab, settings_tab, status_bar, toolbar,
};
use crate::{fonts, theme};

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
    /// Whether the "Open URL…" entry dialog is showing (set from the File menu too).
    pub(crate) url_dialog_open: bool,
    /// Text in the "Open URL…" entry field.
    url_input: String,
    /// A target (path or URL) queued at startup (CLI arg) to open on the first frame, once a `Context`
    /// exists to drive a remote download's progress UI.
    pending_target: Option<String>,
    /// S3 credentials/config (from Settings) applied when opening `s3://` URLs.
    cloud: CloudConfig,
    /// Whether the cloud file-browser dialog is showing.
    pub(crate) browse_open: bool,
    /// The browse tree's root level (the buckets). Cached across opens so revisiting never re-lists.
    browse_root: ChildState,
    /// In-flight listings (one per expanding folder, plus the root), keyed by their location.
    browse_jobs: Vec<BrowseJob>,
    /// The "go to" field: a bucket/prefix URL to add to the tree and expand.
    browse_goto: String,
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

/// What [`TableizerApp::show_browse`] asks the update loop to do after rendering the tree.
enum BrowseAction {
    None,
    /// List a folder's children (its URL; `""` = root) on a background thread.
    Load(String),
    /// Open the chosen file URL (and close the browser).
    Open(String),
    /// Re-discover buckets, resetting the tree.
    Refresh,
    /// Add a typed bucket/prefix URL as a top-level node and expand it.
    Goto(String),
    /// Close the browser (keeping the tree cached).
    Close,
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
            url_dialog_open: false,
            url_input: String::new(),
            pending_target: target,
            cloud: cloud::load(),
            browse_open: false,
            browse_root: ChildState::Unloaded,
            browse_jobs: Vec::new(),
            browse_goto: String::new(),
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
        self.url_dialog_open = false;
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

    /// The "Open URL…" entry dialog. Returns the entered URL when the user submits (Open / Enter);
    /// `None` otherwise. Esc or Cancel dismisses it.
    fn show_url_dialog(&mut self, ctx: &egui::Context) -> Option<String> {
        if !self.url_dialog_open {
            return None;
        }
        let mut submitted = None;
        let mut close = false;
        let mut browse = false;
        egui::Window::new("Open URL")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                ui.label("Open a file from cloud storage (S3, GCS, Azure, or HTTP):");
                ui.add_space(6.0);
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.url_input)
                        .hint_text("s3://bucket/path/data.csv")
                        .desired_width(f32::INFINITY),
                );
                edit.request_focus();
                let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ready = !self.url_input.trim().is_empty();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(ready, egui::Button::new("Open")).clicked()
                        || (entered && ready)
                    {
                        submitted = Some(self.url_input.trim().to_string());
                    }
                    // Browse from the typed location (or the bucket) instead of opening it directly.
                    if ui.button("Browse…").clicked() {
                        browse = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        if browse {
            self.url_dialog_open = false;
            self.browse_open = true;
            self.browse_goto = self.url_input.trim().to_string();
            self.url_input.clear();
            return None;
        }
        if submitted.is_some() || close {
            self.url_dialog_open = false;
            self.url_input.clear();
        }
        submitted
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

    /// Spawn a background listing for `location` (`""` = discover buckets, else list a prefix). The
    /// result is installed on the matching tree node by [`poll_browse`](Self::poll_browse). Multiple
    /// may run at once (expanding several folders); the tree node was already marked `Loading`.
    fn load_children(&mut self, location: String, ctx: &egui::Context) {
        let cancel = CancellationToken::new();
        let outcome: Arc<Mutex<Option<Result<tableizer_core::remote::DirListing, String>>>> =
            Arc::new(Mutex::new(None));
        self.browse_jobs.push(BrowseJob {
            location: location.clone(),
            cancel: cancel.clone(),
            outcome: Arc::clone(&outcome),
        });
        let ctx = ctx.clone();
        if location.is_empty() {
            // Root: enumerate buckets reachable with the configured credentials.
            let auth = self.s3_auth();
            std::thread::spawn(move || {
                let result =
                    tableizer_core::remote::list_s3_buckets(&auth).map_err(|e| e.to_string());
                *outcome.lock().expect("browse outcome lock") = Some(result);
                ctx.request_repaint();
            });
            return;
        }
        let auth = self.cloud_auth(&location);
        std::thread::spawn(move || {
            let result = (|| -> Result<tableizer_core::remote::DirListing, String> {
                let options = auth.resolve()?;
                tableizer_core::remote::list_dir(&location, &options, &cancel)
                    .map_err(|e| e.to_string())
            })();
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
        if running {
            ctx.request_repaint();
        }
    }

    /// Add a typed bucket/prefix URL as a top-level tree node and expand it (a non-discovered bucket,
    /// or a jump to a deep prefix). Already-present nodes are just expanded — using the cached subtree.
    fn goto_browse(&mut self, url: String, ctx: &egui::Context) {
        if url.is_empty() {
            return;
        }
        if !matches!(self.browse_root, ChildState::Loaded(_)) {
            self.browse_root = ChildState::Loaded(Vec::new());
        }
        let mut load = false;
        if let ChildState::Loaded(nodes) = &mut self.browse_root {
            let node = match nodes.iter_mut().position(|n| n.url == url) {
                Some(i) => &mut nodes[i],
                None => {
                    nodes.push(BrowseNode {
                        name: browse_label(&url),
                        url: url.clone(),
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
                node.children = ChildState::Loading;
                load = true;
            }
        }
        if load {
            self.load_children(url, ctx);
        }
    }

    /// Close the browser, keeping the discovered tree cached so reopening doesn't re-list.
    fn close_browser(&mut self) {
        self.browse_open = false;
    }

    /// Render the cloud file browser as a lazy **tree**: a "go to" field + Refresh, then the expandable
    /// bucket/folder hierarchy (expanded subtrees stay cached). Returns the action to apply.
    fn show_browse(&mut self, ctx: &egui::Context) -> BrowseAction {
        if !self.browse_open {
            return BrowseAction::None;
        }
        let mut action = BrowseAction::None;
        // Split-borrow so the tree is mutable (expand toggles) while the "go to" field stays editable.
        let TableizerApp {
            browse_root,
            browse_goto,
            ..
        } = self;
        egui::Window::new("Browse cloud storage")
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 460.0])
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let submit = ui
                        .add(
                            egui::TextEdit::singleline(browse_goto)
                                .hint_text("s3://bucket/prefix/ — jump to")
                                .desired_width(ui.available_width() - 132.0),
                        )
                        .lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ready = !browse_goto.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Go")).clicked() || (submit && ready)
                    {
                        action = BrowseAction::Goto(browse_goto.trim().to_string());
                    }
                    if ui.button("Refresh").clicked() {
                        action = BrowseAction::Refresh;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        show_browse_children(ui, browse_root, 0, &mut action);
                    });
                ui.separator();
                if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = BrowseAction::Close;
                }
            });
        action
    }
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
    action: &mut BrowseAction,
) {
    let indent = depth as f32 * 16.0 + 18.0;
    match state {
        ChildState::Unloaded => {}
        ChildState::Loading => {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.weak("Listing…");
            });
        }
        ChildState::Failed(error) => {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.colored_label(ui.visuals().error_fg_color, error.as_str());
            });
        }
        ChildState::Loaded(nodes) if nodes.is_empty() => {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.weak("(empty)");
            });
        }
        ChildState::Loaded(nodes) => {
            for node in nodes.iter_mut() {
                show_browse_node(ui, node, depth, action);
            }
        }
    }
}

/// Render one tree node (folder = expandable disclosure; file = clickable row with size) and recurse.
fn show_browse_node(
    ui: &mut egui::Ui,
    node: &mut BrowseNode,
    depth: usize,
    action: &mut BrowseAction,
) {
    let indent = depth as f32 * 16.0;
    if node.is_dir {
        let mut toggle = false;
        ui.horizontal(|ui| {
            ui.add_space(indent);
            ui.spacing_mut().item_spacing.x = 2.0;
            if disclosure(ui, node.expanded).clicked() {
                toggle = true;
            }
            if ui
                .selectable_label(false, format!("{}/", node.name))
                .clicked()
            {
                toggle = true;
            }
        });
        if toggle {
            node.expanded = !node.expanded;
            // Expanding an unlisted (or previously failed) folder kicks off a one-time listing.
            if node.expanded
                && matches!(node.children, ChildState::Unloaded | ChildState::Failed(_))
            {
                node.children = ChildState::Loading;
                *action = BrowseAction::Load(node.url.clone());
            }
        }
        if node.expanded {
            show_browse_children(ui, &mut node.children, depth + 1, action);
        }
    } else {
        ui.horizontal(|ui| {
            ui.add_space(indent + 18.0); // align past the disclosure column
            if ui.selectable_label(false, node.name.as_str()).clicked() {
                *action = BrowseAction::Open(node.url.clone());
            }
            if let Some(size) = node.size {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(fmt_bytes(size));
                });
            }
        });
    }
}

/// A small disclosure triangle (▶ collapsed / ▼ expanded) drawn as a shape (font-independent, like the
/// grid's painted arrows). Returns its click response.
fn disclosure(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
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
    response
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
        // its progress dialog. The "Open URL…" entry dialog starts a download when submitted.
        self.poll_download(&ctx);
        self.show_download(&ctx);
        // A gzipped file (local or just-downloaded) is decompressed to the cache before opening.
        self.poll_decompress(&ctx);
        self.show_decompress(&ctx);
        if let Some(url) = self.show_url_dialog(&ctx) {
            self.open_target(url, &ctx);
        }
        // The cloud file browser: discover buckets on first open (the tree is cached across opens),
        // install finished listings, render the tree, and apply the chosen action.
        if self.browse_open && matches!(self.browse_root, ChildState::Unloaded) {
            self.browse_root = ChildState::Loading;
            self.load_children(String::new(), &ctx);
        }
        self.poll_browse(&ctx);
        match self.show_browse(&ctx) {
            BrowseAction::Load(location) => self.load_children(location, &ctx),
            BrowseAction::Open(url) => {
                self.close_browser();
                self.open_target(url, &ctx);
            }
            BrowseAction::Refresh => {
                for job in self.browse_jobs.drain(..) {
                    job.cancel.cancel(); // abandon in-flight listings of the old tree
                }
                self.browse_root = ChildState::Loading;
                self.load_children(String::new(), &ctx);
            }
            BrowseAction::Goto(url) => self.goto_browse(url, &ctx),
            BrowseAction::Close => self.close_browser(),
            BrowseAction::None => {}
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
        let mut open_url = false;
        let mut open_browse = false;
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
        // ⌘O / Ctrl+O opens a file (same as File ▸ Open…).
        if ctx.input_mut(|i| i.consume_shortcut(&OPEN_SHORTCUT))
            && let Some(path) = rfd::FileDialog::new().pick_file()
        {
            to_open = Some(path);
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

        egui::Panel::top("menu_bar").show_inside(ui, |ui| {
            // `wide_menu` gives the bar buttons *and* every dropdown popup roomier horizontal item
            // padding than egui's default `menu_style` (which hugs the text at 2px). `.config(..)`
            // carries it into the submenus, which inherit the bar's menu config.
            egui::MenuBar::new()
                .style(crate::ui::wide_menu)
                .config(egui::containers::menu::MenuConfig::new().style(crate::ui::wide_menu))
                .ui(ui, |ui| menu_bar(ui, self, &mut to_open, &mut to_export));
        });

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::top("toolbar").show_inside(ui, |ui| {
                if let View::Loaded(loaded) = &mut self.view {
                    toolbar(ui, loaded, focus_find);
                }
            });
        }

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
                if let View::Loaded(loaded) = &self.view {
                    status_bar(ui, loaded, &palette);
                }
            });
        }

        // Right-side tabbed panel (Columns / Parsing / Settings): resizable, slides in/out when
        // toggled. Shown before the central panel so the grid takes the remaining width; the default
        // width suits the Settings tab (its font picker is the widest content).
        self.fix_panel_tab();
        let panel_open = self.panel_open;
        egui::Panel::right("side_panel")
            .resizable(true)
            .default_size(280.0)
            .min_size(280.0)
            .max_size(500.0)
            .show_animated_inside(ui, panel_open, |ui| self.side_panel_contents(ui));

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
            .show_inside(ui, |ui| match &mut self.view {
                View::Empty => empty_view(
                    ui,
                    &self.recent,
                    &mut to_open,
                    &mut open_url,
                    &mut open_browse,
                ),
                View::Failed { path, error } => {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("Could not open file");
                        ui.label(format!("{}: {error}", path.display()));
                    });
                }
                View::Loaded(loaded) => grid(ui, loaded, &palette),
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
        if open_url {
            self.url_dialog_open = true;
        }
        if open_browse {
            self.browse_open = true;
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
/// Open a file (⌘O / Ctrl+O).
pub(crate) const OPEN_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
/// Close the current file (⌘W / Ctrl+W).
pub(crate) const CLOSE_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);
