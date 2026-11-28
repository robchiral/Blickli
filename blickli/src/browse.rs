//! Browse window: folder selection, filter, sort, thumbnail size, and status bar.

use super::*;

impl AppDelegate {
    pub(crate) fn open_folder(&self) {
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setPrompt(Some(ns_string!("Open")));
        let ok = self.run_panel(&panel);
        if !ok {
            return;
        }
        let Some(path) = panel.URL().and_then(grants::remember) else {
            return;
        };
        self.scan_root(path, true);
    }

    pub fn scan_root(&self, root: PathBuf, recursive: bool) {
        self.scan_root_inner(root, recursive, true);
    }

    /// Current scanned folder and recursion flag, used to skip redundant scans.
    pub fn current_scan(&self) -> Option<(PathBuf, bool)> {
        self.ivars().current_scan.borrow().clone()
    }

    pub(crate) fn scan_root_inner(&self, root: PathBuf, recursive: bool, remember: bool) {
        if self.pending_folder().as_deref() != Some(root.as_path()) {
            self.ivars().pending_open.borrow_mut().clear();
        }
        self.present_folder(&root);
        if remember {
            save_last_folder(&root);
        }
        *self.ivars().current_scan.borrow_mut() = Some((root.clone(), recursive));
        self.ivars()
            .engine
            .get()
            .unwrap()
            .scan(root, recursive, sidebar::is_package);
    }

    /// Show explicit files without scanning their parent folder (sandbox single-file grant).
    fn show_files(&self, folder: PathBuf, files: Vec<PathBuf>) {
        self.present_folder(&folder);
        *self.ivars().current_scan.borrow_mut() = Some((folder, false));
        self.ivars().engine.get().unwrap().show(files);
    }

    fn present_folder(&self, root: &Path) {
        self.close_filter();
        if let Some(grid) = self.ivars().grid.get() {
            grid.reset();
        }
        if let Some(window) = self.ivars().window.get() {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.to_string_lossy().into_owned());
            window.setTitle(&NSString::from_str(&name));
        }
        if let Some(sidebar) = self.ivars().sidebar.get() {
            sidebar.reveal(root);
        }
    }

    pub(crate) fn toggle_sidebar(&self) {
        let Some(split) = self.ivars().split.get() else {
            return;
        };
        unsafe { split.toggleSidebar(None) };
    }

    pub fn request_thumbnails(&self, requests: Vec<blickli_core::ThumbnailRequest>) {
        if let Some(engine) = self.ivars().engine.get() {
            engine.request_thumbnails(requests);
        }
    }

    pub(crate) fn apply_sort(&self) {
        let order = self.ivars().sort_order.get();
        let descending = self.ivars().sort_desc.get();
        if let Some(grid) = self.ivars().grid.get() {
            grid.resort(order, descending);
            if order.needs_file_dates() {
                self.ivars().engine.get().unwrap().request_file_dates(
                    grid.paths_missing_dates(order),
                    order.needs_exif(),
                );
            }
        }
    }

    /// Open a folder or image files received from Finder (or CLI args).
    pub fn open_from_finder(&self, paths: Vec<PathBuf>) {
        self.end_slideshow();
        self.close_filter();
        if let Some(window) = self.ivars().window.get() {
            window.makeKeyAndOrderFront(None);
        }
        let Some(first) = paths.first().cloned() else {
            return;
        };
        if first.is_dir() {
            self.ivars().opened_from_finder.set(true);
            self.ivars().pending_open.borrow_mut().clear();
            self.scan_root(first, true);
            return;
        }
        let files: Vec<PathBuf> = paths.into_iter().filter(|path| path.is_file()).collect();
        // Leave opened_from_finder unset if all paths vanished so startup fallback still runs.
        let Some(folder) = files
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
        else {
            return;
        };
        self.ivars().opened_from_finder.set(true);
        *self.ivars().pending_open.borrow_mut() = files.clone();
        if self.select_pending() {
            return;
        }
        if !grants::readable(&folder) {
            self.show_files(folder, files);
            return;
        }
        self.scan_root(folder, true);
    }

    /// Select pending Finder-opened files once the grid contains all of them.
    pub(crate) fn select_pending(&self) -> bool {
        let pending = self.ivars().pending_open.borrow().clone();
        if pending.is_empty() {
            return true;
        }
        let Some(grid) = self.ivars().grid.get() else {
            return false;
        };
        if !grid.holds_all(&pending) {
            return false;
        }
        grid.select_paths(&pending);
        self.ivars().pending_open.borrow_mut().clear();
        self.focus_grid();
        self.start_from_grid();
        true
    }

    pub(crate) fn pending_folder(&self) -> Option<PathBuf> {
        self.ivars()
            .pending_open
            .borrow()
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
    }

    pub(crate) fn filter_changed(&self) {
        let Some(field) = self.ivars().filter_field.get() else {
            return;
        };
        let text = field.stringValue().to_string();
        self.ivars().filter_active.set(!text.is_empty());
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_filter(&text);
        }
    }

    pub fn escape_pressed(&self) {
        if self.ivars().filter_active.get() {
            self.close_filter();
            return;
        }
        if self.ivars().upscaling.get() && !self.ivars().upscaling_slide.get() {
            self.cancel_upscale();
            return;
        }
        if self.cancel_compress() {
            return;
        }
        self.close_filter();
    }

    /// Escape clears the filter and focuses the grid; Return keeps the filter and focuses the grid.
    pub(crate) fn filter_field_command(&self, command: objc2::runtime::Sel) -> bool {
        if command == sel!(cancelOperation:) {
            self.close_filter();
            self.focus_grid();
            return true;
        }
        if command == sel!(insertNewline:) {
            self.focus_grid();
            return true;
        }
        false
    }

    pub(crate) fn open_filter(&self) {
        let (Some(field), Some(window)) =
            (self.ivars().filter_field.get(), self.ivars().window.get())
        else {
            return;
        };
        if let Some(toolbar) = self.ivars().toolbar.get() {
            toolbar.begin_search();
        }
        self.ivars().filter_active.set(true);
        window.makeFirstResponder(Some(field));
    }

    fn close_filter(&self) {
        let Some(field) = self.ivars().filter_field.get() else {
            return;
        };
        let had_text = !field.stringValue().is_empty();
        if !self.ivars().filter_active.get() && !had_text {
            return;
        }
        self.ivars().filter_active.set(false);
        field.setStringValue(ns_string!(""));
        if let Some(toolbar) = self.ivars().toolbar.get() {
            toolbar.end_search();
        }
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_filter("");
        }
        self.focus_grid();
    }

    pub(crate) fn focus_grid(&self) {
        let (Some(window), Some(grid)) = (self.ivars().window.get(), self.ivars().grid.get())
        else {
            return;
        };
        window.makeFirstResponder(Some(grid));
    }

    /// Lay out the right pane: scroll view above, status bar below.
    pub(crate) fn layout_content(&self) {
        let (Some(content), Some(scroll)) = (self.ivars().content.get(), self.ivars().scroll.get())
        else {
            return;
        };
        let size = content.bounds().size;
        scroll.setFrame(NSRect::new(
            CGPoint::new(0.0, STATUS_H),
            CGSize::new(size.width, size.height - STATUS_H),
        ));
        // Sync grid width to the clip view so columns reflow; setFrameSize updates height.
        if let Some(grid) = self.ivars().grid.get() {
            let width = scroll.contentSize().width;
            if (grid.frame().size.width - width).abs() > 0.5 {
                grid.setFrameSize(CGSize::new(width, 0.0));
                grid.setNeedsDisplay(true);
            }
        }
    }

    pub fn thumb_size_changed(&self, size: f64) {
        if let Some(slider) = self.ivars().thumb_slider.get() {
            slider.setDoubleValue(size);
        }
        if !e2e::enabled() && !snapshot::enabled() {
            NSUserDefaults::standardUserDefaults().setDouble_forKey(size, ns_string!("thumbSize"));
        }
    }

    pub(crate) fn thumb_slider_moved(&self) {
        let (Some(slider), Some(grid)) = (self.ivars().thumb_slider.get(), self.ivars().grid.get())
        else {
            return;
        };
        grid.set_cell_size(slider.doubleValue());
    }

    pub(crate) fn apply_thumb_cap(&self, cap: f64) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_cell_cap(cap);
            if let Some(slider) = self.ivars().thumb_slider.get() {
                slider.setMaxValue(cap);
                slider.setDoubleValue(grid.cell_size());
            }
        }
        if !e2e::enabled() && !snapshot::enabled() {
            NSUserDefaults::standardUserDefaults().setDouble_forKey(cap, ns_string!("thumbCap"));
        }
    }

    /// Display a temporary status message until the next selection or scan update.
    pub(crate) fn show_status_message(&self, text: &str) {
        if let Some(status) = self.ivars().status.get() {
            status.setStringValue(&NSString::from_str(text));
        }
    }

    pub fn selection_changed(&self) {
        self.update_status();
        self.refresh_info();
    }

    pub(crate) fn update_status(&self) {
        let (Some(status), Some(grid)) = (self.ivars().status.get(), self.ivars().grid.get())
        else {
            return;
        };
        let scanning = grid.scanning();
        let busy = scanning || self.ivars().upscaling.get() || self.ivars().compressing.get();
        if let Some(spinner) = self.ivars().spinner.get() {
            // SAFETY: no sender is passed.
            unsafe {
                if busy {
                    spinner.startAnimation(None);
                } else {
                    spinner.stopAnimation(None);
                }
            }
        }
        let (shown, in_folder) = grid.counts();
        if let Some(window) = self.ivars().window.get() {
            let subtitle = match (scanning, in_folder) {
                (true, 0) => "Loading…".to_owned(),
                (_, 1) => "1 image".to_owned(),
                (_, n) => format!("{} images", count_text(n)),
            };
            window.setSubtitle(&NSString::from_str(&subtitle));
        }
        let mut all_text = if grid.filtering() {
            format!("{} of {} images", count_text(shown), count_text(in_folder))
        } else if in_folder == 1 {
            "1 image".to_owned()
        } else {
            format!("{} images", count_text(in_folder))
        };
        if scanning {
            all_text = if in_folder == 0 {
                "Loading…".to_owned()
            } else {
                format!("Loading… {all_text}")
            };
        }
        let selection = grid.selection();
        let text = match (selection.count, selection.first) {
            (1, Some(info)) => {
                let dims = self
                    .dimensions(&info)
                    .map(|(w, h)| format!(" — {w}×{h}"))
                    .unwrap_or_default();
                format!(
                    "{}{dims} — {} · {all_text}",
                    file_name(&info.path),
                    format_bytes(info.size)
                )
            }
            (0, _) | (_, None) => all_text,
            (many, _) => {
                let mut text = format!(
                    "{} of {} selected — {}",
                    count_text(many),
                    count_text(shown),
                    format_bytes(selection.bytes)
                );
                if grid.filtering() || scanning {
                    text.push_str(&format!(" · {all_text}"));
                }
                text
            }
        };
        status.setStringValue(&NSString::from_str(&text));
    }

    fn dimensions(&self, info: &std::rc::Rc<blickli_core::FileInfo>) -> Dimensions {
        let mut cached = self.ivars().status_dims.borrow_mut();
        if let Some((file, dims)) = cached.as_ref() {
            if std::rc::Rc::ptr_eq(file, info) {
                return *dims;
            }
        }
        let dims = decoder::image_dimensions(&info.path);
        *cached = Some((std::rc::Rc::clone(info), dims));
        dims
    }
}

pub(crate) fn build_browse_window(
    mtm: MainThreadMarker,
    delegate: &Retained<AppDelegate>,
) -> Retained<NSWindow> {
    use objc2_app_kit::{
        NSAutoresizingMaskOptions as Resize, NSControlSize, NSProgressIndicator,
        NSProgressIndicatorStyle, NSSlider, NSView, NSWindowTitleVisibility,
        NSWindowToolbarStyle,
    };

    let frame = NSRect::new(CGPoint::new(200.0, 200.0), CGSize::new(1000.0, 700.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable
                | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenNone);
    // Skip AppKit window restoration so snapshots and titles don't persist on disk.
    window.setRestorable(false);
    window.setTitle(ns_string!("Blickli"));
    window.setMinSize(CGSize::new(640.0, 400.0));
    let (toolbar_delegate, toolbar) =
        crate::toolbar::BrowseToolbar::new(mtm, delegate, menu::sort_menu(mtm));
    window.setToolbar(Some(&toolbar));
    window.setToolbarStyle(NSWindowToolbarStyle::Unified);
    window.setTitleVisibility(NSWindowTitleVisibility::Visible);

    let (sidebar, sidebar_scroll) = Sidebar::new(mtm, delegate);

    let size = CGSize::new(frame.size.width - sidebar::WIDTH, frame.size.height);
    let rect = |x, y, width, height| NSRect::new(CGPoint::new(x, y), CGSize::new(width, height));
    let content =
        NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, size.width, size.height));

    let scroll = NSScrollView::new(mtm);
    scroll.setHasVerticalScroller(true);
    scroll.setFrame(rect(0.0, STATUS_H, size.width, size.height - STATUS_H));
    scroll.setAutoresizingMask(Resize::ViewWidthSizable | Resize::ViewHeightSizable);
    let grid = GridView::new(mtm, frame);
    let _ = grid.ivars().delegate.set(delegate.clone());
    scroll.setDocumentView(Some(&grid));

    // Status bar: status text, scan spinner, and thumbnail size slider.
    let slider_x = size.width - THUMB_SLIDER_W - 12.0;
    let slider = NSSlider::initWithFrame(
        NSSlider::alloc(mtm),
        rect(slider_x, 5.0, THUMB_SLIDER_W, 20.0),
    );
    slider.setMinValue(60.0);
    slider.setMaxValue(thumb_cap_pref());
    slider.setDoubleValue(thumb_size_pref());
    slider.setControlSize(NSControlSize::Small);
    slider.setToolTip(Some(ns_string!("Thumbnail size (⌘+, ⌘-)")));
    slider.setAutoresizingMask(Resize::ViewMinXMargin | Resize::ViewMaxYMargin);
    unsafe {
        slider.setTarget(Some(&**delegate));
        slider.setAction(Some(sel!(thumbSliderMoved:)));
    }

    let spinner_x = slider_x - SPINNER_W - 10.0;
    let spinner = NSProgressIndicator::initWithFrame(
        NSProgressIndicator::alloc(mtm),
        rect(spinner_x, 7.0, SPINNER_W, SPINNER_W),
    );
    spinner.setStyle(NSProgressIndicatorStyle::Spinning);
    spinner.setControlSize(NSControlSize::Small);
    spinner.setDisplayedWhenStopped(false);
    spinner.setAutoresizingMask(Resize::ViewMinXMargin | Resize::ViewMaxYMargin);

    let status = NSTextField::labelWithString(ns_string!(""), mtm);
    status.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    status.setTextColor(Some(&NSColor::secondaryLabelColor()));
    status.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
    status.setFrame(rect(12.0, 7.0, spinner_x - 20.0, 16.0));
    status.setAutoresizingMask(Resize::ViewWidthSizable | Resize::ViewMaxYMargin);

    let hairline = objc2_app_kit::NSBox::initWithFrame(
        objc2_app_kit::NSBox::alloc(mtm),
        rect(0.0, STATUS_H - 1.0, size.width, 1.0),
    );
    hairline.setBoxType(objc2_app_kit::NSBoxType::Separator);
    hairline.setAutoresizingMask(Resize::ViewWidthSizable | Resize::ViewMaxYMargin);

    let filter_field = toolbar_delegate
        .search_field()
        .map(Retained::into_super)
        .expect("the toolbar makes its filter field up front");

    let views: [&NSView; 5] = [&scroll, &hairline, &status, &spinner, &slider];
    for view in views {
        content.addSubview(view);
    }

    let sidebar_vc = NSViewController::new(mtm);
    sidebar_vc.setView(&sidebar_scroll);
    let content_vc = NSViewController::new(mtm);
    content_vc.setView(&content);

    let split = NSSplitViewController::new(mtm);
    let sidebar_pane = NSSplitViewItem::sidebarWithViewController(&sidebar_vc);
    sidebar_pane.setMinimumThickness(sidebar::MIN_WIDTH);
    sidebar_pane.setMaximumThickness(sidebar::MAX_WIDTH);
    // Skip autosave in e2e runs so restored widths do not skew layout checks.
    if !e2e::enabled() && !snapshot::enabled() {
        split
            .splitView()
            .setAutosaveName(Some(ns_string!("sidebarWidth2")));
    }
    split.addSplitViewItem(&sidebar_pane);
    split.addSplitViewItem(&NSSplitViewItem::splitViewItemWithViewController(
        &content_vc,
    ));
    window.setContentViewController(Some(&split));
    window.setFrame_display(frame, false);
    window.makeFirstResponder(Some(&grid));

    // Re-run manual layout when the content pane resizes, and repaint grid
    // selection highlights when the browse window gains or loses key status.
    content.setPostsFrameChangedNotifications(true);
    unsafe {
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        center.addObserver_selector_name_object(
            delegate,
            sel!(contentResized:),
            Some(objc2_app_kit::NSViewFrameDidChangeNotification),
            Some(&content),
        );
        for name in [
            objc2_app_kit::NSWindowDidBecomeKeyNotification,
            objc2_app_kit::NSWindowDidResignKeyNotification,
        ] {
            center.addObserver_selector_name_object(
                delegate,
                sel!(browseWindowKeyChanged:),
                Some(name),
                Some(&window),
            );
        }
    }

    let ivars = delegate.ivars();
    let _ = ivars.grid.set(grid);
    let _ = ivars.thumb_slider.set(slider);
    let _ = ivars.spinner.set(spinner);
    let _ = ivars.status.set(status);
    let _ = ivars.content.set(content);
    let _ = ivars.scroll.set(scroll);
    let _ = ivars.filter_field.set(filter_field);
    let _ = ivars.toolbar.set(toolbar_delegate);
    let _ = ivars.sidebar.set(sidebar);
    let _ = ivars.split.set(split);
    let _ = ivars.window.set(window.clone());
    delegate.layout_content();
    window
}
