//! Move files to macOS Trash via `NSFileManager` (never permanently deletes).

use std::path::PathBuf;

use objc2_foundation::{NSFileManager, NSURL};

pub struct Trashed {
    /// `(original, trash_path)` pairs; `trash_path` is needed to undo since Trash may rename collisions.
    pub moved: Vec<(PathBuf, PathBuf)>,
    pub failed: Vec<String>,
}

pub fn move_to_trash(paths: &[PathBuf]) -> Trashed {
    let manager = NSFileManager::defaultManager();
    let mut result = Trashed {
        moved: Vec::with_capacity(paths.len()),
        failed: Vec::new(),
    };
    for path in paths {
        let Some(url) = NSURL::from_file_path(path) else {
            result.failed.push("Invalid file path".to_owned());
            continue;
        };
        let mut landed: Option<objc2::rc::Retained<NSURL>> = None;
        match manager.trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed)) {
            Ok(()) => {
                let landed = landed.and_then(|url| url.to_file_path());
                match landed {
                    Some(landed) => result.moved.push((path.clone(), landed)),
                    // Trashed, but not restorable.
                    None => result.moved.push((path.clone(), PathBuf::new())),
                }
            }
            Err(error) => {
                let why = error.localizedDescription().to_string();
                eprintln!("trash failed: {}: {why}", path.display());
                result.failed.push(blickli_core::messages::reason(&why));
            }
        }
    }
    result
}
