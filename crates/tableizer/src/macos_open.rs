//! macOS file-open receiver — opens files Finder hands us ("Open With" / double-click / when set as
//! the default handler), which arrive as a `kAEOpenDocuments` Apple Event, *not* as `argv`.
//!
//! eframe's backend (winit 0.30) keeps its own `NSApplicationDelegate` and *panics* if you replace it
//! ([winit#4015]), and egui has no support ([egui#7057]) — so instead of touching the delegate we
//! register our own [`NSAppleEventManager`] handler for the open-documents event. Received paths are
//! queued and drained by the app each frame (`app.rs`). The bundle declares which types it handles
//! via `CFBundleDocumentTypes` in `scripts/package-macos.sh`.
//!
//! Cold launch (app not running) is the subtle case: the launch document is dispatched during
//! AppKit's `finishLaunching`, *after* AppKit installs its own open-documents handler that shadows
//! ours — so [`install`] re-asserts ours from a one-shot run-loop-entry observer ([`reassert_handler`])
//! that fires in the gap before the document is serviced. **Verified working (warm + cold) on
//! macOS 26.** This leans on AppKit/winit launch timing, so re-verify if either is upgraded.
//!
//! [winit#4015]: https://github.com/rust-windowing/winit/issues/4015
//! [egui#7057]: https://github.com/emilk/egui/issues/7057

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use eframe::egui;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AnyThread, define_class, msg_send, sel};
use objc2_core_foundation::{
    CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, kCFRunLoopCommonModes,
};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager};

/// FourCharCode Apple Event constants (Carbon `aevt`/`odoc`/`----`/`furl`).
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt"); // kCoreEventClass
const AE_OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc"); // kAEOpenDocuments
const KEY_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----"); // keyDirectObject
const TYPE_FILE_URL: u32 = u32::from_be_bytes(*b"furl"); // typeFileURL

/// Files received since the last drain. Touched only on the main thread (Apple Events dispatch there,
/// and so does the egui UI), but a `Mutex` keeps it sound regardless.
static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// The egui context, stashed so the Apple Event handler can wake an idle UI to drain `PENDING`.
static REPAINT: OnceLock<egui::Context> = OnceLock::new();

/// Remember the egui context so an incoming open-event can request a repaint (call once at startup).
pub(crate) fn set_repaint_ctx(ctx: egui::Context) {
    let _ = REPAINT.set(ctx);
}

/// Drain the files received since the last call — invoked each frame on the UI thread (`app.rs`).
pub(crate) fn take_pending() -> Vec<PathBuf> {
    std::mem::take(&mut PENDING.lock().expect("open-queue lock"))
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TableizerOpenHandler"]
    struct OpenHandler;

    impl OpenHandler {
        #[unsafe(method(handleOpenEvent:withReplyEvent:))]
        fn handle_open_event(
            &self,
            event: &NSAppleEventDescriptor,
            _reply: &NSAppleEventDescriptor,
        ) {
            let paths = decode_paths(event);
            if !paths.is_empty() {
                eprintln!("[tableizer] open event: {paths:?}");
                PENDING.lock().expect("open-queue lock").extend(paths);
                if let Some(ctx) = REPAINT.get() {
                    ctx.request_repaint();
                }
            }
        }
    }
);

/// Extract file paths from a `kAEOpenDocuments` descriptor's direct-object list.
fn decode_paths(event: &NSAppleEventDescriptor) -> Vec<PathBuf> {
    let Some(list) = event.paramDescriptorForKeyword(KEY_DIRECT_OBJECT) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for i in 1..=list.numberOfItems() {
        let Some(item) = list.descriptorAtIndex(i) else {
            continue;
        };
        let file = item.coerceToDescriptorType(TYPE_FILE_URL).unwrap_or(item);
        if let Some(url) = file.fileURLValue()
            && let Some(path) = url.path()
        {
            paths.push(PathBuf::from(path.to_string()));
        }
    }
    paths
}

/// Register (or re-register) our open-documents handler with the shared Apple Event manager. Must run
/// on the main thread; the latest registration wins.
#[allow(unsafe_code)] // SAFETY notes inline at each `unsafe` block below.
fn register_handler() {
    let handler: Retained<OpenHandler> = unsafe { msg_send![OpenHandler::alloc(), init] };
    let manager = NSAppleEventManager::sharedAppleEventManager();
    // SAFETY: `handler` implements `handleOpenEvent:withReplyEvent:` with the `(event, reply)`
    // signature the manager invokes, and we leak it below so it outlives the registration (the
    // manager stores its handler unretained).
    unsafe {
        manager.setEventHandler_andSelector_forEventClass_andEventID(
            &handler,
            sel!(handleOpenEvent:withReplyEvent:),
            CORE_EVENT_CLASS,
            AE_OPEN_DOCUMENTS,
        );
    }
    std::mem::forget(handler);
}

/// One-shot run-loop-entry callback: re-assert our handler. AppKit installs its *own* open-documents
/// handler during `finishLaunching` (displacing the one we set in `main`) and then dispatches the
/// queued launch document. `kCFRunLoopEntry` fires after `finishLaunching` but before that document
/// is serviced, so re-registering here is what lets a *cold* launch reach us (warm "Open With"
/// already works without it).
#[allow(unsafe_code)] // a plain re-registration; no foreign state touched.
unsafe extern "C-unwind" fn reassert_handler(
    _observer: *mut CFRunLoopObserver,
    _activity: CFRunLoopActivity,
    _info: *mut core::ffi::c_void,
) {
    register_handler();
}

/// Register the handler now, and schedule the one-shot re-assert at run-loop entry (the cold-launch
/// fix described on [`reassert_handler`]). Call once, early, on the main thread.
#[allow(unsafe_code)] // SAFETY: standard CFRunLoopObserver setup; details inline.
pub(crate) fn install() {
    register_handler();
    // SAFETY: a one-shot (`repeats = false`) observer whose callback only re-registers our handler;
    // added to the main run loop in the common modes so it fires whatever mode launch runs in.
    unsafe {
        let Some(observer) = CFRunLoopObserver::new(
            None,
            CFRunLoopActivity::Entry.0,
            false,
            0,
            Some(reassert_handler),
            core::ptr::null_mut(),
        ) else {
            return;
        };
        if let Some(run_loop) = CFRunLoop::main() {
            run_loop.add_observer(Some(&observer), kCFRunLoopCommonModes);
        }
    }
}
