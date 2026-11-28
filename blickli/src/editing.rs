//! Background edit worker and AppDelegate coordination for rotations, crops, and edit undo.
//!
//! Slideshow rotations are deferred until leaving the slide so consecutive turns write once.
//! Grid rotations write immediately.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

use blickli_core::history::Edit;
use blickli_core::messages::edit_message;
use blickli_core::Orientation;

use super::*;
use crate::compress::{self, BatchProgress, BatchResult, CompressOptions};
use crate::compress_ui::{self, CompressScope};
use crate::edit::{self, CropRect, Cropped};
use crate::upscale::{self, LoadedModel, Progress as UpscaleProgress, UpscaleSaveMode, UpscaleScale};

pub(crate) enum EditJob {
    Rotate(Vec<(PathBuf, Orientation)>),
    Crop {
        path: PathBuf,
        view: Orientation,
        crop: CropRect,
        replace: bool,
    },
    SaveUpscaled {
        path: PathBuf,
        image: Img,
        replace: bool,
    },
}

pub(crate) enum UpscaleJob {
    Preview {
        id: u64,
        path: PathBuf,
        view: Orientation,
        model_path: PathBuf,
        cancel: Arc<AtomicBool>,
    },
    Direct {
        id: u64,
        paths: Vec<(PathBuf, Orientation)>,
        model_path: PathBuf,
        scale: UpscaleScale,
        replace: bool,
        cancel: Arc<AtomicBool>,
    },
}

pub(crate) struct CompressJob {
    pub id: u64,
    pub paths: Vec<PathBuf>,
    pub options: CompressOptions,
    pub cancel: Arc<AtomicBool>,
}

type Rotated = Vec<(PathBuf, Orientation, Result<Edit, String>)>;

static QUEUED: AtomicUsize = AtomicUsize::new(0);
static BACKGROUND_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Decrements a worker active counter on scope exit (including panics).
pub(crate) struct ActiveGuard(&'static AtomicUsize);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Increment `BACKGROUND_ACTIVE` until the returned guard drops (used by file transfer threads).
pub(crate) fn track_background() -> ActiveGuard {
    BACKGROUND_ACTIVE.fetch_add(1, Ordering::AcqRel);
    ActiveGuard(&BACKGROUND_ACTIVE)
}

/// Returns `true` while an edit job is running on the worker thread.
pub(crate) fn editing() -> bool {
    QUEUED.load(Ordering::Acquire) > 0
}

/// Wait up to 10 seconds for the edit worker to finish. Returns `true` if idle.
fn wait_for_worker() -> bool {
    let start = std::time::Instant::now();
    while editing() {
        if start.elapsed() >= std::time::Duration::from_secs(10) {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    true
}

fn wait_for_background() {
    let start = std::time::Instant::now();
    while BACKGROUND_ACTIVE.load(Ordering::Acquire) > 0
        && start.elapsed() < std::time::Duration::from_secs(5)
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn spawn_worker() -> Sender<EditJob> {
    let (tx, rx) = channel::<EditJob>();
    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let _guard = ActiveGuard(&QUEUED);
            match job {
                EditJob::Rotate(files) => {
                    let results: Rotated = files
                        .into_iter()
                        .map(|(path, view)| {
                            let result =
                                objc2::rc::autoreleasepool(|_| edit::rotate_file(&path, view));
                            (path, view, result)
                        })
                        .collect();
                    on_main(move |delegate| delegate.finish_rotations(results, true));
                }
                EditJob::Crop {
                    path,
                    view,
                    crop,
                    replace,
                } => {
                    let result = objc2::rc::autoreleasepool(|_| {
                        edit::crop_file(&path, view, crop, replace)
                    });
                    on_main(move |delegate| delegate.crop_done(path, result));
                }
                EditJob::SaveUpscaled {
                    path,
                    image,
                    replace,
                } => {
                    let result = objc2::rc::autoreleasepool(|_| {
                        edit::save_upscaled(&path, &image, replace)
                    });
                    on_main(move |delegate| delegate.upscale_save_done(path, result));
                }
            }
        }
    });
    tx
}

fn ensure_loaded_model<'a>(
    cached: &'a mut Option<LoadedModel>,
    model_path: &Path,
) -> Result<&'a LoadedModel, String> {
    let reuse = cached
        .as_ref()
        .is_some_and(|m| m.source_path == model_path);
    if !reuse {
        *cached = None;
        let loaded = LoadedModel::load(model_path)?;
        *cached = Some(loaded);
    }
    Ok(cached.as_ref().expect("model just loaded"))
}

fn spawn_upscale_worker() -> Sender<UpscaleJob> {
    let (tx, rx) = channel::<UpscaleJob>();
    std::thread::spawn(move || {
        let mut cached_model: Option<LoadedModel> = None;
        while let Ok(job) = rx.recv() {
            let _guard = ActiveGuard(&BACKGROUND_ACTIVE);
            match job {
                UpscaleJob::Preview {
                    id,
                    path,
                    view,
                    model_path,
                    cancel,
                } => {
                    let result = objc2::rc::autoreleasepool(|_| {
                        if cancel.load(Ordering::Acquire) {
                            return Err("cancelled".to_owned());
                        }
                        on_main(move |delegate| {
                            delegate.upscale_preview_progress(id, UpscaleProgress::PreparingModel);
                        });
                        let model = ensure_loaded_model(&mut cached_model, &model_path)?;
                        if cancel.load(Ordering::Acquire) {
                            return Err("cancelled".to_owned());
                        }
                        let source = edit::decode_oriented(&path, view)?;
                        model.upscale_image(&source, &cancel, &mut |progress| {
                            on_main(move |delegate| {
                                delegate.upscale_preview_progress(id, progress);
                            });
                        })
                    });
                    on_main(move |delegate| delegate.upscale_preview_ready(id, path, result));
                }
                UpscaleJob::Direct {
                    id,
                    paths,
                    model_path,
                    scale,
                    replace,
                    cancel,
                } => {
                    let total = paths.len();
                    let mut saved: Vec<(PathBuf, Cropped)> = Vec::new();
                    let mut failed: Vec<String> = Vec::new();
                    let mut cancelled = cancel.load(Ordering::Acquire);
                    if !cancelled {
                        on_main(move |delegate| {
                            delegate
                                .upscale_direct_progress(id, "Preparing upscale model…".to_owned());
                        });
                        match ensure_loaded_model(&mut cached_model, &model_path) {
                            Ok(model) => {
                                for (index, (path, view)) in paths.into_iter().enumerate() {
                                    if cancel.load(Ordering::Acquire) {
                                        cancelled = true;
                                        break;
                                    }
                                    let item_name = file_name(&path);
                                    let step_res = objc2::rc::autoreleasepool(|_| {
                                        let source = edit::decode_oriented(&path, view)?;
                                        let out = model.upscale_image(
                                            &source,
                                            &cancel,
                                            &mut |progress| {
                                                if let UpscaleProgress::Tile { done, total: tiles } =
                                                    progress
                                                {
                                                    let msg = if total == 1 {
                                                        if tiles <= 1 {
                                                            format!("Upscaling {item_name}…")
                                                        } else {
                                                            format!(
                                                                "Upscaling {item_name}… {done}/{tiles}"
                                                            )
                                                        }
                                                    } else if tiles <= 1 {
                                                        format!(
                                                            "Upscaling {} of {}…",
                                                            count_text(index + 1),
                                                            count_text(total)
                                                        )
                                                    } else {
                                                        format!(
                                                            "Upscaling {} of {} ({done}/{tiles})…",
                                                            count_text(index + 1),
                                                            count_text(total)
                                                        )
                                                    };
                                                    on_main(move |delegate| {
                                                        delegate.upscale_direct_progress(id, msg);
                                                    });
                                                }
                                            },
                                        )?;
                                        let final_img = upscale::image_for_scale(
                                            &out.upscaled_native,
                                            out.native_scale,
                                            scale,
                                        )?;
                                        if cancel.load(Ordering::Acquire) {
                                            return Err("cancelled".to_owned());
                                        }
                                        edit::save_upscaled(&path, &final_img, replace)
                                    });
                                    match step_res {
                                        Ok(outcome) => saved.push((path, outcome)),
                                        Err(why) if why == "cancelled" => {
                                            cancelled = true;
                                            break;
                                        }
                                        Err(why) => failed.push(why),
                                    }
                                }
                            }
                            Err(why) => {
                                failed.push(why);
                            }
                        }
                    }
                    on_main(move |delegate| {
                        delegate.upscale_direct_done(id, saved, failed, cancelled);
                    });
                }
            }
        }
    });
    tx
}

fn spawn_compress_worker() -> Sender<CompressJob> {
    let (tx, rx) = channel::<CompressJob>();
    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let _guard = ActiveGuard(&BACKGROUND_ACTIVE);
            let id = job.id;
            let result =
                compress::compress_batch(&job.paths, job.options, &job.cancel, |progress| {
                    on_main(move |delegate| {
                        delegate.compress_progress(id, progress);
                    });
                });
            on_main(move |delegate| {
                delegate.compress_done(id, result);
            });
        }
    });
    tx
}

/// Per-slideshow tracking for pending background rotation writes.
#[derive(Default)]
pub(crate) struct Saving {
    /// Queued orientation writes not yet completed on disk.
    in_flight: HashMap<PathBuf, Orientation>,
    /// Failed orientation writes, recorded to avoid retrying on every slide leave.
    refused: HashMap<PathBuf, Orientation>,
    /// Files modified on disk whose next decode must replace the slide rather than keep zoom.
    pub redecode: std::collections::HashSet<PathBuf>,
}

impl Saving {
    pub fn in_flight(&self, path: &Path) -> Orientation {
        self.in_flight.get(path).copied().unwrap_or_default()
    }

    pub fn remove(&mut self, path: &Path) {
        self.in_flight.remove(path);
        self.refused.remove(path);
        self.redecode.remove(path);
    }
}

impl AppDelegate {
    /// Returns `true` while any mutating file transfer, edit, upscale, or compression is active.
    pub(crate) fn is_busy(&self) -> bool {
        self.ivars().transfer_busy.get()
            || editing()
            || self.ivars().pending_edits.get() > 0
            || self.is_upscaling_or_comparing()
            || self.is_compressing()
    }

    fn queue_edit(&self, job: EditJob) {
        let worker = self.ivars().edit_worker.get_or_init(spawn_worker);
        QUEUED.fetch_add(1, Ordering::AcqRel);
        self.ivars()
            .pending_edits
            .set(self.ivars().pending_edits.get() + 1);
        if worker.send(job).is_err() {
            QUEUED.fetch_sub(1, Ordering::AcqRel);
            self.ivars()
                .pending_edits
                .set(self.ivars().pending_edits.get().saturating_sub(1));
        }
    }

    /// Unsaved orientation delta for `path`, if `save_rotations` is enabled.
    fn unsaved_view(&self, path: &Path) -> Option<Orientation> {
        if !self.ivars().save_rotations.get() {
            return None;
        }
        let show = self.ivars().show.borrow();
        let show = show.as_ref()?;
        let state = show.view_states.get(path)?;
        let view = Orientation::from_view(state.rotation, state.flipped);
        let in_flight = show
            .saving
            .in_flight
            .get(path)
            .copied()
            .unwrap_or_default();
        let unsaved = in_flight.inverse().then(view);
        let refused = show.saving.refused.get(path) == Some(&view);
        (unsaved != Orientation::UP && !refused).then_some(unsaved)
    }

    /// Remaining uncommitted orientation delta for `path` (from `view_states` minus `in_flight`),
    /// avoiding stale orientation on `show.view` while a reload decode is pending.
    pub(crate) fn pending_view_orientation(show: &Show, path: &Path) -> Orientation {
        let view = show
            .view_states
            .get(path)
            .map(|s| Orientation::from_view(s.rotation, s.flipped))
            .unwrap_or_default();
        show.saving.in_flight(path).inverse().then(view)
    }

    pub(crate) fn save_slide_rotation(&self, path: &Path) {
        let Some(unsaved) = self.unsaved_view(path) else {
            return;
        };
        self.mark_in_flight(path, unsaved);
        self.queue_edit(EditJob::Rotate(vec![(path.to_path_buf(), unsaved)]));
    }

    /// Synchronously flush pending rotation for `path` (before moving, copying,
    /// upscaling, compressing, or quitting).
    pub(crate) fn save_slide_rotation_now(&self, path: &Path) {
        let has_in_flight = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .is_some_and(|show| show.saving.in_flight(path) != Orientation::UP);
        let unsaved = self.unsaved_view(path);
        if !has_in_flight && unsaved.is_none() {
            return;
        }
        if !wait_for_worker() {
            return;
        }
        if let Some(unsaved) = unsaved {
            let result = edit::rotate_file(path, unsaved);
            if has_in_flight {
                // An earlier worker `on_main(finish_rotations)` is still queued ahead of us.
                if result.is_ok() {
                    self.mark_in_flight(path, unsaved);
                }
                self.ivars()
                    .pending_edits
                    .set(self.ivars().pending_edits.get() + 1);
                let path = path.to_path_buf();
                on_main(move |delegate| {
                    delegate.finish_rotations(vec![(path, unsaved, result)], true);
                });
            } else {
                // Reconcile synchronously so undo order, overlay state, and failed-rotation
                // orientation fallback are immediately consistent for the caller.
                self.mark_in_flight(path, unsaved);
                self.finish_rotations(vec![(path.to_path_buf(), unsaved, result)], false);
            }
        }
    }

    pub(crate) fn finish_edits_before_quit(&self) {
        if let Some(cancel) = self.ivars().upscale_cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Release);
        }
        if let Some(cancel) = self.ivars().compress_cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Release);
        }
        let current = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|show| show.playlist.current().cloned());
        if let Some(path) = current {
            self.save_slide_rotation_now(&path);
        }
        let _ = wait_for_worker();
        wait_for_background();
    }

    fn mark_in_flight(&self, path: &Path, orientation: Orientation) {
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            let entry = show
                .saving
                .in_flight
                .entry(path.to_path_buf())
                .or_default();
            *entry = entry.then(orientation);
        }
    }

    /// Rotate the active slide in memory, or rotate selected grid files on disk.
    pub(crate) fn rotate_targets(&self, view: Orientation) {
        if self.ivars().transfer_busy.get()
            || self.is_upscaling_or_comparing()
            || self.is_compressing()
        {
            return;
        }
        let in_show = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .map(|show| (show.view.clone(), show.is_current_displayed()));
        if let Some((slide, current_displayed)) = in_show {
            if !current_displayed || slide.crop_view().is_some() || slide.compare_view().is_some() {
                return;
            }
            if view.mirrored {
                slide.flip();
            }
            if view.degrees() != 0 {
                slide.rotate(view.degrees());
            }
            return;
        }
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        if targets.len() > 1 {
            self.show_status_message(&format!("Rotating {} images…", count_text(targets.len())));
        }
        self.queue_edit(EditJob::Rotate(
            targets.into_iter().map(|path| (path, view)).collect(),
        ));
    }

    pub(crate) fn crop_selection(&self) {
        if self.ivars().transfer_busy.get()
            || self.is_upscaling_or_comparing()
            || self.is_compressing()
        {
            return;
        }
        if self.ivars().show.borrow().is_none() {
            if editing() || self.ivars().pending_edits.get() > 0 {
                return;
            }
            let one = self
                .ivars()
                .grid
                .get()
                .is_some_and(|grid| grid.selected_count() == 1);
            if !one {
                return;
            }
            self.start_from_grid();
        }
        self.start_crop();
    }

    fn finish_rotations(&self, results: Rotated, from_async: bool) {
        if from_async {
            self.ivars()
                .pending_edits
                .set(self.ivars().pending_edits.get().saturating_sub(1));
        }
        let mut edits = Vec::new();
        let mut saved = Vec::new();
        let mut touched = Vec::new();
        let mut failed = Vec::new();
        for (path, view, result) in results {
            match result {
                Ok(edit) => {
                    self.rotation_landed(&path, view, true);
                    touched.push(path.clone());
                    touched.push(edit.path().clone());
                    saved.push(path);
                    edits.push(edit);
                }
                Err(why) => {
                    self.rotation_landed(&path, view, false);
                    failed.push(why);
                }
            }
        }
        self.ivars()
            .history
            .borrow_mut()
            .record("rotation", Step::Edited(edits));
        self.refresh_history_items();
        self.files_edited(&touched);
        let message = edit_message("Rotated", &saved, &failed);
        self.show_status_message(&message);
        if !failed.is_empty() {
            self.flash_overlay(&message);
        }
    }

    /// Reconcile slideshow view state after a rotation write completes or fails.
    fn rotation_landed(&self, path: &Path, orientation: Orientation, saved: bool) {
        let mut show = self.ivars().show.borrow_mut();
        let Some(show) = show.as_mut() else { return };
        if let Some(in_flight) = show.saving.in_flight.get_mut(path) {
            *in_flight = orientation.inverse().then(*in_flight);
            if *in_flight == Orientation::UP {
                show.saving.in_flight.remove(path);
            }
        }
        if !saved {
            if let Some(state) = show.view_states.get(path) {
                show.saving.refused.insert(
                    path.to_path_buf(),
                    Orientation::from_view(state.rotation, state.flipped),
                );
            }
            return;
        }
        if let Some(state) = show.view_states.get(path).copied() {
            let rest = orientation
                .inverse()
                .then(Orientation::from_view(state.rotation, state.flipped));
            let next = SlideState {
                rotation: rest.degrees(),
                flipped: rest.mirrored,
                ..state
            };
            if next == SlideState::default() {
                show.view_states.remove(path);
            } else {
                show.view_states.insert(path.to_path_buf(), next);
            }
        }
    }

    /// Invalidate cached decodes and refresh the grid/slide for files edited by the app.
    fn files_edited(&self, touched: &[PathBuf]) {
        if let Some(engine) = self.ivars().engine.get() {
            engine.invalidate_slides();
        }
        let mut seen: HashSet<&Path> = HashSet::with_capacity(touched.len());
        let unique: Vec<PathBuf> = touched
            .iter()
            .filter(|path| seen.insert(path.as_path()))
            .cloned()
            .collect();
        let touched = unique.as_slice();
        let stale_header = self
            .ivars()
            .header_rows
            .borrow()
            .as_ref()
            .is_some_and(|(cached, _)| seen.contains(cached.as_path()));
        if stale_header {
            *self.ivars().header_rows.borrow_mut() = None;
        }
        let current_touched = {
            let mut show = self.ivars().show.borrow_mut();
            match show.as_mut() {
                Some(show) => {
                    for path in touched {
                        show.cache.remove(path);
                        show.saving.redecode.insert(path.clone());
                    }
                    show.playlist
                        .current()
                        .filter(|current| seen.contains(current.as_path()))
                        .cloned()
                }
                None => None,
            }
        };
        let changed = self.notice_files(touched);
        // `notice_files` returns paths that emitted `Event::FileChanged` (which already
        // called `show_current()`). Format-converting edits (remove+insert) and files
        // outside the active scan did not emit `FileChanged`, so refresh explicitly.
        if let Some(current) = current_touched {
            if !changed.contains(&current) {
                self.show_current();
            }
        }
        self.refresh_info();
    }

    /// Reconcile slideshow state when an edit replaces `orig` with `now` (baking orientation).
    fn reconcile_replaced_slide(&self, orig: &Path, now: &Path) {
        if now != orig {
            self.slide_renamed(orig, now);
        }
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.view_states.remove(now);
            show.saving.in_flight.remove(orig);
        }
    }

    pub(crate) fn queue_crop(&self, path: PathBuf, view: Orientation, crop: CropRect, replace: bool) {
        self.show_status_message("Cropping…");
        self.queue_edit(EditJob::Crop {
            path,
            view,
            crop,
            replace,
        });
    }

    fn crop_done(&self, path: PathBuf, result: Result<Cropped, String>) {
        self.ivars()
            .pending_edits
            .set(self.ivars().pending_edits.get().saturating_sub(1));
        let message = match result {
            Ok(Cropped::Replaced(edit)) => {
                let now = edit.path().clone();
                // Crop output already has the view rotation baked in.
                self.reconcile_replaced_slide(&path, &now);
                self.ivars()
                    .history
                    .borrow_mut()
                    .record("crop", Step::Edited(vec![edit]));
                self.files_edited(&[path, now.clone()]);
                format!("Cropped {}", file_name(&now))
            }
            Ok(Cropped::Copy(copy)) => {
                self.ivars()
                    .history
                    .borrow_mut()
                    .record("crop", Step::Copied(vec![copy.clone()]));
                self.notice_files(std::slice::from_ref(&copy));
                format!("Saved copy as {}", file_name(&copy))
            }
            Err(why) => format!("Could not crop {}: {why}", file_name(&path)),
        };
        self.refresh_history_items();
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    pub(crate) fn is_upscaling_or_comparing(&self) -> bool {
        self.ivars().upscaling.get()
            || self
                .ivars()
                .show
                .borrow()
                .as_ref()
                .is_some_and(|show| show.view.compare_view().is_some())
    }

    /// Prompt the user to choose a folder containing Core AI upscaling models.
    pub(crate) fn choose_models_folder(&self) -> Option<PathBuf> {
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setMessage(Some(ns_string!(
            "Choose a folder containing a Core AI upscaler (.aimodel), such as SwinIR-L."
        )));
        panel.setPrompt(Some(ns_string!("Choose")));
        if !self.run_panel(&panel) {
            return None;
        }
        let path = panel.URL().and_then(grants::remember)?;
        self.set_models_folder(Some(path.as_path()));
        Some(path)
    }

    /// Locate a usable Core AI model in the configured folder, prompting to pick one if needed.
    pub(crate) fn ensure_model_path(&self) -> Option<PathBuf> {
        if let Some(folder_str) = prefs::models_folder_pref() {
            let folder = PathBuf::from(&folder_str);
            if grants::readable(&folder) {
                if let Ok(model) = upscale::find_model(&folder) {
                    return Some(model);
                }
            }
        }

        let mtm = self.mtm();
        let alert = objc2_app_kit::NSAlert::new(mtm);
        alert.setAlertStyle(objc2_app_kit::NSAlertStyle::Informational);
        alert.setMessageText(ns_string!("Choose a models folder"));
        let info = match prefs::models_folder_pref() {
            Some(folder) => format!(
                "No Core AI upscaling model (.aimodel) could be read in:\n{folder}\n\nDownload SwinIR-L from huggingface.co/arraypress/upscale-swinir-l-real and choose the folder containing it."
            ),
            None => "Blickli runs upscaling locally using Core AI without network access.\n\nDownload SwinIR-L from huggingface.co/arraypress/upscale-swinir-l-real and choose the folder containing the .aimodel bundle.".to_owned(),
        };
        alert.setInformativeText(&NSString::from_str(&info));
        alert.addButtonWithTitle(ns_string!("Choose Folder…"));
        alert.addButtonWithTitle(ns_string!("Cancel"));
        if alert.runModal() != objc2_app_kit::NSAlertFirstButtonReturn {
            return None;
        }

        let folder = self.choose_models_folder()?;
        match upscale::find_model(&folder) {
            Ok(model) => Some(model),
            Err(why) => {
                let err_alert = objc2_app_kit::NSAlert::new(mtm);
                err_alert.setAlertStyle(objc2_app_kit::NSAlertStyle::Warning);
                err_alert.setMessageText(ns_string!("No Core AI model found"));
                err_alert.setInformativeText(&NSString::from_str(&format!(
                    "Could not use the selected folder: {why}."
                )));
                err_alert.addButtonWithTitle(ns_string!("OK"));
                err_alert.runModal();
                None
            }
        }
    }

    /// Entry point for Image -> Upscale… (toolbar, menu, context menu, and `u` shortcut).
    pub(crate) fn upscale_selection(&self) {
        if self.is_busy() {
            return;
        }
        let in_show = self.ivars().show.borrow().is_some();
        let save_mode = self.ivars().upscale_save_mode.get();
        let scale = self.ivars().upscale_scale.get();

        if in_show {
            if save_mode == UpscaleSaveMode::Ask {
                self.start_upscale_preview();
                return;
            }
            let ready = {
                let mut show_guard = self.ivars().show.borrow_mut();
                let Some(show) = show_guard.as_mut() else {
                    return;
                };
                if !show.is_current_displayed() || show.view.crop_view().is_some() {
                    return;
                }
                if show.view.is_animated() {
                    drop(show_guard);
                    self.flash_overlay("Animated images cannot be upscaled");
                    return;
                }
                let Some(path) = show.playlist.current().cloned() else {
                    return;
                };
                if let Some(timer) = show.timer.take() {
                    timer.invalidate();
                }
                path
            };
            let Some(model_path) = self.ensure_model_path() else {
                self.update_overlay();
                return;
            };
            self.save_slide_rotation_now(&ready);
            let target = {
                let show_guard = self.ivars().show.borrow();
                let Some(show) = show_guard.as_ref() else {
                    return;
                };
                if show.playlist.current() != Some(&ready) {
                    return;
                }
                let orient = Self::pending_view_orientation(show, &ready);
                (ready, orient)
            };
            let replace = save_mode == UpscaleSaveMode::Replace;
            self.queue_upscale_direct(vec![target], model_path, scale, replace);
            return;
        }

        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let Some(model_path) = self.ensure_model_path() else {
            return;
        };

        // Single image + Ask each time opens the slideshow directly into interactive preview.
        if targets.len() == 1 && save_mode == UpscaleSaveMode::Ask {
            self.start_from_grid();
            self.start_upscale_preview();
            return;
        }

        let replace = match save_mode {
            UpscaleSaveMode::Copy => false,
            UpscaleSaveMode::Replace => true,
            UpscaleSaveMode::Ask => {
                let mtm = self.mtm();
                let alert = objc2_app_kit::NSAlert::new(mtm);
                alert.setAlertStyle(objc2_app_kit::NSAlertStyle::Informational);
                alert.setMessageText(&NSString::from_str(&format!(
                    "Upscale {} images ({})?",
                    count_text(targets.len()),
                    scale.label()
                )));
                alert.setInformativeText(ns_string!(
                    "Save upscaled copies beside the originals, or replace the originals (Undo brings originals back from Trash)."
                ));
                alert.addButtonWithTitle(ns_string!("Save Copies"));
                alert.addButtonWithTitle(ns_string!("Replace Originals"));
                alert.addButtonWithTitle(ns_string!("Cancel"));
                let response = alert.runModal();
                if response == objc2_app_kit::NSAlertFirstButtonReturn {
                    false
                } else if response == objc2_app_kit::NSAlertSecondButtonReturn {
                    true
                } else {
                    return;
                }
            }
        };

        let items = targets
            .into_iter()
            .map(|path| (path, Orientation::UP))
            .collect();
        self.queue_upscale_direct(items, model_path, scale, replace);
    }

    pub(crate) fn queue_upscale_preview(
        &self,
        path: PathBuf,
        view: Orientation,
        model_path: PathBuf,
    ) {
        let cancel = Arc::new(AtomicBool::new(false));
        let id = self.ivars().upscale_job_id.get().wrapping_add(1);
        self.ivars().upscale_job_id.set(id);
        *self.ivars().upscale_cancel.borrow_mut() = Some(Arc::clone(&cancel));
        self.ivars().upscaling.set(true);
        self.ivars().upscaling_slide.set(true);
        self.update_status();
        let worker = self
            .ivars()
            .upscale_worker
            .get_or_init(spawn_upscale_worker);
        BACKGROUND_ACTIVE.fetch_add(1, Ordering::AcqRel);
        if worker
            .send(UpscaleJob::Preview {
                id,
                path,
                view,
                model_path,
                cancel,
            })
            .is_err()
        {
            BACKGROUND_ACTIVE.fetch_sub(1, Ordering::AcqRel);
            self.ivars().upscaling.set(false);
            self.ivars().upscaling_slide.set(false);
            *self.ivars().upscale_cancel.borrow_mut() = None;
            self.update_status();
        }
    }

    fn queue_upscale_direct(
        &self,
        paths: Vec<(PathBuf, Orientation)>,
        model_path: PathBuf,
        scale: UpscaleScale,
        replace: bool,
    ) {
        let cancel = Arc::new(AtomicBool::new(false));
        let id = self.ivars().upscale_job_id.get().wrapping_add(1);
        self.ivars().upscale_job_id.set(id);
        *self.ivars().upscale_cancel.borrow_mut() = Some(Arc::clone(&cancel));
        self.ivars().upscaling.set(true);
        self.ivars().upscaling_slide.set(false);
        self.update_status();
        let start_msg = if paths.len() == 1 {
            format!("Upscaling {}…", file_name(&paths[0].0))
        } else {
            format!("Upscaling {} images…", count_text(paths.len()))
        };
        self.show_status_message(&start_msg);
        self.flash_overlay(&start_msg);
        let worker = self
            .ivars()
            .upscale_worker
            .get_or_init(spawn_upscale_worker);
        BACKGROUND_ACTIVE.fetch_add(1, Ordering::AcqRel);
        if worker
            .send(UpscaleJob::Direct {
                id,
                paths,
                model_path,
                scale,
                replace,
                cancel,
            })
            .is_err()
        {
            BACKGROUND_ACTIVE.fetch_sub(1, Ordering::AcqRel);
            self.ivars().upscaling.set(false);
            *self.ivars().upscale_cancel.borrow_mut() = None;
            self.update_status();
        }
    }

    pub(crate) fn upscale_direct_progress(&self, job_id: u64, message: String) {
        if self.ivars().upscale_job_id.get() != job_id || !self.ivars().upscaling.get() {
            return;
        }
        self.show_status_message(&message);
    }

    pub(crate) fn upscale_direct_done(
        &self,
        job_id: u64,
        saved: Vec<(PathBuf, Cropped)>,
        failed: Vec<String>,
        cancelled: bool,
    ) {
        if self.ivars().upscale_job_id.get() != job_id {
            return;
        }
        self.ivars().upscaling.set(false);
        *self.ivars().upscale_cancel.borrow_mut() = None;

        let mut edits = Vec::new();
        let mut copies = Vec::new();
        let mut touched = Vec::new();
        let mut done_paths = Vec::new();
        for (orig, outcome) in saved {
            match outcome {
                Cropped::Replaced(edit) => {
                    let now = edit.path().clone();
                    self.reconcile_replaced_slide(&orig, &now);
                    touched.push(orig);
                    touched.push(now.clone());
                    done_paths.push(now);
                    edits.push(edit);
                }
                Cropped::Copy(copy) => {
                    touched.push(copy.clone());
                    done_paths.push(copy.clone());
                    copies.push(copy);
                }
            }
        }
        if !edits.is_empty() {
            self.ivars()
                .history
                .borrow_mut()
                .record("upscale", Step::Edited(edits));
            self.files_edited(&touched);
        } else if !copies.is_empty() {
            self.ivars()
                .history
                .borrow_mut()
                .record("upscale", Step::Copied(copies));
            self.notice_files(&touched);
        }
        self.refresh_history_items();
        self.update_status();

        let message = if cancelled && done_paths.is_empty() {
            "Upscale cancelled".to_owned()
        } else {
            edit_message("Upscaled", &done_paths, &failed)
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    pub(crate) fn queue_save_upscaled(&self, path: PathBuf, image: Img, replace: bool) {
        self.show_status_message("Saving upscaled image…");
        self.queue_edit(EditJob::SaveUpscaled {
            path,
            image,
            replace,
        });
    }

    fn upscale_save_done(&self, path: PathBuf, result: Result<Cropped, String>) {
        self.ivars()
            .pending_edits
            .set(self.ivars().pending_edits.get().saturating_sub(1));
        let message = match result {
            Ok(Cropped::Replaced(edit)) => {
                let now = edit.path().clone();
                self.reconcile_replaced_slide(&path, &now);
                self.ivars()
                    .history
                    .borrow_mut()
                    .record("upscale", Step::Edited(vec![edit]));
                self.files_edited(&[path, now.clone()]);
                format!("Upscaled {}", file_name(&now))
            }
            Ok(Cropped::Copy(copy)) => {
                self.ivars()
                    .history
                    .borrow_mut()
                    .record("upscale", Step::Copied(vec![copy.clone()]));
                self.notice_files(std::slice::from_ref(&copy));
                format!("Saved upscaled copy as {}", file_name(&copy))
            }
            Err(why) => format!("Could not save upscaled {}: {why}", file_name(&path)),
        };
        self.refresh_history_items();
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// Update slideshow references when a format-converting edit renames a file.
    fn slide_renamed(&self, from: &Path, to: &Path) {
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.playlist.rename(from, to);
            show.cache.remove(&from.to_path_buf());
            if let Some(state) = show.view_states.remove(from) {
                show.view_states.insert(to.to_path_buf(), state);
            }
            if show.on_screen.as_deref() == Some(from) {
                show.on_screen = Some(to.to_path_buf());
            }
            if show.displayed_path.as_deref() == Some(from) {
                show.displayed_path = Some(to.to_path_buf());
            }
        }
    }

    pub(crate) fn take_back_edits(&self, label: String, edits: Vec<Edit>, undoing: bool) {
        let mut next = Vec::with_capacity(edits.len());
        let mut touched = Vec::new();
        let mut failed = Vec::new();
        for item in edits {
            match item {
                Edit::Oriented {
                    path,
                    before,
                    after,
                } => match edit::write_orientation(&path, if undoing { before } else { after }) {
                    Ok(()) => {
                        touched.push(path.clone());
                        next.push(Edit::Oriented {
                            path,
                            before,
                            after,
                        });
                    }
                    Err(why) => failed.push(why),
                },
                Edit::Replaced {
                    current,
                    stashed,
                    home,
                } => match edit::swap_back(&current, &stashed, &home) {
                    Ok(swapped) => {
                        if current != home {
                            self.slide_renamed(&current, &home);
                        }
                        touched.push(current);
                        touched.push(home);
                        next.push(swapped);
                    }
                    Err(why) => failed.push(why),
                },
            }
        }
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            for path in &touched {
                show.view_states.remove(path);
            }
        }
        self.files_edited(&touched);
        let count = next.len();
        {
            let mut history = self.ivars().history.borrow_mut();
            let entry = Entry {
                label: label.clone(),
                step: Step::Edited(next),
            };
            if undoing {
                history.push_undone(entry);
            } else {
                history.push_done(entry);
            }
        }
        self.refresh_history_items();
        let (verb, failed_verb) = if undoing {
            ("Undid", "undo")
        } else {
            ("Redid", "redo")
        };
        let message = match (count, failed.first()) {
            (_, Some(why)) => format!("Could not {failed_verb} {label}: {why}"),
            (1, None) => format!("{verb} {label}"),
            (n, None) => format!("{verb} {label} ({n} images)"),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    pub(crate) fn is_compressing(&self) -> bool {
        self.ivars().compressing.get()
    }

    pub(crate) fn cancel_compress(&self) -> bool {
        if !self.ivars().compressing.get() {
            return false;
        }
        if let Some(cancel) = self.ivars().compress_cancel.borrow().as_ref() {
            cancel.store(true, Ordering::Release);
        }
        self.show_status_message("Cancelling compression…");
        true
    }

    /// Entry point for Image -> Compress… (toolbar, menu, grid/slide context menu).
    pub(crate) fn compress_selection(&self) {
        if self.is_busy() {
            return;
        }
        let in_show = self.ivars().show.borrow().is_some();
        if in_show {
            let current = {
                let mut show_guard = self.ivars().show.borrow_mut();
                let Some(show) = show_guard.as_mut() else {
                    return;
                };
                if !show.is_current_displayed()
                    || show.view.crop_view().is_some()
                    || show.view.compare_view().is_some()
                {
                    return;
                }
                if show.view.is_animated() {
                    drop(show_guard);
                    self.flash_overlay("Animated images cannot be compressed");
                    return;
                }
                if let Some(timer) = show.timer.take() {
                    timer.invalidate();
                }
                show.playlist.current().cloned()
            };
            let Some(path) = current else { return };
            // Flush any pending slide rotation before reading from disk.
            self.save_slide_rotation_now(&path);
            self.run_compress_dialog(&CompressScope::Selection(vec![path]));
            self.update_overlay();
            return;
        }

        let selected = self.action_targets();
        if !selected.is_empty() {
            self.run_compress_dialog(&CompressScope::Selection(selected));
        } else if let Some((root, recursive)) = self.current_scan() {
            self.compress_folder(root, recursive);
        }
    }

    /// Entry point for Image -> Compress Folder… (current browsed folder).
    pub(crate) fn compress_current_folder(&self) {
        if self.is_busy() {
            return;
        }
        if let Some((root, recursive)) = self.current_scan() {
            self.compress_folder(root, recursive);
        }
    }

    /// Compress a folder (called from sidebar context menu or Image -> Compress Folder…).
    pub fn compress_folder(&self, root: PathBuf, initial_recursive: bool) {
        if self.is_busy() {
            return;
        }
        if !grants::readable(&root) {
            self.show_status_message("Cannot read selected folder");
            return;
        }
        // Reuse the grid's already-scanned inventory when compressing the active folder.
        let preloaded = if self.current_scan() == Some((root.clone(), initial_recursive)) {
            self.ivars()
                .grid
                .get()
                .filter(|grid| !grid.scanning())
                .map(|grid| grid.file_sizes())
        } else {
            None
        };
        self.run_compress_dialog(&CompressScope::Folder {
            root,
            initial_recursive,
            preloaded,
        });
    }

    fn run_compress_dialog(&self, scope: &CompressScope) {
        let initial = self.ivars().compress_options.get();
        let Some((options, paths)) = compress_ui::run_dialog(self.mtm(), scope, initial) else {
            return;
        };
        self.set_compress_options(options);
        if paths.is_empty() {
            return;
        }
        self.queue_compress(paths, options);
    }

    fn queue_compress(&self, paths: Vec<PathBuf>, options: CompressOptions) {
        let cancel = Arc::new(AtomicBool::new(false));
        let id = self.ivars().compress_job_id.get().wrapping_add(1);
        self.ivars().compress_job_id.set(id);
        *self.ivars().compress_cancel.borrow_mut() = Some(Arc::clone(&cancel));
        self.ivars().compressing.set(true);
        self.update_status();
        let start_msg = if paths.len() == 1 {
            format!("Compressing {}…", file_name(&paths[0]))
        } else {
            format!("Compressing {} images…", count_text(paths.len()))
        };
        self.show_status_message(&start_msg);
        self.flash_overlay(&start_msg);
        let worker = self
            .ivars()
            .compress_worker
            .get_or_init(spawn_compress_worker);
        BACKGROUND_ACTIVE.fetch_add(1, Ordering::AcqRel);
        if worker
            .send(CompressJob {
                id,
                paths,
                options,
                cancel,
            })
            .is_err()
        {
            BACKGROUND_ACTIVE.fetch_sub(1, Ordering::AcqRel);
            self.ivars().compressing.set(false);
            *self.ivars().compress_cancel.borrow_mut() = None;
            self.update_status();
        }
    }

    pub(crate) fn compress_progress(&self, job_id: u64, progress: BatchProgress) {
        if self.ivars().compress_job_id.get() != job_id || !self.ivars().compressing.get() {
            return;
        }
        let msg = if progress.total == 1 {
            "Compressing 1 image…".to_owned()
        } else if progress.saved_bytes > 0 {
            format!(
                "Compressing {} of {}… (saved {})",
                count_text(progress.done),
                count_text(progress.total),
                format_bytes(progress.saved_bytes)
            )
        } else {
            format!(
                "Compressing {} of {}…",
                count_text(progress.done),
                count_text(progress.total)
            )
        };
        self.show_status_message(&msg);
    }

    pub(crate) fn compress_done(&self, job_id: u64, result: BatchResult) {
        if self.ivars().compress_job_id.get() != job_id {
            return;
        }
        self.ivars().compressing.set(false);
        *self.ivars().compress_cancel.borrow_mut() = None;

        let mut touched = Vec::new();
        for (orig, edit) in result.orig_paths.iter().zip(result.edits.iter()) {
            let now = edit.path();
            // Compression operates on disk files without baking unpersisted view rotations,
            // so preserve `view_states` (renaming the key when format conversion changes the path).
            if now != orig {
                self.slide_renamed(orig, now);
            }
            touched.push(orig.clone());
            if now != orig {
                touched.push(now.clone());
            }
        }

        let compressed_count = result.edits.len();
        let first_name = result
            .edits
            .first()
            .map(|e| file_name(e.path()))
            .unwrap_or_default();
        if !result.edits.is_empty() {
            self.ivars()
                .history
                .borrow_mut()
                .record("compress", Step::Edited(result.edits));
            self.files_edited(&touched);
        }
        self.refresh_history_items();
        self.update_status();

        let saved = result.orig_bytes.saturating_sub(result.final_bytes);
        let mut message = if result.cancelled && compressed_count == 0 {
            "Compression cancelled".to_owned()
        } else if compressed_count == 0 {
            match result.failed.first() {
                Some(why) => format!("Could not compress: {why}"),
                None => "No images needed compression".to_owned(),
            }
        } else if compressed_count == 1 {
            format!(
                "Compressed {first_name} — saved {} ({} → {})",
                format_bytes(saved),
                format_bytes(result.orig_bytes),
                format_bytes(result.final_bytes)
            )
        } else {
            format!(
                "Compressed {} images — saved {} ({} → {})",
                count_text(compressed_count),
                format_bytes(saved),
                format_bytes(result.orig_bytes),
                format_bytes(result.final_bytes)
            )
        };
        if result.over_limit > 0 {
            message.push_str(&format!(", {} still above cap", result.over_limit));
        }
        if !result.failed.is_empty() && compressed_count > 0 {
            message.push_str(&format!(
                ", {} failed: {}",
                result.failed.len(),
                result.failed[0]
            ));
        }
        if result.cancelled && compressed_count > 0 {
            message.push_str(" (cancelled)");
        }
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }
}
