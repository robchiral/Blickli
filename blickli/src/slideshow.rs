//! Slideshow window and Core Animation slide view (zoom, pan, rotate, and flip
//! via layer transforms without redrawing bitmaps).

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSColor, NSEvent, NSEventPhase, NSImage, NSMenu, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSRect;
use objc2_quartz_core::{kCAFilterTrilinear, CALayer, CATransaction, CATransform3D};

use crate::compare::CompareView;
use crate::crop::CropView;
use crate::shortcuts::{Action, KeyCombo};
use crate::AppDelegate;

const PAGE: i64 = 10;
const PAN_DRAG_SLOP: f64 = 4.0;
const TRACKPAD_THRESHOLD: f64 = 18.0;
const SMOOTH_WHEEL_THRESHOLD: f64 = 6.0;
const TRACKPAD_GATE: std::time::Duration = std::time::Duration::from_millis(120);
const SMOOTH_WHEEL_GATE: std::time::Duration = std::time::Duration::from_millis(100);
const SCROLL_IDLE_RESET: std::time::Duration = std::time::Duration::from_millis(250);

/// Read `(precise, dx, dy)` from an AppKit scroll event, falling back to legacy
/// `deltaX`/`deltaY` when non-precise mouse drivers leave scrolling deltas zero.
pub(crate) fn scroll_deltas(event: &NSEvent) -> (bool, f64, f64) {
    let precise = event.hasPreciseScrollingDeltas();
    let (sdx, sdy) = (event.scrollingDeltaX(), event.scrollingDeltaY());
    if precise || sdx != 0.0 || sdy != 0.0 {
        (precise, sdx, sdy)
    } else {
        (false, event.deltaX(), event.deltaY())
    }
}

/// Translates discrete mouse-wheel ratchets, smooth-scroll wheel bursts, and
/// trackpad gestures into single slide steps.
#[derive(Default)]
struct ScrollTracker {
    accum: f64,
    last_event: Option<std::time::Instant>,
    last_step: Option<std::time::Instant>,
    last_dir: i64,
}

#[derive(Clone, Copy)]
struct ScrollSample {
    now: std::time::Instant,
    precise: bool,
    has_phase: bool,
    phase_began: bool,
    phase_ended: bool,
    is_momentum: bool,
    step: f64,
}

impl ScrollTracker {
    fn feed(&mut self, sample: ScrollSample) -> Option<i64> {
        if !sample.precise {
            // Discrete notched wheels (e.g. MX Master ratchet mode) send one
            // event per click, often with small deltaY (~0.1); step 1:1.
            self.accum = 0.0;
            if sample.is_momentum || sample.step == 0.0 {
                return None;
            }
            return Some(if sample.step > 0.0 { 1 } else { -1 });
        }
        if sample.is_momentum || sample.phase_ended {
            self.accum = 0.0;
            return None;
        }
        if sample.phase_began {
            self.accum = 0.0;
            self.last_step = None;
            self.last_dir = 0;
        } else if self
            .last_event
            .is_some_and(|last| sample.now.duration_since(last) > SCROLL_IDLE_RESET)
        {
            self.accum = 0.0;
        }
        self.last_event = Some(sample.now);
        if sample.step == 0.0 {
            return None;
        }
        if self.accum != 0.0 && self.accum.signum() != sample.step.signum() {
            self.accum = 0.0;
        }
        self.accum += sample.step;
        let (threshold, gate) = if sample.has_phase {
            (TRACKPAD_THRESHOLD, TRACKPAD_GATE)
        } else {
            (SMOOTH_WHEEL_THRESHOLD, SMOOTH_WHEEL_GATE)
        };
        if self.accum.abs() < threshold {
            return None;
        }
        let dir = if self.accum > 0.0 { 1 } else { -1 };
        if dir == self.last_dir
            && self
                .last_step
                .is_some_and(|last| sample.now.duration_since(last) < gate)
        {
            self.accum = 0.0;
            return None;
        }
        self.accum = 0.0;
        self.last_step = Some(sample.now);
        self.last_dir = dir;
        Some(dir)
    }
}

pub(crate) const ZOOM_LADDER: &[f64] = &[
    0.125, 0.1875, 0.25, 0.375, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0,
];

/// Clamp zoom to `[min(ladder[0], fit), ladder.last()]`; returns `None` at or below fit.
fn settle_zoom(next: f64, fit: f64) -> Option<f64> {
    let next = next.clamp(ZOOM_LADDER[0].min(fit), ZOOM_LADDER[ZOOM_LADDER.len() - 1]);
    (next > fit).then_some(next)
}

/// Next zoom ladder step up or down; returns `0.0` below the bottom rung to settle at fit.
pub(crate) fn ladder_step(current: f64, direction: i32) -> f64 {
    if direction > 0 {
        ZOOM_LADDER
            .iter()
            .find(|&&z| z > current * 1.001)
            .copied()
            .unwrap_or(ZOOM_LADDER[ZOOM_LADDER.len() - 1])
    } else {
        ZOOM_LADDER
            .iter()
            .rev()
            .find(|&&z| z < current * 0.999)
            .copied()
            .unwrap_or(0.0)
    }
}

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliSlideshowWindow"]
    pub struct SlideshowWindow;
);

impl SlideshowWindow {
    pub fn new(mtm: MainThreadMarker, screen_frame: NSRect) -> Retained<Self> {
        let size = CGSize::new(
            screen_frame.size.width * 0.75,
            screen_frame.size.height * 0.75,
        );
        let frame = NSRect::new(
            CGPoint::new(
                screen_frame.origin.x + (screen_frame.size.width - size.width) / 2.0,
                screen_frame.origin.y + (screen_frame.size.height - size.height) / 2.0,
            ),
            size,
        );
        // Extend slide content edge-to-edge beneath a transparent title bar.
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let this = Self::alloc(mtm).set_ivars(());
        let window: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        window.setTitlebarAppearsTransparent(true);
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            window.setAppearance(Some(&dark));
        }
        window.setMinSize(CGSize::new(480.0, 320.0));
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenNone);
        window.setRestorable(false);
        unsafe { window.setReleasedWhenClosed(false) };
        window
    }
}

/// Per-file view state preserved during a slideshow session.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct SlideState {
    /// `None` fits the window (capped at 100% unless scale-up is enabled).
    pub zoom: Option<f64>,
    /// Clockwise degrees: 0, 90, 180, or 270.
    pub rotation: i32,
    pub flipped: bool,
    pub offset: (f64, f64),
}

pub struct SlideViewIvars {
    image: RefCell<Option<Retained<NSImage>>>,
    clip_layer: OnceCell<Retained<CALayer>>,
    slide_layer: OnceCell<Retained<CALayer>>,
    /// Source pixel dimensions (zoom is relative to natural size, not decoded size).
    natural: Cell<CGSize>,
    frames: RefCell<Vec<(Retained<NSImage>, f64)>>,
    frame_index: Cell<usize>,
    anim_timer: RefCell<Option<Retained<objc2_foundation::NSTimer>>>,
    zoom: Cell<Option<f64>>,
    rotation: Cell<i32>,
    flipped: Cell<bool>,
    offset: Cell<(f64, f64)>,
    press: Cell<Option<CGPoint>>,
    dragged: Cell<bool>,
    stepped_on_click1: Cell<bool>,
    scroll: RefCell<ScrollTracker>,
    crop: RefCell<Option<Retained<CropView>>>,
    compare: RefCell<Option<Retained<CompareView>>>,
    /// Fit margins `(top, right, bottom, left)` used in crop mode to clear chrome.
    insets: Cell<(f64, f64, f64, f64)>,
    pub delegate: OnceCell<Retained<AppDelegate>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliSlideView"]
    #[ivars = SlideViewIvars]
    pub struct SlideView;

    impl SlideView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: CGSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            if self.ivars().zoom.get().is_some() {
                let clamped = self.clamp_offset(self.ivars().offset.get());
                self.ivars().offset.set(clamped);
            }
            self.layout_slide();
            if let Some(delegate) = self.ivars().delegate.get() {
                delegate.slide_view_resized();
            }
        }

        // The pointer moving brings the controls back.
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, _event: &NSEvent) {
            if let Some(delegate) = self.ivars().delegate.get() {
                delegate.slide_pointer_moved();
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if self.ivars().delegate.get().is_some_and(|d| d.is_upscaling_or_comparing()) {
                return;
            }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.ivars().press.set(Some(point));
            self.ivars().dragged.set(false);
            if event.clickCount() == 1 {
                self.ivars().stepped_on_click1.set(false);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.ivars().delegate.get().is_some_and(|d| d.is_upscaling_or_comparing()) {
                return;
            }
            if self.ivars().zoom.get().is_none() {
                return;
            }
            if !self.ivars().dragged.get() {
                if let Some(start) = self.ivars().press.get() {
                    let point = self.convertPoint_fromView(event.locationInWindow(), None);
                    if (point.x - start.x).abs() < PAN_DRAG_SLOP
                        && (point.y - start.y).abs() < PAN_DRAG_SLOP
                    {
                        return;
                    }
                }
                self.ivars().dragged.set(true);
            }
            let (dx, dy) = self.ivars().offset.get();
            let (ex, ey) = (event.deltaX(), event.deltaY());
            // deltaY is flipped relative to the non-flipped view.
            let next = self.clamp_offset((dx + ex, dy - ey));
            self.ivars().offset.set(next);
            self.notify_pan_change();
        }

        // Click steps forward, drag pans, double-click ends the show on the current slide.
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.ivars().press.set(None);
            if self.ivars().delegate.get().is_some_and(|d| d.is_upscaling_or_comparing()) {
                return;
            }
            if self.ivars().dragged.get() {
                return;
            }
            let Some(delegate) = self.ivars().delegate.get() else { return };
            if event.clickCount() >= 2 {
                // Undo the single-click advance so double-clicking slide N returns to slide N in the grid.
                if self.ivars().stepped_on_click1.replace(false) {
                    delegate.step_slideshow(-1);
                }
                delegate.end_slideshow();
            } else {
                self.ivars().stepped_on_click1.set(true);
                delegate.step_slideshow(1);
            }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            if self.ivars().delegate.get().is_some_and(|d| d.is_upscaling_or_comparing()) {
                return;
            }
            let Some(delegate) = self.ivars().delegate.get() else { return };
            let Some(menu) = delegate.slide_context_menu() else { return };
            NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
        }

        // Pinch zooms around the fingers.
        #[unsafe(method(magnifyWithEvent:))]
        fn magnify(&self, event: &NSEvent) {
            if self.ivars().delegate.get().is_some_and(|d| d.is_upscaling_or_comparing()) {
                return;
            }
            if self.ivars().image.borrow().is_none() {
                return;
            }
            let current = self.effective_scale(self.bounds().size);
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.zoom_to(current * (1.0 + event.magnification()), Some(point));
        }

        // The wheel steps slides at fit and pans when zoomed.
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            if self.ivars().delegate.get().is_some_and(|d| d.is_upscaling_or_comparing()) {
                return;
            }
            let (precise, dx, dy) = scroll_deltas(event);
            if self.ivars().zoom.get().is_some() {
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                let scale = if precise { 1.0 } else { 12.0 };
                let (ox, oy) = self.ivars().offset.get();
                let next = self.clamp_offset((ox + dx * scale, oy - dy * scale));
                self.ivars().offset.set(next);
                self.notify_pan_change();
                return;
            }
            let step = if dy.abs() >= dx.abs() { -dy } else { -dx };
            let phase = event.phase();
            let momentum = event.momentumPhase();
            let dir = self.ivars().scroll.borrow_mut().feed(ScrollSample {
                now: std::time::Instant::now(),
                precise,
                has_phase: !phase.is_empty(),
                phase_began: phase.contains(NSEventPhase::Began),
                phase_ended: phase.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled),
                is_momentum: !momentum.is_empty(),
                step,
            });
            if let Some(dir) = dir {
                if let Some(delegate) = self.ivars().delegate.get() {
                    delegate.step_slideshow(dir);
                }
            }
        }

        // Dispatch ⌘-modified custom slideshow shortcuts (which AppKit routes here instead of keyDown:).
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let has_cmd = event
                .modifierFlags()
                .contains(objc2_app_kit::NSEventModifierFlags::Command);
            if !has_cmd || !self.window().is_some_and(|w| w.isKeyWindow()) {
                false
            } else if let Some(delegate) = self.ivars().delegate.get() {
                if let Some(action) = KeyCombo::from_event(event)
                    .as_ref()
                    .and_then(|c| delegate.slideshow_action_for(c))
                {
                    self.dispatch_slideshow_action(delegate, action)
                } else {
                    false
                }
            } else {
                false
            }
        }

        #[unsafe(method(advanceFrame:))]
        fn advance_frame(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            let (image, delay) = {
                let frames = self.ivars().frames.borrow();
                if frames.len() < 2 {
                    return;
                }
                let next = (self.ivars().frame_index.get() + 1) % frames.len();
                self.ivars().frame_index.set(next);
                frames[next].clone()
            };
            self.set_image(Some(image));
            self.schedule_frame(delay);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let Some(delegate) = self.ivars().delegate.get() else {
                return;
            };
            let key_code = event.keyCode();
            let flags = event.modifierFlags();
            let option = flags.contains(objc2_app_kit::NSEventModifierFlags::Option);
            let has_cmd_or_ctrl = flags.intersects(
                objc2_app_kit::NSEventModifierFlags::Command
                    | objc2_app_kit::NSEventModifierFlags::Control,
            );
            let has_shortcut_mod = flags.intersects(
                objc2_app_kit::NSEventModifierFlags::Command
                    | objc2_app_kit::NSEventModifierFlags::Control
                    | objc2_app_kit::NSEventModifierFlags::Option,
            );
            let plain_or_opt =
                !has_cmd_or_ctrl && !flags.contains(objc2_app_kit::NSEventModifierFlags::Shift);
            let chars = event
                .charactersIgnoringModifiers()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let combo = KeyCombo::from_event(event);
            // Crop mode takes return and escape, and nothing steps slides.
            if self.ivars().crop.borrow().is_some() {
                match key_code {
                    53 if !has_shortcut_mod => delegate.cancel_crop(),
                    36 | 76 if plain_or_opt => delegate.apply_crop(!option),
                    _ if combo
                        .as_ref()
                        .and_then(|c| delegate.slideshow_action_for(c))
                        == Some(Action::SlideCrop) =>
                    {
                        delegate.cancel_crop();
                    }
                    _ => {}
                }
                return;
            }
            // Upscaling or compare preview takes escape (and blocks slide stepping).
            if self.ivars().compare.borrow().is_some() || delegate.is_upscaling_or_comparing() {
                match key_code {
                    53 if !has_shortcut_mod => delegate.cancel_upscale(),
                    36 | 76 if plain_or_opt && self.ivars().compare.borrow().is_some() => {
                        delegate.apply_upscale(!option)
                    }
                    _ => {}
                }
                return;
            }
            if key_code == 53 && !has_shortcut_mod {
                return delegate.end_slideshow();
            }
            // Custom slideshow bindings take precedence over fallback keys.
            if let Some(action) = combo.as_ref().and_then(|c| delegate.slideshow_action_for(c)) {
                if self.dispatch_slideshow_action(delegate, action) {
                    return;
                }
            }
            match key_code {
                123 | 126 if option && !has_cmd_or_ctrl => {
                    return delegate.step_slideshow_original(-1)
                }
                124 | 125 if option && !has_cmd_or_ctrl => {
                    return delegate.step_slideshow_original(1)
                }
                123 | 126 if !has_cmd_or_ctrl => return delegate.step_slideshow(-1), // left, up
                124 | 125 if !has_cmd_or_ctrl => return delegate.step_slideshow(1),  // right, down
                115 if !has_shortcut_mod => return delegate.jump_slideshow_start(),  // home
                119 if !has_shortcut_mod => return delegate.jump_slideshow_end(),    // end
                116 if !has_shortcut_mod => return delegate.jump_slideshow(-PAGE),   // page up
                121 if !has_shortcut_mod => return delegate.jump_slideshow(PAGE),    // page down
                _ => {}
            }
            if has_shortcut_mod {
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            }
            match chars.as_str() {
                "?" => delegate.toggle_help_overlay(),
                "0" => delegate.set_auto_advance(None),
                "!" => delegate.set_auto_advance(Some(0.5)),
                "@" => delegate.set_auto_advance(Some(1.5)),
                d @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                    delegate.set_auto_advance(Some(d.parse::<f64>().unwrap()));
                }
                _ => {
                    let _: () = unsafe { msg_send![super(self), keyDown: event] };
                }
            }
        }
    }
);

impl SlideView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SlideViewIvars {
            image: RefCell::new(None),
            clip_layer: OnceCell::new(),
            slide_layer: OnceCell::new(),
            natural: Cell::new(CGSize::new(0.0, 0.0)),
            frames: RefCell::new(Vec::new()),
            frame_index: Cell::new(0),
            anim_timer: RefCell::new(None),
            zoom: Cell::new(None),
            rotation: Cell::new(0),
            flipped: Cell::new(false),
            offset: Cell::new((0.0, 0.0)),
            press: Cell::new(None),
            dragged: Cell::new(false),
            stepped_on_click1: Cell::new(false),
            scroll: RefCell::new(ScrollTracker::default()),
            crop: RefCell::new(None),
            compare: RefCell::new(None),
            insets: Cell::new((0.0, 0.0, 0.0, 0.0)),
            delegate: OnceCell::new(),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.install_layers();
        // Pointer moves, for the controls; the area follows the view's size.
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0)),
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveInKeyWindow
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&this),
                None,
            )
        };
        this.addTrackingArea(&tracking);
        this
    }

    fn dispatch_slideshow_action(&self, delegate: &AppDelegate, action: Action) -> bool {
        if self.ivars().crop.borrow().is_some() {
            if action == Action::SlideCrop {
                delegate.cancel_crop();
                return true;
            }
            return false;
        }
        let compare = self.ivars().compare.borrow().clone();
        if let Some(compare) = compare {
            match action {
                Action::SlideZoomIn => compare.zoom_step(1),
                Action::SlideZoomOut => compare.zoom_step(-1),
                Action::SlideZoomActual => compare.zoom_actual(),
                Action::SlideZoomReset => compare.fit(),
                _ => return false,
            }
            return true;
        }
        if delegate.is_upscaling_or_comparing() {
            return false;
        }
        match action {
            Action::SlideNext => delegate.step_slideshow(1),
            Action::SlidePrev => delegate.step_slideshow(-1),
            Action::SlidePlayPause => delegate.space_pressed(),
            Action::SlideClose => delegate.end_slideshow(),
            Action::SlideRotateCw => {
                delegate.rotate_targets(blickli_core::Orientation::from_view(90, false))
            }
            Action::SlideRotateCcw => {
                delegate.rotate_targets(blickli_core::Orientation::from_view(-90, false))
            }
            Action::SlideFlip => {
                delegate.rotate_targets(blickli_core::Orientation::from_view(0, true))
            }
            Action::SlideCrop if !delegate.is_compressing() => delegate.start_crop(),
            Action::SlideUpscale if !delegate.is_compressing() => delegate.upscale_selection(),
            Action::SlideZoomIn => self.zoom_step(1),
            Action::SlideZoomOut => self.zoom_step(-1),
            Action::SlideZoomActual => self.zoom_to(1.0, self.pointer()),
            Action::SlideZoomReset => {
                self.apply_state(SlideState::default());
                self.notify_state_change();
            }
            Action::SlideToggleControls => delegate.toggle_controls(),
            Action::SlideToggleOverlay => delegate.toggle_overlay(),
            Action::SlideToggleExif => delegate.toggle_exif_overlay(),
            Action::SlideTogglePath => delegate.toggle_path_overlay(),
            Action::SlideToggleHelp => delegate.toggle_help_overlay(),
            _ => return false,
        }
        true
    }

    /// Layers: the view's own layer holds a view-sized clip, under the overlay,
    /// holding the slide.
    fn install_layers(&self) {
        self.setWantsLayer(true);
        let Some(backing) = self.layer() else { return };
        let clip = CALayer::layer();
        clip.setMasksToBounds(true);
        // Below the overlay whatever order AppKit keeps.
        clip.setZPosition(-1.0);
        let slide = CALayer::layer();
        // Mipmapped minification keeps a big photo smooth when fit small.
        slide.setMinificationFilter(unsafe { kCAFilterTrilinear });
        slide.setHidden(true);
        clip.addSublayer(&slide);
        backing.addSublayer(&clip);
        let _ = self.ivars().clip_layer.set(clip);
        let _ = self.ivars().slide_layer.set(slide);
        self.layout_slide();
    }

    /// Hand the image to the layer; runs once per picture, not per gesture.
    fn set_image(&self, image: Option<Retained<NSImage>>) {
        if let Some(slide) = self.ivars().slide_layer.get() {
            without_animation(|| unsafe {
                slide.setContents(image.as_deref().map(|image| image.as_ref()));
            });
        }
        *self.ivars().image.borrow_mut() = image;
    }

    /// Position and transform the slide layer for current scale, pan, rotation, and flip.
    fn layout_slide(&self) {
        let (Some(clip), Some(slide)) = (
            self.ivars().clip_layer.get(),
            self.ivars().slide_layer.get(),
        ) else {
            return;
        };
        let bounds = self.bounds();
        let natural = self.ivars().natural.get();
        let shown =
            self.ivars().image.borrow().is_some() && natural.width > 0.0 && natural.height > 0.0;
        without_animation(|| {
            clip.setFrame(bounds);
            slide.setHidden(!shown);
            if !shown {
                return;
            }
            let scale = self.effective_scale(bounds.size);
            let (dx, dy) = self.ivars().offset.get();
            let (cx, cy) = self.centre();
            slide.setBounds(CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(natural.width * scale, natural.height * scale),
            ));
            slide.setPosition(CGPoint::new(cx + dx, cy + dy));
            slide.setTransform(slide_transform(
                self.ivars().rotation.get(),
                self.ivars().flipped.get(),
            ));
        });
        if shown {
            let crop = self.ivars().crop.borrow().clone();
            if let Some(crop) = crop {
                crop.set_image_rect(self.image_rect());
            }
        }
    }

    /// Where the picture sits in the view, turned as shown.
    pub fn image_rect(&self) -> CGRect {
        let bounds = self.bounds();
        let natural = self.ivars().natural.get();
        let scale = self.effective_scale(bounds.size);
        let (w, h) = if self.ivars().rotation.get() % 180 == 90 {
            (natural.height * scale, natural.width * scale)
        } else {
            (natural.width * scale, natural.height * scale)
        };
        let (dx, dy) = self.ivars().offset.get();
        let (cx, cy) = self.centre();
        CGRect::new(
            CGPoint::new(cx + dx - w / 2.0, cy + dy - h / 2.0),
            CGSize::new(w, h),
        )
    }

    /// The middle of the area a fitted picture fills.
    fn centre(&self) -> (f64, f64) {
        let bounds = self.bounds();
        let (top, right, bottom, left) = self.ivars().insets.get();
        (
            left + (bounds.size.width - left - right) / 2.0,
            bottom + (bounds.size.height - top - bottom) / 2.0,
        )
    }

    pub fn set_fit_insets(&self, insets: (f64, f64, f64, f64)) {
        self.ivars().insets.set(insets);
        self.layout_slide();
    }

    /// Crop mode: the overlay goes on top of the picture, under `below`.
    pub fn begin_crop(&self, crop: Retained<CropView>, below: &NSView) {
        self.addSubview_positioned_relativeTo(
            &crop,
            objc2_app_kit::NSWindowOrderingMode::Below,
            Some(below),
        );
        *self.ivars().crop.borrow_mut() = Some(crop);
    }

    pub fn end_crop(&self) -> Option<Retained<CropView>> {
        let crop = self.ivars().crop.borrow_mut().take();
        if let Some(crop) = &crop {
            crop.removeFromSuperview();
            self.set_fit_insets((0.0, 0.0, 0.0, 0.0));
        }
        crop
    }

    pub fn crop_view(&self) -> Option<Retained<CropView>> {
        self.ivars().crop.borrow().clone()
    }

    /// Upscale comparison preview: mounted over the slide, beneath the floating controls.
    pub fn begin_compare(&self, compare: Retained<CompareView>, below: &NSView) {
        self.addSubview_positioned_relativeTo(
            &compare,
            objc2_app_kit::NSWindowOrderingMode::Below,
            Some(below),
        );
        *self.ivars().compare.borrow_mut() = Some(compare);
    }

    pub fn end_compare(&self) -> Option<Retained<CompareView>> {
        let compare = self.ivars().compare.borrow_mut().take();
        if let Some(compare) = &compare {
            compare.removeFromSuperview();
        }
        compare
    }

    pub fn compare_view(&self) -> Option<Retained<CompareView>> {
        self.ivars().compare.borrow().clone()
    }

    /// Fit the window, keeping the turns and flip.
    pub fn fit(&self) {
        self.ivars().zoom.set(None);
        self.ivars().offset.set((0.0, 0.0));
        self.notify_state_change();
    }

    /// Whether the slide is an animation playing.
    pub fn is_animated(&self) -> bool {
        self.ivars().frames.borrow().len() >= 2
    }

    /// Re-lay out after an outside change, such as the scale-up preference.
    pub fn refresh(&self) {
        self.layout_slide();
    }

    /// The fit scale; small images stay at 100% unless scale-up is on.
    fn fit_scale(&self, bounds: CGSize) -> f64 {
        let (top, right, bottom, left) = self.ivars().insets.get();
        let bounds = CGSize::new(
            (bounds.width - left - right).max(1.0),
            (bounds.height - top - bottom).max(1.0),
        );
        let natural = self.ivars().natural.get();
        let (iw, ih) = if self.ivars().rotation.get() % 180 == 90 {
            (natural.height, natural.width)
        } else {
            (natural.width, natural.height)
        };
        if iw <= 0.0 || ih <= 0.0 {
            return 1.0;
        }
        let fit = (bounds.width / iw).min(bounds.height / ih);
        let scales_up = self
            .ivars()
            .delegate
            .get()
            .is_some_and(|delegate| delegate.scales_small_images_up());
        if scales_up {
            fit
        } else {
            fit.min(1.0)
        }
    }

    fn effective_scale(&self, bounds: CGSize) -> f64 {
        match self.ivars().zoom.get() {
            Some(zoom) => zoom,
            None => self.fit_scale(bounds),
        }
    }

    #[cfg(any(test, feature = "e2e"))]
    pub fn drawn_scale(&self) -> f64 {
        if self.ivars().image.borrow().is_none() {
            return 0.0;
        }
        self.effective_scale(self.bounds().size)
    }

    pub fn natural_size(&self) -> CGSize {
        self.ivars().natural.get()
    }

    pub fn decoded_long_edge(&self) -> f64 {
        self.ivars()
            .image
            .borrow()
            .as_ref()
            .map(|image| {
                let size = image.size();
                size.width.max(size.height)
            })
            .unwrap_or(0.0)
    }

    pub fn rotate(&self, delta_degrees: i32) {
        let next = (self.ivars().rotation.get() + delta_degrees).rem_euclid(360);
        self.ivars().rotation.set(next);
        self.notify_state_change();
    }

    /// Horizontal flip in screen space. Since `slide_transform` flips before
    /// rotating, composing a screen-space horizontal flip negates `rotation`
    /// (`(-r).rem_euclid(360)`), matching `Orientation::then`.
    pub fn flip(&self) {
        let next_rot = (-self.ivars().rotation.get()).rem_euclid(360);
        self.ivars().rotation.set(next_rot);
        self.ivars().flipped.set(!self.ivars().flipped.get());
        self.notify_state_change();
    }

    fn clamp_offset(&self, (dx, dy): (f64, f64)) -> (f64, f64) {
        if self.ivars().zoom.get().is_none() {
            return (0.0, 0.0);
        }
        let bounds = self.bounds().size;
        let natural = self.ivars().natural.get();
        let scale = self.effective_scale(bounds);
        let (iw, ih) = if self.ivars().rotation.get() % 180 == 90 {
            (natural.height * scale, natural.width * scale)
        } else {
            (natural.width * scale, natural.height * scale)
        };
        let max_x = ((iw - bounds.width) / 2.0).max(0.0);
        let max_y = ((ih - bounds.height) / 2.0).max(0.0);
        (dx.clamp(-max_x, max_x), dy.clamp(-max_y, max_y))
    }

    fn notify_state_change(&self) {
        self.layout_slide();
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.slide_state_changed();
        }
    }

    fn notify_pan_change(&self) {
        self.layout_slide();
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.slide_pan_changed();
        }
    }

    pub fn state(&self) -> SlideState {
        SlideState {
            zoom: self.ivars().zoom.get(),
            rotation: self.ivars().rotation.get(),
            flipped: self.ivars().flipped.get(),
            offset: self.ivars().offset.get(),
        }
    }

    pub fn apply_state(&self, state: SlideState) {
        self.ivars().zoom.set(state.zoom);
        self.ivars().rotation.set(state.rotation);
        self.ivars().flipped.set(state.flipped);
        self.ivars().offset.set(state.offset);
        self.layout_slide();
    }

    fn set_zoom(&self, zoom: Option<f64>) {
        self.ivars().zoom.set(zoom);
        if zoom.is_none() {
            self.ivars().offset.set((0.0, 0.0));
        }
        self.notify_state_change();
    }

    /// Zoom to `next` while keeping `anchor` fixed in view coordinates (`None` = center).
    /// Drops back to fit mode (clearing pan) when reaching fit scale.
    pub fn zoom_to(&self, next: f64, anchor: Option<CGPoint>) {
        if self.ivars().image.borrow().is_none() {
            return;
        }
        let bounds = self.bounds();
        let current = self.effective_scale(bounds.size);
        if current <= 0.0 {
            return;
        }
        let Some(next) = settle_zoom(next, self.fit_scale(bounds.size)) else {
            self.set_zoom(None);
            return;
        };
        // Scale the pan by the same factor as the zoom.
        let factor = next / current;
        let (ax, ay) = match anchor {
            Some(point) => (
                point.x - (bounds.origin.x + bounds.size.width / 2.0),
                point.y - (bounds.origin.y + bounds.size.height / 2.0),
            ),
            None => (0.0, 0.0),
        };
        let (dx, dy) = self.ivars().offset.get();
        self.ivars().zoom.set(Some(next));
        let next_offset = self.clamp_offset((
            ax * (1.0 - factor) + factor * dx,
            ay * (1.0 - factor) + factor * dy,
        ));
        self.ivars().offset.set(next_offset);
        self.notify_state_change();
    }

    pub fn pointer(&self) -> Option<CGPoint> {
        let window = self.window()?;
        let point = self.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
        let bounds = self.bounds();
        let inside = point.x >= bounds.origin.x
            && point.y >= bounds.origin.y
            && point.x <= bounds.origin.x + bounds.size.width
            && point.y <= bounds.origin.y + bounds.size.height;
        inside.then_some(point)
    }

    pub fn zoom_step(&self, direction: i32) {
        if self.ivars().image.borrow().is_none() {
            return;
        }
        let current = self.effective_scale(self.bounds().size);
        self.zoom_to(ladder_step(current, direction), self.pointer());
    }

    pub fn has_image(&self) -> bool {
        self.ivars().image.borrow().is_some()
    }

    /// Clear the displayed slide and stop any active animation (e.g. on decode failure).
    pub fn clear_image(&self) {
        self.stop_animation();
        self.set_image(None);
        self.ivars().natural.set(CGSize::new(0.0, 0.0));
        self.layout_slide();
    }

    /// A new slide, at default state; the delegate reapplies saved state.
    pub fn show_image(&self, image: Retained<NSImage>, natural: CGSize) {
        self.stop_animation();
        self.set_image(Some(image));
        self.ivars().natural.set(natural);
        self.apply_state(SlideState::default());
    }

    /// A bigger decode of the slide on screen; only sharpness changes.
    pub fn swap_decode(&self, image: Retained<NSImage>) {
        self.set_image(Some(image));
    }

    /// A new animated slide: frame 0 now, the rest on their delays.
    pub fn show_frames(&self, frames: Vec<(Retained<NSImage>, f64)>, natural: CGSize) {
        self.stop_animation();
        let Some((first, delay)) = frames.first().cloned() else {
            return;
        };
        self.set_image(Some(first));
        self.ivars().natural.set(natural);
        *self.ivars().frames.borrow_mut() = frames;
        self.ivars().frame_index.set(0);
        self.apply_state(SlideState::default());
        self.schedule_frame(delay);
    }

    /// The frame timer retains the view, so stop it to let the view go.
    pub fn stop_animation(&self) {
        if let Some(timer) = self.ivars().anim_timer.borrow_mut().take() {
            timer.invalidate();
        }
        self.ivars().frames.borrow_mut().clear();
        self.ivars().frame_index.set(0);
    }

    fn schedule_frame(&self, delay: f64) {
        let timer = unsafe {
            objc2_foundation::NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                delay,
                self,
                objc2::sel!(advanceFrame:),
                None,
                false,
            )
        };
        unsafe {
            objc2_foundation::NSRunLoop::mainRunLoop()
                .addTimer_forMode(&timer, objc2_foundation::NSRunLoopCommonModes)
        };
        *self.ivars().anim_timer.borrow_mut() = Some(timer);
    }

    #[cfg(any(test, feature = "e2e"))]
    pub fn is_animating(&self) -> bool {
        self.ivars().frames.borrow().len() >= 2 && self.ivars().anim_timer.borrow().is_some()
    }
}

/// Flip, then turn clockwise (the view's y axis points up).
fn slide_transform(rotation: i32, flipped: bool) -> CATransform3D {
    let turn = CATransform3D::new_rotation(-(rotation as f64).to_radians(), 0.0, 0.0, 1.0);
    if flipped {
        CATransform3D::new_scale(-1.0, 1.0, 1.0).concat(turn)
    } else {
        turn
    }
}

/// Disable Core Animation's implicit 0.25s transition for immediate pan/zoom updates.
fn without_animation(change: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    change();
    CATransaction::commit();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layer_turns_and_flips_like_the_old_drawing() {
        // Reference: flip x, then rotate by -angle in y-up coordinates.
        let drawn = |rotation: i32, flipped: bool, (x, y): (f64, f64)| {
            let x = if flipped { -x } else { x };
            let a = -(rotation as f64).to_radians();
            (x * a.cos() - y * a.sin(), x * a.sin() + y * a.cos())
        };
        // Core Animation multiplies row vectors: (x, y, 0, 1) * M.
        let layered = |rotation: i32, flipped: bool, (x, y): (f64, f64)| {
            let m = slide_transform(rotation, flipped);
            (x * m.m11 + y * m.m21 + m.m41, x * m.m12 + y * m.m22 + m.m42)
        };
        for rotation in [0, 90, 180, 270] {
            for flipped in [false, true] {
                for point in [(1.0, 0.0), (0.0, 1.0), (3.0, -2.0)] {
                    let (a, b) = (
                        drawn(rotation, flipped, point),
                        layered(rotation, flipped, point),
                    );
                    assert!(
                        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9,
                        "{rotation}° flipped {flipped}: {a:?} vs {b:?}"
                    );
                }
            }
        }
        // A quarter turn is clockwise on screen: up becomes right.
        let (x, y) = layered(90, false, (0.0, 1.0));
        assert!((x - 1.0).abs() < 1e-9 && y.abs() < 1e-9);
    }

    #[test]
    fn the_zoom_ladder_never_zooms_the_wrong_way() {
        // A panorama that fits at 0.05: minus stays at fit, a pinch may
        // stop below the lowest rung, and plus takes the first rung.
        assert_eq!(settle_zoom(ladder_step(0.05, -1), 0.05), None);
        assert_eq!(settle_zoom(ladder_step(0.1, -1), 0.05), None);
        assert_eq!(settle_zoom(0.08, 0.05), Some(0.08));
        assert_eq!(settle_zoom(ladder_step(0.05, 1), 0.05), Some(0.125));
        // An image that fits at 0.5 keeps the ladder above fit.
        assert_eq!(settle_zoom(ladder_step(1.0, -1), 0.5), Some(0.75));
        assert_eq!(settle_zoom(ladder_step(0.5, -1), 0.5), None);
        assert_eq!(settle_zoom(100.0, 0.5), Some(16.0));
    }

    #[test]
    fn screen_space_flip_matches_orientation_group() {
        use blickli_core::Orientation;
        for rot in [0, 90, 180, 270] {
            for flipped in [false, true] {
                let before = Orientation::from_view(rot, flipped);
                let expected = before.then(Orientation::from_view(0, true));
                let next_rot = (-rot).rem_euclid(360);
                let next_flipped = !flipped;
                assert_eq!(
                    Orientation::from_view(next_rot, next_flipped),
                    expected,
                    "flip after rot={rot} flipped={flipped}"
                );
            }
        }
    }

    #[test]
    fn scroll_tracker_steps_every_discrete_ratchet_and_accumulates_smooth_scroll() {
        let mut tracker = ScrollTracker::default();
        let t0 = std::time::Instant::now();
        let discrete = |now, step| ScrollSample {
            now,
            precise: false,
            has_phase: false,
            phase_began: false,
            phase_ended: false,
            is_momentum: false,
            step,
        };
        // Slow discrete wheel ratchets (~0.1 delta each) advance 1:1 immediately.
        assert_eq!(tracker.feed(discrete(t0, 0.1)), Some(1));
        assert_eq!(
            tracker.feed(discrete(t0 + std::time::Duration::from_millis(50), 0.1)),
            Some(1)
        );
        assert_eq!(
            tracker.feed(discrete(t0 + std::time::Duration::from_secs(1), -0.1)),
            Some(-1)
        );

        // Smooth-scroll mouse driver (precise, no touch phase) accumulates small deltas.
        let smooth = |now, step| ScrollSample {
            now,
            precise: true,
            has_phase: false,
            phase_began: false,
            phase_ended: false,
            is_momentum: false,
            step,
        };
        let t1 = t0 + std::time::Duration::from_secs(2);
        assert_eq!(tracker.feed(smooth(t1, 2.5)), None);
        assert_eq!(
            tracker.feed(smooth(t1 + std::time::Duration::from_millis(16), 2.5)),
            None
        );
        assert_eq!(
            tracker.feed(smooth(t1 + std::time::Duration::from_millis(32), 2.5)),
            Some(1)
        );
        // Remaining tail frames of that click are absorbed during the short gate.
        assert_eq!(
            tracker.feed(smooth(t1 + std::time::Duration::from_millis(48), 10.0)),
            None
        );
        // Immediate direction reversal is not suppressed by the same-direction gate.
        assert_eq!(
            tracker.feed(smooth(t1 + std::time::Duration::from_millis(60), -10.0)),
            Some(-1)
        );

        // Trackpad momentum coasting does not skip extra slides.
        let t2 = t1 + std::time::Duration::from_secs(1);
        assert_eq!(
            tracker.feed(ScrollSample {
                now: t2,
                precise: true,
                has_phase: true,
                phase_began: false,
                phase_ended: false,
                is_momentum: true,
                step: 40.0,
            }),
            None
        );
        assert_eq!(
            tracker.feed(ScrollSample {
                now: t2,
                precise: true,
                has_phase: true,
                phase_began: true,
                phase_ended: false,
                is_momentum: false,
                step: 20.0,
            }),
            Some(1)
        );
    }
}
