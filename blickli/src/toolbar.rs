//! Browse window toolbar. Buttons share actions and validation with menu items.

use std::cell::{OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuToolbarItem, NSSearchToolbarItem, NSToolbar, NSToolbarDelegate,
    NSToolbarDisplayMode, NSToolbarFlexibleSpaceItemIdentifier, NSToolbarItem,
    NSToolbarItemVisibilityPriorityHigh, NSToolbarItemVisibilityPriorityLow,
    NSToolbarItemVisibilityPriorityStandard, NSToolbarSidebarTrackingSeparatorItemIdentifier,
    NSToolbarToggleSidebarItemIdentifier,
};
use objc2_foundation::{ns_string, NSArray, NSCopying, NSObject, NSObjectProtocol, NSString};

use crate::shortcuts::{Action, ShortcutStore};
use crate::AppDelegate;

struct ButtonSpec {
    label: &'static str,
    base_tip: &'static str,
    symbols: &'static [&'static str],
    selector: Option<Sel>,
    shortcut: Option<Action>,
}

fn button_spec(id: &str) -> Option<ButtonSpec> {
    Some(match id {
        "rotateLeft" => ButtonSpec {
            label: "Rotate Left",
            base_tip: "Rotate left",
            symbols: &["rotate.left"],
            selector: Some(sel!(rotateLeft:)),
            shortcut: Some(Action::RotateLeft),
        },
        "rotateRight" => ButtonSpec {
            label: "Rotate Right",
            base_tip: "Rotate right",
            symbols: &["rotate.right"],
            selector: Some(sel!(rotateRight:)),
            shortcut: Some(Action::RotateRight),
        },
        "crop" => ButtonSpec {
            label: "Crop",
            base_tip: "Crop",
            symbols: &["crop"],
            selector: Some(sel!(cropImage:)),
            shortcut: Some(Action::CropImage),
        },
        "upscale" => ButtonSpec {
            label: "Upscale",
            base_tip: "Upscale",
            symbols: &["sparkles", "arrow.up.left.and.arrow.down.right"],
            selector: Some(sel!(upscaleImage:)),
            shortcut: Some(Action::UpscaleImage),
        },
        "compress" => ButtonSpec {
            label: "Compress",
            base_tip: "Compress oversized images",
            symbols: &["arrow.down.right.and.arrow.up.left", "archivebox"],
            selector: Some(sel!(compressImage:)),
            shortcut: Some(Action::CompressImage),
        },
        "move" => ButtonSpec {
            label: "Move To",
            base_tip: "Move to folder",
            symbols: &["folder"],
            selector: Some(sel!(moveToFolder:)),
            shortcut: Some(Action::MoveToFolder),
        },
        "moveAgain" => ButtonSpec {
            label: "Move Again",
            base_tip: "Move Again",
            symbols: &["arrow.forward.folder", "arrow.uturn.forward"],
            selector: Some(sel!(moveAgain:)),
            shortcut: Some(Action::MoveAgain),
        },
        "trash" => ButtonSpec {
            label: "Trash",
            base_tip: "Move to Trash",
            symbols: &["trash"],
            selector: Some(sel!(moveToTrash:)),
            shortcut: Some(Action::MoveToTrash),
        },
        "info" => ButtonSpec {
            label: "Info",
            base_tip: "Get info",
            symbols: &["info.circle"],
            selector: Some(sel!(getInfo:)),
            shortcut: Some(Action::GetInfo),
        },
        "slideshow" => ButtonSpec {
            label: "Slideshow",
            base_tip: "Start slideshow",
            symbols: &["play.rectangle", "play.fill"],
            selector: Some(sel!(startSlideshow:)),
            shortcut: Some(Action::StartSlideshow),
        },
        "sort" => ButtonSpec {
            label: "Sort",
            base_tip: "Sort order",
            symbols: &["arrow.up.arrow.down"],
            selector: None,
            shortcut: None,
        },
        _ => return None,
    })
}

const SEARCH: &str = "search";

fn format_tooltip(base: &str, store: Option<&ShortcutStore>, action: Option<Action>) -> String {
    match (store, action) {
        (Some(store), Some(action)) => store.format_tooltip(base, action, None),
        _ => base.to_owned(),
    }
}

pub struct ToolbarIvars {
    delegate: OnceCell<Retained<AppDelegate>>,
    search: OnceCell<Retained<NSSearchToolbarItem>>,
    sort_menu: OnceCell<Retained<NSMenu>>,
    items: RefCell<Vec<(Action, &'static str, Retained<NSToolbarItem>)>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliToolbar"]
    #[ivars = ToolbarIvars]
    pub struct BrowseToolbar;

    unsafe impl NSObjectProtocol for BrowseToolbar {}

    unsafe impl NSToolbarDelegate for BrowseToolbar {
        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        #[unsafe(method_family = none)]
        fn default_identifiers(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        #[unsafe(method_family = none)]
        fn allowed_identifiers(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            identifiers()
        }

        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        #[unsafe(method_family = none)]
        fn item_for_identifier(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSString,
            inserted: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            self.make_item(identifier, inserted)
        }
    }
);

fn identifiers() -> Retained<NSArray<NSString>> {
    let mut ids: Vec<Retained<NSString>> = unsafe {
        vec![
            NSToolbarToggleSidebarItemIdentifier.copy(),
            NSToolbarSidebarTrackingSeparatorItemIdentifier.copy(),
            NSToolbarFlexibleSpaceItemIdentifier.copy(),
        ]
    };
    for id in [
        "sort",
        "rotateLeft",
        "rotateRight",
        "crop",
        "upscale",
        "compress",
        "move",
        "moveAgain",
        "trash",
        "info",
        "slideshow",
        SEARCH,
    ] {
        ids.push(NSString::from_str(id));
    }
    NSArray::from_retained_slice(&ids)
}

fn symbol(names: &[&str], description: &str) -> Option<Retained<NSImage>> {
    names.iter().find_map(|name| {
        NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(name),
            Some(&NSString::from_str(description)),
        )
    })
}

impl BrowseToolbar {
    pub fn new(
        mtm: MainThreadMarker,
        delegate: &Retained<AppDelegate>,
        sort_menu: Retained<NSMenu>,
    ) -> (Retained<Self>, Retained<NSToolbar>) {
        let this = Self::alloc(mtm).set_ivars(ToolbarIvars {
            delegate: OnceCell::new(),
            search: OnceCell::new(),
            sort_menu: OnceCell::new(),
            items: RefCell::new(Vec::new()),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().delegate.set(delegate.clone());
        let _ = this.ivars().sort_menu.set(sort_menu);

        // Create eagerly so `AppDelegate` can store the search field during window setup.
        let search = NSSearchToolbarItem::initWithItemIdentifier(
            NSSearchToolbarItem::alloc(mtm),
            &NSString::from_str(SEARCH),
        );
        let field = search.searchField();
        field.setPlaceholderString(Some(ns_string!("Filter")));
        unsafe {
            field.setDelegate(Some(ProtocolObject::from_ref(&**delegate)));
            field.setTarget(Some(&**delegate));
            field.setAction(Some(sel!(filterAction:)));
        }
        search.setResignsFirstResponderWithCancel(true);
        search.setPreferredWidthForSearchField(190.0);
        search.setVisibilityPriority(NSToolbarItemVisibilityPriorityHigh);
        search.setLabel(ns_string!("Filter"));
        let filter_tip = format_tooltip(
            "Filter by name",
            Some(&delegate.ivars().shortcuts.borrow()),
            Some(Action::FilterByName),
        );
        search.setToolTip(Some(&NSString::from_str(&filter_tip)));
        let _ = this.ivars().search.set(search);

        let toolbar = NSToolbar::initWithIdentifier(
            NSToolbar::alloc(mtm),
            ns_string!("BlickliBrowseToolbar"),
        );
        toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
        toolbar.setAllowsUserCustomization(false);
        toolbar.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        (this, toolbar)
    }

    pub fn search_field(&self) -> Option<Retained<objc2_app_kit::NSSearchField>> {
        self.ivars().search.get().map(|item| item.searchField())
    }

    pub fn begin_search(&self) {
        if let Some(item) = self.ivars().search.get() {
            item.beginSearchInteraction();
        }
    }

    pub fn end_search(&self) {
        if let Some(item) = self.ivars().search.get() {
            item.endSearchInteraction();
        }
    }

    fn tooltip_base(&self, action: Option<Action>, fallback: &str) -> String {
        if action == Some(Action::MoveAgain) {
            if let Some(delegate) = self.ivars().delegate.get() {
                return delegate.repeat_title(blickli_core::transfer::Transfer::Move);
            }
        }
        fallback.to_owned()
    }

    pub fn sync_tooltips(&self, store: &ShortcutStore) {
        if let Some(search) = self.ivars().search.get() {
            let tip = format_tooltip("Filter by name", Some(store), Some(Action::FilterByName));
            search.setToolTip(Some(&NSString::from_str(&tip)));
        }
        for (action, base, item) in self.ivars().items.borrow().iter() {
            let base = self.tooltip_base(Some(*action), base);
            let tip = format_tooltip(&base, Some(store), Some(*action));
            item.setToolTip(Some(&NSString::from_str(&tip)));
        }
    }

    fn make_item(&self, identifier: &NSString, inserted: bool) -> Option<Retained<NSToolbarItem>> {
        let mtm = self.mtm();
        let id = identifier.to_string();
        if id == SEARCH {
            return self
                .ivars()
                .search
                .get()
                .map(|item| Retained::into_super(item.clone()));
        }
        let spec = button_spec(&id)?;
        let image = symbol(spec.symbols, spec.label);
        let base_tip = self.tooltip_base(spec.shortcut, spec.base_tip);
        let tip = match self.ivars().delegate.get() {
            Some(delegate) => format_tooltip(
                &base_tip,
                Some(&delegate.ivars().shortcuts.borrow()),
                spec.shortcut,
            ),
            None => base_tip,
        };
        if id == "sort" {
            let item = NSMenuToolbarItem::initWithItemIdentifier(NSMenuToolbarItem::alloc(mtm), identifier);
            if let Some(menu) = self.ivars().sort_menu.get() {
                item.setMenu(menu);
            }
            item.setShowsIndicator(true);
            item.setImage(image.as_deref());
            item.setLabel(&NSString::from_str(spec.label));
            item.setPaletteLabel(&NSString::from_str(spec.label));
            item.setToolTip(Some(&NSString::from_str(&tip)));
            return Some(Retained::into_super(item));
        }
        let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(mtm), identifier);
        item.setLabel(&NSString::from_str(spec.label));
        item.setPaletteLabel(&NSString::from_str(spec.label));
        item.setToolTip(Some(&NSString::from_str(&tip)));
        item.setImage(image.as_deref());
        item.setBordered(true);
        unsafe {
            item.setTarget(self.ivars().delegate.get().map(|d| d.as_ref() as &objc2::runtime::AnyObject));
            item.setAction(spec.selector);
        }
        // Keep Slideshow and Filter visible longest in narrow windows.
        let priority = if id == "slideshow" {
            NSToolbarItemVisibilityPriorityHigh
        } else if id == "info" {
            NSToolbarItemVisibilityPriorityLow
        } else {
            NSToolbarItemVisibilityPriorityStandard
        };
        item.setVisibilityPriority(priority);
        if inserted {
            if let Some(action) = spec.shortcut {
                let mut items = self.ivars().items.borrow_mut();
                items.retain(|(existing, _, _)| *existing != action);
                items.push((action, spec.base_tip, item.clone()));
            }
        }
        Some(item)
    }
}
