//! Settings window and `NSUserDefaults` persistence.

use super::*;

impl AppDelegate {
    /// Load saved preferences at launch (skipped in e2e runs to keep defaults deterministic).
    pub(crate) fn load_prefs(&self) {
        self.apply_thumb_cap(thumb_cap_pref());
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_cell_size(thumb_size_pref());
        }
        self.ivars().show_controls.set(true);
        if e2e::enabled() {
            // Keep save_rotations off in e2e runs so fixture files stay unmodified.
            self.sync_controls_item();
            return;
        }
        let d = NSUserDefaults::standardUserDefaults();
        let on_by_default =
            |key: &NSString| d.objectForKey(key).is_none() || d.boolForKey(key);
        self.ivars()
            .save_rotations
            .set(on_by_default(ns_string!("saveRotations")));
        self.ivars()
            .upscale_scale
            .set(upscale_scale_pref());
        self.ivars()
            .upscale_save_mode
            .set(upscale_save_mode_pref());
        self.ivars()
            .compress_options
            .set(compress_options_pref());
        self.ivars().shortcuts.borrow_mut().load_from_defaults();
        self.apply_shortcuts();
        self.ivars()
            .show_controls
            .set(on_by_default(ns_string!("showControls")));
        self.sync_controls_item();
        if d.boolForKey(ns_string!("slideshowLoop")) {
            self.ivars().loop_enabled.set(true);
            if let Some(item) = self.ivars().loop_item.get() {
                item.setState(1);
            }
        }
        if d.boolForKey(ns_string!("slideshowShuffle")) {
            self.ivars().shuffle_enabled.set(true);
            if let Some(item) = self.ivars().shuffle_item.get() {
                item.setState(1);
            }
        }
        self.ivars()
            .drag_out_copies
            .set(d.boolForKey(ns_string!("dragOutCopies")));
        if d.boolForKey(ns_string!("scaleUpSmall")) {
            self.ivars().scale_up_small.set(true);
            if let Some(item) = self.ivars().scale_up_item.get() {
                item.setState(1);
            }
        }
        let seconds = seconds(d.integerForKey(ns_string!("autoAdvanceTenths")));
        self.ivars().default_interval.set(seconds);
        self.update_auto_menu(seconds);
        if d.boolForKey(ns_string!("showFilenames")) {
            if let Some(grid) = self.ivars().grid.get() {
                grid.set_labels(true);
            }
            if let Some(item) = self.ivars().labels_item.get() {
                item.setState(1);
            }
        }
        // Skip unavailable repeat folders (e.g. unmounted volume) without erasing the pref.
        for (key, slot) in [
            (ns_string!("lastMoveFolder"), &self.ivars().last_move),
            (ns_string!("lastCopyFolder"), &self.ivars().last_copy),
        ] {
            let folder = d
                .stringForKey(key)
                .map(|s| PathBuf::from(s.to_string()))
                .filter(|p| p.is_dir());
            *slot.borrow_mut() = folder;
        }
        self.sync_repeat_items();
    }

    pub(crate) fn save_prefs(&self) {
        if e2e::enabled() || snapshot::enabled() {
            return;
        }
        let d = NSUserDefaults::standardUserDefaults();
        d.setBool_forKey(self.ivars().loop_enabled.get(), ns_string!("slideshowLoop"));
        d.setBool_forKey(
            self.ivars().shuffle_enabled.get(),
            ns_string!("slideshowShuffle"),
        );
        let tenths = tenths(self.ivars().default_interval.get());
        d.setInteger_forKey(tenths, ns_string!("autoAdvanceTenths"));
        if let Some(grid) = self.ivars().grid.get() {
            d.setBool_forKey(grid.labels_visible(), ns_string!("showFilenames"));
        }
    }

    pub fn show_prefs(&self) {
        let mtm = self.mtm();
        if self.ivars().prefs_window.get().is_none() {
            let window = build_prefs_window(mtm, self);
            let _ = self.ivars().prefs_window.set(window);
        }
        self.stop_shortcut_recording();
        self.sync_prefs_controls();
        self.sync_models_summary();
        self.sync_shortcut_recorders();
        if let Some(window) = self.ivars().prefs_window.get() {
            window.makeFirstResponder(None);
            window.makeKeyAndOrderFront(None);
        }
    }

    pub(crate) fn stop_shortcut_recording(&self) {
        let mut any = false;
        for recorder in self.ivars().shortcut_recorders.borrow().values() {
            any |= recorder.cancel_recording();
        }
        if any {
            if let Some(window) = self.ivars().prefs_window.get() {
                window.makeFirstResponder(None);
            }
            self.shortcut_feedback("Click a shortcut to change it.");
        }
    }

    pub(crate) fn prefs_tab_changed(&self) {
        let (Some(window), Some(tabs), Some(general), Some(shortcuts_view)) = (
            self.ivars().prefs_window.get(),
            self.ivars().prefs_tabs.get(),
            self.ivars().prefs_general_view.get(),
            self.ivars().prefs_shortcuts_view.get(),
        ) else {
            return;
        };
        window.makeFirstResponder(None);
        let show_shortcuts = tabs.selectedSegment() == 1;
        general.setHidden(show_shortcuts);
        shortcuts_view.setHidden(!show_shortcuts);
        if show_shortcuts {
            self.sync_shortcut_recorders();
            self.shortcut_feedback("Click a shortcut to change it.");
        }
        let body_h = if show_shortcuts {
            SHORTCUTS_BODY_H
        } else {
            self.ivars().prefs_general_h.get()
        };
        let target_content = CGSize::new(PREFS_WIDTH, body_h + PREFS_TAB_BAR_H);
        let old_frame = window.frame();
        let new_rect = window.frameRectForContentRect(NSRect::new(CGPoint::new(0.0, 0.0), target_content));
        let top = old_frame.origin.y + old_frame.size.height;
        let mut next_frame = NSRect::new(
            CGPoint::new(old_frame.origin.x, top - new_rect.size.height),
            new_rect.size,
        );
        if let Some(screen) = window
            .screen()
            .or_else(|| NSScreen::mainScreen(self.mtm()))
        {
            let vis = screen.visibleFrame();
            if next_frame.origin.y < vis.origin.y {
                next_frame.origin.y = vis.origin.y;
            }
            if next_frame.origin.y + next_frame.size.height > vis.origin.y + vis.size.height {
                next_frame.origin.y = vis.origin.y + vis.size.height - next_frame.size.height;
            }
        }
        window.setFrame_display_animate(next_frame, true, false);
    }

    pub(crate) fn shortcut_feedback(&self, text: &str) {
        if let Some(label) = self.ivars().shortcut_feedback.get() {
            label.setStringValue(&NSString::from_str(text));
        }
    }

    pub(crate) fn sync_shortcut_recorders(&self) {
        let store = self.ivars().shortcuts.borrow();
        let recorders = self.ivars().shortcut_recorders.borrow();
        for (&action, recorder) in recorders.iter() {
            recorder.set_combo(store.get(action).cloned());
        }
    }

    /// Assign or clear a shortcut from the Settings recorder; returns `true` if accepted.
    pub(crate) fn update_shortcut(
        &self,
        action: crate::shortcuts::Action,
        combo: Option<crate::shortcuts::KeyCombo>,
    ) -> bool {
        let outcome = self.ivars().shortcuts.borrow_mut().assign(action, combo.clone());
        match outcome {
            crate::shortcuts::AssignOutcome::Assigned { cleared } => {
                if !e2e::enabled() {
                    self.ivars().shortcuts.borrow().save_to_defaults();
                }
                self.apply_shortcuts();
                self.sync_shortcut_recorders();
                let msg = match (combo, cleared) {
                    (None, _) => format!("Cleared {}.", action.label()),
                    (Some(c), Some(prev)) => {
                        format!("Set {} to {} (cleared {}).", action.label(), c.display(), prev.label())
                    }
                    (Some(c), None) => format!("Set {} to {}.", action.label(), c.display()),
                };
                self.shortcut_feedback(&msg);
                true
            }
            crate::shortcuts::AssignOutcome::Rejected(why) => {
                self.shortcut_feedback(&why);
                false
            }
        }
    }

    pub(crate) fn reset_shortcuts(&self) {
        if let Some(window) = self.ivars().prefs_window.get() {
            window.makeFirstResponder(None);
        }
        self.ivars().shortcuts.borrow_mut().reset_all();
        if !e2e::enabled() {
            self.ivars().shortcuts.borrow().save_to_defaults();
        }
        self.apply_shortcuts();
        self.sync_shortcut_recorders();
        self.shortcut_feedback("Shortcuts reset to defaults.");
    }

    pub(crate) fn sync_models_summary(&self) {
        let Some(window) = self.ivars().prefs_window.get() else {
            return;
        };
        let Some(content) = window.contentView() else {
            return;
        };
        let models = models_folder_pref();
        if let Some(view) = content.viewWithTag(14) {
            if let Some(field) = view.downcast_ref::<NSTextField>() {
                let text = NSString::from_str(models.as_deref().unwrap_or("Not set"));
                field.setStringValue(&text);
                field.setToolTip(models.as_ref().map(|_| &*text));
            }
        }
        if let Some(view) = content.viewWithTag(16) {
            if let Some(button) = view.downcast_ref::<objc2_app_kit::NSButton>() {
                button.setEnabled(models.is_some());
            }
        }
        if let Some(view) = content.viewWithTag(17) {
            if let Some(field) = view.downcast_ref::<NSTextField>() {
                let summary =
                    crate::upscale::model_summary(models.as_deref().map(std::path::Path::new));
                field.setStringValue(&NSString::from_str(&summary));
            }
        }
    }

    pub(crate) fn sync_prefs_controls(&self) {
        let Some(window) = self.ivars().prefs_window.get() else {
            return;
        };
        let Some(content) = window.contentView() else {
            return;
        };
        let set_check = |tag: isize, on: bool| {
            if let Some(view) = content.viewWithTag(tag) {
                if let Some(button) = view.downcast_ref::<objc2_app_kit::NSButton>() {
                    button.setState(if on { 1 } else { 0 });
                }
            }
        };
        let set_popup = |tag: isize, value: isize| {
            if let Some(view) = content.viewWithTag(tag) {
                if let Some(popup) = view.downcast_ref::<objc2_app_kit::NSPopUpButton>() {
                    if !popup.selectItemWithTag(value) {
                        popup.selectItem(None);
                    }
                }
            }
        };
        set_check(2, self.ivars().loop_enabled.get());
        set_check(3, self.ivars().shuffle_enabled.get());
        set_check(
            4,
            self.ivars().grid.get().is_some_and(|g| g.labels_visible()),
        );
        set_check(9, self.ivars().drag_out_copies.get());
        set_check(10, self.ivars().scale_up_small.get());
        set_check(12, self.ivars().save_rotations.get());
        set_check(13, self.ivars().show_controls.get());
        set_popup(6, tenths(self.ivars().default_interval.get()));
        let cap = self
            .ivars()
            .grid
            .get()
            .map(|grid| grid.cell_cap())
            .unwrap_or(grid::CELL_CAPS[1]);
        set_popup(8, cap as isize);
        let folder = startup_folder_pref();
        if let Some(view) = content.viewWithTag(7) {
            if let Some(field) = view.downcast_ref::<NSTextField>() {
                let text = NSString::from_str(folder.as_deref().unwrap_or("Last opened folder"));
                field.setStringValue(&text);
                field.setToolTip(folder.as_ref().map(|_| &*text));
            }
        }
        if let Some(view) = content.viewWithTag(11) {
            if let Some(button) = view.downcast_ref::<objc2_app_kit::NSButton>() {
                button.setEnabled(folder.is_some());
            }
        }
        set_popup(18, self.ivars().upscale_scale.get().tag());
        set_popup(19, self.ivars().upscale_save_mode.get().tag());
        let compress = self.ivars().compress_options.get();
        set_popup(20, compress.target_kb);
        set_popup(21, compress.max_dims.tag());
        set_popup(22, compress.fallback.tag());
    }

    fn sync_controls_item(&self) {
        if let Some(item) = self.ivars().controls_item.get() {
            item.setState(if self.ivars().show_controls.get() { 1 } else { 0 });
        }
    }

    pub(crate) fn toggle_save_rotations(&self) {
        let on = !self.ivars().save_rotations.get();
        self.ivars().save_rotations.set(on);
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults().setBool_forKey(on, ns_string!("saveRotations"));
        }
        self.sync_prefs_controls();
    }

    pub fn scales_small_images_up(&self) -> bool {
        self.ivars().scale_up_small.get()
    }

    pub(crate) fn toggle_scale_up_small(&self) {
        let on = !self.ivars().scale_up_small.get();
        self.ivars().scale_up_small.set(on);
        if let Some(item) = self.ivars().scale_up_item.get() {
            item.setState(if on { 1 } else { 0 });
        }
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults().setBool_forKey(on, ns_string!("scaleUpSmall"));
        }
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.view.refresh();
        }
        self.sync_prefs_controls();
    }

    pub fn drag_out_copies(&self) -> bool {
        self.ivars().drag_out_copies.get()
    }

    pub(crate) fn toggle_drag_out_copies(&self) {
        let on = !self.ivars().drag_out_copies.get();
        self.ivars().drag_out_copies.set(on);
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults().setBool_forKey(on, ns_string!("dragOutCopies"));
        }
        self.sync_prefs_controls();
    }

    pub(crate) fn set_models_folder(&self, path: Option<&Path>) {
        if !e2e::enabled() && !snapshot::enabled() {
            let defaults = NSUserDefaults::standardUserDefaults();
            match path {
                Some(folder) => {
                    let value = NSString::from_str(&folder.to_string_lossy());
                    unsafe { defaults.setObject_forKey(Some(&value), ns_string!("modelsFolder")) };
                }
                None => unsafe { defaults.setObject_forKey(None, ns_string!("modelsFolder")) },
            }
        }
        self.sync_models_summary();
    }

    pub(crate) fn set_upscale_scale(&self, scale: crate::upscale::UpscaleScale) {
        self.ivars().upscale_scale.set(scale);
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults()
                .setInteger_forKey(scale.tag(), ns_string!("upscaleScale"));
        }
        self.sync_prefs_controls();
    }

    pub(crate) fn set_upscale_save_mode(&self, mode: crate::upscale::UpscaleSaveMode) {
        self.ivars().upscale_save_mode.set(mode);
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults()
                .setInteger_forKey(mode.tag(), ns_string!("upscaleSaveMode"));
        }
        self.sync_prefs_controls();
    }

    pub(crate) fn set_compress_options(&self, options: crate::compress::CompressOptions) {
        self.ivars().compress_options.set(options);
        if !e2e::enabled() {
            let d = NSUserDefaults::standardUserDefaults();
            d.setInteger_forKey(options.target_kb, ns_string!("compressTargetKb"));
            d.setInteger_forKey(options.max_dims.tag(), ns_string!("compressMaxDims"));
            d.setInteger_forKey(options.fallback.tag(), ns_string!("compressFallback"));
        }
        self.sync_prefs_controls();
    }
}

/// Auto-advance interval encoded in tenths of a second (`0` = off).
pub(crate) fn tenths(seconds: Option<f64>) -> isize {
    seconds.map_or(0, |s| (s * 10.0).round() as isize)
}

pub(crate) fn seconds(tenths: isize) -> Option<f64> {
    (tenths > 0).then(|| tenths as f64 / 10.0)
}

pub(crate) fn last_folder_pref() -> Option<PathBuf> {
    if e2e::enabled() {
        return None;
    }
    NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("lastFolder"))
        .map(|s| PathBuf::from(s.to_string()))
        .filter(|p| !p.as_os_str().is_empty())
}

pub(crate) fn save_last_folder(root: &std::path::Path) {
    if e2e::enabled() || snapshot::enabled() {
        return;
    }
    let value = NSString::from_str(&root.to_string_lossy());
    let defaults = NSUserDefaults::standardUserDefaults();
    let key = ns_string!("lastFolder");
    if defaults.stringForKey(key).as_deref() != Some(&*value) {
        unsafe { defaults.setObject_forKey(Some(&value), key) };
    }
}

pub(crate) const THUMB_SLIDER_W: f64 = 110.0;

pub(crate) fn thumb_cap_pref() -> f64 {
    if e2e::enabled() {
        return grid::CELL_CAPS[1];
    }
    let stored = NSUserDefaults::standardUserDefaults().doubleForKey(ns_string!("thumbCap"));
    if grid::CELL_CAPS.contains(&stored) {
        stored
    } else {
        grid::CELL_CAPS[1]
    }
}

pub(crate) fn thumb_size_pref() -> f64 {
    if e2e::enabled() {
        return grid::CELL;
    }
    let stored = NSUserDefaults::standardUserDefaults().doubleForKey(ns_string!("thumbSize"));
    if stored >= 60.0 {
        stored.min(thumb_cap_pref())
    } else {
        grid::CELL
    }
}

pub(crate) fn startup_folder_pref() -> Option<String> {
    if e2e::enabled() {
        return None;
    }
    NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("startupFolder"))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn models_folder_pref() -> Option<String> {
    if e2e::enabled() {
        return None;
    }
    NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("modelsFolder"))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn upscale_scale_pref() -> crate::upscale::UpscaleScale {
    if e2e::enabled() {
        return crate::upscale::UpscaleScale::X4;
    }
    let stored = NSUserDefaults::standardUserDefaults().integerForKey(ns_string!("upscaleScale"));
    crate::upscale::UpscaleScale::from_tag(stored)
}

fn upscale_save_mode_pref() -> crate::upscale::UpscaleSaveMode {
    if e2e::enabled() {
        return crate::upscale::UpscaleSaveMode::Ask;
    }
    let stored =
        NSUserDefaults::standardUserDefaults().integerForKey(ns_string!("upscaleSaveMode"));
    crate::upscale::UpscaleSaveMode::from_tag(stored)
}

fn compress_options_pref() -> crate::compress::CompressOptions {
    if e2e::enabled() {
        return crate::compress::CompressOptions::default();
    }
    let d = NSUserDefaults::standardUserDefaults();
    let target_kb = if d.objectForKey(ns_string!("compressTargetKb")).is_some() {
        crate::compress::normalize_target_kb(d.integerForKey(ns_string!("compressTargetKb")))
    } else {
        crate::compress::DEFAULT_TARGET_KB
    };
    let max_dims = if d.objectForKey(ns_string!("compressMaxDims")).is_some() {
        crate::compress::MaxDimensions::from_tag(d.integerForKey(ns_string!("compressMaxDims")))
    } else {
        crate::compress::MaxDimensions::default()
    };
    let fallback = crate::compress::FallbackFormat::from_tag(
        d.integerForKey(ns_string!("compressFallback")),
    );
    crate::compress::CompressOptions {
        target_kb,
        max_dims,
        fallback,
    }
}

const PREFS_WIDTH: f64 = 480.0;
const PREFS_TAB_BAR_H: f64 = 44.0;
const SHORTCUTS_BODY_H: f64 = 520.0;

define_class!(
    #[unsafe(super(objc2_app_kit::NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliFlippedView"]
    pub(crate) struct FlippedView;

    unsafe impl NSObjectProtocol for FlippedView {}

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl FlippedView {
    pub(crate) fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub(crate) fn build_prefs_window(
    mtm: MainThreadMarker,
    delegate: &AppDelegate,
) -> Retained<NSWindow> {
    use objc2_app_kit::{
        NSAutoresizingMaskOptions as Resize, NSBox, NSBoxType, NSButton, NSLineBreakMode,
        NSPopUpButton, NSSegmentStyle, NSSegmentSwitchTracking, NSSegmentedControl, NSView,
    };

    const WIDTH: f64 = PREFS_WIDTH;
    const HEIGHT: f64 = 880.0;
    const MARGIN: f64 = 24.0;
    const INNER_WIDTH: f64 = WIDTH - MARGIN * 2.0;
    const POPUP_WIDTH: f64 = 196.0;
    let frame = NSRect::new(CGPoint::new(360.0, 360.0), CGSize::new(WIDTH, HEIGHT));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(ns_string!("Settings"));
    window.setRestorable(false);
    unsafe { window.setReleasedWhenClosed(false) };

    let content = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), frame.size),
    );
    let general = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, HEIGHT)),
    );
    let rect = |x, y, width, height| NSRect::new(CGPoint::new(x, y), CGSize::new(width, height));
    // Laid out top down inside `general`: `y` is the top of the next row.
    let y = std::cell::Cell::new(HEIGHT - 16.0);
    let row = |height: f64, gap: f64| {
        let top = y.get();
        y.set(top - height - gap);
        top - height
    };

    let heading = |text: &str| {
        let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        field.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, 0.4)));
        field.setFrame(rect(MARGIN, row(18.0, 8.0), INNER_WIDTH, 18.0));
        general.addSubview(&field);
    };
    let note = |text: &str| {
        let field = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
        field.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        field.setTextColor(Some(&NSColor::secondaryLabelColor()));
        field.setFrame(rect(MARGIN + 20.0, row(28.0, 6.0), INNER_WIDTH - 20.0, 28.0));
        general.addSubview(&field);
    };
    let checkbox = |title: &str, action: objc2::runtime::Sel, tag: isize| {
        let b = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(title),
                Some(delegate),
                Some(action),
                mtm,
            )
        };
        b.sizeToFit();
        b.setFrame(rect(MARGIN, row(20.0, 5.0), b.frame().size.width, 20.0));
        b.setTag(tag);
        general.addSubview(&b);
    };
    let separator = || {
        let line = NSBox::initWithFrame(
            NSBox::alloc(mtm),
            rect(MARGIN, row(1.0, 12.0), INNER_WIDTH, 1.0),
        );
        line.setBoxType(NSBoxType::Separator);
        general.addSubview(&line);
        y.set(y.get() + 2.0);
    };
    let popup = |title: &str, action, tag, entries: &[(&str, isize)]| {
        let top = row(26.0, 6.0);
        let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
        label.setFrame(rect(MARGIN, top + 4.0, INNER_WIDTH - POPUP_WIDTH, 18.0));
        general.addSubview(&label);
        let control = NSPopUpButton::new(mtm);
        control.setFrame(rect(WIDTH - MARGIN - POPUP_WIDTH, top, POPUP_WIDTH, 26.0));
        control.setTag(tag);
        for &(title, value) in entries {
            control.addItemWithTitle(&NSString::from_str(title));
            if let Some(item) = control.lastItem() {
                item.setTag(value);
            }
        }
        unsafe {
            control.setTarget(Some(delegate));
            control.setAction(Some(action));
        }
        general.addSubview(&control);
    };

    let folder_row = |gap: f64,
                      field_tag: isize,
                      choose_action: objc2::runtime::Sel,
                      clear_action: objc2::runtime::Sel,
                      clear_tag: isize| {
        let top = row(28.0, gap);
        let field = NSTextField::labelWithString(ns_string!(""), mtm);
        field.setFrame(rect(MARGIN, top + 4.0, INNER_WIDTH - 162.0, 20.0));
        field.setTag(field_tag);
        field.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        field.setTextColor(Some(&NSColor::secondaryLabelColor()));
        general.addSubview(&field);
        for (title, action, tag, x, width) in [
            ("Choose…", choose_action, 0, WIDTH - MARGIN - 152.0, 84.0),
            ("Clear", clear_action, clear_tag, WIDTH - MARGIN - 64.0, 64.0),
        ] {
            let b = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(title),
                    Some(delegate),
                    Some(action),
                    mtm,
                )
            };
            b.setFrame(rect(x, top, width, 28.0));
            b.setTag(tag);
            general.addSubview(&b);
        }
    };

    heading("Startup Folder");
    folder_row(6.0, 7, sel!(prefsChooseStartup:), sel!(prefsClearStartup:), 11);

    separator();
    heading("Slideshow");
    checkbox("Loop", sel!(toggleLoop:), 2);
    checkbox("Shuffle", sel!(toggleShuffle:), 3);
    checkbox("Scale up small images", sel!(toggleScaleUp:), 10);
    checkbox("Show controls", sel!(toggleControls:), 13);
    popup(
        "Auto-advance",
        sel!(prefsAutoAdvance:),
        6,
        &[
            ("Off", 0),
            ("1 second", 10),
            ("3 seconds", 30),
            ("5 seconds", 50),
            ("10 seconds", 100),
        ],
    );

    separator();
    heading("Editing");
    checkbox("Save rotations to file", sel!(prefsSaveRotations:), 12);
    note("Saved on slide change. Lossless for JPEG, HEIC, PNG, and TIFF.");

    separator();
    heading("Upscaling");
    folder_row(4.0, 14, sel!(prefsChooseModels:), sel!(prefsClearModels:), 16);
    let models_note = NSTextField::wrappingLabelWithString(ns_string!(""), mtm);
    models_note.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    models_note.setTextColor(Some(&NSColor::secondaryLabelColor()));
    models_note.setTag(17);
    models_note.setFrame(rect(MARGIN, row(28.0, 6.0), INNER_WIDTH, 28.0));
    general.addSubview(&models_note);
    popup(
        "Default scale",
        sel!(prefsUpscaleScale:),
        18,
        &[("4×", 4), ("2×", 2)],
    );
    popup(
        "When saving",
        sel!(prefsUpscaleSaveMode:),
        19,
        &[
            ("Ask each time", 0),
            ("Save a copy", 1),
            ("Replace original", 2),
        ],
    );

    separator();
    heading("Compression");
    popup(
        "Target max size",
        sel!(prefsCompressTarget:),
        20,
        &crate::compress::TARGET_KB_PRESETS,
    );
    popup(
        "Max dimensions",
        sel!(prefsCompressMaxDims:),
        21,
        &crate::compress::MAX_DIMS_PRESETS,
    );
    popup(
        "Format fallback",
        sel!(prefsCompressFallback:),
        22,
        &crate::compress::FALLBACK_PRESETS,
    );

    separator();
    heading("Grid & Files");
    let captions = grid::CELL_CAPS.map(|cap| format!("{cap:.0} pt"));
    let caps = std::array::from_fn::<_, { grid::CELL_CAPS.len() }, _>(|i| {
        (captions[i].as_str(), grid::CELL_CAPS[i] as isize)
    });
    popup("Max thumbnail size", sel!(prefsThumbCap:), 8, &caps);
    checkbox("Show filenames", sel!(toggleLabels:), 4);
    checkbox(
        "Copy when dragging out of Blickli",
        sel!(prefsDragCopies:),
        9,
    );

    // Trim `general` height to fit the laid-out rows and wrap in a scroll view so
    // the Settings window never overflows compact displays.
    let general_h = HEIGHT - y.get() + 14.0;
    let shift = HEIGHT - general_h;
    for view in general.subviews().iter() {
        let mut frame = view.frame();
        frame.origin.y -= shift;
        view.setFrame(frame);
    }
    general.setFrame(rect(0.0, 0.0, WIDTH, general_h));

    let max_body_h = NSScreen::mainScreen(mtm)
        .map(|s| (s.visibleFrame().size.height - PREFS_TAB_BAR_H - 96.0).max(SHORTCUTS_BODY_H))
        .unwrap_or(SHORTCUTS_BODY_H);
    let visible_general_h = general_h.min(max_body_h);
    let general_scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(0.0, 0.0, WIDTH, visible_general_h),
    );
    general_scroll.setHasVerticalScroller(true);
    general_scroll.setAutohidesScrollers(true);
    general_scroll.setDrawsBackground(false);
    general_scroll.setDocumentView(Some(&general));
    // Scroll non-flipped document to the top so the first section is visible immediately.
    general.scrollPoint(CGPoint::new(0.0, (general_h - visible_general_h).max(0.0)));
    let general_container: Retained<NSView> = Retained::into_super(general_scroll);
    delegate.ivars().prefs_general_h.set(visible_general_h);

    // Build the Shortcuts tab pane.
    let shortcuts_view = build_shortcuts_pane(mtm, delegate, WIDTH, SHORTCUTS_BODY_H, MARGIN);
    shortcuts_view.setHidden(true);

    let total_h = visible_general_h + PREFS_TAB_BAR_H;
    content.setFrameSize(CGSize::new(WIDTH, total_h));
    content.addSubview(&general_container);
    content.addSubview(&shortcuts_view);

    // Top tab switcher pinned to the top edge across tab height changes.
    let tab_labels = NSArray::from_slice(&[ns_string!("General"), ns_string!("Shortcuts")]);
    let tabs = unsafe {
        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
            &tab_labels,
            NSSegmentSwitchTracking::SelectOne,
            Some(delegate),
            Some(sel!(prefsTabChanged:)),
            mtm,
        )
    };
    tabs.setSegmentStyle(NSSegmentStyle::Automatic);
    tabs.setSelectedSegment(0);
    tabs.setWidth_forSegment(110.0, 0);
    tabs.setWidth_forSegment(110.0, 1);
    tabs.sizeToFit();
    let tab_size = tabs.frame().size;
    tabs.setFrame(rect(
        (WIDTH - tab_size.width) / 2.0,
        total_h - PREFS_TAB_BAR_H + (PREFS_TAB_BAR_H - tab_size.height) / 2.0,
        tab_size.width,
        tab_size.height,
    ));
    tabs.setAutoresizingMask(Resize::ViewMinYMargin);
    content.addSubview(&tabs);

    let tab_line = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        rect(0.0, total_h - PREFS_TAB_BAR_H, WIDTH, 1.0),
    );
    tab_line.setBoxType(NSBoxType::Separator);
    tab_line.setAutoresizingMask(Resize::ViewMinYMargin | Resize::ViewWidthSizable);
    content.addSubview(&tab_line);

    let _ = delegate.ivars().prefs_tabs.set(tabs);
    let _ = delegate.ivars().prefs_general_view.set(general_container);
    let _ = delegate.ivars().prefs_shortcuts_view.set(shortcuts_view);

    window.setContentSize(CGSize::new(WIDTH, total_h));
    window.setContentView(Some(&content));
    window.center();
    // Cancel active shortcut recording if Settings closes or loses key focus.
    unsafe {
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        for name in [
            objc2_app_kit::NSWindowWillCloseNotification,
            objc2_app_kit::NSWindowDidResignKeyNotification,
        ] {
            center.addObserver_selector_name_object(
                delegate,
                sel!(prefsWindowInactive:),
                Some(name),
                Some(&window),
            );
        }
    }
    window
}

fn build_shortcuts_pane(
    mtm: MainThreadMarker,
    delegate: &AppDelegate,
    width: f64,
    height: f64,
    margin: f64,
) -> Retained<objc2_app_kit::NSView> {
    use objc2_app_kit::{NSBox, NSBoxType, NSButton, NSLineBreakMode, NSView};

    let inner_w = width - margin * 2.0;
    let rect = |x, y, w, h| NSRect::new(CGPoint::new(x, y), CGSize::new(w, h));
    let pane = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, width, height));

    let hint = NSTextField::labelWithString(
        ns_string!("Click a shortcut to record, ⌫ to clear, Esc to cancel."),
        mtm,
    );
    hint.setFont(Some(&NSFont::systemFontOfSize(11.5)));
    hint.setTextColor(Some(&NSColor::secondaryLabelColor()));
    hint.setFrame(rect(margin, height - 28.0, inner_w, 16.0));
    pane.addSubview(&hint);

    let scroll_y = 48.0;
    let scroll_h = height - scroll_y - 36.0;
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(margin, scroll_y, inner_w, scroll_h),
    );
    scroll.setHasVerticalScroller(true);
    scroll.setBorderType(objc2_app_kit::NSBorderType::BezelBorder);
    scroll.setDrawsBackground(true);

    let doc_w = scroll.contentSize().width;
    let mut y = 8.0;
    let mut rows: Vec<(bool, &'static str, Option<crate::shortcuts::Action>, f64)> = Vec::new();
    for section in crate::shortcuts::Section::ALL {
        if !rows.is_empty() {
            y += 10.0;
        }
        rows.push((true, section.title(), None, y));
        y += 24.0;
        for action in crate::shortcuts::Action::ALL {
            if action.section() == section {
                rows.push((false, action.label(), Some(action), y));
                y += 28.0;
            }
        }
    }
    let doc_h = (y + 8.0).max(scroll_h);
    let doc = FlippedView::new(mtm, rect(0.0, 0.0, doc_w, doc_h));

    let store = delegate.ivars().shortcuts.borrow();
    let mut recorders = delegate.ivars().shortcut_recorders.borrow_mut();
    let delegate_ref = objc2::Message::retain(delegate);
    const PILL_W: f64 = 128.0;
    const PILL_H: f64 = 22.0;

    for (is_header, title, action, row_y) in rows {
        if is_header {
            if row_y > 10.0 {
                let sep = NSBox::initWithFrame(
                    NSBox::alloc(mtm),
                    rect(12.0, row_y - 6.0, doc_w - 24.0, 1.0),
                );
                sep.setBoxType(NSBoxType::Separator);
                doc.addSubview(&sep);
            }
            let head = NSTextField::labelWithString(&NSString::from_str(title), mtm);
            head.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.4)));
            head.setTextColor(Some(&NSColor::secondaryLabelColor()));
            head.setFrame(rect(12.0, row_y + 2.0, doc_w - 24.0, 18.0));
            doc.addSubview(&head);
        } else if let Some(action) = action {
            let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
            label.setFont(Some(&NSFont::systemFontOfSize(12.5)));
            label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            label.setFrame(rect(12.0, row_y + 5.0, doc_w - PILL_W - 28.0, 18.0));
            doc.addSubview(&label);

            let recorder = crate::shortcuts::ShortcutRecorderView::new(
                mtm,
                rect(doc_w - PILL_W - 10.0, row_y + 3.0, PILL_W, PILL_H),
                action,
                store.get(action).cloned(),
                delegate_ref.clone(),
            );
            doc.addSubview(&recorder);
            recorders.insert(action, recorder);
        }
    }
    drop(recorders);
    drop(store);

    scroll.setDocumentView(Some(&doc));
    pane.addSubview(&scroll);

    let reset_w = 126.0;
    let feedback = NSTextField::labelWithString(ns_string!("Click a shortcut to change it."), mtm);
    feedback.setFont(Some(&NSFont::systemFontOfSize(11.5)));
    feedback.setTextColor(Some(&NSColor::secondaryLabelColor()));
    feedback.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    feedback.setFrame(rect(margin, 15.0, inner_w - reset_w - 10.0, 16.0));
    pane.addSubview(&feedback);
    let _ = delegate.ivars().shortcut_feedback.set(feedback);

    let reset = unsafe {
        NSButton::buttonWithTitle_target_action(
            ns_string!("Reset to Defaults"),
            Some(delegate),
            Some(sel!(prefsResetShortcuts:)),
            mtm,
        )
    };
    reset.setFrame(rect(width - margin - reset_w, 10.0, reset_w, 28.0));
    pane.addSubview(&reset);

    pane
}
