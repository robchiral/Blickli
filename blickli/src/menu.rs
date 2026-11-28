//! Application menu bar construction and item validation.

use super::*;
use crate::shortcuts::Action;

pub(crate) fn menu_item(
    mtm: MainThreadMarker,
    title: &NSString,
    action: objc2::runtime::Sel,
    key: &NSString,
) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            title,
            Some(action),
            key,
        )
    }
}

fn register_shortcut(delegate: &AppDelegate, action: Action, item: &Retained<NSMenuItem>) {
    delegate
        .ivars()
        .shortcut_items
        .borrow_mut()
        .insert(action, item.clone());
}

impl AppDelegate {
    /// Apply all active shortcuts from `ShortcutStore` to menu items, toolbar tooltips, and overlay.
    pub(crate) fn apply_shortcuts(&self) {
        let store = self.ivars().shortcuts.borrow();
        let items = self.ivars().shortcut_items.borrow();
        for (&action, item) in items.iter() {
            match store.get(action) {
                Some(combo) => {
                    item.setKeyEquivalent(&NSString::from_str(&combo.key));
                    item.setKeyEquivalentModifierMask(combo.ns_modifiers());
                }
                None => {
                    item.setKeyEquivalent(ns_string!(""));
                    item.setKeyEquivalentModifierMask(
                        objc2_app_kit::NSEventModifierFlags::empty(),
                    );
                }
            }
        }
        if let Some(shifted) = self.ivars().larger_thumbs_shifted.get() {
            if store.use_larger_thumbs_shifted_twin() {
                shifted.setKeyEquivalent(ns_string!("="));
                shifted.setKeyEquivalentModifierMask(
                    objc2_app_kit::NSEventModifierFlags::Command
                        | objc2_app_kit::NSEventModifierFlags::Shift,
                );
            } else {
                shifted.setKeyEquivalent(ns_string!(""));
                shifted.setKeyEquivalentModifierMask(
                    objc2_app_kit::NSEventModifierFlags::empty(),
                );
            }
        }
        if let Some(toolbar) = self.ivars().toolbar.get() {
            toolbar.sync_tooltips(&store);
        }
        if let Some(slider) = self.ivars().thumb_slider.get() {
            let up = store.display_or_empty(Action::LargerThumbs);
            let down = store.display_or_empty(Action::SmallerThumbs);
            let tip = match (up.is_empty(), down.is_empty()) {
                (false, false) => format!("Thumbnail size ({up}, {down})"),
                (false, true) => format!("Thumbnail size ({up})"),
                (true, false) => format!("Thumbnail size ({down})"),
                (true, true) => "Thumbnail size".to_owned(),
            };
            slider.setToolTip(Some(&NSString::from_str(&tip)));
        }
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.controls
                .sync_tooltips(&store, &self.repeat_title(Transfer::Move));
        }
        drop(items);
        drop(store);
        self.update_overlay();
    }
    pub(crate) fn menu_item_enabled(&self, item: &NSMenuItem) -> bool {
        let action = item.action();
        if action == Some(sel!(toggleSidebarMenu:)) {
            if let Some(split) = self.ivars().split.get() {
                let collapsed = split
                    .splitViewItems()
                    .firstObject()
                    .is_some_and(|item| item.isCollapsed());
                item.setState(if collapsed { 0 } else { 1 });
            }
        }
        if action == Some(sel!(setSortMenu:)) {
            let selected = sort_tag(self.ivars().sort_order.get());
            item.setState(if item.tag() == selected { 1 } else { 0 });
        }
        // Restore app-level undo/redo titles after an NSTextView field editor changes them.
        if action == Some(sel!(undo:)) {
            item.setTitle(&NSString::from_str(
                &self.ivars().history.borrow().undo_title(),
            ));
        } else if action == Some(sel!(redo:)) {
            item.setTitle(&NSString::from_str(
                &self.ivars().history.borrow().redo_title(),
            ));
        }
        self.action_enabled(action)
    }

    pub(crate) fn action_enabled(&self, action: Option<objc2::runtime::Sel>) -> bool {
        let prefs_key = self
            .ivars()
            .prefs_window
            .get()
            .is_some_and(|window| window.isKeyWindow());
        let editing_text = || {
            NSApplication::sharedApplication(self.mtm())
                .keyWindow()
                .and_then(|window| window.firstResponder())
                .is_some_and(|responder| responder.isKindOfClass(NSTextView::class()))
        };
        let in_crop = || {
            self.ivars()
                .show
                .borrow()
                .as_ref()
                .is_some_and(|show| show.view.crop_view().is_some())
        };
        if action == Some(sel!(undo:)) || action == Some(sel!(redo:)) {
            if editing_text() || prefs_key || self.is_busy() || in_crop() {
                return false;
            }
            let history = self.ivars().history.borrow();
            return if action == Some(sel!(undo:)) {
                history.can_undo()
            } else {
                history.can_redo()
            };
        }
        if action == Some(sel!(setAutoAdvanceMenu:)) {
            return !prefs_key && !self.is_upscaling_or_comparing() && !in_crop();
        }
        if action == Some(sel!(showFilter:))
            || action == Some(sel!(biggerThumbs:))
            || action == Some(sel!(smallerThumbs:))
        {
            return !prefs_key && self.ivars().show.borrow().is_none();
        }
        if action == Some(sel!(startSlideshow:)) {
            return !prefs_key
                && self.ivars().show.borrow().is_none()
                && self.ivars().grid.get().is_some_and(|grid| grid.counts().0 > 0);
        }
        let edits = [
            sel!(rotateLeft:),
            sel!(rotateRight:),
            sel!(flipHorizontal:),
            sel!(cropImage:),
            sel!(upscaleImage:),
            sel!(compressImage:),
        ];
        if edits.iter().any(|wanted| action == Some(*wanted)) {
            let needs_idle_worker =
                action == Some(sel!(upscaleImage:)) || action == Some(sel!(compressImage:));
            let edits_pending = crate::editing::editing() || self.ivars().pending_edits.get() > 0;
            if prefs_key
                || self.ivars().transfer_busy.get()
                || self.is_upscaling_or_comparing()
                || self.is_compressing()
                || (needs_idle_worker && edits_pending)
            {
                return false;
            }
            if let Some(show) = self.ivars().show.borrow().as_ref() {
                return show.is_current_displayed()
                    && show.view.crop_view().is_none()
                    && show.view.compare_view().is_none()
                    && !((action == Some(sel!(cropImage:))
                        || action == Some(sel!(upscaleImage:))
                        || action == Some(sel!(compressImage:)))
                        && show.view.is_animated());
            }
            let selected = self
                .ivars()
                .grid
                .get()
                .map_or(0, |grid| grid.selected_count());
            return if action == Some(sel!(cropImage:)) {
                selected == 1 && !edits_pending
            } else if action == Some(sel!(compressImage:)) {
                selected > 0
                    || (self.current_scan().is_some()
                        && self.ivars().grid.get().is_some_and(|grid| grid.counts().1 > 0))
            } else {
                selected > 0
            };
        }
        if action == Some(sel!(compressFolder:)) {
            return !prefs_key
                && self.ivars().show.borrow().is_none()
                && !self.is_busy()
                && self.current_scan().is_some();
        }
        if action == Some(sel!(paste:)) {
            let has_files = unsafe {
                crate::file_ops::image_pasteboard().canReadItemWithDataConformingToTypes(
                    &NSArray::from_slice(&[objc2_app_kit::NSPasteboardTypeFileURL]),
                )
            };
            return !prefs_key
                && has_files
                && self.current_scan().is_some()
                && !self.is_busy()
                && !in_crop();
        }
        let mutating_file_ops = [
            sel!(moveToTrash:),
            sel!(moveToFolder:),
            sel!(copyToFolder:),
            sel!(moveAgain:),
            sel!(copyAgain:),
        ];
        let is_mutating_op = mutating_file_ops
            .iter()
            .any(|wanted| action == Some(*wanted));
        let acts_on_images = is_mutating_op
            || [
                sel!(revealInFinder:),
                sel!(openWith:),
                sel!(copy:),
                sel!(copyPath:),
                sel!(copyFileUrl:),
                sel!(setDesktopPicture:),
            ]
            .iter()
            .any(|wanted| action == Some(*wanted));
        if !acts_on_images {
            return true;
        }
        if is_mutating_op
            && (editing_text()
                || self.is_busy()
                || self
                    .ivars()
                    .show
                    .borrow()
                    .as_ref()
                    .is_some_and(|show| show.view.crop_view().is_some()))
        {
            return false;
        }
        if action == Some(sel!(moveAgain:)) && self.ivars().last_move.borrow().is_none() {
            return false;
        }
        if action == Some(sel!(copyAgain:)) && self.ivars().last_copy.borrow().is_none() {
            return false;
        }
        let in_show = self.ivars().show.borrow().is_some();
        let selected = self
            .ivars()
            .grid
            .get()
            .is_some_and(|grid| grid.has_selection());
        (in_show || selected) && !prefs_key
    }
}

pub(crate) const SORT_ITEMS: [(&str, SortOrder); 6] = [
    ("Sort by Name", SortOrder::Name),
    ("Sort by Date Modified", SortOrder::Modified),
    ("Sort by Size", SortOrder::Size),
    ("Sort by File Path", SortOrder::Path),
    ("Sort by EXIF Date", SortOrder::ExifDate),
    ("Sort by Date Added", SortOrder::Added),
];

pub(crate) fn sort_tag(order: SortOrder) -> isize {
    SORT_ITEMS
        .iter()
        .position(|&(_, o)| o == order)
        .unwrap_or(0) as isize
        + 1
}

pub(crate) fn sort_for_tag(tag: isize) -> SortOrder {
    usize::try_from(tag - 1)
        .ok()
        .and_then(|index| SORT_ITEMS.get(index))
        .map_or(SortOrder::default(), |&(_, order)| order)
}

/// Standalone sort menu for the toolbar popup; checkmarks stay synced via validation.
pub(crate) fn sort_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    for (title, order) in SORT_ITEMS {
        let short = title.trim_start_matches("Sort by ");
        let item = menu_item(mtm, &NSString::from_str(short), sel!(setSortMenu:), ns_string!(""));
        item.setTag(sort_tag(order));
        menu.addItem(&item);
    }
    menu
}

pub(crate) fn build_menu(mtm: MainThreadMarker, app: &NSApplication, delegate: &AppDelegate) {
    let menubar = NSMenu::new(mtm);

    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    let prefs = menu_item(
        mtm,
        ns_string!("Settings…"),
        sel!(showPrefs:),
        ns_string!(","),
    );
    app_menu.addItem(&prefs);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (title, action, key, alternate) in [
        ("Hide Blickli", sel!(hide:), "h", false),
        ("Hide Others", sel!(hideOtherApplications:), "h", true),
        ("Show All", sel!(unhideAllApplications:), "", false),
    ] {
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        );
        if alternate {
            item.setKeyEquivalentModifierMask(
                objc2_app_kit::NSEventModifierFlags::Command
                    | objc2_app_kit::NSEventModifierFlags::Option,
            );
        }
        app_menu.addItem(&item);
    }
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = menu_item(
        mtm,
        ns_string!("Quit Blickli"),
        sel!(terminate:),
        ns_string!("q"),
    );
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    menubar.addItem(&app_item);

    let file_item = NSMenuItem::new(mtm);
    let file_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("File"));
    let open = menu_item(
        mtm,
        ns_string!("Open Folder…"),
        sel!(openDocument:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::OpenFolder, &open);
    file_menu.addItem(&open);
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let reveal = menu_item(
        mtm,
        ns_string!("Reveal in Finder"),
        sel!(revealInFinder:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::RevealInFinder, &reveal);
    file_menu.addItem(&reveal);
    let info = menu_item(mtm, ns_string!("Get Info"), sel!(getInfo:), ns_string!(""));
    register_shortcut(delegate, Action::GetInfo, &info);
    file_menu.addItem(&info);
    let open_with = NSMenuItem::new(mtm);
    open_with.setTitle(ns_string!("Open With"));
    open_with.setSubmenu(Some(&delegate.lazy_open_with_menu(mtm)));
    file_menu.addItem(&open_with);
    let desktop = menu_item(
        mtm,
        ns_string!("Set as Desktop Picture"),
        sel!(setDesktopPicture:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::SetDesktopPicture, &desktop);
    file_menu.addItem(&desktop);
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let trash = menu_item(
        mtm,
        ns_string!("Move to Trash"),
        sel!(moveToTrash:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::MoveToTrash, &trash);
    file_menu.addItem(&trash);
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (title, sel_action, shortcut_action) in [
        ("Move to…", sel!(moveToFolder:), Action::MoveToFolder),
        ("Copy to…", sel!(copyToFolder:), Action::CopyToFolder),
        ("Move Again", sel!(moveAgain:), Action::MoveAgain),
        ("Copy Again", sel!(copyAgain:), Action::CopyAgain),
    ] {
        let item = menu_item(mtm, &NSString::from_str(title), sel_action, ns_string!(""));
        register_shortcut(delegate, shortcut_action, &item);
        file_menu.addItem(&item);
        if sel_action == sel!(moveAgain:) {
            let _ = delegate.ivars().move_again_item.set(item);
        } else if sel_action == sel!(copyAgain:) {
            let _ = delegate.ivars().copy_again_item.set(item);
        }
    }
    file_item.setSubmenu(Some(&file_menu));
    menubar.addItem(&file_item);

    let edit_item = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    let select_all = menu_item(
        mtm,
        ns_string!("Select All"),
        sel!(selectAll:),
        ns_string!("a"),
    );
    // Nil target so active text fields receive ⌘Z and ⇧⌘Z first down the responder chain.
    let undo = menu_item(mtm, ns_string!("Undo"), sel!(undo:), ns_string!("z"));
    edit_menu.addItem(&undo);
    let _ = delegate.ivars().undo_item.set(undo);
    let redo = menu_item(mtm, ns_string!("Redo"), sel!(redo:), ns_string!("z"));
    redo.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Shift,
    );
    edit_menu.addItem(&redo);
    let _ = delegate.ivars().redo_item.set(redo);
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&select_all);
    let filter = menu_item(
        mtm,
        ns_string!("Filter by Name"),
        sel!(showFilter:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::FilterByName, &filter);
    edit_menu.addItem(&filter);
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Nil target so active text fields receive ⌘X, ⌘C, and ⌘V first.
    let cut = menu_item(mtm, ns_string!("Cut"), sel!(cut:), ns_string!("x"));
    edit_menu.addItem(&cut);
    let copy_images = menu_item(mtm, ns_string!("Copy"), sel!(copy:), ns_string!("c"));
    edit_menu.addItem(&copy_images);
    let paste = menu_item(mtm, ns_string!("Paste"), sel!(paste:), ns_string!("v"));
    edit_menu.addItem(&paste);
    let copy_path = menu_item(
        mtm,
        ns_string!("Copy Path"),
        sel!(copyPath:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::CopyPath, &copy_path);
    edit_menu.addItem(&copy_path);
    let copy_url = menu_item(
        mtm,
        ns_string!("Copy File URL"),
        sel!(copyFileUrl:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::CopyFileUrl, &copy_url);
    edit_menu.addItem(&copy_url);
    edit_item.setSubmenu(Some(&edit_menu));
    menubar.addItem(&edit_item);

    let image_item = NSMenuItem::new(mtm);
    let image_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Image"));
    for entry in [
        Some(("Rotate Left", sel!(rotateLeft:), Action::RotateLeft)),
        Some(("Rotate Right", sel!(rotateRight:), Action::RotateRight)),
        Some(("Flip Horizontally", sel!(flipHorizontal:), Action::FlipHorizontal)),
        None,
        Some(("Crop…", sel!(cropImage:), Action::CropImage)),
        Some(("Upscale…", sel!(upscaleImage:), Action::UpscaleImage)),
        Some(("Compress…", sel!(compressImage:), Action::CompressImage)),
        Some(("Compress Folder…", sel!(compressFolder:), Action::CompressFolder)),
    ] {
        let Some((title, sel_action, shortcut_action)) = entry else {
            image_menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        };
        let item = menu_item(mtm, &NSString::from_str(title), sel_action, ns_string!(""));
        register_shortcut(delegate, shortcut_action, &item);
        image_menu.addItem(&item);
    }
    image_item.setSubmenu(Some(&image_menu));
    menubar.addItem(&image_item);

    let window_item = NSMenuItem::new(mtm);
    let window_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Window"));
    for (title, action, key) in [
        ("Close", sel!(performClose:), "w"),
        ("Minimize", sel!(performMiniaturize:), "m"),
        ("Zoom", sel!(performZoom:), ""),
    ] {
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        );
        window_menu.addItem(&item);
    }
    window_item.setSubmenu(Some(&window_menu));
    // Register explicitly so AppKit appends window items here rather than in View.
    app.setWindowsMenu(Some(&window_menu));

    let show_item = NSMenuItem::new(mtm);
    let show_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Slideshow"));
    let start = menu_item(
        mtm,
        ns_string!("Start Slideshow"),
        sel!(startSlideshow:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::StartSlideshow, &start);
    show_menu.addItem(&start);
    show_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let loop_item = menu_item(mtm, ns_string!("Loop"), sel!(toggleLoop:), ns_string!(""));
    register_shortcut(delegate, Action::ToggleLoop, &loop_item);
    let shuffle_item = menu_item(
        mtm,
        ns_string!("Shuffle"),
        sel!(toggleShuffle:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::ToggleShuffle, &shuffle_item);
    show_menu.addItem(&loop_item);
    show_menu.addItem(&shuffle_item);

    show_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let auto_item = NSMenuItem::new(mtm);
    let auto_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Auto-Advance"));
    for (title, tag) in [
        ("Off", 0isize),
        ("1 Second", 10),
        ("3 Seconds", 30),
        ("5 Seconds", 50),
        ("10 Seconds", 100),
    ] {
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            sel!(setAutoAdvanceMenu:),
            ns_string!(""),
        );
        item.setTag(tag);
        if tag == 0 {
            item.setState(1);
        }
        auto_menu.addItem(&item);
    }
    auto_item.setSubmenu(Some(&auto_menu));
    auto_item.setTitle(ns_string!("Auto-Advance"));
    show_menu.addItem(&auto_item);
    show_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let controls_item = menu_item(
        mtm,
        ns_string!("Show Controls"),
        sel!(toggleControls:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::ToggleControls, &controls_item);
    show_menu.addItem(&controls_item);
    let _ = delegate.ivars().controls_item.set(controls_item);

    show_item.setSubmenu(Some(&show_menu));
    menubar.addItem(&show_item);

    let view_item = NSMenuItem::new(mtm);
    let view_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("View"));
    for (title, order) in SORT_ITEMS {
        let tag = sort_tag(order);
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            sel!(setSortMenu:),
            ns_string!(""),
        );
        item.setTag(tag);
        if order == SortOrder::default() {
            item.setState(1);
        }
        register_shortcut(delegate, Action::sort_order_action(order), &item);
        view_menu.addItem(&item);
    }
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Hidden shifted twin lets ⌘+ trigger Larger Thumbnails when bound to ⌘=.
    let bigger = menu_item(
        mtm,
        ns_string!("Larger Thumbnails"),
        sel!(biggerThumbs:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::LargerThumbs, &bigger);
    let bigger_shifted = menu_item(
        mtm,
        ns_string!("Larger Thumbnails"),
        sel!(biggerThumbs:),
        ns_string!(""),
    );
    bigger_shifted.setHidden(true);
    let _ = delegate.ivars().larger_thumbs_shifted.set(bigger_shifted.clone());
    let smaller = menu_item(
        mtm,
        ns_string!("Smaller Thumbnails"),
        sel!(smallerThumbs:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::SmallerThumbs, &smaller);
    view_menu.addItem(&bigger);
    view_menu.addItem(&bigger_shifted);
    view_menu.addItem(&smaller);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let scale_up = menu_item(
        mtm,
        ns_string!("Scale Up Small Images"),
        sel!(toggleScaleUp:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::ScaleUpSmall, &scale_up);
    view_menu.addItem(&scale_up);
    let _ = delegate.ivars().scale_up_item.set(scale_up);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let sidebar_item = menu_item(
        mtm,
        ns_string!("Show Sidebar"),
        sel!(toggleSidebarMenu:),
        ns_string!(""),
    );
    sidebar_item.setState(1);
    register_shortcut(delegate, Action::ToggleSidebar, &sidebar_item);
    view_menu.addItem(&sidebar_item);
    let labels_item = menu_item(
        mtm,
        ns_string!("Show Filenames"),
        sel!(toggleLabels:),
        ns_string!(""),
    );
    register_shortcut(delegate, Action::ToggleLabels, &labels_item);
    view_menu.addItem(&labels_item);
    let _ = delegate.ivars().labels_item.set(labels_item);
    view_item.setSubmenu(Some(&view_menu));
    menubar.addItem(&view_item);
    menubar.addItem(&window_item);

    let _ = delegate.ivars().loop_item.set(loop_item);
    let _ = delegate.ivars().shuffle_item.set(shuffle_item);
    let _ = delegate.ivars().auto_menu.set(auto_menu);

    delegate.apply_shortcuts();
    app.setMainMenu(Some(&menubar));
}
