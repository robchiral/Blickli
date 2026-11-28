//! macOS application entry point, `AppDelegate`, and main-thread event pump.

mod browse;
mod compare;
mod compress;
mod compress_ui;
mod controls;
mod crop;
mod decoder;
mod edit;
mod editing;
#[cfg(any(test, feature = "e2e"))]
mod e2e;
#[cfg(not(any(test, feature = "e2e")))]
mod e2e {
    pub const fn enabled() -> bool {
        false
    }
}
mod file_ops;
mod grants;
mod grid;
mod info;
mod menu;
mod prefs;
mod shortcuts;
mod show;
mod sidebar;
mod slideshow;
#[cfg(feature = "snapshot")]
mod snapshot;
#[cfg(not(feature = "snapshot"))]
mod snapshot {
    pub const fn enabled() -> bool {
        false
    }
}
mod toolbar;
mod trash;
mod upscale;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};

use blickli_core::history::{Entry, History, Reversal, Step};
use blickli_core::messages::{
    folder_label, format_bytes, put_back_message, reason, transfer_message, trash_message,
};
use blickli_core::transfer::{self, Transfer};
use blickli_core::{Engine, Event, LruCache, Playlist, SortOrder};
use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{
    define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadMarker,
    MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSColor, NSControlTextEditingDelegate, NSFont, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
    NSOpenPanel, NSScreen, NSScrollView, NSSearchFieldDelegate, NSSplitViewController,
    NSSplitViewItem, NSTextField,
    NSTextFieldDelegate, NSTextView, NSViewController, NSWindow, NSWindowCollectionBehavior,
    NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{
    ns_string, NSArray, NSNotification, NSObject, NSObjectProtocol, NSRect, NSString, NSTimer,
    NSUserDefaults, NSURL,
};

use info::{count_text, exif_date_text, file_name, rows_text};
use menu::{build_menu, menu_item};
use prefs::{
    last_folder_pref, save_last_folder, startup_folder_pref, thumb_cap_pref, thumb_size_pref,
    THUMB_SLIDER_W,
};

use decoder::ImageIoDecoder;
use grid::GridView;
use sidebar::Sidebar;
use slideshow::{SlideState, SlideView, SlideshowWindow};

type Img = CFRetained<CGImage>;
type HeaderRows = Vec<(String, String)>;
type Dimensions = Option<(i64, i64)>;

/// Cache budget for decoded slides (~15 Retina-sized images).
const SLIDE_CACHE_BYTES: usize = 512 * 1024 * 1024;

const STATUS_H: f64 = 30.0;
const SPINNER_W: f64 = 16.0;

static EVENTS: OnceLock<Mutex<Receiver<Event<Img>>>> = OnceLock::new();
static EVENT_DRAIN_PENDING: AtomicBool = AtomicBool::new(false);
static DELEGATE: OnceLock<MainThreadBound<Retained<AppDelegate>>> = OnceLock::new();

struct UpscalePreview {
    path: PathBuf,
    original: Img,
    upscaled_native: Img,
    native_scale: usize,
    upscaled_alt: Option<(upscale::UpscaleScale, Img)>,
    selected_scale: upscale::UpscaleScale,
}

struct Show {
    window: Retained<SlideshowWindow>,
    view: Retained<SlideView>,
    overlay: Retained<NSTextField>,
    overlay_box: Retained<objc2_app_kit::NSView>,
    controls: controls::Controls,
    controls_timer: Option<Retained<NSTimer>>,
    controls_fade_timer: Option<Retained<NSTimer>>,
    /// Enter crop mode once the first slide decode finishes.
    crop_when_ready: bool,
    /// Start interactive upscale preview once the first slide decode finishes.
    upscale_when_ready: bool,
    /// Cached bitmaps for the active interactive upscale comparison.
    upscale_preview: Option<UpscalePreview>,
    playlist: Playlist,
    /// In-flight high-res zoom decode `(path, target_max_px)`.
    pending_decode: Option<(PathBuf, i32)>,
    cache: LruCache<PathBuf, (Retained<NSImage>, CGSize)>,
    view_states: HashMap<PathBuf, SlideState>,
    /// Auto-advance interval in seconds; paused when `Some` with `timer == None`.
    interval: Option<f64>,
    timer: Option<Retained<NSTimer>>,
    /// Temporary status banner shown over the slide.
    flash: Option<String>,
    flash_timer: Option<Retained<NSTimer>>,
    /// Current playlist slide path, used to flush rotations on slide change.
    on_screen: Option<PathBuf>,
    /// Path of the bitmap currently loaded into `view` (distinguishes same-dimension slides on zoom re-decode).
    displayed_path: Option<PathBuf>,
    /// Last title assigned to `window`, avoiding redundant AppKit title updates.
    last_title: String,
    saving: editing::Saving,
}

impl Show {
    pub(crate) fn is_current_displayed(&self) -> bool {
        self.view.has_image()
            && self.displayed_path.as_deref() == self.playlist.current().map(PathBuf::as_path)
    }
}

#[derive(Default)]
pub struct DelegateIvars {
    engine: OnceCell<Engine<Img>>,
    window: OnceCell<Retained<NSWindow>>,
    grid: OnceCell<Retained<GridView>>,
    sidebar: OnceCell<Retained<Sidebar>>,
    split: OnceCell<Retained<NSSplitViewController>>,
    content: OnceCell<Retained<objc2_app_kit::NSView>>,
    scroll: OnceCell<Retained<NSScrollView>>,
    filter_field: OnceCell<Retained<NSTextField>>,
    filter_active: Cell<bool>,
    toolbar: OnceCell<Retained<toolbar::BrowseToolbar>>,
    /// Files opened via Finder waiting for their folder scan to populate the grid.
    pending_open: RefCell<Vec<PathBuf>>,
    /// Set when Finder opens paths at launch so startup folder restore is skipped.
    opened_from_finder: Cell<bool>,
    current_scan: RefCell<Option<(PathBuf, bool)>>,
    last_move: RefCell<Option<PathBuf>>,
    last_copy: RefCell<Option<PathBuf>>,
    move_again_item: OnceCell<Retained<NSMenuItem>>,
    copy_again_item: OnceCell<Retained<NSMenuItem>>,
    transfer_busy: Cell<bool>,
    prefs_window: OnceCell<Retained<NSWindow>>,
    prefs_tabs: OnceCell<Retained<objc2_app_kit::NSSegmentedControl>>,
    prefs_general_view: OnceCell<Retained<objc2_app_kit::NSView>>,
    prefs_shortcuts_view: OnceCell<Retained<objc2_app_kit::NSView>>,
    prefs_general_h: Cell<f64>,
    shortcuts: RefCell<shortcuts::ShortcutStore>,
    shortcut_items: RefCell<HashMap<shortcuts::Action, Retained<NSMenuItem>>>,
    larger_thumbs_shifted: OnceCell<Retained<NSMenuItem>>,
    shortcut_recorders: RefCell<HashMap<shortcuts::Action, Retained<shortcuts::ShortcutRecorderView>>>,
    shortcut_feedback: OnceCell<Retained<NSTextField>>,
    thumb_slider: OnceCell<Retained<objc2_app_kit::NSSlider>>,
    history: RefCell<History>,
    undo_item: OnceCell<Retained<NSMenuItem>>,
    redo_item: OnceCell<Retained<NSMenuItem>>,
    info_window: OnceCell<Retained<NSWindow>>,
    info_label: OnceCell<Retained<NSTextField>>,
    scale_up_small: Cell<bool>,
    drag_out_copies: Cell<bool>,
    scale_up_item: OnceCell<Retained<NSMenuItem>>,
    show: RefCell<Option<Show>>,
    loop_enabled: Cell<bool>,
    shuffle_enabled: Cell<bool>,
    overlay_visible: Cell<bool>,
    overlay_exif: Cell<bool>,
    overlay_path: Cell<bool>,
    header_rows: RefCell<Option<(PathBuf, HeaderRows)>>,
    #[cfg(any(test, feature = "e2e"))]
    header_reads: Cell<usize>,
    overlay_help: Cell<bool>,
    status: OnceCell<Retained<NSTextField>>,
    spinner: OnceCell<Retained<objc2_app_kit::NSProgressIndicator>>,
    status_dims: RefCell<Option<(std::rc::Rc<blickli_core::FileInfo>, Dimensions)>>,
    loop_item: OnceCell<Retained<NSMenuItem>>,
    shuffle_item: OnceCell<Retained<NSMenuItem>>,
    labels_item: OnceCell<Retained<NSMenuItem>>,
    auto_menu: OnceCell<Retained<NSMenu>>,
    sort_order: Cell<SortOrder>,
    sort_desc: Cell<bool>,
    default_interval: Cell<Option<f64>>,
    save_rotations: Cell<bool>,
    show_controls: Cell<bool>,
    controls_item: OnceCell<Retained<NSMenuItem>>,
    edit_worker: OnceCell<std::sync::mpsc::Sender<editing::EditJob>>,
    /// Edit jobs whose main-thread completion callback has not run yet.
    pending_edits: Cell<usize>,
    upscale_scale: Cell<upscale::UpscaleScale>,
    upscale_save_mode: Cell<upscale::UpscaleSaveMode>,
    upscale_worker: OnceCell<std::sync::mpsc::Sender<editing::UpscaleJob>>,
    upscale_cancel: RefCell<Option<std::sync::Arc<AtomicBool>>>,
    upscale_job_id: Cell<u64>,
    upscaling: Cell<bool>,
    upscaling_slide: Cell<bool>,
    compress_options: Cell<compress::CompressOptions>,
    compress_worker: OnceCell<std::sync::mpsc::Sender<editing::CompressJob>>,
    compress_cancel: RefCell<Option<std::sync::Arc<AtomicBool>>>,
    compress_job_id: Cell<u64>,
    compressing: Cell<bool>,
    #[cfg(any(test, feature = "e2e"))]
    e2e: RefCell<e2e::E2eState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliAppDelegate"]
    #[ivars = DelegateIvars]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSTextFieldDelegate for AppDelegate {}

    unsafe impl NSSearchFieldDelegate for AppDelegate {
        #[unsafe(method(searchFieldDidEndSearching:))]
        fn search_field_did_end_searching(&self, _sender: &objc2_app_kit::NSSearchField) {
            self.focus_grid();
        }
    }

    unsafe impl NSControlTextEditingDelegate for AppDelegate {
        // Only the filter field has this delegate.
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            self.filter_changed();
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_do_command(
            &self,
            _control: &objc2_app_kit::NSControl,
            _text_view: &objc2_app_kit::NSTextView,
            command: objc2::runtime::Sel,
        ) -> bool {
            self.filter_field_command(command)
        }
    }

    // "Open with", rebuilt from the selection each time it opens.
    unsafe impl NSMenuDelegate for AppDelegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.fill_open_with(menu);
        }
    }

    unsafe impl NSWindowDelegate for AppDelegate {
        // Slideshow windows only; the close button and end_slideshow end here.
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.cancel_slide_upscale_for_close();
            let on_screen = self.ivars().show.borrow().as_ref().and_then(|s| s.on_screen.clone());
            if let Some(path) = on_screen {
                self.save_slide_rotation(&path);
            }
            let mut last_slide = None;
            let show = self.ivars().show.borrow_mut().take();
            if let Some(show) = show {
                last_slide = show.playlist.current().cloned();
                if let Some(timer) = show.timer {
                    timer.invalidate();
                }
                if let Some(timer) = show.controls_timer {
                    timer.invalidate();
                }
                if let Some(timer) = show.controls_fade_timer {
                    timer.invalidate();
                }
                if let Some(timer) = show.flash_timer {
                    timer.invalidate();
                }
                // The frame timer retains the view.
                show.view.stop_animation();
            }
            if let Some(window) = self.ivars().window.get() {
                window.makeKeyAndOrderFront(None);
                // Re-activate after closing a slideshow launched from Finder.
                #[allow(deprecated)]
                NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
            }
            // Sync grid selection to the last viewed slide.
            let mut synced = false;
            if let (Some(path), Some(grid)) = (last_slide, self.ivars().grid.get()) {
                if grid.holds_all(std::slice::from_ref(&path)) {
                    grid.select_paths(std::slice::from_ref(&path));
                    self.focus_grid();
                    synced = true;
                }
            }
            if !synced {
                self.refresh_info();
            }
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            if let Some(show) = self.ivars().show.borrow().as_ref() {
                if let Some(compare) = show.view.compare_view() {
                    compare.clear_peek();
                }
            }
        }
    }

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(
            &self,
            _app: &NSApplication,
            urls: &objc2_foundation::NSArray<objc2_foundation::NSURL>,
        ) {
            if e2e::enabled() {
                return;
            }
            let sandboxed = grants::sandboxed();
            let paths: Vec<_> = urls.iter().filter_map(|url| {
                let path = local_file_path(&url)?;
                if sandboxed && path.is_dir() {
                    grants::remember(url);
                }
                Some(path)
            }).collect();
            if !paths.is_empty() {
                self.open_from_finder(paths);
            }
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            // On a cold start Finder's open call came first.
            if self.ivars().opened_from_finder.get() {
                return;
            }
            // A folder argument, else the startup folder, the last folder, or
            // Pictures.
            let arg = std::env::args_os()
                .nth(1)
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .map(|p| std::path::absolute(&p).unwrap_or(p));
            match arg {
                Some(path) if path.is_dir() => {
                    self.scan_root(path, true);
                    return;
                }
                // An unbundled run gets files on the command line.
                Some(path) => {
                    self.open_from_finder(vec![path]);
                    return;
                }
                None => {}
            }
            let home = grants::home();
            let fallback = startup_folder_pref()
                .map(PathBuf::from)
                .filter(|p| grants::readable(p))
                .or_else(|| last_folder_pref().filter(|p| grants::readable(p)))
                .or_else(|| Some(home.join("Pictures")).filter(|p| grants::readable(p)));
            if let Some(root) = fallback {
                self.scan_root_inner(root, true, false);
            }
        }

        /// A rotation still on screen, or on its way to disk, is saved first.
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            self.finish_edits_before_quit();
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window(&self, _app: &NSApplication) -> bool {
            true
        }
    }

    impl AppDelegate {
        #[unsafe(method(openDocument:))]
        fn open_document(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.open_folder();
        }

        #[unsafe(method(startSlideshow:))]
        fn start_slideshow_menu(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.start_from_grid();
        }

        #[unsafe(method(toggleLoop:))]
        fn toggle_loop(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let enabled = !self.ivars().loop_enabled.get();
            self.ivars().loop_enabled.set(enabled);
            if let Some(item) = self.ivars().loop_item.get() {
                item.setState(if enabled { 1 } else { 0 });
            }
            if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
                show.playlist.looping = enabled;
            }
            self.update_overlay();
            self.save_prefs();
            self.sync_prefs_controls();
        }

        #[unsafe(method(toggleShuffle:))]
        fn toggle_shuffle(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let enabled = !self.ivars().shuffle_enabled.get();
            self.ivars().shuffle_enabled.set(enabled);
            if let Some(item) = self.ivars().shuffle_item.get() {
                item.setState(if enabled { 1 } else { 0 });
            }
            if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
                show.playlist.set_shuffled(enabled);
            }
            self.update_overlay();
            self.save_prefs();
            self.sync_prefs_controls();
        }

        #[unsafe(method(setAutoAdvanceMenu:))]
        fn set_auto_advance_menu(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSMenuItem>())
                .map(|item| item.tag())
                .unwrap_or(0);
            self.set_auto_advance(prefs::seconds(tag));
        }

        #[unsafe(method(setSortMenu:))]
        fn set_sort_menu(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSMenuItem>())
                .map(|item| item.tag())
                .unwrap_or(1);
            let order = menu::sort_for_tag(tag);
            // Re-selecting the active sort order flips direction; date orders default to newest first.
            if self.ivars().sort_order.get() == order {
                self.ivars().sort_desc.set(!self.ivars().sort_desc.get());
            } else {
                self.ivars().sort_order.set(order);
                self.ivars().sort_desc.set(matches!(
                    order,
                    SortOrder::Modified | SortOrder::ExifDate | SortOrder::Added
                ));
            }
            self.apply_sort();
        }

        #[unsafe(method(showPrefs:))]
        fn show_prefs_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.show_prefs();
        }

        #[unsafe(method(prefsTabChanged:))]
        fn prefs_tab_changed_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.prefs_tab_changed();
        }

        #[unsafe(method(prefsResetShortcuts:))]
        fn prefs_reset_shortcuts_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.reset_shortcuts();
        }

        #[unsafe(method(prefsWindowInactive:))]
        fn prefs_window_inactive(&self, _notification: Option<&NSNotification>) {
            self.stop_shortcut_recording();
        }

        #[unsafe(method(toggleScaleUp:))]
        fn toggle_scale_up(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_scale_up_small();
        }

        #[unsafe(method(prefsDragCopies:))]
        fn prefs_drag_copies(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_drag_out_copies();
        }

        #[unsafe(method(prefsThumbCap:))]
        fn prefs_thumb_cap(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let Some(popup) = sender.and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
            else {
                return;
            };
            let cap = popup.selectedItem().map(|item| item.tag()).unwrap_or(320) as f64;
            self.apply_thumb_cap(cap);
        }

        #[unsafe(method(prefsAutoAdvance:))]
        fn prefs_auto_advance(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
                .map(|p| p.selectedTag())
                .unwrap_or(0);
            self.set_auto_advance(prefs::seconds(tag));
        }

        #[unsafe(method(prefsChooseStartup:))]
        fn prefs_choose_startup(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let mtm = self.mtm();
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseDirectories(true);
            panel.setCanChooseFiles(false);
            if !self.run_panel(&panel) {
                return;
            }
            let Some(path) = panel.URL().and_then(grants::remember) else { return };
            if !e2e::enabled() {
                let value = NSString::from_str(&path.to_string_lossy());
                unsafe {
                    NSUserDefaults::standardUserDefaults()
                        .setObject_forKey(Some(&value), ns_string!("startupFolder"));
                }
            }
            self.sync_prefs_controls();
        }

        #[unsafe(method(prefsClearStartup:))]
        fn prefs_clear_startup(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if !e2e::enabled() {
                unsafe {
                    NSUserDefaults::standardUserDefaults()
                        .setObject_forKey(None, ns_string!("startupFolder"));
                }
            }
            self.sync_prefs_controls();
        }

        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            self.menu_item_enabled(item)
        }

        #[unsafe(method(contentResized:))]
        fn content_resized(&self, _notification: Option<&NSNotification>) {
            self.layout_content();
        }

        #[unsafe(method(browseWindowKeyChanged:))]
        fn browse_window_key_changed(&self, _notification: Option<&NSNotification>) {
            if let Some(grid) = self.ivars().grid.get() {
                if grid.has_selection() {
                    grid.setNeedsDisplay(true);
                }
            }
        }

        #[unsafe(method(showFilter:))]
        fn show_filter(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.open_filter();
        }

        #[unsafe(method(moveToTrash:))]
        fn move_to_trash(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.trash_selection();
        }

        #[unsafe(method(thumbSliderMoved:))]
        fn thumb_slider_moved_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.thumb_slider_moved();
        }

        #[unsafe(method(undo:))]
        fn undo_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.take_back(true);
        }

        #[unsafe(method(redo:))]
        fn redo_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.take_back(false);
        }

        #[unsafe(method(openWith:))]
        fn open_with(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) else { return };
            let Some(app) = item.representedObject() else { return };
            let Ok(app) = app.downcast::<NSURL>() else { return };
            self.open_targets_with(&app);
        }

        #[unsafe(method(copy:))]
        fn copy_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.copy_images_to_clipboard();
        }

        #[unsafe(method(paste:))]
        fn paste_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.paste_images();
        }

        #[unsafe(method(copyPath:))]
        fn copy_path(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.copy_targets(false);
        }

        #[unsafe(method(copyFileUrl:))]
        fn copy_file_url(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.copy_targets(true);
        }

        #[unsafe(method(setDesktopPicture:))]
        fn set_desktop_picture_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.use_as_desktop_picture();
        }

        #[unsafe(method(getInfo:))]
        fn get_info(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_info();
        }

        #[unsafe(method(revealInFinder:))]
        fn reveal_in_finder(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.reveal_targets();
        }

        #[unsafe(method(moveToFolder:))]
        fn move_to_folder(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.transfer_to_chosen_folder(Transfer::Move);
        }

        #[unsafe(method(copyToFolder:))]
        fn copy_to_folder(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.transfer_to_chosen_folder(Transfer::Copy);
        }

        #[unsafe(method(moveAgain:))]
        fn move_again(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let folder = self.ivars().last_move.borrow().clone();
            if let Some(folder) = folder {
                self.transfer_selection(folder, Transfer::Move);
            }
        }

        #[unsafe(method(copyAgain:))]
        fn copy_again(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let folder = self.ivars().last_copy.borrow().clone();
            if let Some(folder) = folder {
                self.transfer_selection(folder, Transfer::Copy);
            }
        }

        #[unsafe(method(clearFlash:))]
        fn clear_flash(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            let (overlay, active_edit) = {
                let mut show = self.ivars().show.borrow_mut();
                let Some(show) = show.as_mut() else { return };
                show.flash = None;
                show.flash_timer = None;
                (show.overlay_box.clone(), show.controls.active_edit())
            };
            overlay.setHidden(active_edit || !self.ivars().overlay_visible.get());
            self.update_overlay();
        }

        #[unsafe(method(toggleSidebarMenu:))]
        fn toggle_sidebar_menu(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_sidebar();
        }

        #[unsafe(method(toggleLabels:))]
        fn toggle_labels(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let Some(grid) = self.ivars().grid.get() else { return };
            let visible = !grid.labels_visible();
            grid.set_labels(visible);
            if let Some(item) = self.ivars().labels_item.get() {
                item.setState(if visible { 1 } else { 0 });
            }
            self.save_prefs();
            self.sync_prefs_controls();
        }

        #[unsafe(method(biggerThumbs:))]
        fn bigger_thumbs(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if let Some(grid) = self.ivars().grid.get() {
                grid.scale_cells(1.25);
            }
        }

        #[unsafe(method(smallerThumbs:))]
        fn smaller_thumbs(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if let Some(grid) = self.ivars().grid.get() {
                grid.scale_cells(0.8);
            }
        }

        #[cfg(feature = "snapshot")]
        #[unsafe(method(snapshotStep:))]
        fn snapshot_step(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            snapshot::step(self);
        }

        #[cfg(any(test, feature = "e2e"))]
        #[unsafe(method(e2eStep:))]
        fn e2e_step(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            e2e::run_step(self);
        }

        #[unsafe(method(hideControls:))]
        fn hide_controls_action(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            self.hide_controls();
        }

        #[unsafe(method(controlsFaded:))]
        fn controls_faded_action(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            self.controls_faded();
        }

        #[unsafe(method(slidePrevious:))]
        fn slide_previous(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.step_slideshow(-1);
        }

        #[unsafe(method(slideNext:))]
        fn slide_next(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.step_slideshow(1);
        }

        #[unsafe(method(slidePlayPause:))]
        fn slide_play_pause(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.play_pause();
        }

        #[unsafe(method(slideZoomIn:))]
        fn slide_zoom_in(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.zoom_slide(1);
        }

        #[unsafe(method(slideZoomOut:))]
        fn slide_zoom_out(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.zoom_slide(-1);
        }

        #[unsafe(method(slideZoomFit:))]
        fn slide_zoom_fit(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.zoom_slide(0);
        }

        #[unsafe(method(slideMore:))]
        fn slide_more_action(&self, sender: Option<&objc2::runtime::AnyObject>) {
            self.slide_more(sender);
        }

        #[unsafe(method(endSlideshow:))]
        fn end_slideshow_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.end_slideshow();
        }

        #[unsafe(method(rotateLeft:))]
        fn rotate_left(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.rotate_targets(blickli_core::Orientation::from_view(-90, false));
        }

        #[unsafe(method(rotateRight:))]
        fn rotate_right(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.rotate_targets(blickli_core::Orientation::from_view(90, false));
        }

        #[unsafe(method(flipHorizontal:))]
        fn flip_horizontal(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.rotate_targets(blickli_core::Orientation::from_view(0, true));
        }

        #[unsafe(method(cropImage:))]
        fn crop_image(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.crop_selection();
        }

        #[unsafe(method(cropCancel:))]
        fn crop_cancel(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.cancel_crop();
        }

        #[unsafe(method(cropSave:))]
        fn crop_save(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.apply_crop(true);
        }

        #[unsafe(method(cropSaveCopy:))]
        fn crop_save_copy(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.apply_crop(false);
        }

        #[unsafe(method(cropAspect:))]
        fn crop_aspect(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.crop_aspect_chosen();
        }

        #[unsafe(method(upscaleImage:))]
        fn upscale_image(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.upscale_selection();
        }

        #[unsafe(method(upscaleCancel:))]
        fn upscale_cancel(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.cancel_upscale();
        }

        #[unsafe(method(upscaleSave:))]
        fn upscale_save(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.apply_upscale(true);
        }

        #[unsafe(method(upscaleSaveCopy:))]
        fn upscale_save_copy(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.apply_upscale(false);
        }

        #[unsafe(method(upscaleCompareMode:))]
        fn upscale_compare_mode(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.upscale_compare_mode_chosen();
        }

        #[unsafe(method(upscalePreviewScale:))]
        fn upscale_preview_scale(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.upscale_preview_scale_chosen();
        }

        #[unsafe(method(prefsChooseModels:))]
        fn prefs_choose_models(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let _ = self.choose_models_folder();
        }

        #[unsafe(method(prefsClearModels:))]
        fn prefs_clear_models(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.set_models_folder(None);
        }

        #[unsafe(method(prefsUpscaleScale:))]
        fn prefs_upscale_scale(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
                .map(|p| p.selectedTag())
                .unwrap_or(4);
            self.set_upscale_scale(upscale::UpscaleScale::from_tag(tag));
        }

        #[unsafe(method(prefsUpscaleSaveMode:))]
        fn prefs_upscale_save_mode(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
                .map(|p| p.selectedTag())
                .unwrap_or(0);
            self.set_upscale_save_mode(upscale::UpscaleSaveMode::from_tag(tag));
        }

        #[unsafe(method(compressImage:))]
        fn compress_image(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.compress_selection();
        }

        #[unsafe(method(compressFolder:))]
        fn compress_folder_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.compress_current_folder();
        }

        #[unsafe(method(prefsCompressTarget:))]
        fn prefs_compress_target(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
                .map(|p| p.selectedTag())
                .unwrap_or(compress::DEFAULT_TARGET_KB);
            let mut opts = self.ivars().compress_options.get();
            opts.target_kb = compress::normalize_target_kb(tag);
            self.set_compress_options(opts);
        }

        #[unsafe(method(prefsCompressMaxDims:))]
        fn prefs_compress_max_dims(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
                .map(|p| p.selectedTag())
                .unwrap_or(compress::MaxDimensions::default().tag());
            let mut opts = self.ivars().compress_options.get();
            opts.max_dims = compress::MaxDimensions::from_tag(tag);
            self.set_compress_options(opts);
        }

        #[unsafe(method(prefsCompressFallback:))]
        fn prefs_compress_fallback(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
                .map(|p| p.selectedTag())
                .unwrap_or(0);
            let mut opts = self.ivars().compress_options.get();
            opts.fallback = compress::FallbackFormat::from_tag(tag);
            self.set_compress_options(opts);
        }

        #[unsafe(method(toggleControls:))]
        fn toggle_controls_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_controls();
        }

        #[unsafe(method(prefsSaveRotations:))]
        fn prefs_save_rotations(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_save_rotations();
        }

        #[unsafe(method(filterAction:))]
        fn filter_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.filter_changed();
        }

        #[unsafe(method(validateToolbarItem:))]
        fn validate_toolbar_item(&self, item: &objc2_app_kit::NSToolbarItem) -> bool {
            self.action_enabled(item.action())
        }

        #[unsafe(method(advanceSlide:))]
        fn advance_slide(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            let stepped = match self.ivars().show.borrow_mut().as_mut() {
                Some(show) => show.playlist.step(1),
                None => return,
            };
            if stepped {
                self.show_current();
            } else {
                self.set_auto_advance(None);
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn handle_event(&self, event: Event<Img>) {
        if self.ivars().engine.get().unwrap().is_stale(&event) {
            return;
        }
        match event {
            Event::FilesInserted { files, .. } => {
                let order = self.ivars().sort_order.get();
                if order.needs_file_dates() {
                    self.ivars().engine.get().unwrap().request_file_dates(
                        files.iter().map(|(_, file)| file.path.clone()).collect(),
                        order.needs_exif(),
                    );
                }
                self.ivars().grid.get().unwrap().insert_files(
                    files,
                    order,
                    self.ivars().sort_desc.get(),
                );
            }
            Event::FileDatesReady { files, .. } => {
                self.ivars().grid.get().unwrap().apply_file_dates(
                    &files,
                    self.ivars().sort_order.get(),
                    self.ivars().sort_desc.get(),
                );
            }
            Event::FilesRemoved { paths, .. } => {
                self.ivars().grid.get().unwrap().remove_paths(&paths);
                self.drop_from_slideshow(&paths);
            }
            Event::FileChanged { info, .. } => {
                let stale = matches!(
                    self.ivars().header_rows.borrow().as_ref(),
                    Some((cached, _)) if *cached == info.path
                );
                if stale {
                    *self.ivars().header_rows.borrow_mut() = None;
                }
                let grid = self.ivars().grid.get().unwrap();
                let order = self.ivars().sort_order.get();
                grid.file_changed(&info, order, self.ivars().sort_desc.get());
                let engine = self.ivars().engine.get().unwrap();
                engine.invalidate_slides();
                if order.needs_file_dates() {
                    engine.request_file_dates(vec![info.path.clone()], order.needs_exif());
                }
                let is_current = {
                    let mut show = self.ivars().show.borrow_mut();
                    if let Some(show) = show.as_mut() {
                        show.cache.remove(&info.path);
                        show.playlist.current() == Some(&info.path)
                    } else {
                        false
                    }
                };
                if is_current {
                    self.show_current();
                }
            }
            Event::ScanDone { .. } => {
                self.ivars().grid.get().unwrap().finish_scan();
                // Their folder is scanned and they're missing: stop waiting.
                let theirs = self.pending_folder();
                if !self.select_pending()
                    && theirs.is_some()
                    && theirs == self.current_scan().map(|(root, _)| root)
                {
                    self.ivars().pending_open.borrow_mut().clear();
                }
                self.update_status();
                #[cfg(feature = "snapshot")]
                if snapshot::enabled() {
                    snapshot::start(self);
                }
                #[cfg(any(test, feature = "e2e"))]
                if e2e::enabled() && !self.ivars().e2e.borrow().started {
                    self.ivars().e2e.borrow_mut().started = true;
                    unsafe {
                        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                            0.9,
                            self,
                            sel!(e2eStep:),
                            None,
                            true,
                        )
                    };
                }
            }
            Event::Thumbnail { request, image } => {
                self.ivars()
                    .grid
                    .get()
                    .unwrap()
                    .update_thumbnail(request, image);
            }
            Event::SlideReady { path, image, .. } => {
                let cost = image_cost(&image);
                let image = ns_image(&image);
                let natural = Self::natural_size(&path);
                let display = {
                    let mut show = self.ivars().show.borrow_mut();
                    let Some(show) = show.as_mut() else { return };
                    let is_current = show.playlist.current() == Some(&path);
                    let state = show.view_states.get(&path).copied();
                    // The file changed under the view: no zoom to keep.
                    let fresh = is_current && show.saving.redecode.remove(&path);
                    if show
                        .pending_decode
                        .as_ref()
                        .is_some_and(|(waiting, _)| *waiting == path)
                    {
                        show.pending_decode = None;
                    }
                    let same_displayed = show.displayed_path.as_deref() == Some(path.as_path());
                    show.cache.insert(path.clone(), (image.clone(), natural), cost);
                    if is_current {
                        // Preserve zoom only when swapping in a higher-res decode of the same slide.
                        let swap = !fresh
                            && same_displayed
                            && show.view.has_image()
                            && show.view.natural_size() == natural;
                        if !swap {
                            show.displayed_path = Some(path);
                        }
                        Some((show.view.clone(), swap, state))
                    } else {
                        None
                    }
                };
                if let Some((view, swap, state)) = display {
                    let mut restored_zoom = false;
                    if swap {
                        view.swap_decode(image);
                    } else {
                        view.show_image(image, natural);
                        if let Some(state) = state {
                            view.apply_state(state);
                            restored_zoom = state.zoom.is_some();
                        }
                    }
                    if restored_zoom {
                        self.ensure_zoom_resolution();
                    }
                    self.update_overlay();
                    let (crop, upscale) = {
                        let mut show = self.ivars().show.borrow_mut();
                        match show.as_mut() {
                            Some(show) => (
                                std::mem::take(&mut show.crop_when_ready),
                                std::mem::take(&mut show.upscale_when_ready),
                            ),
                            None => (false, false),
                        }
                    };
                    if crop {
                        self.start_crop();
                    } else if upscale {
                        self.start_upscale_preview();
                    }
                }
            }
            Event::SlideFramesReady { path, frames, .. } => {
                let display = {
                    let mut show = self.ivars().show.borrow_mut();
                    let Some(show) = show.as_mut() else { return };
                    if show.playlist.current() == Some(&path) {
                        show.saving.redecode.remove(&path);
                        let state = show.view_states.get(&path).copied();
                        let view = show.view.clone();
                        show.displayed_path = Some(path.clone());
                        Some((view, state))
                    } else {
                        None
                    }
                };
                if let Some((view, state)) = display {
                    let frames: Vec<_> =
                        frames.iter().map(|(img, d)| (ns_image(img), *d)).collect();
                    view.show_frames(frames, Self::natural_size(&path));
                    if let Some(state) = state {
                        view.apply_state(state);
                    }
                    self.update_overlay();
                    let (asked_crop, asked_upscale) = {
                        let mut show = self.ivars().show.borrow_mut();
                        match show.as_mut() {
                            Some(show) => (
                                std::mem::take(&mut show.crop_when_ready),
                                std::mem::take(&mut show.upscale_when_ready),
                            ),
                            None => (false, false),
                        }
                    };
                    if asked_crop {
                        self.flash_overlay("Animated images cannot be cropped");
                    } else if asked_upscale {
                        self.flash_overlay("Animated images cannot be upscaled");
                    }
                }
            }
            Event::DecodeFailed { path, .. } => {
                let failed_view = {
                    let mut show = self.ivars().show.borrow_mut();
                    match show.as_mut() {
                        Some(show) => {
                            if show
                                .pending_decode
                                .as_ref()
                                .is_some_and(|(waiting, _)| *waiting == path)
                            {
                                show.pending_decode = None;
                            }
                            if show.playlist.current() == Some(&path) {
                                show.crop_when_ready = false;
                                show.upscale_when_ready = false;
                                show.displayed_path = None;
                                Some(show.view.clone())
                            } else {
                                None
                            }
                        }
                        None => None,
                    }
                };
                if let Some(view) = failed_view {
                    view.clear_image();
                    if path.exists() {
                        eprintln!("decode failed: {}", path.display());
                        self.flash_overlay(&format!("Could not open {}", file_name(&path)));
                    }
                } else if path.exists() {
                    eprintln!("decode failed: {}", path.display());
                }
            }
        }
    }
}

fn ns_image(image: &Img) -> Retained<NSImage> {
    NSImage::initWithCGImage_size(NSImage::alloc(), image, CGSize::new(0.0, 0.0))
}

fn image_cost(image: &Img) -> usize {
    CGImage::bytes_per_row(Some(image)).saturating_mul(CGImage::height(Some(image)))
}

fn on_main(f: impl FnOnce(&AppDelegate) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("main queue is the main thread");
        f(DELEGATE.get().expect("delegate is set at launch").get(mtm));
    });
}

fn drain_events(mtm: MainThreadMarker) {
    drain_event_batch(mtm, usize::MAX);
}

fn drain_event_batch(mtm: MainThreadMarker, limit: usize) -> bool {
    let delegate = DELEGATE.get().unwrap().get(mtm);
    let events = EVENTS.get().unwrap();
    for _ in 0..limit {
        // Release the mutex before handling the event so modal panels or nested
        // `notice_files -> drain_events` calls cannot deadlock on `EVENTS`.
        let Ok(event) = events.lock().unwrap().try_recv() else {
            return false;
        };
        delegate.handle_event(event);
    }
    true
}

fn schedule_event_drain() {
    if EVENT_DRAIN_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    DispatchQueue::main().exec_async(|| {
        EVENT_DRAIN_PENDING.store(false, Ordering::Release);
        let mtm = MainThreadMarker::new().expect("main queue is the main thread");
        if drain_event_batch(mtm, 32) {
            schedule_event_drain();
        }
    });
}

fn main() {
    #[cfg(feature = "e2e")]
    if e2e::enabled() && !std::env::args_os().nth(1).is_some_and(e2e::is_fixture) {
        eprintln!("BLICKLI_E2E requires a folder created by scripts/make-fixture.sh");
        std::process::exit(2);
    }

    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Snapshots draw offscreen and must never take the focus.
    app.setActivationPolicy(if snapshot::enabled() {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    });

    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    build_menu(mtm, &app, &delegate);

    // Restore security-scoped bookmarks before building the sidebar so granted
    // folders and external drives are readable when initial rows query subfolders.
    grants::restore();
    let window = browse::build_browse_window(mtm, &delegate);
    // Load preferences before `app.run()` so Finder cold launches (`application:openURLs:`)
    // honor saved loop, shuffle, controls, and auto-advance settings immediately.
    delegate.load_prefs();

    let screen_long_edge = NSScreen::mainScreen(mtm)
        .map(|s| {
            let f = s.frame();
            f.size.width.max(f.size.height)
        })
        .unwrap_or(2048.0);
    let (engine, rx) = Engine::start(
        ImageIoDecoder,
        // Cap initial slide decodes at 2x screen size (256px in e2e to exercise zoom re-decode).
        if e2e::enabled() {
            256
        } else {
            (screen_long_edge * 2.0) as i32
        },
        schedule_event_drain,
    );
    let _ = delegate.ivars().engine.set(engine);
    assert!(EVENTS.set(Mutex::new(rx)).is_ok(), "EVENTS set once");
    assert!(
        DELEGATE.set(MainThreadBound::new(delegate, mtm)).is_ok(),
        "DELEGATE set once"
    );

    if !snapshot::enabled() {
        window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    app.run();
}

fn local_file_path(url: &NSURL) -> Option<PathBuf> {
    if !url.isFileURL()
        || url.host().is_some_and(|host| {
            let host = host.to_string();
            !host.is_empty() && !host.eq_ignore_ascii_case("localhost")
        })
    {
        return None;
    }
    url.to_file_path().filter(|path| path.is_absolute())
}

#[cfg(test)]
#[test]
fn opens_only_local_file_urls() {
    let parse =
        |text: &str| local_file_path(&NSURL::URLWithString(&NSString::from_str(text)).unwrap());
    assert_eq!(
        parse("file:///tmp/my%20image.jpg"),
        Some(PathBuf::from("/tmp/my image.jpg"))
    );
    assert!(parse("https://example.com/tmp/image.jpg").is_none());
    assert!(parse("file://example.com/tmp/image.jpg").is_none());
}
