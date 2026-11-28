//! Directory listing with optional inline stat metadata.
//! Uses `getattrlistbulk` on macOS and falls back to `std::fs::read_dir`.

use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Dir,
    File,
    /// Symlinks, devices, and sockets (ignored during scans).
    Other,
}

pub(crate) struct Listed<'a> {
    pub name: &'a OsStr,
    pub kind: Kind,
    /// `(modified, size)` when returned inline by `getattrlistbulk`.
    pub stat: Option<(SystemTime, u64)>,
}

/// Reusable scratch buffer for bulk directory reads (avoids 128 KB allocation per folder).
pub(crate) struct DirBuffer {
    #[cfg(target_os = "macos")]
    words: Vec<u64>,
}

impl DirBuffer {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            words: vec![0u64; bulk::BUFFER / 8],
        }
    }
}

/// Invoke `each` for every entry in `dir` until it returns `false`.
pub(crate) fn list_dir_with(
    dir: &Path,
    #[allow(unused_variables)] buf: &mut DirBuffer,
    mut each: impl FnMut(Listed) -> bool,
) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    match bulk::list(dir, &mut buf.words, &mut each) {
        Err(error) if bulk::unsupported(&error) => {}
        other => return other,
    }
    portable(dir, &mut each)
}

#[cfg(test)]
pub(crate) fn list_dir(dir: &Path, each: impl FnMut(Listed) -> bool) -> std::io::Result<()> {
    list_dir_with(dir, &mut DirBuffer::new(), each)
}

fn portable(dir: &Path, each: &mut impl FnMut(Listed) -> bool) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let kind = if kind.is_dir() {
            Kind::Dir
        } else if kind.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        let name = entry.file_name();
        if !each(Listed {
            name: &name,
            kind,
            stat: None,
        }) {
            break;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod bulk {
    use std::ffi::OsStr;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::ptr::read_unaligned;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{Kind, Listed};

    // Constants missing from the `libc` crate (<sys/attr.h>, <sys/vnode.h>).
    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    const VREG: u32 = 1;
    const VDIR: u32 = 2;

    pub(super) const BUFFER: usize = 128 * 1024;

    /// Errors indicating `getattrlistbulk` is unsupported on this filesystem.
    pub(super) fn unsupported(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS)
        )
    }

    pub(super) fn list(
        dir: &Path,
        buffer: &mut [u64],
        each: &mut impl FnMut(Listed) -> bool,
    ) -> io::Result<()> {
        let folder = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(dir)?;
        let mut request = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS
                | libc::ATTR_CMN_NAME
                | ATTR_CMN_ERROR
                | libc::ATTR_CMN_OBJTYPE
                | libc::ATTR_CMN_MODTIME,
            volattr: 0,
            dirattr: 0,
            fileattr: libc::ATTR_FILE_DATALENGTH,
            forkattr: 0,
        };
        let byte_len = std::mem::size_of_val(buffer);
        loop {
            // SAFETY: the request is a valid attrlist, and the buffer
            // is writable for the length given.
            let count = unsafe {
                libc::getattrlistbulk(
                    folder.as_raw_fd(),
                    (&raw mut request).cast(),
                    buffer.as_mut_ptr().cast(),
                    byte_len,
                    0,
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                return Ok(());
            }
            // SAFETY: the kernel wrote `count` entries into the buffer.
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), byte_len) };
            let mut start = 0;
            for _ in 0..count {
                let Some(entry) = Entry::parse(bytes, start) else {
                    return Ok(());
                };
                start += entry.length;
                if let Some(listed) = entry.listed() {
                    if !each(listed) {
                        return Ok(());
                    }
                }
            }
        }
    }

    struct Entry<'a> {
        length: usize,
        error: u32,
        name: Option<&'a OsStr>,
        kind: Option<u32>,
        modified: Option<SystemTime>,
        size: Option<u64>,
    }

    impl<'a> Entry<'a> {
        /// Parse one packed entry starting at `start` (bounds-checked).
        fn parse(bytes: &'a [u8], start: usize) -> Option<Self> {
            let length = read::<u32>(bytes, start)? as usize;
            let end = start.checked_add(length)?;
            if length < 4 || end > bytes.len() {
                return None;
            }
            let entry = &bytes[..end];
            let mut at = start + 4;
            let returned = read::<libc::attribute_set_t>(entry, at)?;
            at += size_of::<libc::attribute_set_t>();
            let common = returned.commonattr;
            let mut parsed = Entry {
                length,
                error: 0,
                name: None,
                kind: None,
                modified: None,
                size: None,
            };
            if common & ATTR_CMN_ERROR != 0 {
                parsed.error = read::<u32>(entry, at)?;
                at += 4;
            }
            if common & libc::ATTR_CMN_NAME != 0 {
                let reference = read::<libc::attrreference_t>(entry, at)?;
                let from = at.checked_add_signed(reference.attr_dataoffset as isize)?;
                let to = from.checked_add(reference.attr_length as usize)?;
                let name = entry.get(from..to)?;
                // attr_length includes the trailing NUL byte.
                let name = name.strip_suffix(&[0]).unwrap_or(name);
                parsed.name = Some(OsStr::from_bytes(name));
                at += size_of::<libc::attrreference_t>();
            }
            if common & libc::ATTR_CMN_OBJTYPE != 0 {
                parsed.kind = Some(read::<u32>(entry, at)?);
                at += 4;
            }
            if common & libc::ATTR_CMN_MODTIME != 0 {
                let seconds = read::<i64>(entry, at)?;
                let nanos = read::<i64>(entry, at + 8)?;
                parsed.modified = Some(system_time(seconds, nanos));
                at += 16;
            }
            if returned.fileattr & libc::ATTR_FILE_DATALENGTH != 0 {
                parsed.size = Some(read::<i64>(entry, at)?.max(0) as u64);
            }
            Some(parsed)
        }

        /// Skip entries that returned a per-entry kernel error.
        fn listed(&self) -> Option<Listed<'a>> {
            if self.error != 0 {
                return None;
            }
            let kind = match self.kind? {
                VDIR => Kind::Dir,
                VREG => Kind::File,
                _ => Kind::Other,
            };
            let stat = match (kind, self.modified, self.size) {
                (Kind::File, Some(modified), Some(size)) => Some((modified, size)),
                _ => None,
            };
            Some(Listed {
                name: self.name?,
                kind,
                stat,
            })
        }
    }

    fn read<T: Copy>(bytes: &[u8], at: usize) -> Option<T> {
        let end = at.checked_add(size_of::<T>())?;
        let field = bytes.get(at..end)?;
        // SAFETY: the slice holds size_of::<T>() bytes, and T is a
        // plain integer struct, valid for any bit pattern.
        Some(unsafe { read_unaligned(field.as_ptr().cast::<T>()) })
    }

    /// Match `Metadata::modified()` construction so timestamps compare equal.
    fn system_time(seconds: i64, nanos: i64) -> SystemTime {
        let nanos = Duration::from_nanos(nanos.clamp(0, 999_999_999) as u64);
        let time = if seconds >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
        };
        time.and_then(|time| time.checked_add(nanos))
            .unwrap_or(UNIX_EPOCH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    type Seen = BTreeMap<OsString, (Kind, Option<(SystemTime, u64)>)>;

    fn listed(dir: &Path) -> Seen {
        let mut seen = BTreeMap::new();
        list_dir(dir, |entry| {
            seen.insert(entry.name.to_owned(), (entry.kind, entry.stat));
            true
        })
        .unwrap();
        seen
    }

    /// Reference listing via `read_dir` + `symlink_metadata`. Must match the
    /// bulk path exactly because date updates compare `(modified, size)`.
    fn expected(dir: &Path) -> Seen {
        let mut seen = BTreeMap::new();
        portable(dir, &mut |entry| {
            let stat = (entry.kind == Kind::File).then(|| {
                let meta = std::fs::symlink_metadata(dir.join(entry.name)).unwrap();
                (meta.modified().unwrap(), meta.len())
            });
            seen.insert(entry.name.to_owned(), (entry.kind, stat));
            true
        })
        .unwrap();
        seen
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("blickli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn the_listing_matches_read_dir_and_stat() {
        let root = scratch("listing");
        std::fs::create_dir(root.join("sub folder")).unwrap();
        std::fs::write(root.join("a.jpg"), b"12345").unwrap();
        std::fs::write(root.join("Ünïcødé ✓.heic"), vec![0; 70_000]).unwrap();
        std::os::unix::fs::symlink(root.join("a.jpg"), root.join("link.jpg")).unwrap();
        std::os::unix::fs::symlink(root.join("sub folder"), root.join("dir link")).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::new(1_000_000_000, 123_456_789);
        let file = std::fs::File::options()
            .write(true)
            .open(root.join("a.jpg"));
        file.unwrap().set_modified(old).unwrap();

        let seen = listed(&root);
        assert_eq!(seen, expected(&root));
        assert_eq!(seen[OsStr::new("a.jpg")], (Kind::File, Some((old, 5))));
        assert_eq!(seen[OsStr::new("sub folder")].0, Kind::Dir);
        assert_eq!(seen[OsStr::new("link.jpg")].0, Kind::Other);
        assert_eq!(seen[OsStr::new("dir link")].0, Kind::Other);
        // /usr/bin includes APFS-compressed files whose logical size must match stat.
        for dir in ["/usr/bin", "/System/Library/Desktop Pictures"] {
            let dir = Path::new(dir);
            if dir.is_dir() {
                assert_eq!(listed(dir), expected(dir), "{}", dir.display());
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_big_folder_takes_several_calls_and_can_stop_early() {
        // 4000 entries of about 80 bytes need three 128 KB buffers.
        let root = scratch("listing-big");
        for n in 0..4000 {
            std::fs::write(root.join(format!("IMG_{n:05}.jpg")), n.to_string()).unwrap();
        }
        let seen = listed(&root);
        assert_eq!(seen.len(), 4000);
        assert_eq!(seen, expected(&root));

        let mut calls = 0;
        list_dir(&root, |_| {
            calls += 1;
            calls < 3
        })
        .unwrap();
        assert_eq!(calls, 3);
        assert!(list_dir(&root.join("missing"), |_| true).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
