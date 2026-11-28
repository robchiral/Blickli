//! Finder-style source list sidebar (Favorites and Locations).
//! Folder nodes live in an append-only arena referenced by `NSOutlineView` via
//! `NSNumber` indices so stale indices never alias another folder.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSColor, NSControlTextEditingDelegate, NSDragOperation, NSDraggingInfo, NSEvent,
    NSEventModifierFlags, NSEventType, NSFont, NSImage, NSImageView, NSLineBreakMode, NSMenu,
    NSMenuDelegate, NSMenuItem, NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate,
    NSOutlineViewDropOnItemIndex, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardWriting,
    NSScrollView, NSTableCellView, NSTableColumn, NSTableColumnResizingOptions,
    NSTableViewColumnAutoresizingStyle, NSTableViewRowSizeStyle, NSTableViewStyle, NSTextField,
    NSUserInterfaceItemIdentification, NSView, NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_foundation::{
    ns_string, NSArray, NSFileManager, NSIndexSet, NSInteger, NSNotification, NSNumber, NSObject,
    NSObjectProtocol, NSRect, NSString, NSURLVolumeLocalizedNameKey, NSUserDefaults,
    NSVolumeEnumerationOptions, NSURL,
};

use crate::{e2e, grants, menu_item, AppDelegate};
use blickli_core::transfer::Transfer;

pub const MIN_WIDTH: f64 = 164.0;
pub const MAX_WIDTH: f64 = 360.0;
pub const WIDTH: f64 = 234.0;

const ROW_H: f64 = 28.0;
const ICON: f64 = 18.0;

struct Node {
    path: PathBuf,
    name: String,
    section: bool,
    /// True for direct children of Favorites and Locations; their subfolder probe
    /// is deferred until expanded or clicked to avoid startup TCC prompts and slow-mount stalls.
    top_level: bool,
    /// Subfolders, read on first need.
    children: Option<Vec<usize>>,
    /// Whether it has a subfolder, so empty rows get no triangle.
    expandable: Option<bool>,
}

struct CachedDrag {
    sequence: NSInteger,
    paths: Vec<PathBuf>,
    all_dirs: bool,
    all_files: bool,
}

pub struct SidebarIvars {
    nodes: RefCell<Vec<Node>>,
    favorites_root: Cell<usize>,
    local_root: Cell<usize>,
    favorites: RefCell<Vec<PathBuf>>,
    outline: OnceCell<Retained<NSOutlineView>>,
    delegate: OnceCell<Retained<AppDelegate>>,
    icons: RefCell<HashMap<PathBuf, Retained<NSImage>>>,
    /// Cached drag payload keyed by `draggingSequenceNumber` so `validateDrop:` does not re-stat every frame.
    drag_cache: RefCell<Option<CachedDrag>>,
    /// Set while the highlight follows the grid, so selecting doesn't rescan.
    syncing: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BlickliSidebar"]
    #[ivars = SidebarIvars]
    pub struct Sidebar;

    unsafe impl NSObjectProtocol for Sidebar {}

    unsafe impl NSOutlineViewDataSource for Sidebar {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn number_of_children(
            &self,
            _outline: &NSOutlineView,
            item: Option<&AnyObject>,
        ) -> isize {
            match index_of(item) {
                None => 2,
                Some(index) => {
                    self.load_children(index);
                    self.with_node(index, |node| {
                        node.children.as_ref().map(|c| c.len()).unwrap_or(0) as isize
                    })
                }
            }
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        #[unsafe(method_family = none)]
        fn child_of_item(
            &self,
            _outline: &NSOutlineView,
            index: isize,
            item: Option<&AnyObject>,
        ) -> Retained<AnyObject> {
            let slot = match index_of(item) {
                None => {
                    if index == 0 {
                        self.ivars().favorites_root.get()
                    } else {
                        self.ivars().local_root.get()
                    }
                }
                Some(parent) => {
                    self.load_children(parent);
                    // usize::MAX means no such row; the parent would nest in itself.
                    self.with_node(parent, |node| {
                        node.children
                            .as_ref()
                            .and_then(|c| c.get(index as usize).copied())
                            .unwrap_or(usize::MAX)
                    })
                }
            };
            item_for(slot)
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn is_item_expandable(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            self.expandable(item)
        }

        // Folder rows can be dragged into Favorites.
        #[unsafe(method_id(outlineView:pasteboardWriterForItem:))]
        #[unsafe(method_family = none)]
        fn pasteboard_writer_for_item(
            &self,
            _outline: &NSOutlineView,
            item: &AnyObject,
        ) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
            self.row_pasteboard_writer(item)
        }

        // Folders drop into Favorites; images drop onto a folder.
        #[unsafe(method(outlineView:validateDrop:proposedItem:proposedChildIndex:))]
        fn validate_drop(
            &self,
            outline: &NSOutlineView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            item: Option<&AnyObject>,
            index: NSInteger,
        ) -> NSDragOperation {
            let (paths, all_dirs, all_files) = self.cached_drag(info);
            if !paths.is_empty() && all_dirs {
                return match self.favorites_slot(item, index) {
                    Some(slot) => {
                        // Aim folder drops at the gap, whatever row they're over.
                        unsafe {
                            outline.setDropItem_dropChildIndex(
                                Some(&item_for(self.ivars().favorites_root.get())),
                                slot as NSInteger,
                            )
                        };
                        NSDragOperation::Generic
                    }
                    None => NSDragOperation::None,
                };
            }
            let Some(target) = self.drop_target(item) else {
                return NSDragOperation::None;
            };
            if paths.is_empty() || !all_files || !drop_allowed(&target, &paths) {
                return NSDragOperation::None;
            }
            if index != NSOutlineViewDropOnItemIndex as NSInteger {
                // Between rows means onto the row's folder.
                unsafe {
                    outline.setDropItem_dropChildIndex(
                        item,
                        NSOutlineViewDropOnItemIndex as NSInteger,
                    )
                };
            }
            drop_kind(&target, &paths)
        }

        #[unsafe(method(outlineView:acceptDrop:item:childIndex:))]
        fn accept_drop(
            &self,
            _outline: &NSOutlineView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            item: Option<&AnyObject>,
            index: NSInteger,
        ) -> bool {
            self.take_drop(info, item, index)
        }
    }

    // Required by NSOutlineViewDelegate; the sidebar edits no text.
    unsafe impl NSControlTextEditingDelegate for Sidebar {}

    unsafe impl NSOutlineViewDelegate for Sidebar {
        #[unsafe(method(outlineView:isGroupItem:))]
        fn is_group_item(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            index_of(Some(item)).is_some_and(|index| self.with_node(index, |node| node.section))
        }

        #[unsafe(method(outlineView:shouldSelectItem:))]
        fn should_select_item(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            index_of(Some(item)).is_some_and(|index| self.with_node(index, |node| !node.section))
        }

        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        #[unsafe(method_family = none)]
        fn view_for_item(
            &self,
            outline: &NSOutlineView,
            _column: Option<&NSTableColumn>,
            item: Option<&AnyObject>,
        ) -> Option<Retained<NSView>> {
            self.row_view_for(outline, item)
        }

        #[unsafe(method(outlineViewSelectionDidChange:))]
        fn selection_did_change(&self, _notification: &NSNotification) {
            if self.ivars().syncing.get() {
                return;
            }
            // Mouse clicks invoke `rowClicked:` after selection changes; defer opening to
            // `rowClicked:` so sandbox access prompts never fire twice on click-cancel.
            let is_mouse = NSApplication::sharedApplication(self.mtm())
                .currentEvent()
                .is_some_and(|e| e.r#type() == NSEventType::LeftMouseDown);
            if is_mouse {
                return;
            }
            self.open_selected();
        }
    }

    unsafe impl NSMenuDelegate for Sidebar {
        // Built for the clicked row; section headers get none.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            menu.removeAllItems();
            let Some(index) = self.clicked_index() else { return };
            self.fill_context_menu(menu, index);
        }
    }

    impl Sidebar {
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            let enabled = if item.action() == Some(objc2::sel!(compressFolder:)) {
                !self
                    .ivars()
                    .delegate
                    .get()
                    .is_some_and(|delegate| delegate.is_busy())
            } else {
                true
            };
            enabled
        }

        #[unsafe(method(rowClicked:))]
        fn row_clicked(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            if self.with_node(index, |node| node.section) {
                return;
            }
            self.open_folder(index, true);
        }

        #[unsafe(method(addFavorite:))]
        fn add_favorite_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.add_favorite(&self.path_of(index));
        }

        #[unsafe(method(removeFavorite:))]
        fn remove_favorite_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.remove_favorite(&self.path_of(index));
        }

        #[unsafe(method(scanFolderOnly:))]
        fn scan_folder_only(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.open_folder(index, false);
        }

        #[unsafe(method(compressFolder:))]
        fn compress_folder_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            let Some(delegate) = self.ivars().delegate.get() else {
                return;
            };
            let path = self.path_of(index);
            if grants::sandboxed() && !grants::readable(&path) {
                let Some(granted) = delegate.ask_access(&path) else {
                    return;
                };
                self.refresh_nodes_for(&path);
                if granted != path {
                    self.refresh_nodes_for(&granted);
                }
                delegate.compress_folder(granted, true);
                return;
            }
            delegate.compress_folder(path, true);
        }

        #[unsafe(method(openInFinder:))]
        fn open_in_finder_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.open_in_finder(index);
        }

        #[unsafe(method(volumesChanged:))]
        fn volumes_changed(&self, _notification: Option<&NSNotification>) {
            grants::restore();
            self.reload_section(self.ivars().local_root.get());
        }
    }
);

impl Sidebar {
    /// Build the tree and its scroll view.
    pub fn new(
        mtm: MainThreadMarker,
        delegate: &Retained<AppDelegate>,
    ) -> (Retained<Self>, Retained<NSScrollView>) {
        let favorites = load_favorites();
        let nodes = vec![section_node("Favorites"), section_node("Locations")];

        let this = Self::alloc(mtm).set_ivars(SidebarIvars {
            nodes: RefCell::new(nodes),
            favorites_root: Cell::new(0),
            local_root: Cell::new(1),
            favorites: RefCell::new(favorites),
            outline: OnceCell::new(),
            delegate: OnceCell::new(),
            icons: RefCell::new(HashMap::new()),
            drag_cache: RefCell::new(None),
            syncing: Cell::new(false),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().delegate.set(delegate.clone());

        let frame = NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, 400.0));
        let outline = NSOutlineView::initWithFrame(NSOutlineView::alloc(mtm), frame);
        let column =
            NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), ns_string!("name"));
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        outline.addTableColumn(&column);
        unsafe { outline.setOutlineTableColumn(Some(&column)) };
        outline.setHeaderView(None);
        outline.setStyle(NSTableViewStyle::SourceList);
        outline.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::LastColumnOnlyAutoresizingStyle,
        );
        outline.setAutoresizesOutlineColumn(false);
        outline.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        outline.setRowSizeStyle(NSTableViewRowSizeStyle::Custom);
        outline.setRowHeight(ROW_H);
        outline.setIndentationPerLevel(14.0);
        outline.setFloatsGroupRows(false);
        unsafe {
            outline.setDataSource(Some(ProtocolObject::from_ref(&*this)));
            outline.setDelegate(Some(ProtocolObject::from_ref(&*this)));
            // Clicks arrive through the action, keys through the selection callback.
            outline.setTarget(Some(&this));
            outline.setAction(Some(objc2::sel!(rowClicked:)));
        }

        unsafe {
            outline.registerForDraggedTypes(&NSArray::from_slice(&[NSPasteboardTypeFileURL]))
        };

        let menu = NSMenu::new(mtm);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        unsafe { outline.setMenu(Some(&menu)) };

        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setDocumentView(Some(&outline));

        let _ = this.ivars().outline.set(outline.clone());
        unsafe {
            outline.expandItem(Some(&item_for(this.ivars().favorites_root.get())));
            outline.expandItem(Some(&item_for(this.ivars().local_root.get())));
        }
        outline.sizeLastColumnToFit();

        unsafe {
            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            for name in [
                objc2_app_kit::NSWorkspaceDidMountNotification,
                objc2_app_kit::NSWorkspaceDidUnmountNotification,
            ] {
                center.addObserver_selector_name_object(
                    &this,
                    objc2::sel!(volumesChanged:),
                    Some(name),
                    None,
                );
            }
        }

        (this, scroll)
    }

    fn expandable(&self, item: &AnyObject) -> bool {
        let Some(index) = index_of(Some(item)) else {
            return false;
        };
        if self.with_node(index, |node| node.section) {
            return true;
        }
        if let Some(known) = self.with_node(index, |node| node.expandable) {
            return known;
        }
        // Defer probing top-level Favorites (e.g. Desktop) and Locations (e.g. external/network
        // volumes) until expanded or clicked, avoiding startup TCC prompts and slow-mount stalls.
        if self.with_node(index, |node| node.top_level) {
            return true;
        }
        // Leave `expandable` unset when unreadable so granting sandbox access later
        // does not leave the row stuck without a disclosure triangle.
        let Some(expandable) = has_subfolder(&self.path_of(index)) else {
            return false;
        };
        self.ivars().nodes.borrow_mut()[index].expandable = Some(expandable);
        expandable
    }

    fn row_view_for(
        &self,
        outline: &NSOutlineView,
        item: Option<&AnyObject>,
    ) -> Option<Retained<NSView>> {
        let index = index_of(item)?;
        let (name, path, section) = self.with_node(index, |node| {
            (node.name.clone(), node.path.clone(), node.section)
        });
        let icon = (!section).then(|| self.icon(index));
        let identifier = if section {
            ns_string!("section")
        } else {
            ns_string!("folder")
        };
        let cell = unsafe { outline.makeViewWithIdentifier_owner(identifier, None) }
            .and_then(|view| view.downcast::<NSTableCellView>().ok())
            .unwrap_or_else(|| row_view(self.mtm(), identifier, section));
        if let Some(field) = unsafe { cell.textField() } {
            field.setStringValue(&NSString::from_str(&name));
        }
        if let Some(view) = unsafe { cell.imageView() } {
            view.setImage(icon.as_deref());
        }
        if !section {
            cell.setToolTip(Some(&NSString::from_str(&path.to_string_lossy())));
        }
        Some(Retained::into_super(cell))
    }

    fn open_selected(&self) {
        if let Some(index) = self.selected_index() {
            self.open_folder(index, true);
        }
    }

    fn open_folder(&self, index: usize, recursive: bool) {
        let path = self.path_of(index);
        let Some(delegate) = self.ivars().delegate.get() else {
            return;
        };
        // The sandbox needs the user to grant this folder first.
        if grants::sandboxed() && !grants::readable(&path) {
            let Some(granted) = delegate.ask_access(&path) else {
                // Restore highlight to the currently open folder when access is cancelled.
                if let Some((root, _)) = delegate.current_scan() {
                    self.reveal(&root);
                }
                return;
            };
            self.refresh_nodes_for(&path);
            if granted != path {
                self.refresh_nodes_for(&granted);
            }
            delegate.scan_root(granted, recursive);
            return;
        }
        if self.with_node(index, |node| !node.section && node.expandable.is_none()) {
            self.probe_nodes_for(&path);
        }
        if delegate.current_scan() == Some((path.clone(), recursive)) {
            return;
        }
        delegate.scan_root(path, recursive);
    }

    /// Re-read a row's triangle and children.
    fn refresh_node(&self, index: usize) {
        {
            let mut nodes = self.ivars().nodes.borrow_mut();
            nodes[index].expandable = None;
            nodes[index].children = None;
        }
        unsafe {
            self.outline()
                .reloadItem_reloadChildren(Some(&item_for(index)), true)
        };
    }

    /// Refresh every sidebar row matching `path` (e.g. both Favorites and Locations).
    fn refresh_nodes_for(&self, path: &Path) {
        let indices: Vec<usize> = {
            let nodes = self.ivars().nodes.borrow();
            nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| !node.section && node.path == path)
                .map(|(i, _)| i)
                .collect()
        };
        for index in indices {
            self.refresh_node(index);
        }
    }

    /// Resolve deferred `expandable` status for rows matching `path` once the user clicks or reveals it.
    fn probe_nodes_for(&self, path: &Path) {
        let Some(expandable) = has_subfolder(path) else {
            return;
        };
        let indices: Vec<usize> = {
            let mut nodes = self.ivars().nodes.borrow_mut();
            let mut updated = Vec::new();
            for (i, node) in nodes.iter_mut().enumerate() {
                if !node.section && node.path == path && node.expandable != Some(expandable) {
                    node.expandable = Some(expandable);
                    updated.push(i);
                }
            }
            updated
        };
        for index in indices {
            unsafe {
                self.outline()
                    .reloadItem_reloadChildren(Some(&item_for(index)), false)
            };
        }
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::new().expect("sidebar runs on the main thread")
    }

    fn outline(&self) -> &NSOutlineView {
        self.ivars().outline.get().expect("outline view is set")
    }

    /// Read an arena slot; an unknown index reads as an empty row.
    fn with_node<T>(&self, index: usize, f: impl FnOnce(&Node) -> T) -> T {
        let nodes = self.ivars().nodes.borrow();
        match nodes.get(index) {
            Some(node) => f(node),
            None => f(&folder_node(PathBuf::new(), Some(String::new()), false)),
        }
    }

    pub fn fill_context_menu(&self, menu: &NSMenu, index: usize) {
        if self.with_node(index, |node| node.section) {
            return;
        }
        let mtm = self.mtm();
        let path = self.path_of(index);
        let favorite = self.ivars().favorites.borrow().contains(&path);
        let entries: [Option<(&str, objc2::runtime::Sel)>; 6] = [
            Some(("This Folder Only", objc2::sel!(scanFolderOnly:))),
            Some(("Compress Folder…", objc2::sel!(compressFolder:))),
            None,
            Some(if favorite {
                ("Remove from Favorites", objc2::sel!(removeFavorite:))
            } else {
                ("Add to Favorites", objc2::sel!(addFavorite:))
            }),
            None,
            Some(("Open in Finder", objc2::sel!(openInFinder:))),
        ];
        for entry in entries {
            let Some((title, action)) = entry else {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            let item = menu_item(mtm, &NSString::from_str(title), action, ns_string!(""));
            unsafe { item.setTarget(Some(self)) };
            menu.addItem(&item);
        }
    }

    fn open_in_finder(&self, index: usize) {
        if let Some(url) = NSURL::from_directory_path(self.path_of(index)) {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    }

    fn cached_drag(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> (Vec<PathBuf>, bool, bool) {
        let seq = info.draggingSequenceNumber();
        if let Some(cached) = self.ivars().drag_cache.borrow().as_ref() {
            if cached.sequence == seq {
                return (cached.paths.clone(), cached.all_dirs, cached.all_files);
            }
        }
        let paths = dropped_paths(info);
        let all_dirs = !paths.is_empty() && paths.iter().all(|p| p.is_dir());
        let all_files = !paths.is_empty() && !all_dirs && paths.iter().all(|p| p.is_file());
        *self.ivars().drag_cache.borrow_mut() = Some(CachedDrag {
            sequence: seq,
            paths: paths.clone(),
            all_dirs,
            all_files,
        });
        (paths, all_dirs, all_files)
    }

    fn take_drop(
        &self,
        info: &ProtocolObject<dyn NSDraggingInfo>,
        item: Option<&AnyObject>,
        index: NSInteger,
    ) -> bool {
        let (paths, all_dirs, all_files) = self.cached_drag(info);
        *self.ivars().drag_cache.borrow_mut() = None;
        if !paths.is_empty() && all_dirs {
            let Some(slot) = self.favorites_slot(item, index) else {
                return false;
            };
            self.insert_favorites(&paths, slot);
            return true;
        }
        let Some(target) = self.drop_target(item) else {
            return false;
        };
        let Some(delegate) = self.ivars().delegate.get() else {
            return false;
        };
        if paths.is_empty() || !all_files || !drop_allowed(&target, &paths) {
            return false;
        }
        let kind = if drop_kind(&target, &paths) == NSDragOperation::Copy {
            Transfer::Copy
        } else {
            Transfer::Move
        };
        delegate.transfer_paths(paths, target, kind);
        true
    }

    fn row_pasteboard_writer(
        &self,
        item: &AnyObject,
    ) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
        let path = self.drop_target(Some(item))?;
        let url = NSURL::from_directory_path(&path)?;
        let entry = NSPasteboardItem::new();
        unsafe {
            entry.setString_forType(
                &url.absoluteString().unwrap_or_default(),
                NSPasteboardTypeFileURL,
            )
        };
        Some(ProtocolObject::from_retained(entry))
    }

    /// Where a dropped folder goes in Favorites, or None. A drop on a favorite
    /// means the gap above it.
    fn favorites_slot(&self, item: Option<&AnyObject>, index: NSInteger) -> Option<usize> {
        let root = self.ivars().favorites_root.get();
        let target = index_of(item)?;
        let children = self
            .with_node(root, |node| node.children.clone())
            .unwrap_or_default();
        if target == root {
            return Some(if index < 0 {
                children.len()
            } else {
                index as usize
            });
        }
        children.iter().position(|&child| child == target)
    }

    /// The folder a drop lands in: never a section header.
    fn drop_target(&self, item: Option<&AnyObject>) -> Option<PathBuf> {
        let index = index_of(item)?;
        self.with_node(index, |node| (!node.section).then(|| node.path.clone()))
    }

    fn path_of(&self, index: usize) -> PathBuf {
        self.with_node(index, |node| node.path.clone())
    }

    fn load_children(&self, index: usize) {
        if self.with_node(index, |node| node.children.is_some()) {
            return;
        }
        let (path, section) = self.with_node(index, |node| (node.path.clone(), node.section));
        let rows: Vec<(PathBuf, Option<String>)> = if section {
            if index == self.ivars().favorites_root.get() {
                let home = grants::home();
                self.ivars()
                    .favorites
                    .borrow()
                    .iter()
                    .map(|p| (p.clone(), (p == &home).then(|| "Home".to_owned())))
                    .collect()
            } else {
                volumes()
            }
        } else {
            let Some(found) = subfolders(&path) else {
                return;
            };
            found.into_iter().map(|p| (p, None)).collect()
        };
        let mut nodes = self.ivars().nodes.borrow_mut();
        let mut children = Vec::with_capacity(rows.len());
        for (path, name) in rows {
            children.push(nodes.len());
            nodes.push(folder_node(path, name, section));
        }
        if !section {
            nodes[index].expandable = Some(!children.is_empty());
        }
        nodes[index].children = Some(children);
    }

    fn reload_section(&self, index: usize) {
        self.ivars().nodes.borrow_mut()[index].children = None;
        self.load_children(index);
        let item = item_for(index);
        unsafe { self.outline().reloadItem_reloadChildren(Some(&item), true) };
        if let Some((root, _)) = self
            .ivars()
            .delegate
            .get()
            .and_then(|delegate| delegate.current_scan())
        {
            self.reveal(&root);
        }
    }

    fn icon(&self, index: usize) -> Retained<NSImage> {
        let path = self.path_of(index);
        if let Some(icon) = self.ivars().icons.borrow().get(&path) {
            return icon.clone();
        }
        let icon = NSWorkspace::sharedWorkspace()
            .iconForFile(&NSString::from_str(&path.to_string_lossy()));
        icon.setSize(CGSize::new(ICON, ICON));
        self.ivars().icons.borrow_mut().insert(path, icon.clone());
        icon
    }

    fn clicked_index(&self) -> Option<usize> {
        let row = self.outline().clickedRow();
        if row < 0 {
            return None;
        }
        index_of(self.outline().itemAtRow(row).as_deref())
    }

    #[cfg(any(test, feature = "e2e"))]
    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_index().map(|index| self.path_of(index))
    }

    fn selected_index(&self) -> Option<usize> {
        let row = self.outline().selectedRow();
        if row < 0 {
            return None;
        }
        let index = index_of(self.outline().itemAtRow(row).as_deref())?;
        (!self.with_node(index, |node| node.section)).then_some(index)
    }

    // ---- favorites ----

    pub fn add_favorite(&self, path: &Path) {
        {
            let mut favorites = self.ivars().favorites.borrow_mut();
            if favorites.contains(&path.to_path_buf()) {
                return;
            }
            favorites.push(path.to_path_buf());
        }
        self.save_favorites();
        self.reload_section(self.ivars().favorites_root.get());
    }

    /// Put a folder in Favorites at `at`, moving it if already there.
    #[cfg(any(test, feature = "e2e"))]
    pub fn insert_favorite(&self, path: &Path, at: usize) {
        self.insert_favorites(std::slice::from_ref(&path.to_path_buf()), at);
    }

    /// Put one or more folders in Favorites starting at `at`, saving and reloading once.
    fn insert_favorites(&self, paths: &[PathBuf], at: usize) {
        {
            let mut favorites = self.ivars().favorites.borrow_mut();
            for (offset, path) in paths.iter().enumerate() {
                reorder(&mut favorites, path, at + offset);
            }
        }
        self.save_favorites();
        self.reload_section(self.ivars().favorites_root.get());
    }

    pub fn remove_favorite(&self, path: &Path) {
        self.ivars().favorites.borrow_mut().retain(|p| p != path);
        self.save_favorites();
        self.reload_section(self.ivars().favorites_root.get());
    }

    fn save_favorites(&self) {
        if e2e::enabled() || crate::snapshot::enabled() {
            return;
        }
        let strings: Vec<Retained<NSString>> = self
            .ivars()
            .favorites
            .borrow()
            .iter()
            .map(|p| NSString::from_str(&p.to_string_lossy()))
            .collect();
        let refs: Vec<&NSString> = strings.iter().map(|s| &**s).collect();
        let array = objc2_foundation::NSArray::from_slice(&refs);
        unsafe {
            NSUserDefaults::standardUserDefaults()
                .setObject_forKey(Some(&array), ns_string!("favorites"));
        }
    }

    // ---- following the grid ----

    /// Expand to `path`, highlight it, and scroll to it. The highlight must
    /// match the grid.
    pub fn reveal(&self, path: &Path) {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        // Expanding can move the highlight, so guard the whole walk.
        self.ivars().syncing.set(true);
        self.walk_to(&target);
        self.ivars().syncing.set(false);
    }

    fn walk_to(&self, target: &Path) {
        let Some((mut index, base)) = self.best_root(target) else {
            return unsafe { self.outline().deselectAll(None) };
        };
        // Resolve deferred expandability for a volume or favorite once revealed.
        if self.with_node(index, |node| !node.section && node.expandable.is_none())
            && grants::readable(&base)
        {
            self.probe_nodes_for(&base);
        }
        if let Ok(rest) = target.strip_prefix(&base) {
            for component in rest.components() {
                let name = component.as_os_str();
                unsafe { self.outline().expandItem(Some(&item_for(index))) };
                self.load_children(index);
                let child = |this: &Self| {
                    this.with_node(index, |node| node.children.clone())
                        .and_then(|kids| {
                            kids.into_iter().find(|kid| {
                                this.with_node(*kid, |n| n.path.file_name() == Some(name))
                            })
                        })
                };
                // A new folder needs one re-read before giving up.
                let next = child(self).or_else(|| {
                    self.refresh_node(index);
                    child(self)
                });
                match next {
                    Some(next) => index = next,
                    // Hidden folders and packages have no row; highlighting an ancestor would
                    // mislead.
                    None => return unsafe { self.outline().deselectAll(None) },
                }
            }
        }
        let row = unsafe { self.outline().rowForItem(Some(&item_for(index))) };
        if row < 0 {
            return unsafe { self.outline().deselectAll(None) };
        }
        self.outline().selectRowIndexes_byExtendingSelection(
            &NSIndexSet::indexSetWithIndex(row as usize),
            false,
        );
        self.outline().scrollRowToVisible(row);
    }

    /// Deepest Favorite or mounted volume containing `target` (expects a canonicalized
    /// `target` and compares paths lexically to avoid blocking on disconnected network shares).
    fn best_root(&self, target: &Path) -> Option<(usize, PathBuf)> {
        let mut best: Option<(usize, PathBuf)> = None;
        for section in [
            self.ivars().favorites_root.get(),
            self.ivars().local_root.get(),
        ] {
            self.load_children(section);
            let children = self
                .with_node(section, |node| node.children.clone())
                .unwrap_or_default();
            for child in children {
                let path = self.path_of(child);
                if !target.starts_with(&path) {
                    continue;
                }
                let deeper = best
                    .as_ref()
                    .map(|(_, current)| path.components().count() > current.components().count())
                    .unwrap_or(true);
                if deeper {
                    best = Some((child, path));
                }
            }
        }
        best
    }

    // ---- e2e accessors ----

    #[cfg(any(test, feature = "e2e"))]
    pub fn e2e_click(&self, path: &Path) {
        self.reveal(path);
        if let (Some(path), Some(delegate)) = (self.selected_path(), self.ivars().delegate.get()) {
            delegate.scan_root(path, true);
        }
    }

    /// Titles of the right-click menu for the row that holds `path`.
    #[cfg(any(test, feature = "e2e"))]
    pub fn e2e_context_menu(&self, path: &Path) -> Vec<String> {
        let index = {
            let nodes = self.ivars().nodes.borrow();
            nodes
                .iter()
                .position(|node| !node.section && node.path == path)
        };
        let menu = NSMenu::new(self.mtm());
        if let Some(index) = index {
            self.fill_context_menu(&menu, index);
        }
        menu.itemArray()
            .iter()
            .map(|item| item.title().to_string())
            .collect()
    }

    #[cfg(any(test, feature = "e2e"))]
    pub fn e2e_favorites(&self) -> Vec<PathBuf> {
        self.ivars().favorites.borrow().clone()
    }
}

fn dropped_paths(info: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    crate::file_ops::pasteboard_paths(&info.draggingPasteboard())
}

/// Insert or move `path` to index `at`, adjusting for removal offset.
fn reorder(list: &mut Vec<PathBuf>, path: &Path, at: usize) {
    let mut at = at.min(list.len());
    if let Some(old) = list.iter().position(|held| held == path) {
        list.remove(old);
        if old < at {
            at -= 1;
        }
    }
    list.insert(at.min(list.len()), path.to_path_buf());
}

fn drop_allowed(target: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| path.parent() != Some(target))
}

/// Resolve drop operation using Finder rules (inspects only the first file for volume check).
fn drop_kind(target: &Path, paths: &[PathBuf]) -> NSDragOperation {
    let held = NSEvent::modifierFlags_class();
    let same_disk = paths
        .first()
        .is_some_and(|path| grants::same_volume(path, target));
    match drop_transfer(
        held.contains(NSEventModifierFlags::Option),
        held.contains(NSEventModifierFlags::Command),
        same_disk,
    ) {
        Transfer::Copy => NSDragOperation::Copy,
        Transfer::Move => NSDragOperation::Move,
    }
}

/// Finder drag rules: move within a volume, copy across volumes; Option forces copy, Command forces move.
fn drop_transfer(option: bool, command: bool, same_disk: bool) -> Transfer {
    if option {
        Transfer::Copy
    } else if command || same_disk {
        Transfer::Move
    } else {
        Transfer::Copy
    }
}

fn index_of(item: Option<&AnyObject>) -> Option<usize> {
    item?.downcast_ref::<NSNumber>().map(|n| n.as_usize())
}

fn item_for(index: usize) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(NSNumber::new_usize(index)) }
}

fn section_node(name: &str) -> Node {
    Node {
        path: PathBuf::new(),
        name: name.to_string(),
        section: true,
        top_level: false,
        children: None,
        expandable: Some(true),
    }
}

fn folder_node(path: PathBuf, display: Option<String>, top_level: bool) -> Node {
    let name = display.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    });
    Node {
        path,
        name,
        section: false,
        top_level,
        children: None,
        expandable: None,
    }
}

fn row_view(
    mtm: MainThreadMarker,
    identifier: &NSString,
    section: bool,
) -> Retained<NSTableCellView> {
    let cell = NSTableCellView::initWithFrame(
        NSTableCellView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, ROW_H)),
    );
    cell.setIdentifier(Some(identifier));
    let text_x = if section { 4.0 } else { ICON + 10.0 };
    if !section {
        let view = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(
                CGPoint::new(3.0, ((ROW_H - ICON) / 2.0).round()),
                CGSize::new(ICON, ICON),
            ),
        );
        cell.addSubview(&view);
        unsafe { cell.setImageView(Some(&view)) };
    }
    let field = NSTextField::labelWithString(ns_string!(""), mtm);
    let text_h = 19.0;
    field.setFrame(NSRect::new(
        CGPoint::new(text_x, ((ROW_H - text_h) / 2.0).round()),
        CGSize::new(WIDTH - text_x - 10.0, text_h),
    ));
    field.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
    field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    if section {
        field.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.3)));
        field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    } else {
        field.setFont(Some(&NSFont::systemFontOfSize(14.0)));
    }
    cell.addSubview(&field);
    unsafe { cell.setTextField(Some(&field)) };
    cell
}

/// Saved favorites; the first run seeds Pictures, Desktop and home.
fn load_favorites() -> Vec<PathBuf> {
    let home = grants::home();
    let seed = || vec![home.join("Pictures"), home.join("Desktop"), home.clone()];
    if e2e::enabled() {
        return seed();
    }
    let stored = NSUserDefaults::standardUserDefaults().stringArrayForKey(ns_string!("favorites"));
    // An empty saved list still wins over the seeds.
    match stored {
        Some(array) => array.iter().map(|s| PathBuf::from(s.to_string())).collect(),
        None => seed(),
    }
}

/// Finder-visible mounts and the startup disk.
fn volumes() -> Vec<(PathBuf, Option<String>)> {
    let key = unsafe { NSURLVolumeLocalizedNameKey };
    let keys = NSArray::from_slice(&[key]);
    let mounted = NSFileManager::defaultManager()
        .mountedVolumeURLsIncludingResourceValuesForKeys_options(
            Some(&keys),
            NSVolumeEnumerationOptions::SkipHiddenVolumes,
        );
    let mut volumes = Vec::new();
    if let Some(mounted) = mounted {
        volumes.extend(mounted.iter().filter_map(|url| {
            let path = url.to_file_path()?;
            if path != Path::new("/") && path.parent() != Some(Path::new("/Volumes")) {
                return None;
            }
            let name = url
                .resourceValuesForKeys_error(&keys)
                .ok()
                .and_then(|values| values.objectForKey(key))
                .and_then(|value| value.downcast::<NSString>().ok())
                .map(|name| name.to_string());
            Some((path, name))
        }));
    }
    if !volumes.iter().any(|(path, _)| path == Path::new("/")) {
        volumes.push((PathBuf::from("/"), Some("Startup Disk".to_string())));
    }
    volumes.sort_by(|(a, name_a), (b, name_b)| {
        (b == Path::new("/"))
            .cmp(&(a == Path::new("/")))
            .then_with(|| {
                let name_a = name_a
                    .as_deref()
                    .unwrap_or_else(|| a.to_str().unwrap_or_default());
                let name_b = name_b
                    .as_deref()
                    .unwrap_or_else(|| b.to_str().unwrap_or_default());
                blickli_core::natural_str_cmp(name_a, name_b)
            })
    });
    volumes
}

/// Visible subfolders in natural order, or `None` if the directory cannot be read.
fn subfolders(path: &Path) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut folders: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter(is_folder)
        .map(|e| e.path())
        .filter(|p| !is_package(p))
        .collect();
    folders.sort_by(|a, b| {
        blickli_core::natural_str_cmp(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    Some(folders)
}

/// Packages (apps, photo libraries) are left out, as in Finder.
pub(crate) fn is_package(path: &Path) -> bool {
    objc2::rc::autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace()
            .isFilePackageAtPath(&NSString::from_str(&path.to_string_lossy()))
    })
}

/// Stops at the first subfolder, so big folders are cheap.
/// Returns `None` if the directory cannot be read (e.g. before sandbox access is granted).
fn has_subfolder(path: &Path) -> Option<bool> {
    let entries = std::fs::read_dir(path).ok()?;
    Some(entries.flatten().any(|e| {
        !e.file_name().to_string_lossy().starts_with('.') && is_folder(&e) && !is_package(&e.path())
    }))
}

/// A folder or a link to one; only links cost a stat.
fn is_folder(entry: &std::fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(kind) if kind.is_dir() => true,
        Ok(kind) if kind.is_symlink() => entry.path().is_dir(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{drop_allowed, drop_transfer, has_subfolder, reorder, subfolders};
    use blickli_core::transfer::Transfer;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_folder_lands_where_it_was_dropped() {
        let mut list = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        // A new folder takes the gap it was dropped in.
        reorder(&mut list, Path::new("/new"), 1);
        assert_eq!(list[1], PathBuf::from("/new"));
        assert_eq!(list.len(), 4);
        // Past the end it goes last, and never out of bounds.
        reorder(&mut list, Path::new("/last"), 99);
        assert_eq!(list.last().unwrap(), &PathBuf::from("/last"));
        // A row moved down accounts for the gap it leaves behind.
        let mut list = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        reorder(&mut list, Path::new("/a"), 2);
        assert_eq!(
            list,
            vec![
                PathBuf::from("/b"),
                PathBuf::from("/a"),
                PathBuf::from("/c")
            ]
        );
        // And moving up lands above the row it was dropped on.
        reorder(&mut list, Path::new("/c"), 0);
        assert_eq!(list[0], PathBuf::from("/c"));
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn a_drop_needs_an_image_from_another_folder() {
        let target = Path::new("/pics/keepers");
        let inside = vec![PathBuf::from("/pics/keepers/a.heic")];
        let outside = vec![PathBuf::from("/pics/a.heic")];
        let mixed = vec![
            PathBuf::from("/pics/keepers/a.heic"),
            PathBuf::from("/pics/b.heic"),
        ];
        assert!(!drop_allowed(target, &inside));
        assert!(!drop_allowed(target, &[]));
        assert!(drop_allowed(target, &outside));
        assert!(drop_allowed(target, &mixed));
    }

    #[test]
    fn subfolders_come_from_the_listing() {
        let root = std::env::temp_dir().join(format!("blickli-sidebar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        std::os::unix::fs::symlink(root.join("b"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("file-link")).unwrap();

        // A folder and a link to one, not a file, a link to a file,
        // or a hidden folder.
        assert_eq!(
            subfolders(&root),
            Some(vec![root.join("b"), root.join("link")])
        );
        assert_eq!(has_subfolder(&root), Some(true));
        assert_eq!(has_subfolder(&root.join("b")), Some(false));
        assert_eq!(has_subfolder(&root.join("missing")), None);
        assert_eq!(subfolders(&root.join("missing")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_drop_moves_inside_a_disk_and_copies_across() {
        // option, command, same disk
        assert!(drop_transfer(false, false, true) == Transfer::Move);
        assert!(drop_transfer(false, false, false) == Transfer::Copy);
        assert!(drop_transfer(true, false, true) == Transfer::Copy);
        assert!(drop_transfer(false, true, false) == Transfer::Move);
        assert!(drop_transfer(true, true, false) == Transfer::Copy);
    }
}
