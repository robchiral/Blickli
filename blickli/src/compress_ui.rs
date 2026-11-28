//! Modal options dialog for compressing selected images or an entire folder.

use std::cell::{Cell, OnceCell, RefCell};
use std::path::PathBuf;

use blickli_core::messages::{folder_label, format_bytes};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSButton, NSColor, NSFont, NSPopUpButton,
    NSTextField, NSView,
};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_foundation::{ns_string, NSObject, NSObjectProtocol, NSRect, NSString};

use crate::compress::{
    self, CompressOptions, FallbackFormat, MaxDimensions, FALLBACK_PRESETS, MAX_DIMS_PRESETS,
    TARGET_KB_PRESETS,
};
use crate::info::count_text;

pub enum CompressScope {
    Selection(Vec<PathBuf>),
    Folder {
        root: PathBuf,
        initial_recursive: bool,
        /// Optional pre-scanned `(path, size)` pairs for `initial_recursive` (avoids re-walking).
        preloaded: Option<Vec<(PathBuf, u64)>>,
    },
}

struct ScopeInventory {
    /// For Selection: explicit inspected files. For Folder: flat (non-recursive) files.
    flat: RefCell<Option<Vec<(PathBuf, u64)>>>,
    /// For Folder: recursive files (including subfolders).
    recursive: RefCell<Option<Vec<(PathBuf, u64)>>>,
    folder_root: Option<PathBuf>,
    /// Original selection count before filtering to supported formats (`None` for Folder scope).
    selected_count: Option<usize>,
}

impl ScopeInventory {
    fn from_scope(scope: &CompressScope) -> Self {
        match scope {
            CompressScope::Selection(paths) => Self {
                flat: RefCell::new(Some(compress::inspect_paths(paths))),
                recursive: RefCell::new(None),
                folder_root: None,
                selected_count: Some(paths.len()),
            },
            CompressScope::Folder {
                root,
                initial_recursive,
                preloaded,
            } => {
                let pre = preloaded.as_ref().map(|files| {
                    files
                        .iter()
                        .filter(|(p, _)| compress::is_supported_path(p))
                        .cloned()
                        .collect()
                });
                let (flat, recursive) = if *initial_recursive {
                    (None, pre)
                } else {
                    (pre, None)
                };
                let inv = Self {
                    flat: RefCell::new(flat),
                    recursive: RefCell::new(recursive),
                    folder_root: Some(root.clone()),
                    selected_count: None,
                };
                // Pre-populate the initial mode immediately so the dialog shows live stats.
                inv.ensure_loaded(*initial_recursive);
                inv
            }
        }
    }

    fn ensure_loaded(&self, recursive: bool) {
        let Some(root) = &self.folder_root else {
            return;
        };
        let slot = if recursive {
            &self.recursive
        } else {
            &self.flat
        };
        if slot.borrow().is_none() {
            // If recursive files are already loaded, filter top-level entries in memory instead of re-walking.
            let found = if !recursive {
                if let Some(all) = self.recursive.borrow().as_ref() {
                    all.iter()
                        .filter(|(p, _)| p.parent() == Some(root.as_path()))
                        .cloned()
                        .collect()
                } else {
                    compress::collect_folder_candidates(root, false, crate::sidebar::is_package)
                }
            } else {
                compress::collect_folder_candidates(root, true, crate::sidebar::is_package)
            };
            *slot.borrow_mut() = Some(found);
        }
    }

    fn with_files<R>(&self, recursive: bool, f: impl FnOnce(&[(PathBuf, u64)]) -> R) -> R {
        let slot = if self.folder_root.is_some() {
            self.ensure_loaded(recursive);
            if recursive {
                &self.recursive
            } else {
                &self.flat
            }
        } else {
            &self.flat
        };
        let guard = slot.borrow();
        f(guard.as_deref().unwrap_or(&[]))
    }
}

pub struct DialogIvars {
    inventory: OnceCell<ScopeInventory>,
    is_folder: Cell<bool>,
    recursive: Cell<bool>,
    options: Cell<CompressOptions>,
    summary_label: OnceCell<Retained<NSTextField>>,
    compress_button: OnceCell<Retained<NSButton>>,
    target_popup: OnceCell<Retained<NSPopUpButton>>,
    dims_popup: OnceCell<Retained<NSPopUpButton>>,
    fallback_popup: OnceCell<Retained<NSPopUpButton>>,
    subfolders_check: OnceCell<Retained<NSButton>>,
}

impl Default for DialogIvars {
    fn default() -> Self {
        Self {
            inventory: OnceCell::new(),
            is_folder: Cell::new(false),
            recursive: Cell::new(true),
            options: Cell::new(CompressOptions::default()),
            summary_label: OnceCell::new(),
            compress_button: OnceCell::new(),
            target_popup: OnceCell::new(),
            dims_popup: OnceCell::new(),
            fallback_popup: OnceCell::new(),
            subfolders_check: OnceCell::new(),
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliCompressDialog"]
    #[ivars = DialogIvars]
    struct CompressDialogController;

    unsafe impl NSObjectProtocol for CompressDialogController {}

    impl CompressDialogController {
        #[unsafe(method(targetChanged:))]
        fn target_changed(&self, _sender: Option<&AnyObject>) {
            let mut opts = self.ivars().options.get();
            if let Some(popup) = self.ivars().target_popup.get() {
                opts.target_kb = compress::normalize_target_kb(popup.selectedTag());
            }
            self.ivars().options.set(opts);
            self.refresh_summary();
        }

        #[unsafe(method(dimsChanged:))]
        fn dims_changed(&self, _sender: Option<&AnyObject>) {
            let mut opts = self.ivars().options.get();
            if let Some(popup) = self.ivars().dims_popup.get() {
                opts.max_dims = MaxDimensions::from_tag(popup.selectedTag());
            }
            self.ivars().options.set(opts);
        }

        #[unsafe(method(fallbackChanged:))]
        fn fallback_changed(&self, _sender: Option<&AnyObject>) {
            let mut opts = self.ivars().options.get();
            if let Some(popup) = self.ivars().fallback_popup.get() {
                opts.fallback = FallbackFormat::from_tag(popup.selectedTag());
            }
            self.ivars().options.set(opts);
        }

        #[unsafe(method(subfoldersToggled:))]
        fn subfolders_toggled(&self, _sender: Option<&AnyObject>) {
            if let Some(check) = self.ivars().subfolders_check.get() {
                self.ivars().recursive.set(check.state() != 0);
            }
            self.refresh_summary();
        }
    }
);

impl CompressDialogController {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DialogIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn oversized_paths(&self) -> Vec<PathBuf> {
        let Some(inv) = self.ivars().inventory.get() else {
            return Vec::new();
        };
        let limit = self.ivars().options.get().max_bytes();
        inv.with_files(self.ivars().recursive.get(), |all| {
            all.iter()
                .filter(|(_, size)| *size > limit)
                .map(|(path, _)| path.clone())
                .collect()
        })
    }

    fn refresh_summary(&self) {
        let Some(inv) = self.ivars().inventory.get() else {
            return;
        };
        let opts = self.ivars().options.get();
        let limit = opts.max_bytes();
        let limit_text = format_bytes(limit);
        let (total, over_count, over_bytes, first_size) =
            inv.with_files(self.ivars().recursive.get(), |all| {
                let mut over_count = 0usize;
                let mut over_bytes = 0u64;
                for &(_, size) in all {
                    if size > limit {
                        over_count += 1;
                        over_bytes += size;
                    }
                }
                (all.len(), over_count, over_bytes, all.first().map(|&(_, s)| s).unwrap_or(0))
            });

        let single_selection = inv.selected_count == Some(1);
        let text = if total == 0 {
            if self.ivars().is_folder.get() {
                "No supported images found in this folder.".to_owned()
            } else {
                "No supported images in the current selection.".to_owned()
            }
        } else if total == 1 && single_selection {
            if over_count == 1 {
                format!(
                    "Image is {} (exceeds {limit_text} target).",
                    format_bytes(over_bytes)
                )
            } else {
                format!(
                    "Already {} (under {limit_text}) — choose a smaller target to compress.",
                    format_bytes(first_size)
                )
            }
        } else if over_count == 0 {
            format!(
                "All {} supported images are already under {limit_text}.",
                count_text(total)
            )
        } else if over_count == 1 {
            format!(
                "1 of {} supported images exceeds {limit_text} ({}).",
                count_text(total),
                format_bytes(over_bytes)
            )
        } else {
            format!(
                "{} of {} supported images exceed {limit_text} ({} total).",
                count_text(over_count),
                count_text(total),
                format_bytes(over_bytes)
            )
        };

        if let Some(label) = self.ivars().summary_label.get() {
            label.setStringValue(&NSString::from_str(&text));
        }
        if let Some(button) = self.ivars().compress_button.get() {
            button.setEnabled(over_count > 0);
            let title = match over_count {
                0 => "Compress".to_owned(),
                1 => "Compress 1 Image".to_owned(),
                n => format!("Compress {} Images", count_text(n)),
            };
            button.setTitle(&NSString::from_str(&title));
        }
    }
}

/// Show the modal Compress dialog and return `(chosen_options, oversized_paths)` if confirmed.
pub fn run_dialog(
    mtm: MainThreadMarker,
    scope: &CompressScope,
    initial: CompressOptions,
) -> Option<(CompressOptions, Vec<PathBuf>)> {
    let controller = CompressDialogController::new(mtm);
    let is_folder = matches!(scope, CompressScope::Folder { .. });
    let initial_recursive = match scope {
        CompressScope::Folder {
            initial_recursive, ..
        } => *initial_recursive,
        CompressScope::Selection(_) => false,
    };
    let inventory = ScopeInventory::from_scope(scope);
    let _ = controller.ivars().inventory.set(inventory);
    controller.ivars().is_folder.set(is_folder);
    controller.ivars().recursive.set(initial_recursive);
    controller.ivars().options.set(initial);

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    let title = match scope {
        CompressScope::Folder { root, .. } => format!("Compress “{}”", folder_label(root)),
        CompressScope::Selection(paths) if paths.len() == 1 => {
            format!("Compress “{}”", crate::info::file_name(&paths[0]))
        }
        CompressScope::Selection(paths) => {
            format!("Compress {} selected images", count_text(paths.len()))
        }
    };
    alert.setMessageText(&NSString::from_str(&title));
    alert.setInformativeText(ns_string!(
        "Keeps high quality by scaling oversized images before lowering quality. Originals are moved to Trash so Undo (⌘Z) restores them."
    ));
    let compress_btn = alert.addButtonWithTitle(ns_string!("Compress"));
    alert.addButtonWithTitle(ns_string!("Cancel"));
    let _ = controller.ivars().compress_button.set(compress_btn);

    const WIDTH: f64 = 380.0;
    const POPUP_W: f64 = 220.0;
    let height = if is_folder { 164.0 } else { 136.0 };
    let accessory = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, height)),
    );
    let rect = |x, y, w, h| NSRect::new(CGPoint::new(x, y), CGSize::new(w, h));
    let y = Cell::new(height);
    let row = |h: f64, gap: f64| {
        let top = y.get();
        y.set(top - h - gap);
        top - h
    };

    let summary = NSTextField::wrappingLabelWithString(ns_string!(""), mtm);
    summary.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.23)));
    summary.setTextColor(Some(&NSColor::labelColor()));
    summary.setFrame(rect(0.0, row(30.0, 8.0), WIDTH, 30.0));
    accessory.addSubview(&summary);
    let _ = controller.ivars().summary_label.set(summary);

    let make_popup = |title: &str,
                      action: objc2::runtime::Sel,
                      entries: &[(&str, isize)],
                      selected_tag: isize|
     -> Retained<NSPopUpButton> {
        let top = row(26.0, 6.0);
        let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
        label.setFrame(rect(0.0, top + 4.0, WIDTH - POPUP_W - 8.0, 18.0));
        accessory.addSubview(&label);

        let popup = NSPopUpButton::new(mtm);
        popup.setFrame(rect(WIDTH - POPUP_W, top, POPUP_W, 26.0));
        for &(caption, tag) in entries {
            popup.addItemWithTitle(&NSString::from_str(caption));
            if let Some(item) = popup.lastItem() {
                item.setTag(tag);
            }
        }
        popup.selectItemWithTag(selected_tag);
        unsafe {
            popup.setTarget(Some(&*controller));
            popup.setAction(Some(action));
        }
        accessory.addSubview(&popup);
        popup
    };

    let target_popup = make_popup(
        "Target max size",
        sel!(targetChanged:),
        &TARGET_KB_PRESETS,
        initial.target_kb,
    );
    let _ = controller.ivars().target_popup.set(target_popup);

    let dims_popup = make_popup(
        "Max dimensions",
        sel!(dimsChanged:),
        &MAX_DIMS_PRESETS,
        initial.max_dims.tag(),
    );
    let _ = controller.ivars().dims_popup.set(dims_popup);

    let fallback_popup = make_popup(
        "Format fallback",
        sel!(fallbackChanged:),
        &FALLBACK_PRESETS,
        initial.fallback.tag(),
    );
    let _ = controller.ivars().fallback_popup.set(fallback_popup);

    if is_folder {
        let check = unsafe {
            NSButton::checkboxWithTitle_target_action(
                ns_string!("Include subfolders"),
                Some(&*controller),
                Some(sel!(subfoldersToggled:)),
                mtm,
            )
        };
        check.setState(if initial_recursive { 1 } else { 0 });
        check.sizeToFit();
        let top = row(22.0, 2.0);
        check.setFrame(rect(WIDTH - POPUP_W, top, check.frame().size.width, 22.0));
        accessory.addSubview(&check);
        let _ = controller.ivars().subfolders_check.set(check);
    }

    controller.refresh_summary();
    alert.setAccessoryView(Some(&accessory));

    let response = alert.runModal();
    if response != NSAlertFirstButtonReturn {
        return None;
    }

    let paths = controller.oversized_paths();
    if paths.is_empty() {
        return None;
    }
    Some((controller.ivars().options.get(), paths))
}
