//! Non-destructive file moves and copies with Finder-style numbering on collision.

use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    Move,
    Copy,
}

pub struct Transferred {
    pub target: PathBuf,
    /// `true` if a numeric suffix was appended to avoid overwriting an existing file.
    pub renamed: bool,
}

impl Transfer {
    pub fn present(self) -> &'static str {
        match self {
            Transfer::Move => "Moving",
            Transfer::Copy => "Copying",
        }
    }

    pub fn past(self) -> &'static str {
        match self {
            Transfer::Move => "Moved",
            Transfer::Copy => "Copied",
        }
    }

    /// Move or copy `source` into `folder`, appending a number on name collision.
    pub fn apply(self, source: &Path, folder: &Path) -> std::io::Result<Transferred> {
        check_source(source)?;
        // No-op when moving a file into its current folder (including symlink/case aliases).
        if self == Transfer::Move
            && source
                .parent()
                .is_some_and(|parent| same_directory(parent, folder))
        {
            return Ok(Transferred {
                target: source.to_path_buf(),
                renamed: false,
            });
        }
        let name = source.file_name().ok_or(ErrorKind::InvalidInput)?;
        let place = |ready: &Path| place_in_folder(ready, Path::new(name), folder);
        match self {
            Transfer::Move => move_with(source, folder, place),
            Transfer::Copy => place(&StagedCopy::new(source, folder)?.path),
        }
    }
}

/// Move `source` to `target` without overwriting an existing file (used by undo/redo).
pub fn move_exact(source: &Path, target: &Path) -> std::io::Result<()> {
    check_source(source)?;
    let folder = target.parent().ok_or(ErrorKind::InvalidInput)?;
    move_with(source, folder, |ready| rename_exclusive(ready, target))
}

fn check_source(source: &Path) -> io::Result<()> {
    if fs::symlink_metadata(source)?.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::InvalidInput,
            "source is not a regular file",
        ))
    }
}

fn move_with<T>(
    source: &Path,
    folder: &Path,
    place: impl Fn(&Path) -> io::Result<T>,
) -> io::Result<T> {
    match place(source) {
        Err(error) if error.kind() == ErrorKind::CrossesDevices => {
            move_across(source, folder, place)
        }
        result => result,
    }
}

fn move_across<T>(
    source: &Path,
    folder: &Path,
    place: impl Fn(&Path) -> io::Result<T>,
) -> io::Result<T> {
    let copy = StagedCopy::new(source, folder)?;
    File::open(&copy.path)?.sync_all()?;
    let transferred = place(&copy.path)?;
    fs::remove_file(source)?;
    Ok(transferred)
}

/// Move `ready` into `folder` as `name`, appending a number on collision.
pub fn place_as(ready: &Path, folder: &Path, name: &str) -> io::Result<Transferred> {
    let named = Path::new(name);
    if named.file_name() != Some(named.as_os_str()) {
        return Err(io::Error::new(ErrorKind::InvalidInput, "not a file name"));
    }
    move_with(ready, folder, |ready| place_in_folder(ready, named, folder))
}

/// Move `ready` into `folder` using `source`'s filename, numbering from 2 on collision.
fn place_in_folder(ready: &Path, source: &Path, folder: &Path) -> io::Result<Transferred> {
    let name = source.file_name().ok_or(ErrorKind::InvalidInput)?;
    let target = folder.join(name);
    match rename_exclusive(ready, &target) {
        Ok(()) => {
            return Ok(Transferred {
                target,
                renamed: false,
            })
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = source
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for copy in 2..1000 {
        let target = folder.join(format!("{stem} {copy}{extension}"));
        match rename_exclusive(ready, &target) {
            Ok(()) => {
                return Ok(Transferred {
                    target,
                    renamed: true,
                })
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "no free name in the target folder",
    ))
}

/// Stages a copy in a hidden directory on the target volume so watchers only
/// see the finished file once moved into place.
struct StagedCopy {
    directory: PathBuf,
    path: PathBuf,
}

impl StagedCopy {
    fn new(source: &Path, folder: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        let directory = loop {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let directory = folder.join(format!(".blickli-copy-{}-{seq}", std::process::id()));
            match builder.create(&directory) {
                Ok(()) => break directory,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let copy = Self {
            path: directory.join("image"),
            directory,
        };
        copy_preserving_metadata(source, &copy.path)?;
        Ok(copy)
    }
}

impl Drop for StagedCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn same_directory(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn copy_preserving_metadata(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        if let (Ok(src), Ok(dst)) = (
            CString::new(source.as_os_str().as_bytes()),
            CString::new(target.as_os_str().as_bytes()),
        ) {
            // Preserve APFS clones, timestamps, permissions, and xattrs/Finder tags when supported.
            let rc = unsafe {
                libc::copyfile(
                    src.as_ptr(),
                    dst.as_ptr(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_CLONE | libc::COPYFILE_METADATA | libc::COPYFILE_DATA,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            let _ = fs::remove_file(target);
        }
    }
    let mtime = fs::metadata(source).ok().and_then(|m| m.modified().ok());
    fs::copy(source, target)?;
    if let Some(mtime) = mtime {
        if let Ok(file) = File::options().write(true).open(target) {
            let _ = file.set_modified(mtime);
        }
    }
    Ok(())
}

fn rename_exclusive_fallback(source: &Path, target: &Path) -> io::Result<()> {
    match fs::hard_link(source, target) {
        Ok(()) => fs::remove_file(source),
        #[cfg(unix)]
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EPERM | libc::EINVAL)
            ) =>
        {
            // Non-POSIX volumes (e.g. exFAT/FAT32/SMB) lack both RENAME_EXCL and hard links.
            match fs::symlink_metadata(target) {
                Ok(_) => Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    "destination file already exists",
                )),
                Err(meta_err) if meta_err.kind() == ErrorKind::NotFound => {
                    fs::rename(source, target)
                }
                Err(meta_err) => Err(meta_err),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn rename_exclusive(source: &Path, target: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_c = CString::new(source.as_os_str().as_bytes())?;
    let target_c = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: Both arguments point to live, NUL-terminated paths.
    let result =
        unsafe { libc::renamex_np(source_c.as_ptr(), target_c.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EINVAL)
        ) {
            rename_exclusive_fallback(source, target)
        } else {
            Err(error)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn rename_exclusive(source: &Path, target: &Path) -> io::Result<()> {
    rename_exclusive_fallback(source, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blickli-transfer-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn move_takes_the_file_across() {
        let from = scratch("move-from");
        let to = scratch("move-to");
        let source = from.join("a.heic");
        write(&source, "one");

        let done = Transfer::Move.apply(&source, &to).unwrap();

        assert_eq!(done.target, to.join("a.heic"));
        assert!(!done.renamed);
        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(done.target).unwrap(), "one");
    }

    #[test]
    fn copy_leaves_the_original() {
        let from = scratch("copy-from");
        let to = scratch("copy-to");
        let source = from.join("a.heic");
        write(&source, "one");

        let done = Transfer::Copy.apply(&source, &to).unwrap();

        assert!(source.exists());
        assert_eq!(std::fs::read_to_string(done.target).unwrap(), "one");
        assert_eq!(fs::read_dir(&to).unwrap().count(), 1);
    }

    #[test]
    fn copying_uses_a_new_name_when_the_destination_is_a_symlink() {
        let from = scratch("copy-link-from");
        let to = scratch("copy-link-to");
        let source = from.join("a.heic");
        let other = from.join("other.heic");
        let link = to.join("a.heic");
        write(&source, "one");
        write(&other, "other");
        std::os::unix::fs::symlink(&other, &link).unwrap();

        let done = Transfer::Copy.apply(&source, &to).unwrap();

        assert_eq!(done.target, to.join("a 2.heic"));
        assert_eq!(fs::read_to_string(done.target).unwrap(), "one");
        assert_eq!(fs::read_link(link).unwrap(), other);
        assert_eq!(fs::read_to_string(other).unwrap(), "other");
    }

    #[test]
    fn a_taken_name_gets_the_next_number() {
        let from = scratch("clash-from");
        let to = scratch("clash-to");
        write(&to.join("a.heic"), "theirs");
        let source = from.join("a.heic");
        write(&source, "mine");
        let done = Transfer::Move.apply(&source, &to).unwrap();
        assert!(done.renamed);
        assert_eq!(done.target, to.join("a 2.heic"));
        assert_eq!(fs::read_to_string(to.join("a.heic")).unwrap(), "theirs");
        assert_eq!(fs::read_to_string(to.join("a 2.heic")).unwrap(), "mine");

        let source = from.join("a.heic");
        write(&source, "again");
        let done = Transfer::Copy.apply(&source, &to).unwrap();
        assert_eq!(done.target, to.join("a 3.heic"));

        write(&to.join("bare"), "theirs");
        write(&from.join("bare"), "mine");
        let done = Transfer::Copy.apply(&from.join("bare"), &to).unwrap();
        assert_eq!(done.target, to.join("bare 2"));
    }

    #[test]
    fn an_edit_lands_under_a_free_name() {
        let work = scratch("place-work");
        let to = scratch("place-to");
        write(&to.join("a cropped.jpg"), "theirs");
        let ready = work.join("edit.tmp");
        write(&ready, "mine");

        let done = place_as(&ready, &to, "a cropped.jpg").unwrap();

        assert_eq!(done.target, to.join("a cropped 2.jpg"));
        assert!(done.renamed && !ready.exists());
        assert_eq!(fs::read_to_string(to.join("a cropped.jpg")).unwrap(), "theirs");
        assert!(place_as(&to.join("a cropped 2.jpg"), &to, "../escape.jpg").is_err());
    }

    #[test]
    fn moving_a_file_into_its_own_folder_changes_nothing() {
        let here = scratch("same-folder");
        let source = here.join("a.heic");
        write(&source, "one");

        let done = Transfer::Move.apply(&source, &here).unwrap();

        assert_eq!(done.target, source);
        assert!(!done.renamed);
        assert_eq!(std::fs::read_dir(&here).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "one");
    }

    #[test]
    fn a_failed_transfer_leaves_nothing_behind() {
        let from = scratch("fail-from");
        let to = scratch("fail-to");
        let missing = from.join("gone.heic");
        assert!(Transfer::Copy.apply(&missing, &to).is_err());
        assert!(move_exact(&missing, &to.join("gone.heic")).is_err());
        assert_eq!(fs::read_dir(&to).unwrap().count(), 0);
    }

    #[test]
    fn an_exact_move_lands_on_the_name_it_was_given() {
        let from = scratch("exact-from");
        let to = scratch("exact-to");
        let source = from.join("a.heic");
        write(&source, "one");

        move_exact(&source, &to.join("a.heic")).unwrap();

        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(to.join("a.heic")).unwrap(), "one");
    }

    #[test]
    fn an_exact_move_never_replaces_a_file() {
        let from = scratch("exact-clash-from");
        let to = scratch("exact-clash-to");
        write(&to.join("a.heic"), "theirs");
        let source = from.join("a.heic");
        write(&source, "mine");

        let error = move_exact(&source, &to.join("a.heic")).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(to.join("a.heic")).unwrap(),
            "theirs"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "mine");
    }

    #[test]
    fn a_move_across_disks_keeps_the_copy_when_the_original_stays() {
        let from = scratch("across-from");
        let to = scratch("across-to");
        let source = from.join("a.heic");
        write(&source, "one");
        let target = to.join("a.heic");
        // Read-only source directory prevents unlinking the original after copying.
        let locked = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&from, locked).unwrap();

        let result = move_across(&source, &to, |ready| rename_exclusive(ready, &target));

        std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "one");
    }

    #[test]
    fn same_folder_symlink_alias_is_noop_and_copy_preserves_mtime() {
        let here = scratch("alias-mtime");
        let alias_parent = scratch("alias-link");
        let alias = alias_parent.join("link");
        std::os::unix::fs::symlink(&here, &alias).unwrap();

        let source = here.join("a.heic");
        write(&source, "one");
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let noop = Transfer::Move.apply(&source, &alias).unwrap();
        assert_eq!(noop.target, source);
        assert!(!noop.renamed);

        let dest = scratch("alias-dest");
        let copied = Transfer::Copy.apply(&source, &dest).unwrap();
        assert_eq!(
            fs::metadata(&copied.target).unwrap().modified().unwrap(),
            old
        );
    }
}
