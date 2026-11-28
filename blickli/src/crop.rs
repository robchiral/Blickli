//! Interactive crop overlay with edge/corner handles and testable geometry helpers.

use std::cell::{Cell, OnceCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBezierPath, NSColor, NSCursor, NSEvent, NSView, NSWindingRule};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSRect;

use crate::edit::CropRect;
use crate::AppDelegate;

const MIN_SIDE: f64 = 24.0;
const GRAB: f64 = 12.0;
const HANDLE_LEN: f64 = 22.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sides {
    left: bool,
    right: bool,
    bottom: bool,
    top: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Grip {
    Resize(Sides),
    Move,
    Draw(CGPoint),
}

#[derive(Clone, Copy)]
struct Drag {
    grip: Grip,
    start: CGPoint,
    rect: CGRect,
}

fn rect(left: f64, bottom: f64, right: f64, top: f64) -> CGRect {
    CGRect::new(
        CGPoint::new(left, bottom),
        CGSize::new(right - left, top - bottom),
    )
}

fn max_x(r: CGRect) -> f64 {
    r.origin.x + r.size.width
}

fn max_y(r: CGRect) -> f64 {
    r.origin.y + r.size.height
}

fn contains(r: CGRect, p: CGPoint) -> bool {
    p.x >= r.origin.x && p.x <= max_x(r) && p.y >= r.origin.y && p.y <= max_y(r)
}

fn grip_at(crop: CGRect, point: CGPoint) -> Grip {
    // Cap grab radius to a third of each side so small crops keep a draggable interior.
    let grab_x = GRAB.min(crop.size.width / 3.0);
    let grab_y = GRAB.min(crop.size.height / 3.0);
    let within_x = point.x >= crop.origin.x - grab_x && point.x <= max_x(crop) + grab_x;
    let within_y = point.y >= crop.origin.y - grab_y && point.y <= max_y(crop) + grab_y;
    let sides = Sides {
        left: within_y && (point.x - crop.origin.x).abs() <= grab_x,
        right: within_y && (point.x - max_x(crop)).abs() <= grab_x,
        bottom: within_x && (point.y - crop.origin.y).abs() <= grab_y,
        top: within_x && (point.y - max_y(crop)).abs() <= grab_y,
    };
    if sides != Sides::default() {
        Grip::Resize(sides)
    } else if contains(crop, point) {
        Grip::Move
    } else {
        Grip::Draw(point)
    }
}

enum Anchor {
    Low(f64),
    High(f64),
    Centre(f64),
}

impl Anchor {
    fn room(&self, low: f64, high: f64) -> f64 {
        match *self {
            Anchor::Low(at) => high - at,
            Anchor::High(at) => at - low,
            Anchor::Centre(at) => 2.0 * (at - low).min(high - at),
        }
    }

    fn place(&self, extent: f64) -> f64 {
        match *self {
            Anchor::Low(at) => at,
            Anchor::High(at) => at - extent,
            Anchor::Centre(at) => at - extent / 2.0,
        }
    }
}

fn clamp_safe(value: f64, low: f64, high: f64) -> f64 {
    value.clamp(low, high.max(low))
}

/// Resize `start` by dragging `sides` by `(dx, dy)` within `bounds`, preserving `aspect` if set.
fn resize(
    start: CGRect,
    sides: Sides,
    (dx, dy): (f64, f64),
    bounds: CGRect,
    aspect: Option<f64>,
    min: f64,
) -> CGRect {
    let min = min.min(bounds.size.width).min(bounds.size.height);
    let (mut l, mut b, mut r, mut t) = (start.origin.x, start.origin.y, max_x(start), max_y(start));
    if sides.left {
        l = clamp_safe(l + dx, bounds.origin.x, r - min);
    }
    if sides.right {
        r = clamp_safe(r + dx, l + min, max_x(bounds));
    }
    if sides.bottom {
        b = clamp_safe(b + dy, bounds.origin.y, t - min);
    }
    if sides.top {
        t = clamp_safe(t + dy, b + min, max_y(bounds));
    }
    let Some(aspect) = aspect.filter(|a| a.is_finite() && *a > 0.0) else {
        return rect(l, b, r, t);
    };
    let across = sides.left || sides.right;
    let up_down = sides.bottom || sides.top;
    let (mut w, mut h) = ((r - l).max(1e-6), (t - b).max(1e-6));
    if across && up_down {
        // Fit the constrained box inside the dragged rectangle.
        if w / h > aspect {
            w = h * aspect;
        } else {
            h = w / aspect;
        }
    } else if across {
        h = w / aspect;
    } else {
        w = h * aspect;
    }
    // Keep both dimensions at or above `min` while honoring the locked ratio.
    let min_w = min.max(min * aspect);
    let min_h = min_w / aspect;
    if w < min_w {
        w = min_w;
        h = min_h;
    }
    let x = if sides.left {
        Anchor::High(r)
    } else if sides.right {
        Anchor::Low(l)
    } else {
        Anchor::Centre(start.origin.x + start.size.width / 2.0)
    };
    let y = if sides.bottom {
        Anchor::High(t)
    } else if sides.top {
        Anchor::Low(b)
    } else {
        Anchor::Centre(start.origin.y + start.size.height / 2.0)
    };
    let fit = (x.room(bounds.origin.x, max_x(bounds)) / w)
        .min(y.room(bounds.origin.y, max_y(bounds)) / h)
        .clamp(0.0, 1.0);
    let (w, h) = (w * fit, h * fit);
    CGRect::new(CGPoint::new(x.place(w), y.place(h)), CGSize::new(w, h))
}

fn slide(start: CGRect, (dx, dy): (f64, f64), bounds: CGRect) -> CGRect {
    let x = clamp_safe(start.origin.x + dx, bounds.origin.x, max_x(bounds) - start.size.width);
    let y = clamp_safe(start.origin.y + dy, bounds.origin.y, max_y(bounds) - start.size.height);
    CGRect::new(CGPoint::new(x, y), start.size)
}

fn draw(from: CGPoint, to: CGPoint, bounds: CGRect, aspect: Option<f64>) -> CGRect {
    let sides = Sides {
        left: to.x < from.x,
        right: to.x >= from.x,
        bottom: to.y < from.y,
        top: to.y >= from.y,
    };
    let seed = CGRect::new(from, CGSize::new(0.0, 0.0));
    resize(seed, sides, (to.x - from.x, to.y - from.y), bounds, aspect, 1.0)
}

/// Largest rect with `aspect` inside `bounds`, centered on `around`.
fn fit_aspect(around: CGRect, aspect: f64, bounds: CGRect) -> CGRect {
    let w = bounds.size.width.min(bounds.size.height * aspect);
    let h = w / aspect;
    let centre = (
        around.origin.x + around.size.width / 2.0,
        around.origin.y + around.size.height / 2.0,
    );
    let start = CGRect::new(
        CGPoint::new(centre.0 - w / 2.0, centre.1 - h / 2.0),
        CGSize::new(w, h),
    );
    slide(start, (0.0, 0.0), bounds)
}

/// Convert `crop` to normalized top-left coordinates within `image`.
pub(crate) fn fractions(crop: CGRect, image: CGRect) -> CropRect {
    CropRect {
        x: (crop.origin.x - image.origin.x) / image.size.width,
        y: (max_y(image) - max_y(crop)) / image.size.height,
        width: crop.size.width / image.size.width,
        height: crop.size.height / image.size.height,
    }
}

/// Map `crop` proportionally from image rect `from` to `to` (e.g. on window resize).
fn carry(crop: CGRect, from: CGRect, to: CGRect) -> CGRect {
    if from.size.width <= 0.0 || from.size.height <= 0.0 {
        return to;
    }
    let sx = to.size.width / from.size.width;
    let sy = to.size.height / from.size.height;
    CGRect::new(
        CGPoint::new(
            to.origin.x + (crop.origin.x - from.origin.x) * sx,
            to.origin.y + (crop.origin.y - from.origin.y) * sy,
        ),
        CGSize::new(crop.size.width * sx, crop.size.height * sy),
    )
}

pub struct CropViewIvars {
    image: Cell<CGRect>,
    crop: Cell<CGRect>,
    aspect: Cell<Option<f64>>,
    drag: Cell<Option<Drag>>,
    pub delegate: OnceCell<Retained<AppDelegate>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliCropView"]
    #[ivars = CropViewIvars]
    pub struct CropView;

    impl CropView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw_overlay();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let crop = self.ivars().crop.get();
            let grip = grip_at(crop, point);
            if matches!(grip, Grip::Draw(_)) && !contains(self.ivars().image.get(), point) {
                self.ivars().drag.set(None);
                return;
            }
            if grip == Grip::Move {
                NSCursor::closedHandCursor().set();
            }
            self.ivars().drag.set(Some(Drag {
                grip,
                start: point,
                rect: crop,
            }));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(drag) = self.ivars().drag.get() else { return };
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let delta = (point.x - drag.start.x, point.y - drag.start.y);
            let bounds = self.ivars().image.get();
            let aspect = self.ivars().aspect.get();
            let next = match drag.grip {
                Grip::Move => slide(drag.rect, delta, bounds),
                Grip::Resize(sides) => resize(drag.rect, sides, delta, bounds, aspect, MIN_SIDE),
                Grip::Draw(from) => draw(from, point, bounds, aspect),
            };
            self.set_crop(next);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if let Some(drag) = self.ivars().drag.take() {
                // Restore the previous crop only if a new freehand draw was too small.
                if matches!(drag.grip, Grip::Draw(_)) {
                    let crop = self.ivars().crop.get();
                    let image = self.ivars().image.get();
                    let min_w = MIN_SIDE.min(image.size.width);
                    let min_h = MIN_SIDE.min(image.size.height);
                    if crop.size.width < min_w || crop.size.height < min_h {
                        self.set_crop(drag.rect);
                    }
                }
            }
            if let Some(window) = self.window() {
                window.invalidateCursorRectsForView(self);
            }
        }

        // Consume slide navigation gestures while crop mode is active.
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, _event: &NSEvent) {}

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(magnifyWithEvent:))]
        fn magnify(&self, _event: &NSEvent) {}

        // Replacement resize cursors require macOS 15+.
        #[allow(deprecated)]
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let crop = self.ivars().crop.get();
            let gx = GRAB.min(crop.size.width / 3.0);
            let gy = GRAB.min(crop.size.height / 3.0);
            self.addCursorRect_cursor(self.ivars().image.get(), &NSCursor::crosshairCursor());
            let inner = CGRect::new(
                CGPoint::new(crop.origin.x + gx, crop.origin.y + gy),
                CGSize::new(
                    (crop.size.width - 2.0 * gx).max(0.0),
                    (crop.size.height - 2.0 * gy).max(0.0),
                ),
            );
            self.addCursorRect_cursor(inner, &NSCursor::openHandCursor());
            let band = |x: f64, y: f64, w: f64, h: f64| CGRect::new(CGPoint::new(x, y), CGSize::new(w, h));
            let span_w = (crop.size.width - 2.0 * gx).max(0.0);
            let span_h = (crop.size.height - 2.0 * gy).max(0.0);
            let across = NSCursor::resizeLeftRightCursor();
            let up_down = NSCursor::resizeUpDownCursor();
            self.addCursorRect_cursor(band(crop.origin.x - gx, crop.origin.y + gy, 2.0 * gx, span_h), &across);
            self.addCursorRect_cursor(band(max_x(crop) - gx, crop.origin.y + gy, 2.0 * gx, span_h), &across);
            self.addCursorRect_cursor(band(crop.origin.x + gx, crop.origin.y - gy, span_w, 2.0 * gy), &up_down);
            self.addCursorRect_cursor(band(crop.origin.x + gx, max_y(crop) - gy, span_w, 2.0 * gy), &up_down);
        }
    }
);

impl CropView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, image: CGRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CropViewIvars {
            image: Cell::new(image),
            crop: Cell::new(image),
            aspect: Cell::new(None),
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

    fn set_crop(&self, crop: CGRect) {
        self.ivars().crop.set(crop);
        self.setNeedsDisplay(true);
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.crop_changed();
        }
    }

    pub fn set_image_rect(&self, image: CGRect) {
        let old = self.ivars().image.replace(image);
        if old != image {
            self.set_crop(carry(self.ivars().crop.get(), old, image));
            if let Some(window) = self.window() {
                window.invalidateCursorRectsForView(self);
            }
        }
    }

    /// Lock crop aspect ratio (`width / height`, or `None` for free).
    /// When `match_orientation` is true (fixed presets like 4:3 or 16:9), flip
    /// the ratio to match portrait vs landscape orientation of the current crop;
    /// when false ("Original"), preserve the exact image orientation.
    pub fn set_aspect(&self, aspect: Option<f64>, match_orientation: bool) {
        let crop = self.ivars().crop.get();
        let aspect = aspect.map(|a| {
            if !match_orientation {
                return a;
            }
            let tall = crop.size.height > crop.size.width;
            if tall == (a > 1.0) {
                1.0 / a
            } else {
                a
            }
        });
        self.ivars().aspect.set(aspect);
        if let Some(aspect) = aspect {
            self.set_crop(fit_aspect(crop, aspect, self.ivars().image.get()));
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    pub fn fractions(&self) -> CropRect {
        fractions(self.ivars().crop.get(), self.ivars().image.get())
    }

    pub fn image_rect(&self) -> CGRect {
        self.ivars().image.get()
    }

    fn draw_overlay(&self) {
        let bounds = self.bounds();
        let crop = self.ivars().crop.get();
        // Scrim outside the crop rectangle.
        let shade = NSBezierPath::bezierPathWithRect(bounds);
        shade.appendBezierPathWithRect(crop);
        shade.setWindingRule(NSWindingRule::EvenOdd);
        NSColor::colorWithWhite_alpha(0.0, 0.6).setFill();
        shade.fill();

        // Rule-of-thirds grid.
        NSColor::colorWithWhite_alpha(1.0, 0.35).setStroke();
        let thirds = NSBezierPath::bezierPath();
        thirds.setLineWidth(0.5);
        for k in [1.0, 2.0] {
            let x = crop.origin.x + crop.size.width * k / 3.0;
            let y = crop.origin.y + crop.size.height * k / 3.0;
            thirds.moveToPoint(CGPoint::new(x, crop.origin.y));
            thirds.lineToPoint(CGPoint::new(x, max_y(crop)));
            thirds.moveToPoint(CGPoint::new(crop.origin.x, y));
            thirds.lineToPoint(CGPoint::new(max_x(crop), y));
        }
        thirds.stroke();

        NSColor::colorWithWhite_alpha(1.0, 0.9).setStroke();
        let frame = NSBezierPath::bezierPathWithRect(CGRect::new(
            CGPoint::new(crop.origin.x + 0.5, crop.origin.y + 0.5),
            CGSize::new(crop.size.width - 1.0, crop.size.height - 1.0),
        ));
        frame.setLineWidth(1.0);
        frame.stroke();

        // Corner and edge handles with dark outline for contrast on light images.
        let handles = NSBezierPath::bezierPath();
        let len = HANDLE_LEN.min(crop.size.width / 3.0).min(crop.size.height / 3.0);
        let (l, b, r, t) = (crop.origin.x + 2.0, crop.origin.y + 2.0, max_x(crop) - 2.0, max_y(crop) - 2.0);
        for (x, y, sx, sy) in [(l, b, 1.0, 1.0), (r, b, -1.0, 1.0), (l, t, 1.0, -1.0), (r, t, -1.0, -1.0)] {
            handles.moveToPoint(CGPoint::new(x + sx * len, y));
            handles.lineToPoint(CGPoint::new(x, y));
            handles.lineToPoint(CGPoint::new(x, y + sy * len));
        }
        let (mx, my) = ((l + r) / 2.0, (b + t) / 2.0);
        for (from, to) in [
            ((mx - len / 2.0, b), (mx + len / 2.0, b)),
            ((mx - len / 2.0, t), (mx + len / 2.0, t)),
            ((l, my - len / 2.0), (l, my + len / 2.0)),
            ((r, my - len / 2.0), (r, my + len / 2.0)),
        ] {
            handles.moveToPoint(CGPoint::new(from.0, from.1));
            handles.lineToPoint(CGPoint::new(to.0, to.1));
        }
        NSColor::colorWithWhite_alpha(0.0, 0.35).setStroke();
        handles.setLineWidth(6.0);
        handles.stroke();
        NSColor::whiteColor().setStroke();
        handles.setLineWidth(4.0);
        handles.stroke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn close(a: CGRect, b: CGRect) -> bool {
        (a.origin.x - b.origin.x).abs() < 1e-9
            && (a.origin.y - b.origin.y).abs() < 1e-9
            && (a.size.width - b.size.width).abs() < 1e-9
            && (a.size.height - b.size.height).abs() < 1e-9
    }

    const IMAGE: CGRect = CGRect {
        origin: CGPoint { x: 100.0, y: 50.0 },
        size: CGSize {
            width: 400.0,
            height: 300.0,
        },
    };

    #[test]
    fn a_press_finds_the_handle_under_it() {
        let crop = r(100.0, 100.0, 200.0, 100.0);
        let corner = grip_at(crop, CGPoint::new(103.0, 198.0));
        assert_eq!(
            corner,
            Grip::Resize(Sides {
                left: true,
                top: true,
                ..Sides::default()
            })
        );
        assert_eq!(grip_at(crop, CGPoint::new(200.0, 150.0)), Grip::Move);
        assert!(matches!(grip_at(crop, CGPoint::new(20.0, 20.0)), Grip::Draw(_)));
        // A minimum-sized 24×24 crop remains movable from its center.
        let tiny = r(100.0, 100.0, MIN_SIDE, MIN_SIDE);
        assert_eq!(grip_at(tiny, CGPoint::new(112.0, 112.0)), Grip::Move);
    }

    #[test]
    fn free_resizing_stays_inside_the_picture_and_above_the_minimum() {
        let crop = r(150.0, 100.0, 200.0, 100.0);
        let right = Sides {
            right: true,
            ..Sides::default()
        };
        // Past the picture's right edge it stops there.
        assert!(close(resize(crop, right, (500.0, 0.0), IMAGE, None, 24.0), r(150.0, 100.0, 350.0, 100.0)));
        // Past the left side it stops at the minimum.
        assert!(close(resize(crop, right, (-500.0, 0.0), IMAGE, None, 24.0), r(150.0, 100.0, 24.0, 100.0)));
    }

    #[test]
    fn a_ratio_holds_while_a_corner_moves_and_the_far_corner_stays() {
        let crop = r(150.0, 100.0, 160.0, 90.0);
        let top_right = Sides {
            right: true,
            top: true,
            ..Sides::default()
        };
        let next = resize(crop, top_right, (80.0, 10.0), IMAGE, Some(16.0 / 9.0), 24.0);
        assert!((next.size.width / next.size.height - 16.0 / 9.0).abs() < 1e-9);
        assert_eq!(next.origin, crop.origin);
        // Dragged far out, it fills what room the anchor leaves.
        let big = resize(crop, top_right, (900.0, 900.0), IMAGE, Some(1.0), 24.0);
        assert!(close(big, r(150.0, 100.0, 250.0, 250.0)));
    }

    #[test]
    fn an_edge_with_a_ratio_grows_both_ways_about_the_middle() {
        let crop = r(200.0, 150.0, 100.0, 100.0);
        let right = Sides {
            right: true,
            ..Sides::default()
        };
        let next = resize(crop, right, (100.0, 0.0), IMAGE, Some(1.0), 24.0);
        assert!(close(next, r(200.0, 100.0, 200.0, 200.0)));
    }

    #[test]
    fn moving_and_drawing_stay_inside() {
        let crop = r(150.0, 100.0, 100.0, 100.0);
        assert!(close(slide(crop, (1000.0, -1000.0), IMAGE), r(400.0, 50.0, 100.0, 100.0)));
        let drawn = draw(CGPoint::new(300.0, 200.0), CGPoint::new(50.0, 0.0), IMAGE, None);
        assert!(close(drawn, r(100.0, 50.0, 200.0, 150.0)));
    }

    #[test]
    fn ratios_fit_the_picture_and_fractions_count_from_the_top() {
        let square = fit_aspect(IMAGE, 1.0, IMAGE);
        assert!(close(square, r(150.0, 50.0, 300.0, 300.0)));
        let top_left_quarter = r(100.0, 200.0, 200.0, 150.0);
        let f = fractions(top_left_quarter, IMAGE);
        assert_eq!((f.x, f.y, f.width, f.height), (0.0, 0.0, 0.5, 0.5));
        let moved = carry(top_left_quarter, IMAGE, r(0.0, 0.0, 800.0, 600.0));
        assert!(close(moved, r(0.0, 300.0, 400.0, 300.0)));
    }

    #[test]
    fn drawing_near_an_image_edge_never_panics() {
        // Starting within < 1 pt of any edge previously triggered f64::clamp min > max panic.
        for edge_point in [
            CGPoint::new(100.2, 50.2),
            CGPoint::new(499.8, 349.8),
            CGPoint::new(100.0, 350.0),
            CGPoint::new(500.0, 50.0),
        ] {
            let rect = draw(edge_point, edge_point, IMAGE, Some(1.0));
            assert!(rect.size.width >= 0.0 && rect.size.height >= 0.0);
        }
    }

    #[test]
    fn aspect_locked_resize_respects_minimum_on_both_axes() {
        let crop = r(150.0, 100.0, 160.0, 90.0);
        let right = Sides {
            right: true,
            ..Sides::default()
        };
        let tiny = resize(crop, right, (-500.0, 0.0), IMAGE, Some(16.0 / 9.0), 24.0);
        assert!(tiny.size.width >= 24.0 - 1e-9);
        assert!(tiny.size.height >= 24.0 - 1e-9);
        assert!((tiny.size.width / tiny.size.height - 16.0 / 9.0).abs() < 1e-9);
    }
}
