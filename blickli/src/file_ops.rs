//! File moves, copies, Trash, undo/redo, Open With, clipboard, and desktop wallpaper.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use super::*;

impl AppDelegate {
    /// Run an open panel modally, then restore key focus to any active slideshow window.
    pub(crate) fn run_panel(&self, panel: &NSOpenPanel) -> bool {
        let ok = panel.runModal() == objc2_app_kit::NSModalResponseOK;
        let slide = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .map(|show| show.window.clone());
        if let Some(window) = slide {
            window.makeKeyAndOrderFront(None);
        }
        ok
    }

    pub(crate) fn choose_folder(&self, kind: Transfer) -> Option<PathBuf> {
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setCanCreateDirectories(true);
        panel.setPrompt(Some(&NSString::from_str(match kind {
            Transfer::Move => "Move",
            Transfer::Copy => "Copy",
        })));
        if !self.run_panel(&panel) {
            return None;
        }
        panel.URL().and_then(grants::remember)
    }

    pub(crate) fn transfer_to_chosen_folder(&self, kind: Transfer) {
        if self.is_busy()
            || self
                .ivars()
                .show
                .borrow()
                .as_ref()
                .is_some_and(|show| show.view.crop_view().is_some())
        {
            return;
        }
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        // Pause auto-advance while the modal folder picker is open so the current slide
        // cannot change underneath the user.
        let paused_interval = {
            let mut show = self.ivars().show.borrow_mut();
            show.as_mut().and_then(|show| {
                show.timer.take().map(|timer| {
                    timer.invalidate();
                    show.interval
                })?
            })
        };
        if let Some(folder) = self.choose_folder(kind) {
            if let Some(seconds) = paused_interval {
                self.schedule_timer(seconds);
            }
            self.transfer_paths(targets, folder, kind);
        } else if let Some(seconds) = paused_interval {
            self.schedule_timer(seconds);
            self.update_overlay();
        }
    }

    pub(crate) fn transfer_selection(&self, folder: PathBuf, kind: Transfer) {
        if self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .is_some_and(|show| show.view.crop_view().is_some())
        {
            return;
        }
        self.transfer_paths(self.action_targets(), folder, kind);
    }

    /// Move or copy `paths` into `folder` on a background thread.
    pub fn transfer_paths(&self, paths: Vec<PathBuf>, folder: PathBuf, kind: Transfer) {
        if paths.is_empty() || self.is_busy() {
            return;
        }
        for path in &paths {
            self.save_slide_rotation_now(path);
        }
        self.ivars().transfer_busy.set(true);

        let total = paths.len();
        let progress = Arc::new(Progress {
            kind,
            total,
            reached: AtomicUsize::new(0),
            moved: Mutex::new(Vec::new()),
            posted: AtomicBool::new(false),
        });
        let bg_guard = editing::track_background();
        std::thread::spawn(move || {
            let _bg = bg_guard;
            let mut done: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(total);
            let mut renamed = 0usize;
            let mut failed: Vec<String> = Vec::new();
            for (index, source) in paths.iter().enumerate() {
                progress.reached.store(index + 1, Ordering::Relaxed);
                if total > 1 {
                    progress.post();
                }
                match kind.apply(source, &folder) {
                    Ok(transferred) => {
                        // Moving a file into its own folder is a no-op; keep it in place.
                        if kind == Transfer::Move && transferred.target == *source {
                            continue;
                        }
                        renamed += usize::from(transferred.renamed);
                        // Remove moved items from the UI immediately rather than waiting for the watcher.
                        if kind == Transfer::Move {
                            progress.moved.lock().unwrap().push(source.clone());
                            progress.post();
                        }
                        done.push((source.clone(), transferred.target));
                    }
                    Err(error) => {
                        eprintln!("{} failed: {}: {error}", kind.past(), source.display());
                        failed.push(reason(&error.to_string()));
                    }
                }
            }
            on_main(move |delegate| delegate.finish_transfer(kind, &done, &folder, renamed, &failed));
        });
    }

    fn moved_away(&self, sources: &[PathBuf]) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.remove_and_advance(sources);
        }
        self.drop_from_slideshow(sources);
    }

    fn finish_transfer(
        &self,
        kind: Transfer,
        done: &[(PathBuf, PathBuf)],
        folder: &Path,
        renamed: usize,
        failed: &[String],
    ) {
        self.ivars().transfer_busy.set(false);
        let touched: Vec<PathBuf> = match kind {
            Transfer::Move => done
                .iter()
                .flat_map(|(from, to)| [from.clone(), to.clone()])
                .collect(),
            Transfer::Copy => done.iter().map(|(_, to)| to.clone()).collect(),
        };
        self.notice_files(&touched);
        let step = match kind {
            Transfer::Move => Step::Moved(done.to_vec()),
            Transfer::Copy => Step::Copied(done.iter().map(|(_, to)| to.clone()).collect()),
        };
        let label = match kind {
            Transfer::Move => "move",
            Transfer::Copy => "copy",
        };
        self.ivars().history.borrow_mut().record(label, step);
        self.refresh_history_items();
        if !done.is_empty() {
            let (slot, key) = match kind {
                Transfer::Move => (&self.ivars().last_move, ns_string!("lastMoveFolder")),
                Transfer::Copy => (&self.ivars().last_copy, ns_string!("lastCopyFolder")),
            };
            *slot.borrow_mut() = Some(folder.to_path_buf());
            self.sync_repeat_items();
            if !e2e::enabled() && !snapshot::enabled() {
                let value = NSString::from_str(&folder.to_string_lossy());
                unsafe {
                    NSUserDefaults::standardUserDefaults().setObject_forKey(Some(&value), key);
                }
            }
        }
        let message = transfer_message(kind, done, folder, renamed, failed);
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// Sync app-initiated file changes into the engine and drain events now so
    /// status messages are not overwritten by late watcher notifications.
    /// Returns the subset of `paths` that emitted `Event::FileChanged`.
    pub(crate) fn notice_files(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        let Some(engine) = self.ivars().engine.get() else {
            return Vec::new();
        };
        let scan = self.ivars().current_scan.borrow().clone();
        let Some((root, recursive)) = scan else {
            return Vec::new();
        };
        let ours: Vec<PathBuf> = paths
            .iter()
            .filter(|path| {
                blickli_core::in_folder_scan(&root, path, recursive, sidebar::is_package)
            })
            .cloned()
            .collect();
        if !ours.is_empty() {
            let changed = engine.notice(&ours);
            drain_events(self.mtm());
            return changed;
        }
        Vec::new()
    }

    /// Undo (`undoing == true`) or redo the top history entry.
    pub(crate) fn take_back(&self, undoing: bool) {
        if self.is_busy()
            || self
                .ivars()
                .show
                .borrow()
                .as_ref()
                .is_some_and(|show| show.view.crop_view().is_some())
        {
            return;
        }
        let entry = {
            let mut history = self.ivars().history.borrow_mut();
            if undoing {
                history.take_undo()
            } else {
                history.take_redo()
            }
        };
        let Some(entry) = entry else { return };

        let (moves, next_step) = match entry.step.reversal(undoing) {
            Reversal::Move { moves, then } => (moves, then),
            Reversal::Edit { edits, undoing } => {
                self.take_back_edits(entry.label, edits, undoing);
                return;
            }
            // Move copies (undoing Copy) or original files (redoing Trash) via Finder Trash
            // and record their new Trash paths so the opposite direction can restore them.
            Reversal::Trash(paths) => {
                let attempted = paths.len();
                let trashed = trash::move_to_trash(&paths);
                let restorable: Vec<(PathBuf, PathBuf)> = trashed
                    .moved
                    .iter()
                    .filter(|(_, landed)| !landed.as_os_str().is_empty())
                    .cloned()
                    .collect();
                let gone: Vec<PathBuf> =
                    trashed.moved.iter().map(|(orig, _)| orig.clone()).collect();
                let next_step = if undoing {
                    Step::TrashedCopy(restorable)
                } else {
                    Step::Trashed(restorable)
                };
                self.after_take_back(
                    &gone,
                    &[],
                    attempted,
                    &trashed.failed,
                    &entry.label,
                    next_step,
                    undoing,
                );
                return;
            }
        };

        let mut arrived: Vec<PathBuf> = Vec::new();
        let mut left: Vec<PathBuf> = Vec::new();
        let mut succeeded: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for (from, to) in &moves {
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match transfer::move_exact(from, to) {
                Ok(()) => {
                    arrived.push(to.clone());
                    left.push(from.clone());
                    let pair = if undoing {
                        (to.clone(), from.clone())
                    } else {
                        (from.clone(), to.clone())
                    };
                    succeeded.push(pair);
                }
                Err(error) => {
                    eprintln!(
                        "undo failed: {} -> {}: {error}",
                        from.display(),
                        to.display()
                    );
                    failed.push(reason(&error.to_string()));
                }
            }
        }
        let next_step = match next_step {
            Step::Moved(_) => Step::Moved(succeeded),
            Step::Trashed(_) => Step::Trashed(succeeded),
            Step::Copied(_) => Step::Copied(arrived.clone()),
            other => other,
        };
        self.after_take_back(
            &left,
            &arrived,
            moves.len(),
            &failed,
            &entry.label,
            next_step,
            undoing,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn after_take_back(
        &self,
        left: &[PathBuf],
        arrived: &[PathBuf],
        attempted: usize,
        failed: &[String],
        label: &str,
        step: Step,
        undoing: bool,
    ) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.remove_and_advance(left);
        }
        self.drop_from_slideshow(left);
        let touched: Vec<PathBuf> = left.iter().chain(arrived.iter()).cloned().collect();
        self.notice_files(&touched);
        {
            let mut history = self.ivars().history.borrow_mut();
            let entry = Entry {
                label: label.to_owned(),
                step,
            };
            if undoing {
                history.push_undone(entry);
            } else {
                history.push_done(entry);
            }
        }
        self.refresh_history_items();
        let message = if !failed.is_empty() {
            put_back_message(attempted, failed)
        } else {
            let count = arrived.len().max(left.len());
            let (verb, noun) = if undoing {
                ("Undid", "undo")
            } else {
                ("Redid", "redo")
            };
            match count {
                0 => format!("Nothing to {noun}"),
                1 => format!("{verb} {label}"),
                n => format!("{verb} {label} ({n} images)"),
            }
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    pub(crate) fn refresh_history_items(&self) {
        let history = self.ivars().history.borrow();
        if let Some(item) = self.ivars().undo_item.get() {
            item.setTitle(&NSString::from_str(&history.undo_title()));
        }
        if let Some(item) = self.ivars().redo_item.get() {
            item.setTitle(&NSString::from_str(&history.redo_title()));
        }
    }

    pub fn lazy_open_with_menu(&self, mtm: MainThreadMarker) -> Retained<NSMenu> {
        let menu = NSMenu::new(mtm);
        menu.setDelegate(Some(ProtocolObject::from_ref(self)));
        menu
    }

    #[cfg(any(test, feature = "e2e"))]
    pub fn open_with_menu(&self, mtm: MainThreadMarker) -> Retained<NSMenu> {
        let menu = NSMenu::new(mtm);
        self.fill_open_with(&menu);
        menu
    }

    pub(crate) fn fill_open_with(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let targets = self.action_targets();
        let Some(url) = targets.first().and_then(NSURL::from_file_path) else {
            return;
        };
        let workspace = NSWorkspace::sharedWorkspace();
        for app in workspace.URLsForApplicationsToOpenURL(&url).iter() {
            let Some(path) = app.path() else { continue };
            let name = objc2_foundation::NSFileManager::defaultManager().displayNameAtPath(&path);
            let item = menu_item(mtm, &name, sel!(openWith:), ns_string!(""));
            unsafe {
                item.setTarget(Some(self));
                item.setRepresentedObject(Some(&app));
            }
            menu.addItem(&item);
        }
    }

    pub(crate) fn open_targets_with(&self, app: &NSURL) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let urls: Vec<Retained<NSURL>> = targets.iter().filter_map(NSURL::from_file_path).collect();
        let configuration = objc2_app_kit::NSWorkspaceOpenConfiguration::configuration();
        NSWorkspace::sharedWorkspace()
            .openURLs_withApplicationAtURL_configuration_completionHandler(
                &NSArray::from_retained_slice(&urls),
                app,
                &configuration,
                None,
            );
    }

    /// Write target file URLs to the pasteboard (Finder-compatible copy).
    pub(crate) fn copy_images_to_clipboard(&self) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let urls: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = targets
            .iter()
            .filter_map(NSURL::from_file_path)
            .map(ProtocolObject::from_retained)
            .collect();
        let pasteboard = image_pasteboard();
        pasteboard.clearContents();
        pasteboard.writeObjects(&NSArray::from_retained_slice(&urls));
        let message = match targets.len() {
            1 => format!("Copied {}", file_name(&targets[0])),
            n => format!("Copied {n} images"),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// Copy file URLs from the pasteboard into the active folder (handles `paste:` when not editing text).
    pub(crate) fn paste_images(&self) {
        let Some((folder, _)) = self.current_scan() else {
            return;
        };
        let files: Vec<PathBuf> = pasteboard_paths(&image_pasteboard())
            .into_iter()
            .filter(|path| path.is_file())
            .collect();
        self.transfer_paths(files, folder, Transfer::Copy);
    }

    /// Copy target paths or file URLs as newline-separated text.
    pub(crate) fn copy_targets(&self, as_url: bool) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let text = targets
            .iter()
            .map(|path| {
                if as_url {
                    NSURL::from_file_path(path)
                        .and_then(|url| url.absoluteString())
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                } else {
                    path.to_string_lossy().into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let pasteboard = image_pasteboard();
        pasteboard.clearContents();
        unsafe {
            pasteboard.setString_forType(
                &NSString::from_str(&text),
                objc2_app_kit::NSPasteboardTypeString,
            )
        };
        let what = if as_url { "file URL" } else { "path" };
        let message = match targets.len() {
            1 => format!("Copied {what}"),
            n => format!("Copied {n} {what}s"),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    pub(crate) fn use_as_desktop_picture(&self) {
        let targets = self.action_targets();
        let Some(path) = targets.first() else { return };
        let Some(url) = NSURL::from_file_path(path) else {
            return;
        };
        let screen = self
            .ivars()
            .window
            .get()
            .and_then(|window| window.screen())
            .or_else(|| NSScreen::mainScreen(self.mtm()));
        let Some(screen) = screen else { return };
        let message = match unsafe {
            NSWorkspace::sharedWorkspace().setDesktopImageURL_forScreen_options_error(
                &url,
                &screen,
                &objc2_foundation::NSDictionary::new(),
            )
        } {
            Ok(()) => format!("Desktop picture: {}", file_name(path)),
            Err(_) => "Could not set desktop picture".to_owned(),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    pub(crate) fn reveal_targets(&self) {
        let paths = self.action_targets();
        if paths.is_empty() {
            return;
        }
        let urls: Vec<Retained<NSURL>> = paths.iter().filter_map(NSURL::from_file_path).collect();
        NSWorkspace::sharedWorkspace()
            .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&urls));
    }

    pub fn has_last_move(&self) -> bool {
        self.ivars().last_move.borrow().is_some()
    }

    pub fn repeat_title(&self, kind: Transfer) -> String {
        let (verb, folder) = match kind {
            Transfer::Move => ("Move", self.ivars().last_move.borrow().clone()),
            Transfer::Copy => ("Copy", self.ivars().last_copy.borrow().clone()),
        };
        match folder {
            Some(folder) => format!("{verb} Again to {}", folder_label(&folder)),
            None => format!("{verb} Again"),
        }
    }

    pub(crate) fn sync_repeat_items(&self) {
        for (item, kind) in [
            (self.ivars().move_again_item.get(), Transfer::Move),
            (self.ivars().copy_again_item.get(), Transfer::Copy),
        ] {
            let Some(item) = item else { continue };
            item.setTitle(&NSString::from_str(&self.repeat_title(kind)));
        }
        let store = self.ivars().shortcuts.borrow();
        if let Some(toolbar) = self.ivars().toolbar.get() {
            toolbar.sync_tooltips(&store);
        }
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.controls.sync_move_again(
                &self.repeat_title(Transfer::Move),
                self.has_last_move(),
                &store,
            );
        }
    }

    /// Current slide if a slideshow is active, otherwise selected grid items.
    pub(crate) fn action_targets(&self) -> Vec<PathBuf> {
        let current = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|show| show.playlist.current().cloned());
        match current {
            Some(path) => vec![path],
            None => self
                .ivars()
                .grid
                .get()
                .map(|g| g.selected_paths())
                .unwrap_or_default(),
        }
    }

    pub fn trash_selection(&self) {
        if self.is_busy()
            || self
                .ivars()
                .show
                .borrow()
                .as_ref()
                .is_some_and(|show| show.view.crop_view().is_some())
        {
            return;
        }
        let paths = self.action_targets();
        if paths.is_empty() {
            return;
        }

        let trashed = trash::move_to_trash(&paths);
        let moved: Vec<PathBuf> = trashed.moved.iter().map(|(from, _)| from.clone()).collect();
        let restorable: Vec<(PathBuf, PathBuf)> = trashed
            .moved
            .iter()
            .filter(|(_, landed)| !landed.as_os_str().is_empty())
            .cloned()
            .collect();
        self.ivars()
            .history
            .borrow_mut()
            .record("move to Trash", Step::Trashed(restorable));
        self.refresh_history_items();

        // Update UI and engine immediately so undo works without waiting for FSEvents.
        if let Some(grid) = self.ivars().grid.get() {
            grid.remove_and_advance(&moved);
        }
        self.drop_from_slideshow(&moved);
        self.notice_files(&moved);

        let message = trash_message(&moved, &trashed.failed);
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }
}

/// Coalesces background transfer progress onto the main thread to avoid rebuilding
/// large grids per file.
struct Progress {
    kind: Transfer,
    total: usize,
    reached: AtomicUsize,
    moved: Mutex<Vec<PathBuf>>,
    posted: AtomicBool,
}

impl Progress {
    fn post(self: &Arc<Self>) {
        if self.posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let progress = Arc::clone(self);
        on_main(move |delegate| {
            // Clear before draining so concurrent completions schedule another pass.
            progress.posted.store(false, Ordering::Release);
            let moved = std::mem::take(&mut *progress.moved.lock().unwrap());
            if !moved.is_empty() {
                delegate.moved_away(&moved);
            }
            // Update status after grid removal so selection change does not overwrite progress.
            if progress.total > 1 {
                delegate.show_status_message(&format!(
                    "{} {} of {}…",
                    progress.kind.present(),
                    count_text(progress.reached.load(Ordering::Relaxed)),
                    count_text(progress.total)
                ));
            }
        });
    }
}

pub(crate) fn image_pasteboard() -> Retained<objc2_app_kit::NSPasteboard> {
    #[cfg(any(test, feature = "e2e"))]
    if e2e::enabled() {
        let name = NSString::from_str(&format!("local.blickli.e2e.{}", std::process::id()));
        return objc2_app_kit::NSPasteboard::pasteboardWithName(&name);
    }
    objc2_app_kit::NSPasteboard::generalPasteboard()
}

pub(crate) fn pasteboard_paths(pasteboard: &objc2_app_kit::NSPasteboard) -> Vec<PathBuf> {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| unsafe { item.stringForType(objc2_app_kit::NSPasteboardTypeFileURL) })
        .filter_map(|text| NSURL::URLWithString(&text))
        .filter_map(|url| local_file_path(&url))
        .collect()
}
