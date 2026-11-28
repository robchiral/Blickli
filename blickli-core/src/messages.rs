//! Status bar and slideshow overlay message formatting.

use std::path::{Path, PathBuf};

use crate::transfer::Transfer;

/// Format byte sizes in base-10 units (matching Finder).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn folder_label(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.to_string_lossy().into_owned())
}

/// Strip trailing `(os error N)` and period from an OS error string.
pub fn reason(error: &str) -> String {
    let text = match error.find(" (os error") {
        Some(at) => &error[..at],
        None => error,
    };
    text.trim_end_matches('.').to_owned()
}

fn failed_suffix(failed: &[String]) -> String {
    match failed {
        [] => String::new(),
        [first, ..] => format!(", {} failed: {first}", failed.len()),
    }
}

/// Status message for a move or copy batch. Names a single file by its destination name.
pub fn transfer_message(
    kind: Transfer,
    done: &[(PathBuf, PathBuf)],
    folder: &Path,
    renamed: usize,
    failed: &[String],
) -> String {
    let verb = kind.past();
    let target = folder_label(folder);
    let mut text = match done {
        [] => format!("Nothing {} to {target}", verb.to_lowercase()),
        [(_, one)] => format!(
            "{verb} {} to {target}",
            one.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        many => format!("{verb} {} images to {target}", many.len()),
    };
    if renamed > 0 {
        text.push_str(&format!(", {renamed} renamed"));
    }
    text.push_str(&failed_suffix(failed));
    text
}

/// Error message when an undo or redo fails to restore some files.
pub fn put_back_message(total: usize, failed: &[String]) -> String {
    match failed {
        [] => String::new(),
        [first, ..] => format!("{} of {total} could not be restored: {first}", failed.len()),
    }
}

/// Status message for a rotation or crop batch (`verb` is past tense).
pub fn edit_message(verb: &str, done: &[PathBuf], failed: &[String]) -> String {
    let name = |path: &PathBuf| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    match (done, failed) {
        ([], [first, ..]) => format!("Could not save: {first}"),
        ([], []) => String::new(),
        ([one], _) => format!("{verb} {}{}", name(one), failed_suffix(failed)),
        (many, _) => format!("{verb} {} images{}", many.len(), failed_suffix(failed)),
    }
}

/// Status message after moving files to Trash, listing up to three filenames.
pub fn trash_message(moved: &[PathBuf], failed: &[String]) -> String {
    let total = moved.len() + failed.len();
    if moved.is_empty() {
        let first = failed.first().map(String::as_str).unwrap_or_default();
        return if total == 1 {
            format!("Could not move to Trash: {first}")
        } else {
            format!(
                "{} of {total} could not be moved to Trash: {first}",
                failed.len()
            )
        };
    }
    let names: Vec<String> = moved
        .iter()
        .take(3)
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    let mut moved_text = format!("Moved to Trash: {}", names.join(", "));
    if moved.len() > names.len() {
        moved_text.push_str(&format!(" and {} more", moved.len() - names.len()));
    }
    moved_text.push_str(&failed_suffix(failed));
    moved_text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/x/{n}")))
            .collect()
    }

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1000), "1.0 KB");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
        assert_eq!(format_bytes(2_000_000_000_000_000), "2000.0 TB");
    }

    #[test]
    fn a_reason_loses_the_error_number_and_full_stop() {
        assert_eq!(
            reason("Permission denied (os error 13)"),
            "Permission denied"
        );
        assert_eq!(
            reason("You don’t have permission."),
            "You don’t have permission"
        );
    }

    #[test]
    fn a_transfer_names_one_file_and_counts_many() {
        let folder = Path::new("/x/Keep");
        let one = [(
            PathBuf::from("/a/a.heic"),
            PathBuf::from("/x/Keep/a 2.heic"),
        )];
        assert_eq!(
            transfer_message(Transfer::Move, &one, folder, 1, &[]),
            "Moved a 2.heic to Keep, 1 renamed"
        );
        let two = [
            (PathBuf::from("/a/a.heic"), PathBuf::from("/x/Keep/a.heic")),
            (PathBuf::from("/a/b.heic"), PathBuf::from("/x/Keep/b.heic")),
        ];
        let denied = ["Permission denied".to_owned()];
        assert_eq!(
            transfer_message(Transfer::Copy, &two, folder, 0, &denied),
            "Copied 2 images to Keep, 1 failed: Permission denied"
        );
        let reasons = ["Read-only file system".to_owned(), "No space".to_owned()];
        assert_eq!(
            transfer_message(Transfer::Move, &[], Path::new("/"), 0, &reasons),
            "Nothing moved to /, 2 failed: Read-only file system"
        );
        assert_eq!(put_back_message(3, &[]), "");
        assert_eq!(
            put_back_message(3, &["name taken".to_owned()]),
            "1 of 3 could not be restored: name taken"
        );
    }

    #[test]
    fn a_delete_names_three_files_and_counts_the_rest() {
        assert_eq!(
            trash_message(&paths(&["a.heic"]), &[]),
            "Moved to Trash: a.heic"
        );
        assert_eq!(
            trash_message(&paths(&["a", "b", "c", "d", "e"]), &[]),
            "Moved to Trash: a, b, c and 2 more"
        );
        let locked = ["The file is locked".to_owned()];
        assert_eq!(
            trash_message(&paths(&["a"]), &locked),
            "Moved to Trash: a, 1 failed: The file is locked"
        );
        assert_eq!(
            trash_message(&[], &locked),
            "Could not move to Trash: The file is locked"
        );
        let three = vec!["No trash on this volume".to_owned(); 3];
        assert_eq!(
            trash_message(&[], &three),
            "3 of 3 could not be moved to Trash: No trash on this volume"
        );
    }

    #[test]
    fn edits_name_one_file_and_count_more() {
        let one = paths(&["/a/photo.jpg"]);
        assert_eq!(edit_message("Rotated", &one, &[]), "Rotated photo.jpg");
        let two = paths(&["/a/x.jpg", "/a/y.jpg"]);
        assert_eq!(
            edit_message("Rotated", &two, &["Permission denied".to_owned()]),
            "Rotated 2 images, 1 failed: Permission denied"
        );
        assert_eq!(
            edit_message("Cropped", &[], &["WebP files can't be written".to_owned()]),
            "Could not save: WebP files can't be written"
        );
        assert_eq!(edit_message("Cropped", &[], &[]), "");
    }
}
