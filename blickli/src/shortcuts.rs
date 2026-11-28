//! Customizable keyboard shortcuts, persistence, conflict checks, and recorder control.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSEvent, NSEventModifierFlags, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSMutableParagraphStyle, NSParagraphStyleAttributeName,
    NSStringDrawing, NSTextAlignment, NSView,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    ns_string, NSDictionary, NSMutableDictionary, NSObjectProtocol, NSRect, NSString,
    NSUserDefaults,
};

use crate::AppDelegate;

pub const MOD_CTRL: u8 = 1 << 0;
pub const MOD_OPT: u8 = 1 << 1;
pub const MOD_SHIFT: u8 = 1 << 2;
pub const MOD_CMD: u8 = 1 << 3;

const PREF_KEY: &str = "customShortcuts";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Menu,
    Slideshow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    File,
    EditView,
    Image,
    SlideshowMenu,
    SlideshowKeys,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Section::File,
        Section::EditView,
        Section::Image,
        Section::SlideshowMenu,
        Section::SlideshowKeys,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Section::File => "File",
            Section::EditView => "Edit & View",
            Section::Image => "Image",
            Section::SlideshowMenu => "Slideshow Menu",
            Section::SlideshowKeys => "Slideshow Keys",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    // File
    OpenFolder,
    RevealInFinder,
    GetInfo,
    SetDesktopPicture,
    MoveToTrash,
    MoveToFolder,
    CopyToFolder,
    MoveAgain,
    CopyAgain,
    // Edit & View
    FilterByName,
    CopyPath,
    CopyFileUrl,
    SortByName,
    SortByModified,
    SortBySize,
    SortByPath,
    SortByExifDate,
    SortByAdded,
    LargerThumbs,
    SmallerThumbs,
    ScaleUpSmall,
    ToggleSidebar,
    ToggleLabels,
    // Image
    RotateLeft,
    RotateRight,
    FlipHorizontal,
    CropImage,
    UpscaleImage,
    CompressImage,
    CompressFolder,
    // Slideshow Menu
    StartSlideshow,
    ToggleLoop,
    ToggleShuffle,
    ToggleControls,
    // Slideshow Keys
    SlideNext,
    SlidePrev,
    SlidePlayPause,
    SlideClose,
    SlideRotateCw,
    SlideRotateCcw,
    SlideFlip,
    SlideCrop,
    SlideUpscale,
    SlideZoomIn,
    SlideZoomOut,
    SlideZoomActual,
    SlideZoomReset,
    SlideToggleControls,
    SlideToggleOverlay,
    SlideToggleExif,
    SlideTogglePath,
    SlideToggleHelp,
}

impl Action {
    pub const ALL: [Action; 52] = [
        Action::OpenFolder,
        Action::RevealInFinder,
        Action::GetInfo,
        Action::SetDesktopPicture,
        Action::MoveToTrash,
        Action::MoveToFolder,
        Action::CopyToFolder,
        Action::MoveAgain,
        Action::CopyAgain,
        Action::FilterByName,
        Action::CopyPath,
        Action::CopyFileUrl,
        Action::SortByName,
        Action::SortByModified,
        Action::SortBySize,
        Action::SortByPath,
        Action::SortByExifDate,
        Action::SortByAdded,
        Action::LargerThumbs,
        Action::SmallerThumbs,
        Action::ScaleUpSmall,
        Action::ToggleSidebar,
        Action::ToggleLabels,
        Action::RotateLeft,
        Action::RotateRight,
        Action::FlipHorizontal,
        Action::CropImage,
        Action::UpscaleImage,
        Action::CompressImage,
        Action::CompressFolder,
        Action::StartSlideshow,
        Action::ToggleLoop,
        Action::ToggleShuffle,
        Action::ToggleControls,
        Action::SlideNext,
        Action::SlidePrev,
        Action::SlidePlayPause,
        Action::SlideClose,
        Action::SlideRotateCw,
        Action::SlideRotateCcw,
        Action::SlideFlip,
        Action::SlideCrop,
        Action::SlideUpscale,
        Action::SlideZoomIn,
        Action::SlideZoomOut,
        Action::SlideZoomActual,
        Action::SlideZoomReset,
        Action::SlideToggleControls,
        Action::SlideToggleOverlay,
        Action::SlideToggleExif,
        Action::SlideTogglePath,
        Action::SlideToggleHelp,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Action::OpenFolder => "open_folder",
            Action::RevealInFinder => "reveal_in_finder",
            Action::GetInfo => "get_info",
            Action::SetDesktopPicture => "set_desktop_picture",
            Action::MoveToTrash => "move_to_trash",
            Action::MoveToFolder => "move_to_folder",
            Action::CopyToFolder => "copy_to_folder",
            Action::MoveAgain => "move_again",
            Action::CopyAgain => "copy_again",
            Action::FilterByName => "filter_by_name",
            Action::CopyPath => "copy_path",
            Action::CopyFileUrl => "copy_file_url",
            Action::SortByName => "sort_by_name",
            Action::SortByModified => "sort_by_modified",
            Action::SortBySize => "sort_by_size",
            Action::SortByPath => "sort_by_path",
            Action::SortByExifDate => "sort_by_exif_date",
            Action::SortByAdded => "sort_by_added",
            Action::LargerThumbs => "larger_thumbs",
            Action::SmallerThumbs => "smaller_thumbs",
            Action::ScaleUpSmall => "scale_up_small",
            Action::ToggleSidebar => "toggle_sidebar",
            Action::ToggleLabels => "toggle_labels",
            Action::RotateLeft => "rotate_left",
            Action::RotateRight => "rotate_right",
            Action::FlipHorizontal => "flip_horizontal",
            Action::CropImage => "crop_image",
            Action::UpscaleImage => "upscale_image",
            Action::CompressImage => "compress_image",
            Action::CompressFolder => "compress_folder",
            Action::StartSlideshow => "start_slideshow",
            Action::ToggleLoop => "toggle_loop",
            Action::ToggleShuffle => "toggle_shuffle",
            Action::ToggleControls => "toggle_controls",
            Action::SlideNext => "slide_next",
            Action::SlidePrev => "slide_prev",
            Action::SlidePlayPause => "slide_play_pause",
            Action::SlideClose => "slide_close",
            Action::SlideRotateCw => "slide_rotate_cw",
            Action::SlideRotateCcw => "slide_rotate_ccw",
            Action::SlideFlip => "slide_flip",
            Action::SlideCrop => "slide_crop",
            Action::SlideUpscale => "slide_upscale",
            Action::SlideZoomIn => "slide_zoom_in",
            Action::SlideZoomOut => "slide_zoom_out",
            Action::SlideZoomActual => "slide_zoom_actual",
            Action::SlideZoomReset => "slide_zoom_reset",
            Action::SlideToggleControls => "slide_toggle_controls",
            Action::SlideToggleOverlay => "slide_toggle_overlay",
            Action::SlideToggleExif => "slide_toggle_exif",
            Action::SlideTogglePath => "slide_toggle_path",
            Action::SlideToggleHelp => "slide_toggle_help",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Action::OpenFolder => "Open Folder…",
            Action::RevealInFinder => "Reveal in Finder",
            Action::GetInfo => "Get Info",
            Action::SetDesktopPicture => "Set as Desktop Picture",
            Action::MoveToTrash => "Move to Trash",
            Action::MoveToFolder => "Move to…",
            Action::CopyToFolder => "Copy to…",
            Action::MoveAgain => "Move Again",
            Action::CopyAgain => "Copy Again",
            Action::FilterByName => "Filter by Name",
            Action::CopyPath => "Copy Path",
            Action::CopyFileUrl => "Copy File URL",
            Action::SortByName => "Sort by Name",
            Action::SortByModified => "Sort by Date Modified",
            Action::SortBySize => "Sort by Size",
            Action::SortByPath => "Sort by File Path",
            Action::SortByExifDate => "Sort by EXIF Date",
            Action::SortByAdded => "Sort by Date Added",
            Action::LargerThumbs => "Larger Thumbnails",
            Action::SmallerThumbs => "Smaller Thumbnails",
            Action::ScaleUpSmall => "Scale Up Small Images",
            Action::ToggleSidebar => "Show Sidebar",
            Action::ToggleLabels => "Show Filenames",
            Action::RotateLeft => "Rotate Left",
            Action::RotateRight => "Rotate Right",
            Action::FlipHorizontal => "Flip Horizontally",
            Action::CropImage => "Crop…",
            Action::UpscaleImage => "Upscale…",
            Action::CompressImage => "Compress…",
            Action::CompressFolder => "Compress Folder…",
            Action::StartSlideshow => "Start Slideshow",
            Action::ToggleLoop => "Loop",
            Action::ToggleShuffle => "Shuffle",
            Action::ToggleControls => "Show Controls",
            Action::SlideNext => "Next Slide",
            Action::SlidePrev => "Previous Slide",
            Action::SlidePlayPause => "Next / Pause",
            Action::SlideClose => "Close Slideshow",
            Action::SlideRotateCw => "Rotate Clockwise",
            Action::SlideRotateCcw => "Rotate Counterclockwise",
            Action::SlideFlip => "Flip Horizontally",
            Action::SlideCrop => "Crop",
            Action::SlideUpscale => "Upscale",
            Action::SlideZoomIn => "Zoom In",
            Action::SlideZoomOut => "Zoom Out",
            Action::SlideZoomActual => "Actual Size",
            Action::SlideZoomReset => "Reset View",
            Action::SlideToggleControls => "Toggle Controls",
            Action::SlideToggleOverlay => "Toggle Overlay",
            Action::SlideToggleExif => "EXIF Info",
            Action::SlideTogglePath => "Full Path",
            Action::SlideToggleHelp => "Toggle Help",
        }
    }

    pub const fn section(self) -> Section {
        match self {
            Action::OpenFolder
            | Action::RevealInFinder
            | Action::GetInfo
            | Action::SetDesktopPicture
            | Action::MoveToTrash
            | Action::MoveToFolder
            | Action::CopyToFolder
            | Action::MoveAgain
            | Action::CopyAgain => Section::File,
            Action::FilterByName
            | Action::CopyPath
            | Action::CopyFileUrl
            | Action::SortByName
            | Action::SortByModified
            | Action::SortBySize
            | Action::SortByPath
            | Action::SortByExifDate
            | Action::SortByAdded
            | Action::LargerThumbs
            | Action::SmallerThumbs
            | Action::ScaleUpSmall
            | Action::ToggleSidebar
            | Action::ToggleLabels => Section::EditView,
            Action::RotateLeft
            | Action::RotateRight
            | Action::FlipHorizontal
            | Action::CropImage
            | Action::UpscaleImage
            | Action::CompressImage
            | Action::CompressFolder => Section::Image,
            Action::StartSlideshow
            | Action::ToggleLoop
            | Action::ToggleShuffle
            | Action::ToggleControls => Section::SlideshowMenu,
            Action::SlideNext
            | Action::SlidePrev
            | Action::SlidePlayPause
            | Action::SlideClose
            | Action::SlideRotateCw
            | Action::SlideRotateCcw
            | Action::SlideFlip
            | Action::SlideCrop
            | Action::SlideUpscale
            | Action::SlideZoomIn
            | Action::SlideZoomOut
            | Action::SlideZoomActual
            | Action::SlideZoomReset
            | Action::SlideToggleControls
            | Action::SlideToggleOverlay
            | Action::SlideToggleExif
            | Action::SlideTogglePath
            | Action::SlideToggleHelp => Section::SlideshowKeys,
        }
    }

    pub const fn scope(self) -> Scope {
        match self.section() {
            Section::SlideshowKeys => Scope::Slideshow,
            _ => Scope::Menu,
        }
    }

    pub fn default_combo(self) -> Option<KeyCombo> {
        let k = |key: &str, mods: u8| Some(KeyCombo::new(key, mods));
        match self {
            Action::OpenFolder => k("o", MOD_CMD),
            Action::RevealInFinder => k("r", MOD_CMD),
            Action::GetInfo => k("i", MOD_CMD),
            Action::SetDesktopPicture => k("d", MOD_CMD),
            Action::MoveToTrash => k("\u{8}", MOD_CMD),
            Action::MoveToFolder => k("m", MOD_CMD | MOD_SHIFT),
            Action::CopyToFolder => k("c", MOD_CMD | MOD_SHIFT),
            Action::MoveAgain => k("m", MOD_CMD | MOD_CTRL),
            Action::CopyAgain => k("c", MOD_CMD | MOD_CTRL),
            Action::FilterByName => k("f", MOD_CMD),
            Action::CopyPath => k("c", MOD_CMD | MOD_OPT),
            Action::CopyFileUrl => None,
            Action::SortByName => k("1", MOD_CMD | MOD_CTRL),
            Action::SortByModified => k("2", MOD_CMD | MOD_CTRL),
            Action::SortBySize => k("3", MOD_CMD | MOD_CTRL),
            Action::SortByPath => k("4", MOD_CMD | MOD_CTRL),
            Action::SortByExifDate => k("5", MOD_CMD | MOD_CTRL),
            Action::SortByAdded => k("6", MOD_CMD | MOD_CTRL),
            Action::LargerThumbs => k("=", MOD_CMD),
            Action::SmallerThumbs => k("-", MOD_CMD),
            Action::ScaleUpSmall => k("e", MOD_CMD),
            Action::ToggleSidebar => k("s", MOD_CMD | MOD_CTRL),
            Action::ToggleLabels => None,
            Action::RotateLeft => k("[", MOD_CMD),
            Action::RotateRight => k("]", MOD_CMD),
            Action::FlipHorizontal => k("f", MOD_CMD | MOD_SHIFT),
            Action::CropImage => k("k", MOD_CMD),
            Action::UpscaleImage => k("u", MOD_CMD),
            Action::CompressImage => k("k", MOD_CMD | MOD_SHIFT),
            Action::CompressFolder => None,
            Action::StartSlideshow => k("y", MOD_CMD),
            Action::ToggleLoop => k("l", MOD_CMD),
            Action::ToggleShuffle => k("r", MOD_CMD | MOD_OPT),
            Action::ToggleControls => None,
            Action::SlideNext => k("l", 0),
            Action::SlidePrev => k("j", 0),
            Action::SlidePlayPause => k(" ", 0),
            Action::SlideClose => k("q", 0),
            Action::SlideRotateCw => k("r", 0),
            Action::SlideRotateCcw => k("r", MOD_SHIFT),
            Action::SlideFlip => k("f", 0),
            Action::SlideCrop => k("c", 0),
            Action::SlideUpscale => k("u", 0),
            Action::SlideZoomIn => k("+", 0),
            Action::SlideZoomOut => k("-", 0),
            Action::SlideZoomActual => k("=", 0),
            Action::SlideZoomReset => k("*", 0),
            Action::SlideToggleControls => k("t", 0),
            Action::SlideToggleOverlay => k("i", 0),
            Action::SlideToggleExif => k("i", MOD_SHIFT),
            Action::SlideTogglePath => k("p", 0),
            Action::SlideToggleHelp => k("h", 0),
        }
    }

    pub const fn sort_order_action(order: blickli_core::SortOrder) -> Self {
        match order {
            blickli_core::SortOrder::Name => Action::SortByName,
            blickli_core::SortOrder::Modified => Action::SortByModified,
            blickli_core::SortOrder::Size => Action::SortBySize,
            blickli_core::SortOrder::Path => Action::SortByPath,
            blickli_core::SortOrder::ExifDate => Action::SortByExifDate,
            blickli_core::SortOrder::Added => Action::SortByAdded,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    /// Key equivalent string (ASCII letters normalized to lowercase).
    pub key: String,
    pub mods: u8,
}

impl KeyCombo {
    pub fn new(key: impl Into<String>, mods: u8) -> Self {
        Self {
            key: key.into(),
            mods: mods & (MOD_CTRL | MOD_OPT | MOD_SHIFT | MOD_CMD),
        }
    }

    pub fn from_event(event: &NSEvent) -> Option<Self> {
        let key_code = event.keyCode();
        if key_code == 53 {
            return None;
        }
        let mods = mods_from_flags(event.modifierFlags());
        if let Some(special) = special_key_for_code(key_code) {
            return Some(Self::new(special, mods));
        }
        let chars = event.charactersIgnoringModifiers()?.to_string();
        let mut iter = chars.chars();
        let ch = iter.next()?;
        if iter.next().is_some() || ch.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&ch) {
            return None;
        }
        if ch.is_ascii_alphabetic() {
            Some(Self::new(ch.to_ascii_lowercase().to_string(), mods))
        } else {
            // Unmodified punctuation already encodes Shift in `ch` (e.g. `+`, `*`, `?`).
            let norm_mods = if (mods & (MOD_CMD | MOD_CTRL | MOD_OPT)) == 0 {
                0
            } else {
                mods
            };
            Some(Self::new(ch.to_string(), norm_mods))
        }
    }

    pub fn ns_modifiers(&self) -> NSEventModifierFlags {
        let mut mask = NSEventModifierFlags::empty();
        if self.mods & MOD_CTRL != 0 {
            mask |= NSEventModifierFlags::Control;
        }
        if self.mods & MOD_OPT != 0 {
            mask |= NSEventModifierFlags::Option;
        }
        if self.mods & MOD_SHIFT != 0 {
            mask |= NSEventModifierFlags::Shift;
        }
        if self.mods & MOD_CMD != 0 {
            mask |= NSEventModifierFlags::Command;
        }
        mask
    }

    pub fn has_menu_modifier(&self) -> bool {
        (self.mods & (MOD_CMD | MOD_CTRL | MOD_OPT)) != 0
    }

    pub fn is_function_key(&self) -> bool {
        matches!(self.key.chars().next(), Some('\u{F704}'..='\u{F70F}'))
    }

    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.mods & MOD_CTRL != 0 {
            out.push('⌃');
        }
        if self.mods & MOD_OPT != 0 {
            out.push('⌥');
        }
        if self.mods & MOD_SHIFT != 0 {
            out.push('⇧');
        }
        if self.mods & MOD_CMD != 0 {
            out.push('⌘');
        }
        out.push_str(&key_display(&self.key));
        out
    }

    pub fn encode(&self) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(5);
        if self.mods & MOD_CTRL != 0 {
            parts.push("ctrl");
        }
        if self.mods & MOD_OPT != 0 {
            parts.push("opt");
        }
        if self.mods & MOD_SHIFT != 0 {
            parts.push("shift");
        }
        if self.mods & MOD_CMD != 0 {
            parts.push("cmd");
        }
        let token = key_to_token(&self.key);
        parts.push(&token);
        parts.join("+")
    }

    pub fn decode(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
            return None;
        }
        let mut mods = 0u8;
        let mut key: Option<String> = None;
        for part in raw.split('+') {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" => mods |= MOD_CTRL,
                "opt" => mods |= MOD_OPT,
                "shift" => mods |= MOD_SHIFT,
                "cmd" => mods |= MOD_CMD,
                other => {
                    if key.is_some() {
                        return None;
                    }
                    key = Some(token_to_key(other)?);
                }
            }
        }
        Some(Self::new(key?, mods))
    }
}

fn mods_from_flags(flags: NSEventModifierFlags) -> u8 {
    let mut mods = 0u8;
    if flags.contains(NSEventModifierFlags::Control) {
        mods |= MOD_CTRL;
    }
    if flags.contains(NSEventModifierFlags::Option) {
        mods |= MOD_OPT;
    }
    if flags.contains(NSEventModifierFlags::Shift) {
        mods |= MOD_SHIFT;
    }
    if flags.contains(NSEventModifierFlags::Command) {
        mods |= MOD_CMD;
    }
    mods
}

fn special_key_for_code(code: u16) -> Option<&'static str> {
    Some(match code {
        51 => "\u{8}",
        117 => "\u{F728}",
        49 => " ",
        48 => "\t",
        36 | 76 => "\r",
        123 => "\u{F702}",
        124 => "\u{F703}",
        125 => "\u{F701}",
        126 => "\u{F700}",
        115 => "\u{F729}",
        119 => "\u{F72B}",
        116 => "\u{F72C}",
        121 => "\u{F72D}",
        122 => "\u{F704}",
        120 => "\u{F705}",
        99 => "\u{F706}",
        118 => "\u{F707}",
        96 => "\u{F708}",
        97 => "\u{F709}",
        98 => "\u{F70A}",
        100 => "\u{F70B}",
        101 => "\u{F70C}",
        109 => "\u{F70D}",
        103 => "\u{F70E}",
        111 => "\u{F70F}",
        _ => return None,
    })
}

fn key_display(key: &str) -> String {
    match key {
        "\u{8}" => "⌫".to_owned(),
        "\u{F728}" | "\u{7f}" => "⌦".to_owned(),
        " " => "Space".to_owned(),
        "\t" => "⇥".to_owned(),
        "\r" => "↩".to_owned(),
        "\u{F700}" => "↑".to_owned(),
        "\u{F701}" => "↓".to_owned(),
        "\u{F702}" => "←".to_owned(),
        "\u{F703}" => "→".to_owned(),
        "\u{F729}" => "↖".to_owned(),
        "\u{F72B}" => "↘".to_owned(),
        "\u{F72C}" => "⇞".to_owned(),
        "\u{F72D}" => "⇟".to_owned(),
        "\u{F704}" => "F1".to_owned(),
        "\u{F705}" => "F2".to_owned(),
        "\u{F706}" => "F3".to_owned(),
        "\u{F707}" => "F4".to_owned(),
        "\u{F708}" => "F5".to_owned(),
        "\u{F709}" => "F6".to_owned(),
        "\u{F70A}" => "F7".to_owned(),
        "\u{F70B}" => "F8".to_owned(),
        "\u{F70C}" => "F9".to_owned(),
        "\u{F70D}" => "F10".to_owned(),
        "\u{F70E}" => "F11".to_owned(),
        "\u{F70F}" => "F12".to_owned(),
        other => other.to_ascii_uppercase(),
    }
}

fn key_to_token(key: &str) -> String {
    match key {
        "\u{8}" => "backspace".to_owned(),
        "\u{F728}" | "\u{7f}" => "delete".to_owned(),
        " " => "space".to_owned(),
        "\t" => "tab".to_owned(),
        "\r" => "return".to_owned(),
        "+" => "plus".to_owned(),
        "\u{F700}" => "up".to_owned(),
        "\u{F701}" => "down".to_owned(),
        "\u{F702}" => "left".to_owned(),
        "\u{F703}" => "right".to_owned(),
        "\u{F729}" => "home".to_owned(),
        "\u{F72B}" => "end".to_owned(),
        "\u{F72C}" => "pageup".to_owned(),
        "\u{F72D}" => "pagedown".to_owned(),
        "\u{F704}" => "f1".to_owned(),
        "\u{F705}" => "f2".to_owned(),
        "\u{F706}" => "f3".to_owned(),
        "\u{F707}" => "f4".to_owned(),
        "\u{F708}" => "f5".to_owned(),
        "\u{F709}" => "f6".to_owned(),
        "\u{F70A}" => "f7".to_owned(),
        "\u{F70B}" => "f8".to_owned(),
        "\u{F70C}" => "f9".to_owned(),
        "\u{F70D}" => "f10".to_owned(),
        "\u{F70E}" => "f11".to_owned(),
        "\u{F70F}" => "f12".to_owned(),
        other => other.to_owned(),
    }
}

fn token_to_key(token: &str) -> Option<String> {
    let mapped = match token {
        "backspace" => "\u{8}",
        "delete" => "\u{F728}",
        "space" => " ",
        "tab" => "\t",
        "return" => "\r",
        "plus" => "+",
        "up" => "\u{F700}",
        "down" => "\u{F701}",
        "left" => "\u{F702}",
        "right" => "\u{F703}",
        "home" => "\u{F729}",
        "end" => "\u{F72B}",
        "pageup" => "\u{F72C}",
        "pagedown" => "\u{F72D}",
        "f1" => "\u{F704}",
        "f2" => "\u{F705}",
        "f3" => "\u{F706}",
        "f4" => "\u{F707}",
        "f5" => "\u{F708}",
        "f6" => "\u{F709}",
        "f7" => "\u{F70A}",
        "f8" => "\u{F70B}",
        "f9" => "\u{F70C}",
        "f10" => "\u{F70D}",
        "f11" => "\u{F70E}",
        "f12" => "\u{F70F}",
        other => {
            let mut iter = other.chars();
            let ch = iter.next()?;
            if iter.next().is_some() || ch.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&ch) {
                return None;
            }
            return Some(ch.to_ascii_lowercase().to_string());
        }
    };
    Some(mapped.to_owned())
}

/// System and standard text-editing shortcuts that stay fixed.
pub fn is_reserved(combo: &KeyCombo) -> bool {
    const RESERVED: [(&str, u8); 12] = [
        ("q", MOD_CMD),
        ("w", MOD_CMD),
        ("m", MOD_CMD),
        ("h", MOD_CMD),
        ("h", MOD_CMD | MOD_OPT),
        (",", MOD_CMD),
        ("z", MOD_CMD),
        ("z", MOD_CMD | MOD_SHIFT),
        ("a", MOD_CMD),
        ("c", MOD_CMD),
        ("v", MOD_CMD),
        ("x", MOD_CMD),
    ];
    RESERVED
        .iter()
        .any(|&(key, mods)| combo.key == key && combo.mods == mods)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssignOutcome {
    Assigned { cleared: Option<Action> },
    Rejected(String),
}

pub struct ShortcutStore {
    bindings: HashMap<Action, Option<KeyCombo>>,
}

impl Default for ShortcutStore {
    fn default() -> Self {
        let bindings = Action::ALL
            .into_iter()
            .map(|action| (action, action.default_combo()))
            .collect();
        Self { bindings }
    }
}

impl ShortcutStore {
    pub fn get(&self, action: Action) -> Option<&KeyCombo> {
        self.bindings.get(&action).and_then(Option::as_ref)
    }

    pub fn display_or_empty(&self, action: Action) -> String {
        self.get(action).map(KeyCombo::display).unwrap_or_default()
    }

    /// Append the active shortcut (or `fallback`) in parentheses to a tooltip label.
    pub fn format_tooltip(&self, base: &str, action: Action, fallback: Option<&str>) -> String {
        let key = self.display_or_empty(action);
        if !key.is_empty() {
            format!("{base} ({key})")
        } else if let Some(fb) = fallback {
            format!("{base} ({fb})")
        } else {
            base.to_owned()
        }
    }

    /// True if `LargerThumbs` keeps its default `⌘=` and nothing else claims `⇧⌘=` / `⇧⌘+` / `⌘+`.
    pub fn use_larger_thumbs_shifted_twin(&self) -> bool {
        let default_eq = KeyCombo::new("=", MOD_CMD);
        if self.get(Action::LargerThumbs) != Some(&default_eq) {
            return false;
        }
        let twins = [
            KeyCombo::new("=", MOD_CMD | MOD_SHIFT),
            KeyCombo::new("+", MOD_CMD | MOD_SHIFT),
            KeyCombo::new("+", MOD_CMD),
        ];
        !self.bindings.iter().any(|(&a, c)| {
            a != Action::LargerThumbs && c.as_ref().is_some_and(|combo| twins.contains(combo))
        })
    }

    pub fn assign(&mut self, action: Action, combo: Option<KeyCombo>) -> AssignOutcome {
        let Some(combo) = combo else {
            self.bindings.insert(action, None);
            return AssignOutcome::Assigned { cleared: None };
        };
        if is_reserved(&combo) {
            return AssignOutcome::Rejected(format!("{} is reserved by macOS.", combo.display()));
        }
        match action.scope() {
            Scope::Menu => {
                if !combo.has_menu_modifier() && !combo.is_function_key() {
                    return AssignOutcome::Rejected(
                        "Menu shortcuts need ⌘, ⌃, ⌥, or a function key.".to_owned(),
                    );
                }
            }
            Scope::Slideshow => {
                if let Some(menu_action) = self.find_conflict(Scope::Menu, action, &combo) {
                    return AssignOutcome::Rejected(format!(
                        "{} is already used by {}.",
                        combo.display(),
                        menu_action.label()
                    ));
                }
            }
        }
        let mut cleared = self.find_conflict(action.scope(), action, &combo);
        if let Some(other) = cleared {
            self.bindings.insert(other, None);
        }
        // Clear any slideshow key sharing the same modified combo as a menu shortcut.
        if action.scope() == Scope::Menu {
            if let Some(slide_conflict) = self.find_conflict(Scope::Slideshow, action, &combo) {
                self.bindings.insert(slide_conflict, None);
                if cleared.is_none() {
                    cleared = Some(slide_conflict);
                }
            }
        }
        self.bindings.insert(action, Some(combo));
        AssignOutcome::Assigned { cleared }
    }

    fn find_conflict(&self, scope: Scope, exclude: Action, combo: &KeyCombo) -> Option<Action> {
        Action::ALL.into_iter().find(|&other| {
            other != exclude
                && other.scope() == scope
                && self.get(other) == Some(combo)
        })
    }

    pub fn reset_all(&mut self) {
        *self = Self::default();
    }

    pub fn slideshow_action(&self, combo: &KeyCombo) -> Option<Action> {
        Action::ALL
            .into_iter()
            .find(|&action| action.scope() == Scope::Slideshow && self.get(action) == Some(combo))
    }

    pub fn has_slideshow_key(&self, key: &str, mods: u8) -> bool {
        let target = KeyCombo::new(key, mods);
        self.slideshow_action(&target).is_some()
    }

    pub fn overrides(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for action in Action::ALL {
            let current = self.get(action);
            let default = action.default_combo();
            if current != default.as_ref() {
                let encoded = current.map_or_else(|| "none".to_owned(), KeyCombo::encode);
                out.push((action.id().to_owned(), encoded));
            }
        }
        out
    }

    /// Apply saved overrides in deterministic order: clears first, then Menu, then Slideshow,
    /// so `NSDictionary` key iteration order never drops valid reassigned bindings.
    pub fn apply_overrides(&mut self, pairs: impl IntoIterator<Item = (String, String)>) {
        let mut clears = Vec::new();
        let mut menu = Vec::new();
        let mut slide = Vec::new();
        for (id, raw) in pairs {
            let Some(action) = Action::from_id(&id) else {
                continue;
            };
            if raw.trim().eq_ignore_ascii_case("none") {
                clears.push(action);
            } else if let Some(combo) = KeyCombo::decode(&raw) {
                match action.scope() {
                    Scope::Menu => menu.push((action, combo)),
                    Scope::Slideshow => slide.push((action, combo)),
                }
            }
        }
        for action in clears {
            self.bindings.insert(action, None);
        }
        for (action, combo) in menu {
            let _ = self.assign(action, Some(combo));
        }
        for (action, combo) in slide {
            let _ = self.assign(action, Some(combo));
        }
    }

    pub fn load_from_defaults(&mut self) {
        self.reset_all();
        let d = NSUserDefaults::standardUserDefaults();
        let Some(dict) = d.dictionaryForKey(&NSString::from_str(PREF_KEY)) else {
            return;
        };
        let mut pairs = Vec::new();
        for key in dict.allKeys().iter() {
            if let Some(val) = dict.objectForKey(&key) {
                if let Ok(s) = val.downcast::<NSString>() {
                    pairs.push((key.to_string(), s.to_string()));
                }
            }
        }
        self.apply_overrides(pairs);
    }

    pub fn save_to_defaults(&self) {
        let d = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str(PREF_KEY);
        let overrides = self.overrides();
        if overrides.is_empty() {
            unsafe { d.setObject_forKey(None, &key) };
            return;
        }
        let dict: Retained<NSMutableDictionary<NSString, NSString>> = NSMutableDictionary::new();
        for (id, encoded) in overrides {
            let k = NSString::from_str(&id);
            let v = NSString::from_str(&encoded);
            unsafe { dict.setObject_forKey(&v, objc2::runtime::ProtocolObject::from_ref(&*k)) };
        }
        unsafe { d.setObject_forKey(Some(&dict), &key) };
    }

    /// Build the slideshow cheat sheet using the active shortcuts.
    pub fn slideshow_help_text(&self) -> String {
        let k = |action: Action| {
            self.get(action)
                .map(KeyCombo::display)
                .unwrap_or_else(|| "—".to_owned())
        };
        let join = |parts: &[String]| {
            let non_empty: Vec<&str> = parts
                .iter()
                .map(String::as_str)
                .filter(|s| !s.is_empty() && *s != "—")
                .collect();
            if non_empty.is_empty() {
                "—".to_owned()
            } else {
                non_empty.join(" ")
            }
        };
        let extra = |action: Action, fixed: &[&str]| {
            let disp = k(action);
            if disp == "—" || fixed.contains(&disp.as_str()) {
                String::new()
            } else {
                disp
            }
        };
        let next = join(&["→ ↓".to_owned(), extra(Action::SlideNext, &["→", "↓"])]);
        let prev = join(&["← ↑".to_owned(), extra(Action::SlidePrev, &["←", "↑"])]);
        let rot = format!(
            "{} / {} / {}",
            k(Action::SlideRotateCw),
            k(Action::SlideRotateCcw),
            k(Action::SlideFlip)
        );
        let crop_up = format!("{} / {}", k(Action::SlideCrop), k(Action::SlideUpscale));
        let zoom = format!("{} {}", k(Action::SlideZoomIn), k(Action::SlideZoomOut));
        let close = join(&["Esc".to_owned(), k(Action::SlideClose)]);
        let move_keys = format!("{} / {}", k(Action::MoveToFolder), k(Action::MoveAgain));
        let help_key = if self.has_slideshow_key("?", 0) {
            k(Action::SlideToggleHelp)
        } else {
            format!("{} / ?", k(Action::SlideToggleHelp))
        };

        let rows: [(&str, &str, &str, &str); 13] = [
            (&next, "next slide", &prev, "previous slide"),
            (&k(Action::SlidePlayPause), "next / pause", "⌥ + arrow", "step in sorted order"),
            ("Page Up", "back 10", "Page Down", "forward 10"),
            ("Home / End", "first / last", "1–9 / 0", "auto-advance (s) / off"),
            ("! / @", "0.5 s / 1.5 s", &zoom, "zoom in / out"),
            (&k(Action::SlideZoomActual), "actual size", &k(Action::SlideZoomReset), "reset view"),
            (&rot, "rotate / flip", &crop_up, "crop / upscale"),
            (&k(Action::SlideToggleControls), "toggle controls", &k(Action::SlideToggleOverlay), "toggle overlay"),
            (&k(Action::SlideToggleExif), "EXIF info", &k(Action::SlideTogglePath), "full path"),
            (&help_key, "toggle help", &k(Action::GetInfo), "info panel"),
            (&k(Action::MoveToTrash), "move to Trash", &move_keys, "move / move again"),
            (&k(Action::CompressImage), "compress", &close, "close slideshow"),
            ("Click / Scroll", "step slides", "Drag", "pan when zoomed"),
        ];

        let mut text = String::from("Slideshow keys\n\n");
        for (lk, ld, rk, rd) in rows {
            text.push_str(&format!("{lk:<14} {ld:<20} {rk:<14} {rd}\n"));
        }
        text.push_str("\nCrop mode: Return applies, ⌥Return saves copy, Esc cancels.\n");
        text.push_str("Upscale preview: drag divider, Space peek, s split/side, 2/4 scale, Return replaces, ⌥Return copies.");
        text
    }
}

// ---- Interactive shortcut recorder control ----

pub struct RecorderIvars {
    action: Cell<Action>,
    combo: RefCell<Option<KeyCombo>>,
    recording: Cell<bool>,
    delegate: OnceCell<Retained<AppDelegate>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliShortcutRecorder"]
    #[ivars = RecorderIvars]
    pub struct ShortcutRecorderView;

    unsafe impl NSObjectProtocol for ShortcutRecorderView {}

    impl ShortcutRecorderView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            if self.ivars().recording.replace(false) {
                self.setNeedsDisplay(true);
            }
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if self.ivars().recording.get() {
                self.stop_recording();
                if let Some(delegate) = self.ivars().delegate.get() {
                    delegate.shortcut_feedback("Click a shortcut to change it.");
                }
                return;
            }
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            self.ivars().recording.set(true);
            self.setNeedsDisplay(true);
            if let Some(delegate) = self.ivars().delegate.get() {
                delegate.shortcut_feedback(&format!(
                    "Type shortcut for {}… (Esc cancels, ⌫ clears)",
                    self.ivars().action.get().label()
                ));
            }
        }

        // Intercept ⌘/⌃/⌥ shortcuts before the main menu runs them while recording.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            if !self.ivars().recording.get() {
                false
            } else {
                let is_first = self
                    .window()
                    .and_then(|w| w.firstResponder())
                    .is_some_and(|r| {
                        std::ptr::eq(
                            &*r as *const _ as *const (),
                            self as *const _ as *const (),
                        )
                    });
                if is_first {
                    self.capture_event(event);
                    true
                } else {
                    self.ivars().recording.set(false);
                    self.setNeedsDisplay(true);
                    false
                }
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if self.ivars().recording.get() {
                self.capture_event(event);
                return;
            }
            let _: () = unsafe { msg_send![super(self), keyDown: event] };
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let rect = CGRect::new(
                CGPoint::new(bounds.origin.x + 1.0, bounds.origin.y + 1.0),
                CGSize::new((bounds.size.width - 2.0).max(1.0), (bounds.size.height - 2.0).max(1.0)),
            );
            let pill = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 5.0, 5.0);
            let recording = self.ivars().recording.get();
            let combo = self.ivars().combo.borrow();
            let (fill, stroke, text_color, text) = if recording {
                (
                    NSColor::controlAccentColor().colorWithAlphaComponent(0.14),
                    NSColor::controlAccentColor(),
                    NSColor::controlAccentColor(),
                    "Type shortcut…".to_owned(),
                )
            } else if let Some(combo) = combo.as_ref() {
                (
                    NSColor::quaternaryLabelColor(),
                    NSColor::separatorColor(),
                    NSColor::labelColor(),
                    combo.display(),
                )
            } else {
                (
                    NSColor::clearColor(),
                    NSColor::separatorColor(),
                    NSColor::tertiaryLabelColor(),
                    "None".to_owned(),
                )
            };
            fill.setFill();
            pill.fill();
            pill.setLineWidth(if recording { 1.5 } else { 1.0 });
            stroke.setStroke();
            pill.stroke();

            thread_local! {
                static RECORDER_TEXT: OnceCell<(
                    Retained<NSMutableParagraphStyle>,
                    Retained<NSFont>,
                    Retained<NSFont>,
                )> = const { OnceCell::new() };
            }
            let is_placeholder = recording || combo.is_none();
            RECORDER_TEXT.with(|cell| {
                let (style, regular_font, bold_font) = cell.get_or_init(|| {
                    let style = NSMutableParagraphStyle::new();
                    style.setAlignment(NSTextAlignment::Center);
                    (
                        style,
                        NSFont::systemFontOfSize(11.5),
                        NSFont::systemFontOfSize_weight(12.0, 0.23),
                    )
                });
                let font = if is_placeholder { regular_font } else { bold_font };
                let attrs = unsafe {
                    NSDictionary::from_slices(
                        &[
                            NSFontAttributeName,
                            NSForegroundColorAttributeName,
                            NSParagraphStyleAttributeName,
                        ],
                        &[
                            font.as_ref() as &objc2::runtime::AnyObject,
                            text_color.as_ref(),
                            style.as_ref(),
                        ],
                    )
                };
                let label = NSString::from_str(&text);
                let text_rect = CGRect::new(
                    CGPoint::new(rect.origin.x + 4.0, rect.origin.y + (rect.size.height - 16.0) / 2.0),
                    CGSize::new((rect.size.width - 8.0).max(1.0), 16.0),
                );
                unsafe { label.drawInRect_withAttributes(text_rect, Some(&attrs)) };
            });
        }
    }
);

impl ShortcutRecorderView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        action: Action,
        combo: Option<KeyCombo>,
        delegate: Retained<AppDelegate>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RecorderIvars {
            action: Cell::new(action),
            combo: RefCell::new(combo),
            recording: Cell::new(false),
            delegate: OnceCell::new(),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let _ = this.ivars().delegate.set(delegate);
        this.setToolTip(Some(ns_string!("Click to set shortcut")));
        this
    }

    pub fn set_combo(&self, combo: Option<KeyCombo>) {
        *self.ivars().combo.borrow_mut() = combo;
        self.setNeedsDisplay(true);
    }

    /// Exit recording mode without changing the stored shortcut. Returns `true` if it was recording.
    pub fn cancel_recording(&self) -> bool {
        if self.ivars().recording.replace(false) {
            self.setNeedsDisplay(true);
            true
        } else {
            false
        }
    }

    fn stop_recording(&self) {
        self.ivars().recording.set(false);
        self.setNeedsDisplay(true);
        if let Some(window) = self.window() {
            window.makeFirstResponder(None);
        }
    }

    fn capture_event(&self, event: &NSEvent) {
        let action = self.ivars().action.get();
        let code = event.keyCode();
        let mods = mods_from_flags(event.modifierFlags());
        if code == 53 {
            self.stop_recording();
            if let Some(delegate) = self.ivars().delegate.get() {
                delegate.shortcut_feedback("Click a shortcut to change it.");
            }
            return;
        }
        if (code == 51 || code == 117) && mods == 0 {
            self.stop_recording();
            if let Some(delegate) = self.ivars().delegate.get() {
                delegate.update_shortcut(action, None);
            }
            return;
        }
        let Some(combo) = KeyCombo::from_event(event) else {
            return;
        };
        if let Some(delegate) = self.ivars().delegate.get() {
            if delegate.update_shortcut(action, Some(combo)) {
                self.stop_recording();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn defaults_have_no_conflicts_within_each_scope() {
        let store = ShortcutStore::default();
        let mut menu_seen = HashSet::new();
        let mut slide_seen = HashSet::new();
        for action in Action::ALL {
            if let Some(combo) = store.get(action) {
                assert!(!is_reserved(combo), "default for {action:?} is reserved");
                match action.scope() {
                    Scope::Menu => {
                        assert!(combo.has_menu_modifier() || combo.is_function_key());
                        assert!(menu_seen.insert(combo.clone()), "duplicate menu default: {combo:?}");
                    }
                    Scope::Slideshow => {
                        assert!(slide_seen.insert(combo.clone()), "duplicate slide default: {combo:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn key_combo_encode_decode_roundtrip() {
        let samples = [
            KeyCombo::new("k", MOD_CMD | MOD_SHIFT),
            KeyCombo::new("\u{8}", MOD_CMD),
            KeyCombo::new(" ", 0),
            KeyCombo::new("+", 0),
            KeyCombo::new("=", MOD_CMD),
            KeyCombo::new("\u{F704}", MOD_OPT),
            KeyCombo::new("r", MOD_SHIFT),
        ];
        for sample in samples {
            let encoded = sample.encode();
            assert_eq!(KeyCombo::decode(&encoded), Some(sample.clone()), "roundtrip {encoded}");
        }
        assert_eq!(KeyCombo::decode("none"), None);
        assert_eq!(KeyCombo::decode(""), None);
        assert_eq!(KeyCombo::decode("cmd+bogus"), None);
        assert_eq!(KeyCombo::decode("cmd+\u{F720}"), None);
    }

    #[test]
    fn assignment_validates_reserved_modifiers_and_reassignment() {
        let mut store = ShortcutStore::default();
        // Reserved system shortcut rejected.
        assert!(matches!(
            store.assign(Action::CropImage, Some(KeyCombo::new("q", MOD_CMD))),
            AssignOutcome::Rejected(_)
        ));
        // Menu shortcut requires a modifier or function key.
        assert!(matches!(
            store.assign(Action::CropImage, Some(KeyCombo::new("x", 0))),
            AssignOutcome::Rejected(_)
        ));
        // Reassigning a menu shortcut clears the prior holder.
        assert_eq!(
            store.assign(Action::CompressFolder, Some(KeyCombo::new("u", MOD_CMD))),
            AssignOutcome::Assigned {
                cleared: Some(Action::UpscaleImage)
            }
        );
        assert_eq!(store.get(Action::UpscaleImage), None);
        assert_eq!(
            store.get(Action::CompressFolder),
            Some(&KeyCombo::new("u", MOD_CMD))
        );
        // Slideshow key cannot steal a menu shortcut.
        assert!(matches!(
            store.assign(Action::SlideCrop, Some(KeyCombo::new("k", MOD_CMD))),
            AssignOutcome::Rejected(_)
        ));
        // Reassigning a slideshow key clears the prior slideshow holder.
        assert_eq!(
            store.assign(Action::SlideCrop, Some(KeyCombo::new("u", 0))),
            AssignOutcome::Assigned {
                cleared: Some(Action::SlideUpscale)
            }
        );
        assert_eq!(store.get(Action::SlideUpscale), None);
        // Overrides round-trip cleanly into a fresh store.
        let overrides = store.overrides();
        let mut restored = ShortcutStore::default();
        restored.apply_overrides(overrides);
        assert_eq!(restored.get(Action::UpscaleImage), None);
        assert_eq!(restored.get(Action::CompressFolder), Some(&KeyCombo::new("u", MOD_CMD)));
        assert_eq!(restored.get(Action::SlideCrop), Some(&KeyCombo::new("u", 0)));
        assert_eq!(restored.get(Action::SlideUpscale), None);
    }

    #[test]
    fn help_text_reflects_custom_shortcuts() {
        let mut store = ShortcutStore::default();
        let _ = store.assign(Action::SlideRotateCw, Some(KeyCombo::new("e", 0)));
        let text = store.slideshow_help_text();
        assert!(text.starts_with("Slideshow keys"));
        assert!(text.contains("E / ⇧R / F"));
    }

    #[test]
    fn apply_overrides_is_order_independent_and_tracks_shifted_twin_conflicts() {
        // Slideshow override arriving before the Menu clear that frees ⌘K must still stick.
        let mut store = ShortcutStore::default();
        store.apply_overrides([
            ("slide_crop".to_owned(), "cmd+k".to_owned()),
            ("crop_image".to_owned(), "none".to_owned()),
        ]);
        assert_eq!(store.get(Action::CropImage), None);
        assert_eq!(
            store.get(Action::SlideCrop),
            Some(&KeyCombo::new("k", MOD_CMD))
        );
        // Assigning a Menu shortcut that collides with a Slideshow key reports the cleared action.
        assert_eq!(
            store.assign(Action::CropImage, Some(KeyCombo::new("k", MOD_CMD))),
            AssignOutcome::Assigned {
                cleared: Some(Action::SlideCrop)
            }
        );
        assert_eq!(store.get(Action::SlideCrop), None);

        // LargerThumbs shifted twin yields when another action claims ⇧⌘+ or ⌘+.
        assert!(store.use_larger_thumbs_shifted_twin());
        let _ = store.assign(Action::CompressFolder, Some(KeyCombo::new("+", MOD_CMD | MOD_SHIFT)));
        assert!(!store.use_larger_thumbs_shifted_twin());
        let _ = store.assign(Action::CompressFolder, Some(KeyCombo::new("+", MOD_CMD)));
        assert!(!store.use_larger_thumbs_shifted_twin());
        let _ = store.assign(Action::CompressFolder, None);
        assert!(store.use_larger_thumbs_shifted_twin());
    }
}
