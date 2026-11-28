//! Offscreen window snapshot generator for visual layout checks.
//! Build with `--features snapshot` and run with `BLICKLI_SNAPSHOT=<out_dir>`:
//!
//!     BLICKLI_SNAPSHOT=<out_dir> \
//!         cargo run -p blickli --features snapshot -- <image folder>
//!
//! Runs as an accessory app without activating, writes PNGs for the browse
//! window, slideshow, overlay, crop mode, and Settings, then exits.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use objc2_app_kit::{NSBitmapImageFileType, NSView};
use objc2_core_foundation::{CFString, CFURL};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace,
    CGContext, CGImageAlphaInfo,
};
use objc2_foundation::NSDictionary;
use objc2_image_io::CGImageDestination;

use crate::*;

static STEP: AtomicUsize = AtomicUsize::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    std::env::var_os("BLICKLI_SNAPSHOT").is_some()
}

fn out(name: &str) -> PathBuf {
    let dir = PathBuf::from(std::env::var_os("BLICKLI_SNAPSHOT").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    dir.join(name)
}

/// Render a view hierarchy with AppKit's `cacheDisplayInRect`.
fn cache_display(view: &NSView, path: &Path) {
    view.layoutSubtreeIfNeeded();
    view.displayIfNeeded();
    let bounds = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(bounds) else {
        eprintln!("snapshot: no bitmap for {}", path.display());
        return;
    };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    };
    if let Some(data) = data {
        let _ = data.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true);
        println!("snapshot: {}", path.display());
    }
}

/// Render a view's Core Animation layer tree (captures GPU-backed slide layers).
fn render_layers(view: &NSView, path: &Path, scale: f64) {
    view.layoutSubtreeIfNeeded();
    view.displayIfNeeded();
    let Some(layer) = view.layer() else { return };
    let size = view.bounds().size;
    let (w, h) = ((size.width * scale) as usize, (size.height * scale) as usize);
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
    let Some(context) = (unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            w,
            h,
            8,
            0,
            space.as_deref(),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }) else {
        return;
    };
    CGContext::scale_ctm(Some(&context), scale, scale);
    layer.renderInContext(&context);
    let Some(image) = CGBitmapContextCreateImage(Some(&context)) else { return };
    write_png(&image, path);
}

fn write_png(image: &objc2_core_graphics::CGImage, path: &Path) {
    let Some(url) = CFURL::from_file_path(path) else { return };
    unsafe {
        let Some(destination) =
            CGImageDestination::with_url(&url, &CFString::from_str("public.png"), 1, None)
        else {
            return;
        };
        destination.add_image(image, None);
        if destination.finalize() {
            println!("snapshot: {}", path.display());
        }
    }
}

/// The window's frame view: title bar, toolbar and content.
fn frame_view(window: &NSWindow) -> Option<Retained<NSView>> {
    window.contentView().and_then(|view| unsafe { view.superview() })
}

fn draw_window(window: &NSWindow, name: &str) {
    let scale = window.backingScaleFactor();
    if let Some(frame) = frame_view(window) {
        cache_display(&frame, &out(&format!("{name}.png")));
        render_layers(&frame, &out(&format!("{name}-layers.png")), scale);
    }
}

pub fn start(delegate: &AppDelegate) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            1.5,
            delegate,
            sel!(snapshotStep:),
            None,
            true,
        )
    };
}

pub fn step(delegate: &AppDelegate) {
    let window = delegate.ivars().window.get().cloned();
    let slide_window = || {
        delegate
            .ivars()
            .show
            .borrow()
            .as_ref()
            .map(|show| show.window.clone())
    };
    match STEP.fetch_add(1, Ordering::SeqCst) {
        0 => {
            if let Some(grid) = delegate.ivars().grid.get() {
                grid.select_paths(&grid.paths().into_iter().take(1).collect::<Vec<_>>());
            }
        }
        1 => {
            if let Some(window) = &window {
                draw_window(window, "browse");
            }
            delegate.start_from_grid();
        }
        2 => {
            if let Some(window) = slide_window() {
                draw_window(&window, "slideshow");
            }
            delegate.toggle_overlay();
        }
        3 => {
            if let Some(window) = slide_window() {
                draw_window(&window, "slideshow-overlay");
            }
            delegate.toggle_overlay();
            delegate.toggle_help_overlay();
        }
        4 => {
            if let Some(window) = slide_window() {
                draw_window(&window, "slideshow-help");
            }
            delegate.toggle_help_overlay();
            delegate.start_crop();
        }
        5 => {
            if let Some(window) = slide_window() {
                draw_window(&window, "crop");
            }
            delegate.cancel_crop();
            let prefs = prefs::build_prefs_window(delegate.mtm(), delegate);
            let _ = delegate.ivars().prefs_window.set(prefs);
            delegate.sync_prefs_controls();
            delegate.sync_models_summary();
            delegate.sync_shortcut_recorders();
            if let Some(prefs) = delegate.ivars().prefs_window.get() {
                draw_window(prefs, "settings");
            }
        }
        _ => std::process::exit(0),
    }
}
