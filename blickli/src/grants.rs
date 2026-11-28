//! Security-scoped bookmarks for persisting folder access in the macOS App Sandbox.
//! No-op when running unsandboxed.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use blickli_core::messages::folder_label;
use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_app_kit::NSOpenPanel;
use objc2_foundation::{
    ns_string, NSArray, NSData, NSString, NSURLBookmarkCreationOptions,
    NSURLBookmarkResolutionOptions, NSUserDefaults, NSURL,
};

use crate::{e2e, AppDelegate};

thread_local! {
    /// Active security-scoped URLs; access remains valid while retained.
    static OPEN: RefCell<Vec<(PathBuf, Retained<NSURL>)>> = const { RefCell::new(Vec::new()) };
}

pub fn sandboxed() -> bool {
    std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some()
}

/// Real user home directory (in the App Sandbox, `$HOME` points to the container).
pub fn home() -> PathBuf {
    #[repr(C)]
    struct Passwd {
        name: *const std::ffi::c_char,
        passwd: *const std::ffi::c_char,
        uid: u32,
        gid: u32,
        change: i64,
        class: *const std::ffi::c_char,
        gecos: *const std::ffi::c_char,
        dir: *const std::ffi::c_char,
        shell: *const std::ffi::c_char,
        expire: i64,
    }
    extern "C" {
        fn getuid() -> u32;
        fn getpwuid(uid: u32) -> *const Passwd;
    }
    // SAFETY: getpwuid returns a static record or null, and the layout above
    // matches macOS's struct passwd.
    let dir = unsafe {
        let entry = getpwuid(getuid());
        if entry.is_null() || (*entry).dir.is_null() {
            return PathBuf::new();
        }
        std::ffi::CStr::from_ptr((*entry).dir)
            .to_string_lossy()
            .into_owned()
    };
    PathBuf::from(dir)
}

pub fn readable(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok()
}

pub(crate) fn same_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

/// Whether an active security-scoped grant on `held` already covers `path`.
/// Separate mounts (such as external drives under `/Volumes` when `/` is granted)
/// have distinct device IDs and are never treated as covered by a parent volume.
fn is_covered(held: &Path, path: &Path) -> bool {
    if path == held {
        return true;
    }
    path.starts_with(held) && same_volume(held, path)
}

/// Persist a security-scoped bookmark for a user-selected folder.
pub fn remember(url: Retained<NSURL>) -> Option<PathBuf> {
    let path = crate::local_file_path(&url)?;
    if !sandboxed() || e2e::enabled() {
        return Some(path);
    }
    let covered = OPEN.with(|open| {
        open.borrow()
            .iter()
            .any(|(held, _)| is_covered(held, &path))
    });
    if covered {
        return Some(path);
    }
    let Ok(data) = url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
        NSURLBookmarkCreationOptions::WithSecurityScope,
        None,
        None,
    ) else {
        return Some(path);
    };
    let defaults = NSUserDefaults::standardUserDefaults();
    let mut all = stored(&defaults);
    all.push(data);
    save(&defaults, &all);
    OPEN.with(|open| open.borrow_mut().push((path.clone(), url)));
    Some(path)
}

/// Restore saved bookmarks without UI or volume mounting. Unavailable entries are retained.
/// Safe to call again when external volumes mount later.
pub fn restore() {
    if !sandboxed() || e2e::enabled() {
        return;
    }
    let defaults = NSUserDefaults::standardUserDefaults();
    let mut changed = false;
    let mut kept: Vec<Retained<NSData>> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for data in stored(&defaults) {
        let mut stale = objc2::runtime::Bool::NO;
        let resolved = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &data,
                NSURLBookmarkResolutionOptions::WithSecurityScope
                    | NSURLBookmarkResolutionOptions::WithoutUI
                    | NSURLBookmarkResolutionOptions::WithoutMounting,
                None,
                &mut stale,
            )
        };
        let Ok(url) = resolved else {
            kept.push(data);
            continue;
        };
        let Some(path) = crate::local_file_path(&url) else {
            changed = true;
            continue;
        };
        // Drop duplicate saved entries pointing to the same path.
        if !seen.insert(path.clone()) {
            changed = true;
            continue;
        }
        // Keep bookmarks that are already active and readable from an earlier pass.
        // If a removable volume was unmounted and remounted, re-acquire its scope.
        let still_active =
            OPEN.with(|open| open.borrow().iter().any(|(held, _)| held == &path)) && readable(&path);
        if still_active {
            kept.push(data);
            continue;
        }
        if !unsafe { url.startAccessingSecurityScopedResource() } {
            kept.push(data);
            continue;
        }
        let data = if stale.as_bool() {
            url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                NSURLBookmarkCreationOptions::WithSecurityScope,
                None,
                None,
            )
            .inspect(|_| changed = true)
            .unwrap_or(data)
        } else {
            data
        };
        kept.push(data);
        OPEN.with(|open| {
            let mut open = open.borrow_mut();
            open.retain(|(held, old_url)| {
                if held == &path {
                    unsafe { old_url.stopAccessingSecurityScopedResource() };
                    false
                } else {
                    true
                }
            });
            open.push((path, url));
        });
    }
    if changed {
        save(&defaults, &kept);
    }
}

fn stored(defaults: &NSUserDefaults) -> Vec<Retained<NSData>> {
    defaults
        .arrayForKey(ns_string!("grants"))
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.downcast::<NSData>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn save(defaults: &NSUserDefaults, all: &[Retained<NSData>]) {
    unsafe {
        defaults.setObject_forKey(
            Some(&NSArray::from_retained_slice(all)),
            ns_string!("grants"),
        )
    };
}

impl AppDelegate {
    pub(crate) fn ask_access(&self, path: &Path) -> Option<PathBuf> {
        let panel = NSOpenPanel::openPanel(self.mtm());
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setDirectoryURL(NSURL::from_directory_path(path).as_deref());
        panel.setMessage(Some(&NSString::from_str(&format!(
            "Allow Blickli to read {}.",
            folder_label(path)
        ))));
        panel.setPrompt(Some(ns_string!("Grant Access")));
        if !self.run_panel(&panel) {
            return None;
        }
        panel.URL().and_then(remember)
    }
}

#[cfg(test)]
mod tests {
    use super::{is_covered, same_volume};
    use std::path::Path;

    #[test]
    fn coverage_respects_volume_boundaries() {
        let root = std::env::temp_dir().join(format!("blickli-grants-{}", std::process::id()));
        let child = root.join("sub");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&child).unwrap();

        assert!(is_covered(&root, &root));
        assert!(is_covered(&root, &child));
        assert!(!is_covered(&child, &root));
        // On macOS, `/` (System volume) and Data/external volumes have distinct device IDs.
        if !same_volume(Path::new("/"), &root) {
            assert!(!is_covered(Path::new("/"), &root));
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
