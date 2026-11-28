//! Interactive split and side-by-side comparison view for upscale preview.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSCursor, NSEvent, NSEventModifierFlags, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSGraphicsContext, NSStringDrawing, NSView,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGImage, CGInterpolationQuality};
use objc2_foundation::{NSDictionary, NSRect, NSString};

use crate::shortcuts::{Action, KeyCombo};
use crate::slideshow::{ladder_step, scroll_deltas, ZOOM_LADDER};
use crate::upscale::UpscaleScale;
use crate::AppDelegate;

const DIVIDER_GRAB: f64 = 16.0;
const KNOB_RADIUS: f64 = 15.0;
const PANE_GAP: f64 = 2.0;
const BADGE_INSET: f64 = 14.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompareMode {
    #[default]
    Split,
    SideBySide,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DragKind {
    Divider,
    Pan {
        start: CGPoint,
        start_center: (f64, f64),
        pane: CGRect,
    },
}

fn canvas_rect(bounds: CGRect, insets: (f64, f64, f64, f64)) -> CGRect {
    let (top, right, bottom, left) = insets;
    let w = (bounds.size.width - left - right).max(1.0);
    let h = (bounds.size.height - top - bottom).max(1.0);
    CGRect::new(
        CGPoint::new(bounds.origin.x + left, bounds.origin.y + bottom),
        CGSize::new(w, h),
    )
}

fn side_by_side_panes(canvas: CGRect) -> (CGRect, CGRect) {
    let half = ((canvas.size.width - PANE_GAP) / 2.0).max(1.0);
    let left = CGRect::new(canvas.origin, CGSize::new(half, canvas.size.height));
    let right = CGRect::new(
        CGPoint::new(canvas.origin.x + half + PANE_GAP, canvas.origin.y),
        CGSize::new(half, canvas.size.height),
    );
    (left, right)
}

fn fit_scale(pane: CGSize, image: CGSize) -> f64 {
    if image.width <= 0.0 || image.height <= 0.0 {
        return 1.0;
    }
    (pane.width / image.width).min(pane.height / image.height)
}

fn effective_scale(zoom: Option<f64>, pane: CGSize, image: CGSize) -> f64 {
    let fit = fit_scale(pane, image);
    match zoom {
        Some(z) => z.max(fit),
        None => fit,
    }
}

/// Clamp normalized image center `(u, v)` so the drawn image stays anchored inside `pane`.
fn clamp_center(center: (f64, f64), pane: CGSize, drawn: CGSize) -> (f64, f64) {
    let clamp_axis = |c: f64, pane_len: f64, drawn_len: f64| {
        if drawn_len <= pane_len + 0.5 {
            0.5
        } else {
            let half_visible = (pane_len / 2.0) / drawn_len;
            c.clamp(half_visible, 1.0 - half_visible)
        }
    };
    (
        clamp_axis(center.0, pane.width, drawn.width),
        clamp_axis(center.1, pane.height, drawn.height),
    )
}

/// Rectangle where an image of size `image` renders inside `pane` at `scale` and `center`.
fn drawn_rect(pane: CGRect, image: CGSize, scale: f64, center: (f64, f64)) -> CGRect {
    let drawn = CGSize::new(image.width * scale, image.height * scale);
    let (u, v) = clamp_center(center, pane.size, drawn);
    let cx = pane.origin.x + pane.size.width / 2.0;
    let cy = pane.origin.y + pane.size.height / 2.0;
    CGRect::new(
        CGPoint::new(cx - u * drawn.width, cy - v * drawn.height),
        drawn,
    )
}

/// Compute updated `(zoom, center)` when zooming toward `anchor` inside `pane`.
fn zoom_around(
    current_zoom: Option<f64>,
    current_center: (f64, f64),
    target_zoom: f64,
    anchor: Option<CGPoint>,
    pane: CGRect,
    image: CGSize,
) -> (Option<f64>, (f64, f64)) {
    let fit = fit_scale(pane.size, image);
    let max_z = ZOOM_LADDER[ZOOM_LADDER.len() - 1].max(fit);
    let clamped = target_zoom.clamp(ZOOM_LADDER[0].min(fit), max_z);
    if clamped <= fit * 1.001 {
        return (None, (0.5, 0.5));
    }
    let old_scale = effective_scale(current_zoom, pane.size, image);
    let old_dw = image.width * old_scale;
    let old_dh = image.height * old_scale;
    let new_dw = image.width * clamped;
    let new_dh = image.height * clamped;
    let (u, v) = clamp_center(current_center, pane.size, CGSize::new(old_dw, old_dh));
    let (ax, ay) = match anchor {
        Some(p) => (
            p.x - (pane.origin.x + pane.size.width / 2.0),
            p.y - (pane.origin.y + pane.size.height / 2.0),
        ),
        None => (0.0, 0.0),
    };
    let anchor_u = u + ax / old_dw.max(1.0);
    let anchor_v = v + ay / old_dh.max(1.0);
    let next_u = anchor_u - ax / new_dw.max(1.0);
    let next_v = anchor_v - ay / new_dh.max(1.0);
    let next_center = clamp_center((next_u, next_v), pane.size, CGSize::new(new_dw, new_dh));
    (Some(clamped), next_center)
}

/// Convert a view X position to a split fraction `[0.0, 1.0]` inside `clip`.
fn split_fraction_at(x: f64, clip: CGRect) -> f64 {
    if clip.size.width <= 0.0 {
        return 0.5;
    }
    ((x - clip.origin.x) / clip.size.width).clamp(0.0, 1.0)
}

pub struct CompareViewIvars {
    original: RefCell<CFRetained<CGImage>>,
    upscaled: RefCell<CFRetained<CGImage>>,
    scale_label: Cell<UpscaleScale>,
    mode: Cell<CompareMode>,
    /// Normalized divider position `[0.0, 1.0]` within the visible image width.
    split: Cell<f64>,
    /// Zoom scale in points per upscaled pixel (`None` = fit pane).
    zoom: Cell<Option<f64>>,
    /// Normalized image center `(u, v)` in `[0, 1] × [0, 1]`.
    center: Cell<(f64, f64)>,
    /// Insets `(top, right, bottom, left)` to stay clear of title bar and HUD bar.
    insets: Cell<(f64, f64, f64, f64)>,
    peeking: Cell<bool>,
    drag: Cell<Option<DragKind>>,
    pub delegate: OnceCell<Retained<AppDelegate>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliCompareView"]
    #[ivars = CompareViewIvars]
    pub struct CompareView;

    impl CompareView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            if self.ivars().peeking.replace(false) {
                self.setNeedsDisplay(true);
            }
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: CGSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.reclamp_center();
            if let Some(window) = self.window() {
                window.invalidateCursorRectsForView(self);
            }
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.draw_comparison(dirty);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.ivars().mode.get() == CompareMode::Split && !self.ivars().peeking.get() {
                let div_x = self.divider_x();
                if (point.x - div_x).abs() <= DIVIDER_GRAB {
                    self.ivars().drag.set(Some(DragKind::Divider));
                    return;
                }
            }
            let pane = self.pane_for_point(point);
            self.ivars().drag.set(Some(DragKind::Pan {
                start: point,
                start_center: self.ivars().center.get(),
                pane,
            }));
            if self.ivars().zoom.get().is_some() {
                NSCursor::closedHandCursor().set();
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(drag) = self.ivars().drag.get() else {
                return;
            };
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            match drag {
                DragKind::Divider => {
                    let clip = self.split_reference_rect();
                    let old_split = self.ivars().split.get();
                    let new_split = split_fraction_at(point.x, clip);
                    if (new_split - old_split).abs() < 1e-9 {
                        return;
                    }
                    self.ivars().split.set(new_split);
                    let old_x = (clip.origin.x + clip.size.width * old_split).round();
                    let new_x = (clip.origin.x + clip.size.width * new_split).round();
                    let old_left = old_x - clip.origin.x;
                    let new_left = new_x - clip.origin.x;
                    let old_right = clip.origin.x + clip.size.width - old_x;
                    let new_right = clip.origin.x + clip.size.width - new_x;
                    let badge_flipped = (old_left > 88.0) != (new_left > 88.0)
                        || (old_right > 110.0) != (new_right > 110.0);
                    if badge_flipped {
                        self.setNeedsDisplay(true);
                    } else {
                        let margin = KNOB_RADIUS + 4.0;
                        let x0 = old_x.min(new_x) - margin;
                        let x1 = old_x.max(new_x) + margin;
                        self.setNeedsDisplayInRect(CGRect::new(
                            CGPoint::new(x0, clip.origin.y),
                            CGSize::new((x1 - x0).max(1.0), clip.size.height),
                        ));
                    }
                }
                DragKind::Pan {
                    start,
                    start_center,
                    pane,
                } => {
                    let image_size = self.upscaled_size();
                    let scale = effective_scale(self.ivars().zoom.get(), pane.size, image_size);
                    let drawn = CGSize::new(image_size.width * scale, image_size.height * scale);
                    if drawn.width <= pane.size.width + 0.5 && drawn.height <= pane.size.height + 0.5 {
                        return;
                    }
                    let dx = point.x - start.x;
                    let dy = point.y - start.y;
                    let next = (
                        start_center.0 - dx / drawn.width.max(1.0),
                        start_center.1 - dy / drawn.height.max(1.0),
                    );
                    self.ivars()
                        .center
                        .set(clamp_center(next, pane.size, drawn));
                    self.setNeedsDisplay(true);
                }
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().drag.set(None);
            if let Some(window) = self.window() {
                window.invalidateCursorRectsForView(self);
            }
        }

        #[unsafe(method(magnifyWithEvent:))]
        fn magnify(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let pane = self.pane_for_point(point);
            let image = self.upscaled_size();
            let current = effective_scale(self.ivars().zoom.get(), pane.size, image);
            self.apply_zoom(current * (1.0 + event.magnification()), Some(point), pane);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            if self.ivars().zoom.get().is_none() {
                return;
            }
            let (precise, dx, dy) = scroll_deltas(event);
            if dx == 0.0 && dy == 0.0 {
                return;
            }
            let scale_delta = if precise { 1.0 } else { 12.0 };
            let (dx, dy) = (dx * scale_delta, dy * scale_delta);
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let pane = self.pane_for_point(point);
            let image = self.upscaled_size();
            let scale = effective_scale(self.ivars().zoom.get(), pane.size, image);
            let drawn = CGSize::new(image.width * scale, image.height * scale);
            let (u, v) = self.ivars().center.get();
            let next = (
                u - dx / drawn.width.max(1.0),
                v + dy / drawn.height.max(1.0),
            );
            self.ivars()
                .center
                .set(clamp_center(next, pane.size, drawn));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let Some(delegate) = self.ivars().delegate.get() else {
                return;
            };
            let key_code = event.keyCode();
            let flags = event.modifierFlags();
            let option = flags.contains(NSEventModifierFlags::Option);
            let has_cmd_or_ctrl =
                flags.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Control);
            let has_shortcut_mod = flags.intersects(
                NSEventModifierFlags::Command
                    | NSEventModifierFlags::Control
                    | NSEventModifierFlags::Option,
            );
            let chars = event
                .charactersIgnoringModifiers()
                .map(|s| s.to_string())
                .unwrap_or_default();
            match key_code {
                53 if !has_shortcut_mod => {
                    delegate.cancel_upscale();
                    return;
                }
                36 | 76 if !has_cmd_or_ctrl && !flags.contains(NSEventModifierFlags::Shift) => {
                    delegate.apply_upscale(!option);
                    return;
                }
                _ => {}
            }
            // Honor custom slideshow zoom shortcuts inside comparison preview.
            if let Some(action) = KeyCombo::from_event(event)
                .as_ref()
                .and_then(|c| delegate.slideshow_action_for(c))
            {
                match action {
                    Action::SlideZoomIn => {
                        self.zoom_step(1);
                        return;
                    }
                    Action::SlideZoomOut => {
                        self.zoom_step(-1);
                        return;
                    }
                    Action::SlideZoomActual => {
                        self.zoom_actual();
                        return;
                    }
                    Action::SlideZoomReset => {
                        self.fit();
                        return;
                    }
                    _ => {}
                }
            }
            if has_shortcut_mod {
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            }
            match chars.as_str() {
                " " => {
                    if !event.isARepeat() && !self.ivars().peeking.replace(true) {
                        self.setNeedsDisplay(true);
                    }
                }
                "s" | "S" => {
                    let next = match self.ivars().mode.get() {
                        CompareMode::Split => CompareMode::SideBySide,
                        CompareMode::SideBySide => CompareMode::Split,
                    };
                    delegate.set_compare_mode(next);
                }
                "2" => delegate.set_upscale_preview_scale(UpscaleScale::X2),
                "4" => delegate.set_upscale_preview_scale(UpscaleScale::X4),
                "+" => self.zoom_step(1),
                "-" => self.zoom_step(-1),
                "=" => self.zoom_actual(),
                "*" => self.fit(),
                _ => {
                    let _: () = unsafe { msg_send![super(self), keyDown: event] };
                }
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            let is_space = event.keyCode() == 49
                || event
                    .charactersIgnoringModifiers()
                    .is_some_and(|s| s.to_string() == " ");
            if is_space {
                self.clear_peek();
            }
        }

        #[allow(deprecated)]
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let canvas = self.canvas();
            if self.ivars().zoom.get().is_some() {
                self.addCursorRect_cursor(canvas, &NSCursor::openHandCursor());
            }
            if self.ivars().mode.get() == CompareMode::Split && !self.ivars().peeking.get() {
                let x = self.divider_x();
                let band = CGRect::new(
                    CGPoint::new(x - DIVIDER_GRAB, canvas.origin.y),
                    CGSize::new(DIVIDER_GRAB * 2.0, canvas.size.height),
                );
                self.addCursorRect_cursor(band, &NSCursor::resizeLeftRightCursor());
            }
        }
    }
);

impl CompareView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        original: CFRetained<CGImage>,
        upscaled: CFRetained<CGImage>,
        scale: UpscaleScale,
        insets: (f64, f64, f64, f64),
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CompareViewIvars {
            original: RefCell::new(original),
            upscaled: RefCell::new(upscaled),
            scale_label: Cell::new(scale),
            mode: Cell::new(CompareMode::Split),
            split: Cell::new(0.5),
            zoom: Cell::new(None),
            center: Cell::new((0.5, 0.5)),
            insets: Cell::new(insets),
            peeking: Cell::new(false),
            drag: Cell::new(None),
            delegate: OnceCell::new(),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        this
    }

    pub fn set_mode(&self, mode: CompareMode) {
        if self.ivars().mode.replace(mode) != mode {
            self.reclamp_center();
            self.setNeedsDisplay(true);
            if let Some(window) = self.window() {
                window.invalidateCursorRectsForView(self);
            }
        }
    }

    pub fn clear_peek(&self) {
        if self.ivars().peeking.replace(false) {
            self.setNeedsDisplay(true);
        }
    }

    /// Swap in the 2× or 4× upscaled image while preserving current zoom/pan.
    pub fn set_upscaled(&self, upscaled: CFRetained<CGImage>, scale: UpscaleScale) {
        let old_w = self.upscaled_size().width.max(1.0);
        *self.ivars().upscaled.borrow_mut() = upscaled;
        self.ivars().scale_label.set(scale);
        let new_w = self.upscaled_size().width.max(1.0);
        // Keep physical on-screen magnification identical when toggling 2× and 4×.
        if let Some(z) = self.ivars().zoom.get() {
            let pane = self.primary_pane();
            let fit = fit_scale(pane.size, self.upscaled_size());
            let max_z = ZOOM_LADDER[ZOOM_LADDER.len() - 1].max(fit);
            let adjusted = (z * (old_w / new_w)).min(max_z);
            self.ivars()
                .zoom
                .set((adjusted > fit * 1.001).then_some(adjusted));
        }
        self.reclamp_center();
        self.setNeedsDisplay(true);
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    pub fn zoom_step(&self, direction: i32) {
        let pane = self.primary_pane();
        let image = self.upscaled_size();
        let current = effective_scale(self.ivars().zoom.get(), pane.size, image);
        self.apply_zoom(ladder_step(current, direction), None, pane);
    }

    pub fn zoom_actual(&self) {
        let pane = self.primary_pane();
        self.apply_zoom(1.0, None, pane);
    }

    pub fn fit(&self) {
        self.ivars().zoom.set(None);
        self.ivars().center.set((0.5, 0.5));
        self.setNeedsDisplay(true);
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    fn apply_zoom(&self, target: f64, anchor: Option<CGPoint>, pane: CGRect) {
        let (next_zoom, next_center) = zoom_around(
            self.ivars().zoom.get(),
            self.ivars().center.get(),
            target,
            anchor,
            pane,
            self.upscaled_size(),
        );
        self.ivars().zoom.set(next_zoom);
        self.ivars().center.set(next_center);
        self.setNeedsDisplay(true);
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    fn upscaled_size(&self) -> CGSize {
        let img = self.ivars().upscaled.borrow();
        CGSize::new(
            CGImage::width(Some(&*img)) as f64,
            CGImage::height(Some(&*img)) as f64,
        )
    }

    fn canvas(&self) -> CGRect {
        canvas_rect(self.bounds(), self.ivars().insets.get())
    }

    fn primary_pane(&self) -> CGRect {
        let canvas = self.canvas();
        match self.ivars().mode.get() {
            CompareMode::Split => canvas,
            CompareMode::SideBySide => side_by_side_panes(canvas).0,
        }
    }

    fn pane_for_point(&self, point: CGPoint) -> CGRect {
        let canvas = self.canvas();
        match self.ivars().mode.get() {
            CompareMode::Split => canvas,
            CompareMode::SideBySide => {
                let (left, right) = side_by_side_panes(canvas);
                if point.x >= right.origin.x {
                    right
                } else {
                    left
                }
            }
        }
    }

    fn reclamp_center(&self) {
        let pane = self.primary_pane();
        let image = self.upscaled_size();
        let fit = fit_scale(pane.size, image);
        if let Some(z) = self.ivars().zoom.get() {
            if z <= fit * 1.001 {
                self.ivars().zoom.set(None);
                self.ivars().center.set((0.5, 0.5));
                return;
            }
        }
        let scale = effective_scale(self.ivars().zoom.get(), pane.size, image);
        let drawn = CGSize::new(image.width * scale, image.height * scale);
        self.ivars()
            .center
            .set(clamp_center(self.ivars().center.get(), pane.size, drawn));
    }

    /// Visible horizontal span of the image in Split mode (used to position and clamp the divider).
    fn split_reference_rect(&self) -> CGRect {
        let canvas = self.canvas();
        let image = self.upscaled_size();
        let scale = effective_scale(self.ivars().zoom.get(), canvas.size, image);
        let rect = drawn_rect(canvas, image, scale, self.ivars().center.get());
        intersect_rect(rect, canvas)
    }

    fn divider_x(&self) -> f64 {
        let clip = self.split_reference_rect();
        clip.origin.x + clip.size.width * self.ivars().split.get()
    }

    fn draw_comparison(&self, dirty: CGRect) {
        NSColor::blackColor().setFill();
        objc2_app_kit::NSRectFill(dirty);

        let Some(ns_ctx) = NSGraphicsContext::currentContext() else {
            return;
        };
        let cg = ns_ctx.CGContext();
        let canvas = self.canvas();
        let original = self.ivars().original.borrow();
        let upscaled = self.ivars().upscaled.borrow();
        let image_size = CGSize::new(
            CGImage::width(Some(&*upscaled)) as f64,
            CGImage::height(Some(&*upscaled)) as f64,
        );
        let peeking = self.ivars().peeking.get();
        let scale_text = format!("Upscaled {}", self.ivars().scale_label.get().label());

        match self.ivars().mode.get() {
            CompareMode::Split => {
                let scale = effective_scale(self.ivars().zoom.get(), canvas.size, image_size);
                let dst = drawn_rect(canvas, image_size, scale, self.ivars().center.get());
                let visible = intersect_rect(dst, canvas);

                if peeking {
                    if let Some(clipped) = rect_intersection(visible, dirty) {
                        draw_clipped_image(&cg, clipped, dst, &original);
                    }
                    draw_badge("Original (Space)", CGPoint::new(visible.origin.x + BADGE_INSET, visible.origin.y + visible.size.height - BADGE_INSET), false);
                    return;
                }

                let div_x = (visible.origin.x + visible.size.width * self.ivars().split.get()).round();
                let left_clip = CGRect::new(
                    visible.origin,
                    CGSize::new((div_x - visible.origin.x).max(0.0), visible.size.height),
                );
                let right_clip = CGRect::new(
                    CGPoint::new(div_x, visible.origin.y),
                    CGSize::new((visible.origin.x + visible.size.width - div_x).max(0.0), visible.size.height),
                );
                if let Some(left_dirty) = rect_intersection(left_clip, dirty) {
                    draw_clipped_image(&cg, left_dirty, dst, &original);
                }
                if let Some(right_dirty) = rect_intersection(right_clip, dirty) {
                    draw_clipped_image(&cg, right_dirty, dst, &upscaled);
                }

                draw_split_divider(div_x, visible.origin.y, visible.origin.y + visible.size.height);

                let badge_y = visible.origin.y + visible.size.height - BADGE_INSET;
                if left_clip.size.width > 88.0 {
                    draw_badge("Original", CGPoint::new(visible.origin.x + BADGE_INSET, badge_y), false);
                }
                if right_clip.size.width > 110.0 {
                    draw_badge(
                        &scale_text,
                        CGPoint::new(visible.origin.x + visible.size.width - BADGE_INSET, badge_y),
                        true,
                    );
                }
            }
            CompareMode::SideBySide => {
                let (left_pane, right_pane) = side_by_side_panes(canvas);
                let scale = effective_scale(self.ivars().zoom.get(), left_pane.size, image_size);
                let center = self.ivars().center.get();
                let left_dst = drawn_rect(left_pane, image_size, scale, center);
                let right_dst = drawn_rect(right_pane, image_size, scale, center);

                if let Some(left_dirty) = rect_intersection(left_pane, dirty) {
                    draw_clipped_image(&cg, left_dirty, left_dst, &original);
                }
                let right_img: &CGImage = if peeking { &original } else { &upscaled };
                if let Some(right_dirty) = rect_intersection(right_pane, dirty) {
                    draw_clipped_image(&cg, right_dirty, right_dst, right_img);
                }

                // Subtle separator between the two panes.
                NSColor::colorWithWhite_alpha(1.0, 0.22).setFill();
                objc2_app_kit::NSRectFillUsingOperation(
                    CGRect::new(
                        CGPoint::new(left_pane.origin.x + left_pane.size.width, canvas.origin.y),
                        CGSize::new(PANE_GAP, canvas.size.height),
                    ),
                    objc2_app_kit::NSCompositingOperation::SourceOver,
                );

                let badge_y = canvas.origin.y + canvas.size.height - BADGE_INSET;
                draw_badge("Original", CGPoint::new(left_pane.origin.x + BADGE_INSET, badge_y), false);
                let right_label = if peeking { "Original (Space)" } else { &scale_text };
                draw_badge(
                    right_label,
                    CGPoint::new(right_pane.origin.x + BADGE_INSET, badge_y),
                    false,
                );
            }
        }
    }
}

fn draw_clipped_image(cg: &CGContext, clip: CGRect, dst: CGRect, image: &CGImage) {
    CGContext::save_g_state(Some(cg));
    CGContext::clip_to_rect(Some(cg), clip);
    CGContext::set_interpolation_quality(Some(cg), CGInterpolationQuality::High);
    CGContext::draw_image(Some(cg), dst, Some(image));
    CGContext::restore_g_state(Some(cg));
}

fn draw_split_divider(x: f64, y_min: f64, y_max: f64) {
    // Dark halo behind white line so the divider reads on bright backgrounds.
    let line = NSBezierPath::bezierPath();
    line.moveToPoint(CGPoint::new(x, y_min));
    line.lineToPoint(CGPoint::new(x, y_max));
    NSColor::colorWithWhite_alpha(0.0, 0.45).setStroke();
    line.setLineWidth(3.5);
    line.stroke();
    NSColor::colorWithWhite_alpha(1.0, 0.92).setStroke();
    line.setLineWidth(1.5);
    line.stroke();

    // Centered circular drag knob with left/right chevrons.
    let cy = (y_min + y_max) / 2.0;
    let knob = CGRect::new(
        CGPoint::new(x - KNOB_RADIUS, cy - KNOB_RADIUS),
        CGSize::new(KNOB_RADIUS * 2.0, KNOB_RADIUS * 2.0),
    );
    let circle = NSBezierPath::bezierPathWithOvalInRect(knob);
    NSColor::colorWithWhite_alpha(0.0, 0.65).setFill();
    circle.fill();
    NSColor::colorWithWhite_alpha(1.0, 0.92).setStroke();
    circle.setLineWidth(1.5);
    circle.stroke();

    let chevrons = NSBezierPath::bezierPath();
    chevrons.setLineWidth(1.5);
    // Left chevron
    chevrons.moveToPoint(CGPoint::new(x - 3.5, cy + 4.0));
    chevrons.lineToPoint(CGPoint::new(x - 7.5, cy));
    chevrons.lineToPoint(CGPoint::new(x - 3.5, cy - 4.0));
    // Right chevron
    chevrons.moveToPoint(CGPoint::new(x + 3.5, cy + 4.0));
    chevrons.lineToPoint(CGPoint::new(x + 7.5, cy));
    chevrons.lineToPoint(CGPoint::new(x + 3.5, cy - 4.0));
    NSColor::whiteColor().setStroke();
    chevrons.stroke();
}

fn draw_badge(text: &str, anchor: CGPoint, align_right: bool) {
    thread_local! {
        static BADGE_ATTRS: OnceCell<Retained<NSDictionary<NSString, objc2::runtime::AnyObject>>> =
            const { OnceCell::new() };
    }
    BADGE_ATTRS.with(|cell| {
        let attrs = cell.get_or_init(|| {
            let font = NSFont::systemFontOfSize_weight(11.5, 0.23);
            let color = NSColor::colorWithWhite_alpha(1.0, 0.94);
            unsafe {
                NSDictionary::from_slices(
                    &[NSFontAttributeName, NSForegroundColorAttributeName],
                    &[
                        font.as_ref() as &objc2::runtime::AnyObject,
                        color.as_ref(),
                    ],
                )
            }
        });
        let ns_text = NSString::from_str(text);
        let text_size = unsafe { ns_text.sizeWithAttributes(Some(attrs)) };
        let pad_x = 9.0;
        let pad_y = 4.0;
        let box_w = text_size.width + pad_x * 2.0;
        let box_h = text_size.height + pad_y * 2.0;
        let x = if align_right {
            anchor.x - box_w
        } else {
            anchor.x
        };
        let y = anchor.y - box_h;
        let pill = CGRect::new(CGPoint::new(x, y), CGSize::new(box_w, box_h));
        NSColor::colorWithWhite_alpha(0.0, 0.62).setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, 6.0, 6.0).fill();
        let text_rect = CGRect::new(CGPoint::new(x + pad_x, y + pad_y), text_size);
        unsafe { ns_text.drawInRect_withAttributes(text_rect, Some(attrs)) };
    });
}

fn rect_intersection(a: CGRect, b: CGRect) -> Option<CGRect> {
    let x0 = a.origin.x.max(b.origin.x);
    let y0 = a.origin.y.max(b.origin.y);
    let x1 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y1 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(CGRect::new(CGPoint::new(x0, y0), CGSize::new(x1 - x0, y1 - y0)))
}

fn intersect_rect(a: CGRect, b: CGRect) -> CGRect {
    rect_intersection(a, b).unwrap_or(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_by_side_panes_split_canvas_evenly() {
        let canvas = CGRect::new(CGPoint::new(10.0, 20.0), CGSize::new(802.0, 500.0));
        let (left, right) = side_by_side_panes(canvas);
        assert_eq!(left.size.width, 400.0);
        assert_eq!(right.size.width, 400.0);
        assert_eq!(left.origin.x, 10.0);
        assert_eq!(right.origin.x, 412.0);
    }

    #[test]
    fn zoom_around_keeps_anchor_pixel_fixed() {
        let pane = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(400.0, 400.0));
        let image = CGSize::new(800.0, 800.0); // fit = 0.5
        // Anchor at right quarter (300, 200) -> image u = 0.75 at fit.
        let (zoom, center) = zoom_around(None, (0.5, 0.5), 1.0, Some(CGPoint::new(300.0, 200.0)), pane, image);
        assert_eq!(zoom, Some(1.0));
        // At scale 1.0 (drawn 800px), keeping u=0.75 at x=300 (ax=+100) puts center at 0.75 - 100/800 = 0.625.
        assert!((center.0 - 0.625).abs() < 1e-6);
        assert!((center.1 - 0.5).abs() < 1e-6);
        // Zooming back to fit resets zoom and center.
        let (zoom, center) = zoom_around(zoom, center, 0.5, None, pane, image);
        assert_eq!(zoom, None);
        assert_eq!(center, (0.5, 0.5));
    }

    #[test]
    fn split_fraction_clamps_to_unit_interval() {
        let clip = CGRect::new(CGPoint::new(100.0, 0.0), CGSize::new(400.0, 300.0));
        assert_eq!(split_fraction_at(0.0, clip), 0.0);
        assert_eq!(split_fraction_at(300.0, clip), 0.5);
        assert_eq!(split_fraction_at(900.0, clip), 1.0);
    }
}
