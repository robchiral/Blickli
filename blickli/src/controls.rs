//! Floating slideshow control bar and crop toolbar. Appears on pointer
//! movement and fades after inactivity.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{define_class, sel, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAppearance, NSAppearanceCustomization,
    NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions, NSBox, NSBoxType, NSButton, NSColor,
    NSControlSize, NSEvent, NSFont, NSImage, NSImageSymbolConfiguration, NSPopUpButton,
    NSProgressIndicator, NSProgressIndicatorStyle, NSSegmentStyle, NSSegmentSwitchTracking,
    NSSegmentedControl, NSTextField, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{ns_string, NSArray, NSString};

use crate::compare::CompareMode;
use crate::shortcuts::{Action, ShortcutStore};
use crate::upscale::UpscaleScale;
use crate::AppDelegate;

const BAR_H: f64 = 44.0;
const BUTTON_W: f64 = 34.0;
const BUTTON_H: f64 = 30.0;
const GAP: f64 = 14.0;
const PAD: f64 = 8.0;
const MARGIN: f64 = 20.0;
/// Bottom inset reserved for the control bar during crop mode.
pub const CLEARANCE: f64 = BAR_H + MARGIN + 16.0;

/// Crop aspect ratio presets (`width / height`; `0.0` uses the image's ratio, `None` is free).
pub const ASPECTS: [(&str, Option<f64>); 7] = [
    ("Free", None),
    ("Original", Some(0.0)),
    ("Square", Some(1.0)),
    ("4:3", Some(4.0 / 3.0)),
    ("3:2", Some(1.5)),
    ("16:9", Some(16.0 / 9.0)),
    ("5:4", Some(1.25)),
];

define_class!(
    // Swallow mouse/scroll events so clicks on the bar do not advance the slide underneath.
    #[unsafe(super(NSVisualEffectView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliControlBar"]
    pub struct ControlBar;

    impl ControlBar {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {}

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, _event: &NSEvent) {}

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

/// Button group; lower-priority groups hide first in narrow windows.
struct Group {
    views: Vec<Retained<NSView>>,
    width: f64,
    priority: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarMode {
    Normal,
    Crop,
    Upscaling,
    UpscalePreview,
}

pub struct Controls {
    pub bar: Retained<ControlBar>,
    normal: Vec<Group>,
    separators: Vec<Retained<NSBox>>,
    crop_row: Vec<Retained<NSView>>,
    upscaling_row: Vec<Retained<NSView>>,
    upscale_preview_row: Vec<Retained<NSView>>,
    play: Retained<NSButton>,
    move_again: Retained<NSButton>,
    tooltip_buttons: Vec<(Action, &'static str, Option<&'static str>, Retained<NSButton>)>,
    pub size_label: Retained<NSTextField>,
    pub aspect: Retained<NSPopUpButton>,
    upscale_spinner: Retained<NSProgressIndicator>,
    pub upscale_status: Retained<NSTextField>,
    pub compare_mode: Retained<NSSegmentedControl>,
    pub upscale_scale: Retained<NSSegmentedControl>,
    pub upscale_dims: Retained<NSTextField>,
    mode: Cell<BarMode>,
    playing: Cell<bool>,
    shown: Cell<bool>,
}

fn as_view(view: &NSView) -> Retained<NSView> {
    view.retain()
}

fn symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(description)),
    )?;
    let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(15.0, 0.23);
    Some(image.imageWithSymbolConfiguration(&config).unwrap_or(image))
}

fn icon_button(
    mtm: MainThreadMarker,
    delegate: &AppDelegate,
    names: &[&str],
    tip: &str,
    action: Sel,
) -> Retained<NSButton> {
    let image = names.iter().find_map(|name| symbol(name, tip));
    let button = match image {
        Some(image) => unsafe {
            NSButton::buttonWithImage_target_action(&image, Some(delegate), Some(action), mtm)
        },
        None => unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(tip),
                Some(delegate),
                Some(action),
                mtm,
            )
        },
    };
    button.setBezelStyle(objc2_app_kit::NSBezelStyle::AccessoryBar);
    button.setShowsBorderOnlyWhileMouseInside(true);
    button.setContentTintColor(Some(&NSColor::colorWithWhite_alpha(1.0, 0.92)));
    button.setToolTip(Some(&NSString::from_str(tip)));
    // Keep keyboard focus on the slide view even with Full Keyboard Access enabled.
    button.setRefusesFirstResponder(true);
    button.setFrame(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(BUTTON_W, BUTTON_H),
    ));
    button
}

fn text_button(
    mtm: MainThreadMarker,
    delegate: &AppDelegate,
    title: &str,
    action: Sel,
) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(delegate),
            Some(action),
            mtm,
        )
    };
    button.setRefusesFirstResponder(true);
    button.sizeToFit();
    let size = button.frame().size;
    button.setFrame(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(size.width.max(72.0), size.height),
    ));
    button
}

impl Controls {
    pub fn new(mtm: MainThreadMarker, delegate: &AppDelegate, store: &ShortcutStore) -> Self {
        let bar: Retained<ControlBar> = {
            let this = ControlBar::alloc(mtm).set_ivars(());
            unsafe { objc2::msg_send![super(this), initWithFrame: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(400.0, BAR_H))] }
        };
        bar.setMaterial(NSVisualEffectMaterial::HUDWindow);
        bar.setBlendingMode(NSVisualEffectBlendingMode::WithinWindow);
        bar.setState(NSVisualEffectState::Active);
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            bar.setAppearance(Some(&dark));
        }
        bar.setWantsLayer(true);
        if let Some(layer) = bar.layer() {
            layer.setCornerRadius(12.0);
            layer.setMasksToBounds(true);
        }
        bar.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin | NSAutoresizingMaskOptions::ViewMaxYMargin);

        let mut tooltip_buttons: Vec<(Action, &'static str, Option<&'static str>, Retained<NSButton>)> =
            Vec::new();
        let mut bound = |names: &[&str],
                         base: &'static str,
                         fallback: Option<&'static str>,
                         shortcut: Action,
                         action: Sel| {
            let tip = store.format_tooltip(base, shortcut, fallback);
            let btn = icon_button(mtm, delegate, names, &tip, action);
            tooltip_buttons.push((shortcut, base, fallback, btn.clone()));
            as_view(&btn)
        };
        let play_tip = store.format_tooltip("Play", Action::SlidePlayPause, None);
        let play = icon_button(mtm, delegate, &["play.fill"], &play_tip, sel!(slidePlayPause:));
        let move_again_title = delegate.repeat_title(blickli_core::transfer::Transfer::Move);
        let move_again_tip = store.format_tooltip(&move_again_title, Action::MoveAgain, None);
        let move_again = icon_button(
            mtm,
            delegate,
            &["arrow.forward.folder", "arrow.uturn.forward"],
            &move_again_tip,
            sel!(moveAgain:),
        );
        move_again.setEnabled(delegate.has_last_move());
        let fit = as_view(&icon_button(
            mtm,
            delegate,
            &[
                "arrow.up.left.and.down.right.magnifyingglass",
                "arrow.up.left.and.arrow.down.right",
            ],
            "Fit to window",
            sel!(slideZoomFit:),
        ));
        let more = as_view(&icon_button(mtm, delegate, &["ellipsis.circle"], "More", sel!(slideMore:)));
        let groups: Vec<(u8, Vec<Retained<NSView>>)> = vec![
            (
                9,
                vec![
                    bound(&["chevron.left"], "Previous", Some("←"), Action::SlidePrev, sel!(slidePrevious:)),
                    as_view(&play),
                    bound(&["chevron.right"], "Next", Some("→"), Action::SlideNext, sel!(slideNext:)),
                ],
            ),
            (
                3,
                vec![
                    bound(&["minus.magnifyingglass"], "Zoom out", None, Action::SlideZoomOut, sel!(slideZoomOut:)),
                    fit,
                    bound(&["plus.magnifyingglass"], "Zoom in", None, Action::SlideZoomIn, sel!(slideZoomIn:)),
                ],
            ),
            (
                7,
                vec![
                    bound(&["rotate.left"], "Rotate left", None, Action::SlideRotateCcw, sel!(rotateLeft:)),
                    bound(&["rotate.right"], "Rotate right", None, Action::SlideRotateCw, sel!(rotateRight:)),
                    bound(
                        &["arrow.left.and.right.righttriangle.left.righttriangle.right", "flip.horizontal"],
                        "Flip",
                        None,
                        Action::SlideFlip,
                        sel!(flipHorizontal:),
                    ),
                    bound(&["crop"], "Crop", None, Action::SlideCrop, sel!(cropImage:)),
                    bound(
                        &["sparkles", "arrow.up.left.and.arrow.down.right"],
                        "Upscale",
                        None,
                        Action::SlideUpscale,
                        sel!(upscaleImage:),
                    ),
                ],
            ),
            (
                6,
                vec![
                    bound(&["folder"], "Move to…", None, Action::MoveToFolder, sel!(moveToFolder:)),
                    as_view(&move_again),
                    bound(&["trash"], "Move to Trash", None, Action::MoveToTrash, sel!(moveToTrash:)),
                    bound(&["info.circle"], "Info", None, Action::GetInfo, sel!(getInfo:)),
                    more,
                ],
            ),
            (10, vec![bound(&["xmark"], "Close slideshow", Some("Esc"), Action::SlideClose, sel!(endSlideshow:))]),
        ];
        let mut normal = Vec::new();
        let mut separators = Vec::new();
        for (index, (priority, views)) in groups.into_iter().enumerate() {
            for view in &views {
                bar.addSubview(view);
            }
            if index > 0 {
                let line = NSBox::new(mtm);
                line.setBoxType(NSBoxType::Separator);
                bar.addSubview(&line);
                separators.push(line);
            }
            let width = views.iter().map(|v| v.frame().size.width).sum();
            normal.push(Group {
                views,
                width,
                priority,
            });
        }

        let aspect = NSPopUpButton::new(mtm);
        for (title, _) in ASPECTS {
            aspect.addItemWithTitle(&NSString::from_str(title));
        }
        unsafe {
            aspect.setTarget(Some(delegate));
            aspect.setAction(Some(sel!(cropAspect:)));
        }
        aspect.sizeToFit();
        aspect.setRefusesFirstResponder(true);
        aspect.setToolTip(Some(ns_string!("Aspect ratio")));
        let size_label = NSTextField::labelWithString(ns_string!(""), mtm);
        size_label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.0)));
        size_label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        size_label.setAlignment(objc2_app_kit::NSTextAlignment::Center);
        size_label.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(108.0, 16.0)));
        let crop_button = text_button(mtm, delegate, "Crop", sel!(cropSave:));
        crop_button.setKeyEquivalent(ns_string!("\r"));
        crop_button.setToolTip(Some(ns_string!("Apply crop (Return)")));
        let copy_button = text_button(mtm, delegate, "Save Copy", sel!(cropSaveCopy:));
        copy_button.setToolTip(Some(ns_string!("Save as copy (⌥Return)")));
        let cancel = text_button(mtm, delegate, "Cancel", sel!(cropCancel:));
        cancel.setToolTip(Some(ns_string!("Cancel (Esc)")));
        let crop_row: Vec<Retained<NSView>> = vec![
            as_view(&aspect),
            as_view(&size_label),
            as_view(&cancel),
            as_view(&copy_button),
            as_view(&crop_button),
        ];
        for view in &crop_row {
            view.setHidden(true);
            bar.addSubview(view);
        }

        // Upscaling progress row: spinner, tile progress text, Cancel.
        let upscale_spinner = NSProgressIndicator::initWithFrame(
            NSProgressIndicator::alloc(mtm),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(16.0, 16.0)),
        );
        upscale_spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        upscale_spinner.setControlSize(NSControlSize::Small);
        upscale_spinner.setDisplayedWhenStopped(false);
        let upscale_status = NSTextField::labelWithString(ns_string!("Upscaling…"), mtm);
        upscale_status
            .setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.0)));
        upscale_status.setTextColor(Some(&NSColor::colorWithWhite_alpha(1.0, 0.9)));
        upscale_status.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(190.0, 16.0)));
        let upscale_cancel_running = text_button(mtm, delegate, "Cancel", sel!(upscaleCancel:));
        upscale_cancel_running.setToolTip(Some(ns_string!("Cancel upscaling (Esc)")));
        let upscaling_row: Vec<Retained<NSView>> = vec![
            as_view(&upscale_spinner),
            as_view(&upscale_status),
            as_view(&upscale_cancel_running),
        ];
        for view in &upscaling_row {
            view.setHidden(true);
            bar.addSubview(view);
        }

        // Upscale preview row: Split/Side toggle, 2×/4× toggle, dimensions, Cancel, Save Copy, Replace.
        let compare_labels = NSArray::from_retained_slice(&[
            NSString::from_str("Split"),
            NSString::from_str("Side"),
        ]);
        let compare_mode = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &compare_labels,
                NSSegmentSwitchTracking::SelectOne,
                Some(delegate),
                Some(sel!(upscaleCompareMode:)),
                mtm,
            )
        };
        compare_mode.setSegmentStyle(NSSegmentStyle::Rounded);
        compare_mode.setSelectedSegment(0);
        compare_mode.setToolTip_forSegment(Some(ns_string!("Split wipe (s)")), 0);
        compare_mode.setToolTip_forSegment(Some(ns_string!("Side by side (s)")), 1);
        compare_mode.setRefusesFirstResponder(true);
        compare_mode.sizeToFit();

        let scale_labels = NSArray::from_retained_slice(&[
            NSString::from_str("2×"),
            NSString::from_str("4×"),
        ]);
        let upscale_scale = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &scale_labels,
                NSSegmentSwitchTracking::SelectOne,
                Some(delegate),
                Some(sel!(upscalePreviewScale:)),
                mtm,
            )
        };
        upscale_scale.setSegmentStyle(NSSegmentStyle::Rounded);
        upscale_scale.setSelectedSegment(1);
        upscale_scale.setToolTip_forSegment(Some(ns_string!("2× scale (2)")), 0);
        upscale_scale.setToolTip_forSegment(Some(ns_string!("4× scale (4)")), 1);
        upscale_scale.setRefusesFirstResponder(true);
        upscale_scale.sizeToFit();

        let upscale_dims = NSTextField::labelWithString(ns_string!(""), mtm);
        upscale_dims
            .setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.0)));
        upscale_dims.setTextColor(Some(&NSColor::secondaryLabelColor()));
        upscale_dims.setAlignment(objc2_app_kit::NSTextAlignment::Center);
        upscale_dims.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(168.0, 16.0)));

        let upscale_cancel = text_button(mtm, delegate, "Cancel", sel!(upscaleCancel:));
        upscale_cancel.setToolTip(Some(ns_string!("Discard preview (Esc)")));
        let upscale_copy = text_button(mtm, delegate, "Save Copy", sel!(upscaleSaveCopy:));
        upscale_copy.setToolTip(Some(ns_string!("Save as copy (⌥Return)")));
        let upscale_replace = text_button(mtm, delegate, "Replace", sel!(upscaleSave:));
        upscale_replace.setKeyEquivalent(ns_string!("\r"));
        upscale_replace.setToolTip(Some(ns_string!("Replace original (Return)")));

        let upscale_preview_row: Vec<Retained<NSView>> = vec![
            as_view(&compare_mode),
            as_view(&upscale_scale),
            as_view(&upscale_dims),
            as_view(&upscale_cancel),
            as_view(&upscale_copy),
            as_view(&upscale_replace),
        ];
        for view in &upscale_preview_row {
            view.setHidden(true);
            bar.addSubview(view);
        }

        Self {
            bar,
            normal,
            separators,
            crop_row,
            upscaling_row,
            upscale_preview_row,
            play,
            move_again,
            tooltip_buttons,
            size_label,
            aspect,
            upscale_spinner,
            upscale_status,
            compare_mode,
            upscale_scale,
            upscale_dims,
            mode: Cell::new(BarMode::Normal),
            playing: Cell::new(false),
            shown: Cell::new(true),
        }
    }

    /// Position the bar centered along the bottom edge of `size`.
    pub fn layout(&self, size: CGSize) {
        let room = size.width - 2.0 * MARGIN;
        let width = match self.mode.get() {
            BarMode::Normal => self.layout_buttons(room),
            BarMode::Crop => self.layout_row(&self.crop_row, Some(2), room, Some(1)),
            BarMode::Upscaling => self.layout_row(&self.upscaling_row, Some(2), room, None),
            BarMode::UpscalePreview => {
                // Hide the dimensions label (index 2) first if a narrow window cannot fit the full row.
                self.layout_row(&self.upscale_preview_row, Some(3), room, Some(2))
            }
        };
        self.bar.setFrame(CGRect::new(
            CGPoint::new(((size.width - width) / 2.0).round(), MARGIN),
            CGSize::new(width, BAR_H),
        ));
    }

    fn layout_buttons(&self, room: f64) -> f64 {
        let mut kept: Vec<bool> = vec![true; self.normal.len()];
        let width_of = |kept: &[bool]| {
            let groups: Vec<&Group> = self
                .normal
                .iter()
                .zip(kept)
                .filter_map(|(g, &k)| k.then_some(g))
                .collect();
            groups.iter().map(|g| g.width).sum::<f64>()
                + GAP * groups.len().saturating_sub(1) as f64
                + 2.0 * PAD
        };
        let mut order: Vec<usize> = (0..self.normal.len()).collect();
        order.sort_by_key(|&i| self.normal[i].priority);
        for index in order {
            if width_of(&kept) <= room {
                break;
            }
            if self.normal[index].priority < 9 {
                kept[index] = false;
            }
        }
        let mut x = PAD;
        let mut first = true;
        for (index, group) in self.normal.iter().enumerate() {
            let show = kept[index];
            if index > 0 {
                let line = &self.separators[index - 1];
                line.setHidden(!show || first);
                if show && !first {
                    line.setFrame(CGRect::new(
                        CGPoint::new(x + GAP / 2.0 - 0.5, (BAR_H - 20.0) / 2.0),
                        CGSize::new(1.0, 20.0),
                    ));
                }
            }
            for view in &group.views {
                view.setHidden(!show);
            }
            if !show {
                continue;
            }
            if !first {
                x += GAP;
            }
            first = false;
            for view in &group.views {
                let size = view.frame().size;
                view.setFrameOrigin(CGPoint::new(x, ((BAR_H - size.height) / 2.0).round()));
                x += size.width;
            }
        }
        x + PAD
    }

    fn layout_row(
        &self,
        row: &[Retained<NSView>],
        gap_before: Option<usize>,
        room: f64,
        optional_index: Option<usize>,
    ) -> f64 {
        let measure = |skip: Option<usize>, item_gap: f64, section_gap: f64, pad: f64| {
            let mut x = pad;
            let mut first = true;
            for (index, view) in row.iter().enumerate() {
                if skip == Some(index) {
                    continue;
                }
                if !first {
                    x += item_gap;
                }
                if gap_before == Some(index) && !first {
                    x += section_gap;
                }
                first = false;
                x += view.frame().size.width;
            }
            x + pad
        };

        let mut skip = None;
        let mut item_gap = 6.0;
        let mut section_gap = GAP;
        let mut pad = PAD + 4.0;
        if measure(skip, item_gap, section_gap, pad) > room {
            if let Some(opt) = optional_index {
                skip = Some(opt);
            }
        }
        if measure(skip, item_gap, section_gap, pad) > room {
            item_gap = 4.0;
            section_gap = 8.0;
            pad = PAD;
        }

        let mut x = pad;
        let mut first = true;
        for (index, view) in row.iter().enumerate() {
            let hidden = skip == Some(index);
            view.setHidden(hidden);
            if hidden {
                continue;
            }
            if !first {
                x += item_gap;
            }
            if gap_before == Some(index) && !first {
                x += section_gap;
            }
            first = false;
            let size = view.frame().size;
            view.setFrameOrigin(CGPoint::new(x, ((BAR_H - size.height) / 2.0).round()));
            x += size.width;
        }
        x + pad
    }

    pub fn set_mode(&self, mode: BarMode, size: CGSize) {
        self.mode.set(mode);
        let normal = mode == BarMode::Normal;
        for group in &self.normal {
            for view in &group.views {
                view.setHidden(!normal);
            }
        }
        for line in &self.separators {
            line.setHidden(!normal);
        }
        for view in &self.crop_row {
            view.setHidden(mode != BarMode::Crop);
        }
        for view in &self.upscaling_row {
            view.setHidden(mode != BarMode::Upscaling);
        }
        for view in &self.upscale_preview_row {
            view.setHidden(mode != BarMode::UpscalePreview);
        }
        unsafe {
            if mode == BarMode::Upscaling {
                self.upscale_spinner.startAnimation(None);
            } else {
                self.upscale_spinner.stopAnimation(None);
            }
        }
        if mode == BarMode::Crop {
            self.aspect.selectItemAtIndex(0);
        }
        self.layout(size);
    }

    pub fn sync_move_again(&self, title: &str, enabled: bool, store: &ShortcutStore) {
        self.move_again.setEnabled(enabled);
        let tip = store.format_tooltip(title, Action::MoveAgain, None);
        self.move_again.setToolTip(Some(&NSString::from_str(&tip)));
    }

    pub fn sync_tooltips(&self, store: &ShortcutStore, move_again_title: &str) {
        for &(action, base, fallback, ref button) in &self.tooltip_buttons {
            let tip = store.format_tooltip(base, action, fallback);
            button.setToolTip(Some(&NSString::from_str(&tip)));
        }
        let play_base = if self.playing.get() { "Pause" } else { "Play" };
        let play_tip = store.format_tooltip(play_base, Action::SlidePlayPause, None);
        self.play.setToolTip(Some(&NSString::from_str(&play_tip)));
        let move_tip = store.format_tooltip(move_again_title, Action::MoveAgain, None);
        self.move_again
            .setToolTip(Some(&NSString::from_str(&move_tip)));
    }

    pub fn set_compare_mode(&self, mode: CompareMode) {
        self.compare_mode.setSelectedSegment(match mode {
            CompareMode::Split => 0,
            CompareMode::SideBySide => 1,
        });
    }

    pub fn set_upscale_scale(&self, scale: UpscaleScale) {
        self.upscale_scale.setSelectedSegment(match scale {
            UpscaleScale::X2 => 0,
            UpscaleScale::X4 => 1,
        });
    }

    pub fn cropping(&self) -> bool {
        self.mode.get() == BarMode::Crop
    }

    pub fn active_edit(&self) -> bool {
        self.mode.get() != BarMode::Normal
    }

    pub fn set_playing(&self, playing: bool, store: &ShortcutStore) {
        if self.playing.replace(playing) == playing {
            return;
        }
        let (name, base) = if playing {
            ("pause.fill", "Pause")
        } else {
            ("play.fill", "Play")
        };
        let tip = store.format_tooltip(base, Action::SlidePlayPause, None);
        if let Some(image) = symbol(name, &tip) {
            self.play.setImage(Some(&image));
        }
        self.play.setToolTip(Some(&NSString::from_str(&tip)));
    }

    /// Animate visibility; call `finish_fade` after fading out to hide the bar from hit-testing.
    pub fn set_shown(&self, shown: bool) -> bool {
        if self.shown.replace(shown) == shown {
            return false;
        }
        if shown {
            self.bar.setHidden(false);
        }
        self.bar
            .animator()
            .setAlphaValue(if shown { 1.0 } else { 0.0 });
        true
    }

    pub fn finish_fade(&self) {
        if !self.shown.get() {
            self.bar.setHidden(true);
        }
    }

    /// Immediately show or hide the bar without animation.
    pub fn set_enabled(&self, enabled: bool) {
        self.bar.setHidden(!enabled);
        self.bar.setAlphaValue(if enabled { 1.0 } else { 0.0 });
        self.shown.set(enabled);
    }

    pub fn contains(&self, point: CGPoint) -> bool {
        let frame = self.bar.frame();
        self.shown.get()
            && point.x >= frame.origin.x
            && point.x <= frame.origin.x + frame.size.width
            && point.y >= frame.origin.y
            && point.y <= frame.origin.y + frame.size.height
    }
}
