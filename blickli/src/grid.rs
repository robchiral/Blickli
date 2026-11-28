//! Virtualized thumbnail grid view, selection, filtering, and drag-and-drop.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use blickli_core::transfer::Transfer;
use blickli_core::{
    file_info_cmp, merge_sorted_by, sort_files, FileInfo, LruCache, SortOrder, ThumbnailRequest,
};

use objc2::rc::Retained;
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSImage, NSMenu, NSMenuItem, NSPasteboardItem,
    NSPasteboardTypeFileURL, NSView,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    ns_string, NSArray, NSDictionary, NSObjectProtocol, NSRect, NSString, NSURL,
};

use crate::{image_cost, menu_item, ns_image, AppDelegate, Img};

fn drag_operation(outside: bool, copies_outside: bool) -> NSDragOperation {
    if outside && copies_outside {
        NSDragOperation::Copy
    } else {
        NSDragOperation::Move | NSDragOperation::Copy
    }
}

pub const CELL: f64 = 160.0;
const PAD: f64 = 8.0;
const MIN_CELL: f64 = 60.0;
/// Maximum thumbnail size options offered in Settings.
pub const CELL_CAPS: [f64; 3] = [160.0, 320.0, 512.0];
const DRAG_SLOP: f64 = 4.0;
const LABEL_H: f64 = 16.0;
const THUMB_CACHE_BYTES: usize = 256 * 1024 * 1024;

struct CachedThumbnail {
    image: Retained<NSImage>,
    max_px: i32,
}

pub struct GridIvars {
    /// Filtered view of `all` (shares `Rc` pointers so pointer identity matches files).
    pub files: RefCell<Vec<Rc<FileInfo>>>,
    /// All images in the current folder, sorted by the active order.
    all: RefCell<Vec<Rc<FileInfo>>>,
    /// Lowercased filename substring filter.
    filter: RefCell<String>,
    cell: Cell<f64>,
    cell_cap: Cell<f64>,
    labels: Cell<bool>,
    thumbs: RefCell<LruCache<PathBuf, CachedThumbnail>>,
    requested: RefCell<HashMap<PathBuf, (u64, i32)>>,
    failed: RefCell<HashSet<PathBuf>>,
    next_request: Cell<u64>,
    thumbnail_region: Cell<Option<(CellRange, i32)>>,
    scanning: Cell<bool>,
    filename_style: OnceCell<Retained<NSDictionary<NSString, objc2::runtime::AnyObject>>>,
    /// Selected indices into `files`.
    selected: RefCell<BTreeSet<usize>>,
    focus: Cell<Option<usize>>,
    anchor: Cell<Option<usize>>,
    /// Rubber-band drag `(origin, current)`.
    band: Cell<Option<(CGPoint, CGPoint)>>,
    /// Selection snapshot at the start of a ⌘-rubber-band drag.
    band_base: RefCell<BTreeSet<usize>>,
    /// Mouse-down point on a cell, pending drag threshold.
    press: Cell<Option<CGPoint>>,
    /// Plain click on an already-selected multi-selection item; collapses on mouse-up if not dragged.
    pending_collapse: Cell<Option<usize>>,
    pub delegate: OnceCell<Retained<AppDelegate>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliGridView"]
    #[ivars = GridIvars]
    pub struct GridView;

    unsafe impl NSObjectProtocol for GridView {}

    unsafe impl NSDraggingSource for GridView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn dragging_source_operation_mask(
            &self,
            _session: &NSDraggingSession,
            context: NSDraggingContext,
        ) -> NSDragOperation {
            let copies = self
                .ivars()
                .delegate
                .get()
                .is_some_and(|delegate| delegate.drag_out_copies());
            drag_operation(context == NSDraggingContext::OutsideApplication, copies)
        }
    }

    impl GridView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: CGSize) {
            // The scroll view sets the width; the grid sets its height.
            let height = self.content_height(size.width);
            let _: () = unsafe {
                msg_send![super(self), setFrameSize: CGSize::new(size.width, height)]
            };
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            NSColor::windowBackgroundColor().setFill();
            objc2_app_kit::NSRectFill(dirty);

            let files = self.ivars().files.borrow();
            self.request_visible_thumbnails(&files);
            if files.is_empty() {
                let message = if self.ivars().scanning.get() {
                    ns_string!("Loading…")
                } else if self.filtering() {
                    ns_string!("No matching images")
                } else {
                    ns_string!("No images")
                };
                let visible = self.visibleRect();
                let label = CGRect::new(
                    CGPoint::new(
                        visible.origin.x,
                        visible.origin.y + (visible.size.height * 0.42 - 12.0).max(24.0),
                    ),
                    CGSize::new(visible.size.width, 24.0),
                );
                self.draw_message(message, label);
                return;
            }
            let cells = self.cell_range(dirty, files.len(), 3.0);
            let selected = self.ivars().selected.borrow();
            let labels = self.ivars().labels.get();
            let mut thumbs = self.ivars().thumbs.borrow_mut();
            let highlight = if self.window().is_some_and(|window| window.isKeyWindow()) {
                NSColor::selectedContentBackgroundColor()
            } else {
                NSColor::unemphasizedSelectedContentBackgroundColor()
            };
            let placeholder = NSColor::quaternaryLabelColor();

            for index in cells.indices(files.len()) {
                let cell = self.cell_rect(index, cells.columns);
                if selected.contains(&index) {
                    highlight.setFill();
                    let border = CGRect::new(
                        CGPoint::new(cell.origin.x - 3.0, cell.origin.y - 3.0),
                        CGSize::new(cell.size.width + 6.0, cell.size.height + 6.0),
                    );
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(border, 7.0, 7.0).fill();
                }
                let path = &files[index].path;
                if let Some(thumb) = thumbs.get(path) {
                    draw_thumbnail(&thumb.image, cell);
                } else {
                    placeholder.setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(cell, 4.0, 4.0).fill();
                }
                if labels {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let rect = CGRect::new(
                        CGPoint::new(cell.origin.x, cell.origin.y + cell.size.height + 1.0),
                        CGSize::new(cell.size.width, LABEL_H - 2.0),
                    );
                    self.draw_label(&NSString::from_str(&name), rect);
                }
            }
            if let Some((a, b)) = self.ivars().band.get() {
                let rect = band_rect(a, b);
                NSColor::selectedContentBackgroundColor()
                    .colorWithAlphaComponent(0.25)
                    .setFill();
                objc2_app_kit::NSRectFillUsingOperation(
                    rect,
                    objc2_app_kit::NSCompositingOperation::SourceOver,
                );
                NSColor::selectedContentBackgroundColor()
                    .colorWithAlphaComponent(0.75)
                    .setStroke();
                NSBezierPath::strokeRect(rect);
            }
        }

        #[unsafe(method(magnifyWithEvent:))]
        fn magnify_with_event(&self, event: &NSEvent) {
            self.scale_cells(1.0 + event.magnification());
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let flags = event.modifierFlags();
            let cmd = flags.contains(objc2_app_kit::NSEventModifierFlags::Command);
            let shift = flags.contains(objc2_app_kit::NSEventModifierFlags::Shift);

            let Some(index) = self.index_at(point) else {
                // Empty space starts a rubber band; cmd toggles relative to the prior selection.
                if cmd {
                    *self.ivars().band_base.borrow_mut() = self.ivars().selected.borrow().clone();
                } else {
                    self.ivars().band_base.borrow_mut().clear();
                    self.ivars().selected.borrow_mut().clear();
                    self.ivars().focus.set(None);
                    self.ivars().anchor.set(None);
                }
                self.ivars().press.set(None);
                self.ivars().pending_collapse.set(None);
                self.ivars().band.set(Some((point, point)));
                self.setNeedsDisplay(true);
                self.notify_selection();
                return;
            };

            self.ivars().pending_collapse.set(None);
            if shift {
                let anchor = self.ivars().anchor.get().unwrap_or(index);
                let (lo, hi) = (anchor.min(index), anchor.max(index));
                let mut selected = self.ivars().selected.borrow_mut();
                selected.clear();
                selected.extend(lo..=hi);
                self.ivars().focus.set(Some(index));
            } else if cmd {
                let mut selected = self.ivars().selected.borrow_mut();
                if selected.remove(&index) {
                    let fallback = selected.iter().next_back().copied();
                    self.ivars().focus.set(fallback);
                    self.ivars().anchor.set(fallback);
                } else {
                    selected.insert(index);
                    self.ivars().focus.set(Some(index));
                    self.ivars().anchor.set(Some(index));
                }
            } else {
                let defer_collapse = {
                    let selected = self.ivars().selected.borrow();
                    event.clickCount() == 1 && selected.len() > 1 && selected.contains(&index)
                };
                if defer_collapse {
                    self.ivars().pending_collapse.set(Some(index));
                } else {
                    let mut selected = self.ivars().selected.borrow_mut();
                    selected.clear();
                    selected.insert(index);
                    self.ivars().anchor.set(Some(index));
                }
                self.ivars().focus.set(Some(index));
            }
            self.ivars().press.set(Some(point));
            if self.ivars().pending_collapse.get().is_none() {
                self.setNeedsDisplay(true);
                self.notify_selection();
            }
            if event.clickCount() >= 2 && !cmd && !shift {
                self.activate();
            }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let Some(index) = self.index_at(point) else { return };
            let menu = self.context_click(index);
            NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some((origin, _)) = self.ivars().band.get() else {
                // A press that travels far enough drags the selection out.
                let Some(start) = self.ivars().press.get() else { return };
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                if (point.x - start.x).abs() < DRAG_SLOP && (point.y - start.y).abs() < DRAG_SLOP {
                    return;
                }
                self.ivars().press.set(None);
                self.ivars().pending_collapse.set(None);
                self.begin_drag(event);
                return;
            };
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.ivars().band.set(Some((origin, point)));
            let changed = self.select_band(band_rect(origin, point));
            self.autoscroll(event);
            self.setNeedsDisplay(true);
            if changed {
                self.notify_selection();
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().press.set(None);
            if let Some(index) = self.ivars().pending_collapse.take() {
                if index < self.ivars().files.borrow().len() {
                    let mut selected = self.ivars().selected.borrow_mut();
                    selected.clear();
                    selected.insert(index);
                    drop(selected);
                    self.ivars().anchor.set(Some(index));
                    self.ivars().focus.set(Some(index));
                    self.setNeedsDisplay(true);
                    self.notify_selection();
                }
            }
            if self.ivars().band.get().is_some() {
                self.ivars().band.set(None);
                self.ivars().band_base.borrow_mut().clear();
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(selectAll:))]
        fn select_all_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.select_all();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let has_mod = event.modifierFlags().intersects(
                objc2_app_kit::NSEventModifierFlags::Command
                    | objc2_app_kit::NSEventModifierFlags::Control
                    | objc2_app_kit::NSEventModifierFlags::Option,
            );
            if has_mod {
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            }
            let count = self.ivars().files.borrow().len();
            let cols = self.columns(self.bounds().size.width) as i64;
            let delta: i64 = match event.keyCode() {
                36 | 76 => {
                    // return / numpad enter
                    if self.ivars().focus.get().is_some()
                        || !self.ivars().selected.borrow().is_empty()
                    {
                        self.activate();
                    }
                    return;
                }
                53 => {
                    // escape
                    if let Some(delegate) = self.ivars().delegate.get() {
                        delegate.escape_pressed();
                    }
                    return;
                }
                123 => -1,    // left
                124 => 1,     // right
                126 => -cols, // up
                125 => cols,  // down
                _ => {
                    let _: () = unsafe { msg_send![super(self), keyDown: event] };
                    return;
                }
            };
            if count == 0 {
                return;
            }
            let next = match self.ivars().focus.get() {
                Some(focus) => (focus as i64 + delta).clamp(0, count as i64 - 1) as usize,
                None => 0,
            };
            {
                let mut selected = self.ivars().selected.borrow_mut();
                selected.clear();
                selected.insert(next);
            }
            self.ivars().focus.set(Some(next));
            self.ivars().anchor.set(Some(next));
            self.setNeedsDisplay(true);
            self.scrollRectToVisible(self.cell_rect(next, cols as usize));
            self.notify_selection();
        }
    }
);

fn filename_style() -> Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> {
    use objc2_app_kit::{
        NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSLineBreakMode,
        NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment,
    };
    let style = NSMutableParagraphStyle::new();
    style.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    style.setAlignment(NSTextAlignment::Center);
    let font = NSFont::systemFontOfSize(11.0);
    let color = NSColor::secondaryLabelColor();
    unsafe {
        NSDictionary::from_slices(
            &[
                NSFontAttributeName,
                NSForegroundColorAttributeName,
                NSParagraphStyleAttributeName,
            ],
            &[
                font.as_ref() as &objc2::runtime::AnyObject,
                color.as_ref(),
                style.as_ref(),
            ],
        )
    }
}

fn draw_thumbnail(image: &NSImage, cell: CGRect) {
    let size = image.size();
    if size.width > 0.0 && size.height > 0.0 {
        let scale = (cell.size.width / size.width)
            .min(cell.size.height / size.height)
            .min(1.0);
        let w = size.width * scale;
        let h = size.height * scale;
        image.drawInRect(CGRect::new(
            CGPoint::new(
                cell.origin.x + (cell.size.width - w) / 2.0,
                cell.origin.y + (cell.size.height - h) / 2.0,
            ),
            CGSize::new(w, h),
        ));
    }
}

/// Case-insensitive filename match (`filter` must be pre-lowercased).
fn matches_filter(info: &FileInfo, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let Some(name) = info.path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    // Avoid allocating a lowercased string per file on ASCII filenames.
    if name.is_ascii() && filter.is_ascii() {
        return name
            .as_bytes()
            .windows(filter.len())
            .any(|window| window.eq_ignore_ascii_case(filter.as_bytes()));
    }
    name.to_lowercase().contains(filter)
}

pub struct Selection {
    pub count: usize,
    pub bytes: u64,
    pub first: Option<Rc<FileInfo>>,
}

fn band_rect(a: CGPoint, b: CGPoint) -> CGRect {
    CGRect::new(
        CGPoint::new(a.x.min(b.x), a.y.min(b.y)),
        CGSize::new((a.x - b.x).abs(), (a.y - b.y).abs()),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellRange {
    columns: usize,
    left: usize,
    right: usize,
    top: usize,
    bottom: usize,
}

impl CellRange {
    fn indices(self, count: usize) -> impl Iterator<Item = usize> {
        (self.top..self.bottom)
            .flat_map(move |row| (self.left..self.right).map(move |col| row * self.columns + col))
            .take_while(move |&index| index < count)
    }
}

fn tile_span(start: f64, extent: f64, pitch: f64, count: usize, margin: f64) -> (usize, usize) {
    let first = ((start - PAD - margin) / pitch).floor() as usize;
    let end = ((start + extent - PAD + margin) / pitch).ceil() as usize;
    (first.min(count), end.min(count))
}

fn thumbnail_pixel_size(points: f64, backing_scale: f64) -> i32 {
    // Quantize to 64px buckets so pinch-zooming reuses cached thumbnails.
    let pixels = (points * backing_scale).ceil() as u32;
    pixels.max(1).next_multiple_of(64) as i32
}

impl GridView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(GridIvars {
            files: RefCell::new(Vec::new()),
            all: RefCell::new(Vec::new()),
            filter: RefCell::new(String::new()),
            cell: Cell::new(CELL),
            cell_cap: Cell::new(CELL_CAPS[1]),
            labels: Cell::new(false),
            thumbs: RefCell::new(LruCache::new(THUMB_CACHE_BYTES)),
            requested: RefCell::new(HashMap::new()),
            failed: RefCell::new(HashSet::new()),
            next_request: Cell::new(0),
            thumbnail_region: Cell::new(None),
            scanning: Cell::new(false),
            filename_style: OnceCell::new(),
            selected: RefCell::new(BTreeSet::new()),
            focus: Cell::new(None),
            anchor: Cell::new(None),
            band: Cell::new(None),
            band_base: RefCell::new(BTreeSet::new()),
            press: Cell::new(None),
            pending_collapse: Cell::new(None),
            delegate: OnceCell::new(),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// A larger line for an empty grid.
    fn draw_message(&self, text: &NSString, rect: CGRect) {
        use objc2_app_kit::{
            NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMutableParagraphStyle,
            NSParagraphStyleAttributeName, NSStringDrawing, NSTextAlignment,
        };
        let style = NSMutableParagraphStyle::new();
        style.setAlignment(NSTextAlignment::Center);
        let font = NSFont::systemFontOfSize_weight(15.0, 0.0);
        let color = NSColor::tertiaryLabelColor();
        let attributes = unsafe {
            NSDictionary::from_slices(
                &[
                    NSFontAttributeName,
                    NSForegroundColorAttributeName,
                    NSParagraphStyleAttributeName,
                ],
                &[
                    font.as_ref() as &objc2::runtime::AnyObject,
                    color.as_ref(),
                    style.as_ref(),
                ],
            )
        };
        unsafe { text.drawInRect_withAttributes(rect, Some(&attributes)) };
    }

    fn draw_label(&self, text: &NSString, rect: CGRect) {
        use objc2_app_kit::NSStringDrawing;
        let style = self.ivars().filename_style.get_or_init(filename_style);
        unsafe { text.drawInRect_withAttributes(rect, Some(style)) };
    }

    fn cell_range(&self, rect: CGRect, count: usize, margin: f64) -> CellRange {
        let columns = self.columns(self.bounds().size.width);
        let (left, right) = tile_span(
            rect.origin.x,
            rect.size.width,
            self.column_pitch(),
            columns,
            margin,
        );
        let (top, bottom) = tile_span(
            rect.origin.y,
            rect.size.height,
            self.row_pitch(),
            count.div_ceil(columns),
            margin,
        );
        CellRange {
            columns,
            left,
            right,
            top,
            bottom,
        }
    }

    /// Request thumbnails on screen, then a screen's worth below and above.
    fn request_visible_thumbnails(&self, files: &[Rc<FileInfo>]) {
        let Some(delegate) = self.ivars().delegate.get() else {
            return;
        };
        let visible = self.cell_range(self.visibleRect(), files.len(), 0.0);
        let backing = self
            .window()
            .map_or(1.0, |window| window.backingScaleFactor());
        let max_px = thumbnail_pixel_size(self.cell_size(), backing);
        let region = Some((visible, max_px));
        if self.ivars().thumbnail_region.replace(region) == region {
            return;
        }
        let rows = visible.bottom - visible.top;
        let all_rows = files.len().div_ceil(visible.columns);
        let full_rows = |top: usize, bottom: usize| CellRange {
            left: 0,
            right: visible.columns,
            top,
            bottom,
            ..visible
        };
        let below = full_rows(visible.bottom, (visible.bottom + rows).min(all_rows));
        let above = full_rows(visible.top.saturating_sub(rows), visible.top);
        let mut thumbs = self.ivars().thumbs.borrow_mut();
        let failed = self.ivars().failed.borrow();
        let mut requested = self.ivars().requested.borrow_mut();
        let mut previous = std::mem::take(&mut *requested);
        let requests = visible
            .indices(files.len())
            .chain(below.indices(files.len()))
            .chain(above.indices(files.len()))
            .filter_map(|index| {
                let path = &files[index].path;
                if thumbs.get(path).is_some_and(|thumb| thumb.max_px >= max_px)
                    || failed.contains(path)
                {
                    return None;
                }
                let (id, max_px) = previous
                    .remove(path)
                    .filter(|&(_, pixels)| pixels >= max_px)
                    .unwrap_or_else(|| {
                        let id = self.ivars().next_request.get();
                        self.ivars().next_request.set(id + 1);
                        (id, max_px)
                    });
                requested.insert(path.clone(), (id, max_px));
                Some(ThumbnailRequest {
                    id,
                    max_px,
                    path: path.clone(),
                })
            })
            .collect();
        delegate.request_thumbnails(requests);
    }

    /// The cell under a point; its label strip counts as part of it.
    fn index_at(&self, point: CGPoint) -> Option<usize> {
        let cols = self.columns(self.bounds().size.width);
        let col = ((point.x - PAD) / self.column_pitch()).floor();
        let row = ((point.y - PAD) / self.row_pitch()).floor();
        if col < 0.0 || row < 0.0 || col >= cols as f64 {
            return None;
        }
        // Hits in the gutter don't count.
        let cell = self.ivars().cell.get();
        let hit_h = cell
            + if self.ivars().labels.get() {
                LABEL_H
            } else {
                0.0
            };
        let in_x = point.x - PAD - col * self.column_pitch() - self.column_inset();
        let in_y = point.y - PAD - row * self.row_pitch();
        if in_x < 0.0 || in_x > cell || in_y > hit_h {
            return None;
        }
        let index = row as usize * cols + col as usize;
        (index < self.ivars().files.borrow().len()).then_some(index)
    }

    fn select_band(&self, rect: CGRect) -> bool {
        let count = self.ivars().files.borrow().len();
        let cells = self.cell_range(rect, count, 0.0);
        let mut next = self.ivars().band_base.borrow().clone();
        for index in cells.indices(count) {
            let cell = self.cell_rect(index, cells.columns);
            let intersects = cell.origin.x < rect.origin.x + rect.size.width
                && rect.origin.x < cell.origin.x + cell.size.width
                && cell.origin.y < rect.origin.y + rect.size.height
                && rect.origin.y < cell.origin.y + cell.size.height;
            if intersects && !next.remove(&index) {
                next.insert(index);
            }
        }
        let mut selected = self.ivars().selected.borrow_mut();
        if *selected == next {
            return false;
        }
        *selected = next;
        let current_focus = self.ivars().focus.get();
        if !current_focus.is_some_and(|f| selected.contains(&f)) {
            let fallback = selected.first().copied();
            self.ivars().focus.set(fallback);
            self.ivars().anchor.set(fallback);
        }
        true
    }

    fn pitch(&self) -> f64 {
        self.ivars().cell.get() + PAD
    }

    /// Column pitch distributing leftover horizontal width evenly across columns.
    fn column_pitch(&self) -> f64 {
        let width = self.bounds().size.width;
        let columns = self.columns(width) as f64;
        ((width - PAD) / columns).max(self.pitch())
    }

    fn column_inset(&self) -> f64 {
        (self.column_pitch() - self.pitch()) / 2.0
    }

    fn row_pitch(&self) -> f64 {
        self.pitch()
            + if self.ivars().labels.get() {
                LABEL_H
            } else {
                0.0
            }
    }

    pub fn labels_visible(&self) -> bool {
        self.ivars().labels.get()
    }

    pub fn set_labels(&self, visible: bool) {
        self.ivars().labels.set(visible);
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
    }

    pub fn scale_cells(&self, factor: f64) {
        self.set_cell_size(self.ivars().cell.get() * factor);
    }

    pub fn cell_size(&self) -> f64 {
        self.ivars().cell.get()
    }

    pub fn cell_cap(&self) -> f64 {
        self.ivars().cell_cap.get()
    }

    /// Set the cell size within the cap; returns the size used.
    pub fn set_cell_size(&self, size: f64) -> f64 {
        let next = size.clamp(MIN_CELL, self.ivars().cell_cap.get());
        if next == self.ivars().cell.get() {
            return next;
        }
        let visible = self.visibleRect();
        let width = self.frame().size.width;
        let old_pitch = self.row_pitch();
        let old_row = ((visible.origin.y - PAD) / old_pitch).floor() as usize;
        let first = old_row * self.columns(width);
        self.ivars().cell.set(next);
        self.setFrameSize(CGSize::new(width, 0.0));
        if visible.origin.y > 0.0 {
            let row = first / self.columns(width);
            let offset = visible.origin.y - old_row as f64 * old_pitch;
            self.scrollPoint(CGPoint::new(
                visible.origin.x,
                row as f64 * self.row_pitch() + offset,
            ));
        }
        self.setNeedsDisplay(true);
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.thumb_size_changed(next);
        }
        next
    }

    pub fn set_cell_cap(&self, cap: f64) {
        self.ivars().cell_cap.set(cap.max(MIN_CELL));
        self.set_cell_size(self.ivars().cell.get());
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.ivars()
            .files
            .borrow()
            .iter()
            .map(|f| f.path.clone())
            .collect()
    }

    /// Re-sort on every core, keeping the selection on its files.
    pub fn resort(&self, order: SortOrder, descending: bool) {
        sort_files(&mut self.ivars().all.borrow_mut(), |a, b| {
            file_info_cmp(a, b, order, descending)
        });
        self.rebuild_visible();
    }

    pub fn paths_missing_dates(&self, order: SortOrder) -> Vec<PathBuf> {
        self.ivars()
            .all
            .borrow()
            .iter()
            .filter(|info| info.missing_dates_for(order))
            .map(|info| info.path.clone())
            .collect()
    }

    /// Merge updated file dates into `all` in one pass instead of re-sorting everything.
    pub fn apply_file_dates(&self, files: &[FileInfo], order: SortOrder, descending: bool) {
        let updates: HashMap<&Path, &FileInfo> = files
            .iter()
            .map(|info| (info.path.as_path(), info))
            .collect();
        let mut dated = Vec::new();
        let mut swapped: HashMap<*const FileInfo, Rc<FileInfo>> = HashMap::new();
        {
            let mut all = self.ivars().all.borrow_mut();
            all.retain(|slot| {
                let Some(update) = updates.get(slot.path.as_path()) else {
                    return true;
                };
                // Ignore stale metadata if the file changed since the read started.
                if slot.modified != update.modified || slot.size != update.size {
                    return true;
                }
                let merged = match (slot.dates, update.dates) {
                    (Some(existing), Some(incoming)) => Some(existing.merge(incoming)),
                    (existing, incoming) => existing.or(incoming),
                };
                if merged == slot.dates {
                    return true;
                }
                let new = Rc::new(FileInfo {
                    dates: merged,
                    ..FileInfo::clone(slot)
                });
                swapped.insert(Rc::as_ptr(slot), Rc::clone(&new));
                dated.push(new);
                false
            });
            if dated.is_empty() {
                return;
            }
            let cmp = |a: &FileInfo, b: &FileInfo| file_info_cmp(a, b, order, descending);
            sort_files(&mut dated, cmp);
            merge_sorted_by(&mut all, dated, |a, b| cmp(a, b));
        }
        for slot in self.ivars().files.borrow_mut().iter_mut() {
            if let Some(new) = swapped.get(&Rc::as_ptr(slot)) {
                *slot = Rc::clone(new);
            }
        }
        if order.needs_file_dates() {
            self.rebuild_visible();
        }
    }

    pub fn set_filter(&self, text: &str) {
        *self.ivars().filter.borrow_mut() = text.to_lowercase();
        self.rebuild_visible();
    }

    pub fn filtering(&self) -> bool {
        !self.ivars().filter.borrow().is_empty()
    }

    /// (shown, in the folder); they differ while filtering.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.ivars().files.borrow().len(),
            self.ivars().all.borrow().len(),
        )
    }

    /// Rebuild the filtered `files` list while preserving selection, focus, and anchor by pointer identity.
    fn rebuild_visible(&self) {
        {
            let (selected, focus, anchor) = {
                let files = self.ivars().files.borrow();
                let key = |index: usize| files.get(index).map(Rc::as_ptr);
                let mut selected: Vec<*const FileInfo> = self
                    .ivars()
                    .selected
                    .borrow()
                    .iter()
                    .filter_map(|&index| key(index))
                    .collect();
                selected.sort_unstable();
                (
                    selected,
                    self.ivars().focus.get().and_then(key),
                    self.ivars().anchor.get().and_then(key),
                )
            };
            let filter = self.ivars().filter.borrow();
            let visible: Vec<Rc<FileInfo>> = self
                .ivars()
                .all
                .borrow()
                .iter()
                .filter(|info| matches_filter(info, &filter))
                .cloned()
                .collect();
            let mut new_selected = Vec::with_capacity(selected.len());
            let (mut new_focus, mut new_anchor) = (None, None);
            if !selected.is_empty() || focus.is_some() || anchor.is_some() {
                for (index, info) in visible.iter().enumerate() {
                    let key = Rc::as_ptr(info);
                    if selected.binary_search(&key).is_ok() {
                        new_selected.push(index);
                    }
                    if focus == Some(key) {
                        new_focus = Some(index);
                    }
                    if anchor == Some(key) {
                        new_anchor = Some(index);
                    }
                }
            }
            *self.ivars().selected.borrow_mut() = new_selected.into_iter().collect();
            self.ivars().focus.set(new_focus);
            // The anchor keeps its own file rather than snapping to the focus.
            self.ivars().anchor.set(new_anchor.or(new_focus));
            self.ivars().pending_collapse.set(None);
            *self.ivars().files.borrow_mut() = visible;
        }
        let width = self.frame().size.width;
        self.ivars().thumbnail_region.set(None);
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    fn columns(&self, width: f64) -> usize {
        (((width - PAD) / self.pitch()).floor() as usize).max(1)
    }

    fn content_height(&self, width: f64) -> f64 {
        let count = self.ivars().files.borrow().len();
        let cols = self.columns(width);
        let rows = count.div_ceil(cols).max(1);
        rows as f64 * self.row_pitch() + PAD
    }

    fn cell_rect(&self, index: usize, cols: usize) -> CGRect {
        let row = index / cols;
        let col = index % cols;
        CGRect::new(
            CGPoint::new(
                PAD + col as f64 * self.column_pitch() + self.column_inset(),
                PAD + row as f64 * self.row_pitch(),
            ),
            CGSize::new(self.ivars().cell.get(), self.ivars().cell.get()),
        )
    }

    /// Selection count, total size, and first file, without copying.
    pub fn selection(&self) -> Selection {
        let files = self.ivars().files.borrow();
        let selected = self.ivars().selected.borrow();
        Selection {
            count: selected.len(),
            bytes: selected
                .iter()
                .filter_map(|&index| files.get(index))
                .map(|info| info.size)
                .sum(),
            first: selected
                .first()
                .and_then(|&index| files.get(index))
                .cloned(),
        }
    }

    pub fn scanning(&self) -> bool {
        self.ivars().scanning.get()
    }

    /// Each path's index in the grid, in the order given. One pass however many
    /// paths.
    fn places(&self, paths: &[PathBuf]) -> Vec<Option<usize>> {
        let wanted: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let mut found: HashMap<&Path, usize> = HashMap::with_capacity(wanted.len());
        for (index, info) in self.ivars().files.borrow().iter().enumerate() {
            if let Some(&path) = wanted.get(info.path.as_path()) {
                found.entry(path).or_insert(index);
            }
        }
        paths
            .iter()
            .map(|path| found.get(path.as_path()).copied())
            .collect()
    }

    fn notify_selection(&self) {
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.selection_changed();
        }
    }

    fn activate(&self) {
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.start_from_grid();
        }
    }

    /// Return `(paths, start_index, keep_first)` for starting a slideshow:
    /// 0–1 selected plays all visible files from the selection; 2+ selected plays only the selection.
    pub fn slideshow_request(&self) -> (Vec<PathBuf>, usize, bool) {
        let selected = self.ivars().selected.borrow();
        if selected.len() >= 2 {
            let files = self.ivars().files.borrow();
            let valid: Vec<usize> = selected
                .iter()
                .copied()
                .filter(|&i| i < files.len())
                .collect();
            let paths: Vec<PathBuf> = valid.iter().map(|&i| files[i].path.clone()).collect();
            let start = self
                .ivars()
                .focus
                .get()
                .and_then(|f| valid.iter().position(|&i| i == f))
                .unwrap_or(0);
            (paths, start, false)
        } else {
            let start = selected.first().copied().unwrap_or(0);
            (self.paths(), start, selected.len() == 1)
        }
    }

    pub fn insert_files(
        &self,
        inserts: Vec<(usize, FileInfo)>,
        order: SortOrder,
        descending: bool,
    ) {
        {
            let mut all = self.ivars().all.borrow_mut();
            if order == SortOrder::Name && !descending && inserts_fit_in_order(&all, &inserts) {
                // Fast O(k) path when `all` is in sync with the engine's name-sorted list.
                blickli_core::apply_inserts(&mut all, inserts, Rc::new);
            } else {
                let cmp = |a: &FileInfo, b: &FileInfo| file_info_cmp(a, b, order, descending);
                let mut batch: Vec<Rc<FileInfo>> =
                    inserts.into_iter().map(|(_, info)| Rc::new(info)).collect();
                sort_files(&mut batch, cmp);
                merge_sorted_by(&mut all, batch, |a, b| cmp(a, b));
            }
        }
        self.rebuild_visible();
    }

    /// Remove deleted paths from the grid. No-op if already removed so status messages persist.
    pub fn remove_paths(&self, paths: &[PathBuf]) {
        let going: HashSet<&PathBuf> = paths.iter().collect();
        let removed = {
            let mut all = self.ivars().all.borrow_mut();
            let before = all.len();
            all.retain(|f| !going.contains(&f.path));
            before - all.len()
        };
        if removed > 0 {
            let mut thumbs = self.ivars().thumbs.borrow_mut();
            let mut requested = self.ivars().requested.borrow_mut();
            let mut failed = self.ivars().failed.borrow_mut();
            for path in paths {
                thumbs.remove(path);
                requested.remove(path);
                failed.remove(path);
            }
        }
        if removed == 0 {
            return;
        }
        self.rebuild_visible();
        let selected = self.ivars().selected.borrow();
        if !self
            .ivars()
            .focus
            .get()
            .is_some_and(|f| selected.contains(&f))
        {
            let fallback = selected.first().copied();
            self.ivars().focus.set(fallback);
            self.ivars().anchor.set(fallback);
        }
    }

    /// Select `index` if right-clicked outside the current selection (Finder behavior),
    /// then build the context menu.
    pub fn context_click(&self, index: usize) -> Retained<NSMenu> {
        let outside = !self.ivars().selected.borrow().contains(&index);
        if outside {
            {
                let mut selected = self.ivars().selected.borrow_mut();
                selected.clear();
                selected.insert(index);
            }
            self.ivars().anchor.set(Some(index));
            self.ivars().focus.set(Some(index));
            self.setNeedsDisplay(true);
            self.notify_selection();
        }
        self.context_menu(false)
    }

    /// Context menu for grid items or the active slide (`in_show` omits "Start Slideshow").
    pub fn context_menu(&self, in_show: bool) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let delegate = self.ivars().delegate.get();
        let repeat = |kind: Transfer| match delegate {
            Some(delegate) => delegate.repeat_title(kind),
            None => String::new(),
        };
        let entries: [Option<(String, Sel)>; 19] = [
            Some(("Start Slideshow".to_owned(), sel!(startSlideshow:))),
            None,
            Some(("Reveal in Finder".to_owned(), sel!(revealInFinder:))),
            Some(("Get Info".to_owned(), sel!(getInfo:))),
            Some(("Copy".to_owned(), sel!(copy:))),
            Some(("Copy Path".to_owned(), sel!(copyPath:))),
            None,
            Some(("Rotate Left".to_owned(), sel!(rotateLeft:))),
            Some(("Rotate Right".to_owned(), sel!(rotateRight:))),
            Some(("Crop…".to_owned(), sel!(cropImage:))),
            Some(("Upscale…".to_owned(), sel!(upscaleImage:))),
            Some(("Compress…".to_owned(), sel!(compressImage:))),
            None,
            Some(("Move to…".to_owned(), sel!(moveToFolder:))),
            Some(("Copy to…".to_owned(), sel!(copyToFolder:))),
            Some((repeat(Transfer::Move), sel!(moveAgain:))),
            Some((repeat(Transfer::Copy), sel!(copyAgain:))),
            None,
            Some(("Move to Trash".to_owned(), sel!(moveToTrash:))),
        ];
        let menu = NSMenu::new(mtm);
        let skip = usize::from(in_show) * 2;
        for entry in entries.into_iter().skip(skip) {
            let Some((title, action)) = entry else {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            let item = menu_item(mtm, &NSString::from_str(&title), action, ns_string!(""));
            if let Some(delegate) = delegate {
                unsafe { item.setTarget(Some(delegate)) };
            }
            menu.addItem(&item);
            if action == sel!(revealInFinder:) {
                if let Some(delegate) = delegate {
                    let open_with = NSMenuItem::new(mtm);
                    open_with.setTitle(ns_string!("Open With"));
                    open_with.setSubmenu(Some(&delegate.lazy_open_with_menu(mtm)));
                    menu.addItem(&open_with);
                }
            }
        }
        menu
    }

    fn begin_drag(&self, event: &NSEvent) {
        let cols = self.columns(self.bounds().size.width);
        let selected: Vec<usize> = self.ivars().selected.borrow().iter().copied().collect();
        let mut items = Vec::with_capacity(selected.len());
        for index in selected {
            let Some(path) = self
                .ivars()
                .files
                .borrow()
                .get(index)
                .map(|info| info.path.clone())
            else {
                continue;
            };
            let Some(url) = NSURL::from_file_path(&path) else {
                continue;
            };
            let entry = NSPasteboardItem::new();
            unsafe {
                entry.setString_forType(
                    &url.absoluteString().unwrap_or_default(),
                    NSPasteboardTypeFileURL,
                )
            };
            let item = NSDraggingItem::initWithPasteboardWriter(
                NSDraggingItem::alloc(),
                ProtocolObject::from_ref(&*entry),
            );
            let thumb = self
                .ivars()
                .thumbs
                .borrow_mut()
                .get(&path)
                .map(|thumb| thumb.image.clone());
            unsafe {
                item.setDraggingFrame_contents(
                    self.cell_rect(index, cols),
                    thumb.as_deref().map(|image| image.as_ref()),
                )
            };
            items.push(item);
        }
        if items.is_empty() {
            return;
        }
        self.beginDraggingSessionWithItems_event_source(
            &NSArray::from_retained_slice(&items),
            event,
            ProtocolObject::from_ref(self),
        );
    }

    pub fn selected_count(&self) -> usize {
        self.ivars().selected.borrow().len()
    }

    pub fn has_selection(&self) -> bool {
        !self.ivars().selected.borrow().is_empty()
    }

    /// Snapshot of all scanned `(path, size)` pairs in the active folder.
    pub fn file_sizes(&self) -> Vec<(PathBuf, u64)> {
        self.ivars()
            .all
            .borrow()
            .iter()
            .map(|f| (f.path.clone(), f.size))
            .collect()
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        let files = self.ivars().files.borrow();
        self.ivars()
            .selected
            .borrow()
            .iter()
            .filter_map(|&i| files.get(i).map(|f| f.path.clone()))
            .collect()
    }

    /// Remove paths and advance selection to the following item.
    pub fn remove_and_advance(&self, paths: &[PathBuf]) {
        let follower = {
            let mut indices: Vec<usize> = self.places(paths).into_iter().flatten().collect();
            indices.sort_unstable();
            indices.dedup();
            indices
                .last()
                .map(|&last| (last + 1).saturating_sub(indices.len()))
        };
        self.remove_paths(paths);
        let count = self.ivars().files.borrow().len();
        let Some(follower) = follower.filter(|_| count > 0) else {
            self.notify_selection();
            return;
        };
        let index = follower.min(count - 1);
        {
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            selected.insert(index);
        }
        self.ivars().focus.set(Some(index));
        self.ivars().anchor.set(Some(index));
        let cols = self.columns(self.bounds().size.width);
        self.scrollRectToVisible(self.cell_rect(index, cols));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    pub fn select_all(&self) {
        let count = self.ivars().files.borrow().len();
        if count == 0 {
            return;
        }
        *self.ivars().selected.borrow_mut() = (0..count).collect();
        self.ivars().focus.set(Some(0));
        self.ivars().anchor.set(Some(0));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    pub fn holds_all(&self, paths: &[PathBuf]) -> bool {
        self.places(paths).iter().all(Option::is_some)
    }

    /// Select these files and focus the first found.
    pub fn select_paths(&self, paths: &[PathBuf]) {
        let indices: Vec<usize> = self.places(paths).into_iter().flatten().collect();
        {
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            selected.extend(indices.iter().copied());
        }
        let first = indices.first().copied();
        self.ivars().focus.set(first);
        self.ivars().anchor.set(first);
        if let Some(index) = first {
            let cols = self.columns(self.bounds().size.width);
            self.scrollRectToVisible(self.cell_rect(index, cols));
        }
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    /// Right-click menu titles and states, for the e2e harness.
    #[cfg(any(test, feature = "e2e"))]
    pub fn e2e_context_menu(&self, index: usize) -> Vec<(String, bool)> {
        let menu = self.context_click(index);
        menu.update();
        menu.itemArray()
            .iter()
            .map(|item| (item.title().to_string(), item.isEnabled()))
            .collect()
    }

    #[cfg(any(test, feature = "e2e"))]
    pub fn e2e_set_selected(&self, indices: &[usize]) {
        // A late watcher event can shorten the grid; fail a check, don't crash.
        let count = self.ivars().files.borrow().len();
        let indices: Vec<usize> = indices.iter().copied().filter(|&i| i < count).collect();
        {
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            selected.extend(indices.iter().copied());
        }
        self.ivars().focus.set(indices.first().copied());
        self.ivars().anchor.set(indices.first().copied());
        self.notify_selection();
    }

    /// Update metadata, clear cached thumbnails, and reposition if the sort key changed.
    pub fn file_changed(&self, info: &FileInfo, order: SortOrder, descending: bool) {
        let info = Rc::new(info.clone());
        let cmp = |a: &Rc<FileInfo>, b: &Rc<FileInfo>| file_info_cmp(a, b, order, descending);
        let moved = {
            let mut all = self.ivars().all.borrow_mut();
            let Some(at) = all.iter().position(|f| f.path == info.path) else {
                return;
            };
            all[at] = Rc::clone(&info);
            let in_order = (at == 0 || cmp(&all[at - 1], &all[at]).is_lt())
                && (at + 1 == all.len() || cmp(&all[at], &all[at + 1]).is_lt());
            if !in_order {
                let item = all.remove(at);
                let pos = all.partition_point(|other| cmp(other, &item).is_lt());
                all.insert(pos, item);
            }
            !in_order
        };
        let mut visible_index = None;
        for (index, slot) in self.ivars().files.borrow_mut().iter_mut().enumerate() {
            if slot.path == info.path {
                *slot = Rc::clone(&info);
                visible_index = Some(index);
                break;
            }
        }
        self.ivars().thumbs.borrow_mut().remove(&info.path);
        self.ivars().requested.borrow_mut().remove(&info.path);
        self.ivars().failed.borrow_mut().remove(&info.path);
        self.ivars().thumbnail_region.set(None);
        if moved {
            self.rebuild_visible();
        } else {
            self.redraw_thumbnail(&info.path);
            if visible_index.is_some_and(|i| self.ivars().selected.borrow().contains(&i)) {
                self.notify_selection();
            }
        }
    }

    fn redraw_thumbnail(&self, path: &Path) -> bool {
        let files = self.ivars().files.borrow();
        let visible = self.cell_range(self.visibleRect(), files.len(), 0.0);
        let Some(index) = visible
            .indices(files.len())
            .find(|&index| files[index].path == path)
        else {
            return false;
        };
        self.setNeedsDisplayInRect(self.cell_rect(index, visible.columns));
        true
    }

    pub fn update_thumbnail(&self, request: ThumbnailRequest, image: Option<Img>) {
        let mut requested = self.ivars().requested.borrow_mut();
        if requested.get(&request.path) != Some(&(request.id, request.max_px)) {
            return;
        }
        requested.remove(&request.path);
        drop(requested);
        let Some(image) = image else {
            self.ivars().failed.borrow_mut().insert(request.path);
            return;
        };
        let cost = image_cost(&image);
        let thumb = CachedThumbnail {
            image: ns_image(&image),
            max_px: request.max_px,
        };
        self.redraw_thumbnail(&request.path);
        let evicted = self
            .ivars()
            .thumbs
            .borrow_mut()
            .insert(request.path, thumb, cost);
        for path in evicted {
            if self.redraw_thumbnail(&path) {
                self.ivars().thumbnail_region.set(None);
            }
        }
    }

    pub fn finish_scan(&self) {
        self.ivars().scanning.set(false);
        if self.ivars().files.borrow().is_empty() {
            self.setNeedsDisplay(true);
        }
    }

    pub fn reset(&self) {
        self.ivars().files.borrow_mut().clear();
        self.ivars().all.borrow_mut().clear();
        self.ivars().filter.borrow_mut().clear();
        self.ivars().thumbs.borrow_mut().clear();
        self.ivars().requested.borrow_mut().clear();
        self.ivars().failed.borrow_mut().clear();
        self.ivars().thumbnail_region.set(None);
        self.ivars().scanning.set(true);
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.request_thumbnails(Vec::new());
        }
        self.ivars().selected.borrow_mut().clear();
        self.ivars().focus.set(None);
        self.ivars().anchor.set(None);
        self.ivars().band.set(None);
        self.ivars().band_base.borrow_mut().clear();
        self.ivars().press.set(None);
        self.ivars().pending_collapse.set(None);
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }
}

/// Verify that engine-reported post-insert indices fit `all` in ascending name order
/// (guards against out-of-band `remove_and_advance` shrinking `all` before `FilesInserted` drains).
fn inserts_fit_in_order(all: &[Rc<FileInfo>], inserts: &[(usize, FileInfo)]) -> bool {
    let mut prev_idx: Option<usize> = None;
    for (k, (final_idx, info)) in inserts.iter().enumerate() {
        if prev_idx.is_some_and(|p| *final_idx <= p) || *final_idx < k {
            return false;
        }
        prev_idx = Some(*final_idx);
        let orig_pos = *final_idx - k;
        if orig_pos > all.len() {
            return false;
        }
        if orig_pos > 0
            && !file_info_cmp(&all[orig_pos - 1], info, SortOrder::Name, false).is_lt()
        {
            return false;
        }
        if orig_pos < all.len()
            && !file_info_cmp(info, &all[orig_pos], SortOrder::Name, false).is_lt()
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{
        drag_operation, inserts_fit_in_order, thumbnail_pixel_size, tile_span, CellRange, FileInfo,
    };
    use objc2_app_kit::NSDragOperation;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn info(name: &str) -> FileInfo {
        FileInfo {
            path: PathBuf::from(format!("/tmp/{name}")),
            modified: std::time::UNIX_EPOCH,
            size: 0,
            dates: None,
        }
    }

    #[test]
    fn inserts_fit_rejects_stale_indices_after_out_of_band_removal() {
        let all = vec![Rc::new(info("b.jpg")), Rc::new(info("d.jpg"))];
        // Normal insert between b and d.
        assert!(inserts_fit_in_order(&all, &[(1, info("c.jpg"))]));
        // Stale index past end after out-of-band removal.
        assert!(!inserts_fit_in_order(&all, &[(3, info("e.jpg"))]));
        // Stale index that would place c after d.
        assert!(!inserts_fit_in_order(&all, &[(2, info("c.jpg"))]));
    }

    #[test]
    fn a_drag_out_copies_only_when_the_preference_says_so() {
        let both = NSDragOperation::Move | NSDragOperation::Copy;
        assert_eq!(drag_operation(true, false), both, "out, preference off");
        assert_eq!(
            drag_operation(true, true),
            NSDragOperation::Copy,
            "out, preference on"
        );
        // Inside the app the preference changes nothing.
        assert_eq!(drag_operation(false, true), both, "inside, preference on");
        assert_eq!(drag_operation(false, false), both, "inside, preference off");
    }

    #[test]
    fn a_small_dirty_rect_visits_only_its_tile() {
        let (left, right) = tile_span(180.0, 20.0, 168.0, 6, 3.0);
        let (top, bottom) = tile_span(520.0, 25.0, 168.0, 100, 3.0);
        let cells = CellRange {
            columns: 6,
            left,
            right,
            top,
            bottom,
        };
        assert_eq!(cells.indices(600).collect::<Vec<_>>(), [19]);
        assert!(cells.indices(19).next().is_none());
    }

    #[test]
    fn thumbnails_size_to_the_tile_and_display() {
        assert_eq!(thumbnail_pixel_size(160.0, 2.0), 320);
        assert_eq!(thumbnail_pixel_size(155.0, 2.0), 320);
        assert_eq!(thumbnail_pixel_size(60.0, 2.0), 128);
        assert_eq!(thumbnail_pixel_size(320.0, 2.0), 640);
    }
}
