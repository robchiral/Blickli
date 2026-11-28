//! Parallel directory traversal into the engine's name-sorted file list.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::order::{file_cmp, merge_sorted_by, sort_files};
use crate::{listing, worker_count, Event, FileInfo};

/// Initial batch size to populate the top of the grid quickly.
const FIRST_SCAN_BATCH: usize = 8;
/// Time-based batching avoids O(n^2) UI list rebuilds on large folders.
const SCAN_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const WALKER_CHUNK: usize = 128;
const WALKER_CHUNK_TIME: Duration = Duration::from_millis(50);

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "tif", "tiff", "bmp", "avif", "jxl",
];

pub(crate) fn is_image(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        IMAGE_EXTENSIONS.iter().any(|supported| {
            ext.as_encoded_bytes()
                .eq_ignore_ascii_case(supported.as_bytes())
        })
    })
}

pub(crate) fn hidden(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b".")
}

/// Whether a file belongs in a scan of `root`.
pub fn in_folder_scan(
    root: &Path,
    path: &Path,
    recursive: bool,
    is_package: fn(&Path) -> bool,
) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let Some(parent) = relative.parent() else {
        return false;
    };
    (recursive || parent.as_os_str().is_empty())
        && relative.components().all(|part| !hidden(part.as_os_str()))
        && parent
            .ancestors()
            .take_while(|p| !p.as_os_str().is_empty())
            .all(|p| !is_package(&root.join(p)))
}

pub(crate) fn file_info(path: &Path) -> Option<FileInfo> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(FileInfo {
        path: path.to_path_buf(),
        modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
        size: meta.len(),
        dates: None,
    })
}

/// Shared handle for a single scan generation, used by walkers, the watcher,
/// and `Engine::notice`. Writes verify `is_current()` under the `files` lock.
pub(crate) struct Scan<I> {
    pub number: u64,
    pub current: Arc<AtomicU64>,
    pub files: Arc<Mutex<Vec<FileInfo>>>,
    pub events_tx: Sender<Event<I>>,
    pub wakeup: Arc<dyn Fn() + Send + Sync>,
}

impl<I> Clone for Scan<I> {
    fn clone(&self) -> Self {
        Scan {
            number: self.number,
            current: Arc::clone(&self.current),
            files: Arc::clone(&self.files),
            events_tx: self.events_tx.clone(),
            wakeup: Arc::clone(&self.wakeup),
        }
    }
}

impl<I> Scan<I> {
    pub fn is_current(&self) -> bool {
        self.current.load(Ordering::SeqCst) == self.number
    }

    /// Merge `pending` into the sorted list and emit `FilesInserted`.
    /// Returns `false` if superseded by a newer scan.
    pub fn insert(&self, pending: &mut Vec<FileInfo>) -> bool {
        let mut batch = std::mem::take(pending);
        sort_files(&mut batch, |a, b| file_cmp(&a.path, &b.path));
        let sent = {
            let mut sorted = self.files.lock().unwrap();
            if !self.is_current() {
                return false;
            }
            let landed = merge_sorted_by(&mut sorted, batch, |a, b| file_cmp(&a.path, &b.path));
            let inserts: Vec<(usize, FileInfo)> = landed
                .into_iter()
                .map(|index| (index, sorted[index].clone()))
                .collect();
            // Send while holding the lock so events arrive in insertion order.
            !inserts.is_empty()
                && self
                    .events_tx
                    .send(Event::FilesInserted {
                        scan: self.number,
                        files: inserts,
                    })
                    .is_ok()
        };
        if sent {
            (self.wakeup)();
        }
        true
    }

    pub fn finish(&self) {
        let sent = {
            let sorted = self.files.lock().unwrap();
            if !self.is_current() {
                return;
            }
            let total = sorted.len();
            self.events_tx
                .send(Event::ScanDone {
                    scan: self.number,
                    total,
                })
                .is_ok()
        };
        if sent {
            (self.wakeup)();
        }
    }

    /// Reconcile a path that changed on disk with the list.
    /// When `force` is `false` (watcher callback), skip emitting `FileChanged`
    /// if `(modified, size)` is unchanged so app-edit watcher echoes do not
    /// trigger duplicate thumbnail/slide decodes.
    /// Returns `true` if `Event::FileChanged` was emitted and sent.
    pub fn apply_change(&self, path: &Path, force: bool) -> bool {
        if !self.is_current() || !is_image(path) || path.file_name().is_some_and(hidden) {
            return false;
        }
        let (sent, changed) = {
            let mut sorted = self.files.lock().unwrap();
            if !self.is_current() {
                return false;
            }
            // Stat under the files lock so watcher events cannot race `Engine::notice`
            // during swap-in and delete or clobber a freshly written replacement.
            let info = file_info(path);
            let scan = self.number;
            let existing = sorted.binary_search_by(|p| file_cmp(&p.path, path));
            let event = match (info, existing) {
                (Some(info), Err(index)) => {
                    sorted.insert(index, info.clone());
                    Some(Event::FilesInserted {
                        scan,
                        files: vec![(index, info)],
                    })
                }
                (Some(info), Ok(index)) => {
                    let prev = &sorted[index];
                    if !force && prev.modified == info.modified && prev.size == info.size {
                        None
                    } else {
                        sorted[index] = info.clone();
                        Some(Event::FileChanged { scan, info })
                    }
                }
                (None, Ok(index)) => {
                    sorted.remove(index);
                    Some(Event::FilesRemoved {
                        scan,
                        paths: vec![path.to_path_buf()],
                    })
                }
                (None, Err(_)) => None,
            };
            let is_changed = matches!(event, Some(Event::FileChanged { .. }));
            let sent = event.is_some_and(|event| self.events_tx.send(event).is_ok());
            (sent, sent && is_changed)
        };
        if sent {
            (self.wakeup)();
        }
        changed
    }

    /// Remove all entries under `dir` (for deleted, hidden, or renamed subdirectories).
    pub fn remove_prefix(&self, dir: &Path) {
        if !self.is_current() {
            return;
        }
        let sent = {
            let mut sorted = self.files.lock().unwrap();
            if !self.is_current() {
                return;
            }
            let mut removed = Vec::new();
            sorted.retain(|info| {
                if info.path.starts_with(dir) {
                    removed.push(info.path.clone());
                    false
                } else {
                    true
                }
            });
            !removed.is_empty()
                && self
                    .events_tx
                    .send(Event::FilesRemoved {
                        scan: self.number,
                        paths: removed,
                    })
                    .is_ok()
        };
        if sent {
            (self.wakeup)();
        }
    }

    /// Walk a newly created or moved-in subdirectory and merge any images into this scan.
    pub fn insert_subtree(&self, dir: PathBuf, is_package: fn(&Path) -> bool) {
        if !self.is_current() {
            return;
        }
        let mut stack = vec![dir];
        let mut dir_buf = listing::DirBuffer::new();
        let mut pending = Vec::new();
        while let Some(folder) = stack.pop() {
            if !self.is_current() {
                return;
            }
            let _ = listing::list_dir_with(&folder, &mut dir_buf, |entry| {
                if !self.is_current() {
                    return false;
                }
                if hidden(entry.name) {
                    return true;
                }
                let path = folder.join(entry.name);
                match entry.kind {
                    listing::Kind::Other => {}
                    listing::Kind::Dir => {
                        if !is_package(&path) {
                            stack.push(path);
                        }
                    }
                    listing::Kind::File if is_image(&path) => {
                        let info = match entry.stat {
                            Some((modified, size)) => Some(FileInfo {
                                path,
                                modified,
                                size,
                                dates: None,
                            }),
                            None => file_info(&path),
                        };
                        pending.extend(info);
                    }
                    listing::Kind::File => {}
                }
                true
            });
            if pending.len() >= WALKER_CHUNK && !self.insert(&mut pending) {
                return;
            }
        }
        if !pending.is_empty() {
            let _ = self.insert(&mut pending);
        }
    }
}

/// Walk `root` with parallel directory walkers while this thread batches and merges results.
pub(crate) fn walk<I: Send>(
    scan: &Scan<I>,
    root: PathBuf,
    recursive: bool,
    is_package: fn(&Path) -> bool,
) {
    let folders = Folders::new(root);
    let (found_tx, found) = channel::<Vec<FileInfo>>();
    let walkers = if recursive { worker_count() } else { 1 };
    thread::scope(|scope| {
        for _ in 0..walkers {
            let found_tx = found_tx.clone();
            let folders = &folders;
            scope.spawn(move || walk_folders(scan, folders, recursive, is_package, &found_tx));
        }
        drop(found_tx);

        let mut pending: Vec<FileInfo> = Vec::new();
        let mut first = true;
        let mut last_flush = Instant::now();
        loop {
            // Wake on schedule so pending files flush even while walkers traverse empty subtrees.
            let wait = if pending.is_empty() {
                SCAN_FLUSH_INTERVAL
            } else {
                SCAN_FLUSH_INTERVAL.saturating_sub(last_flush.elapsed())
            };
            match found.recv_timeout(wait) {
                Ok(files) => pending.extend(files),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let due = !pending.is_empty()
                && ((first && pending.len() >= FIRST_SCAN_BATCH)
                    || last_flush.elapsed() >= SCAN_FLUSH_INTERVAL);
            if !scan.is_current() || (due && !scan.insert(&mut pending)) {
                folders.stop();
                return;
            }
            if due {
                first = false;
                last_flush = Instant::now();
            }
        }
        if scan.insert(&mut pending) {
            scan.finish();
        }
    });
}

fn walk_folders<I: Send>(
    scan: &Scan<I>,
    folders: &Folders,
    recursive: bool,
    is_package: fn(&Path) -> bool,
    found: &Sender<Vec<FileInfo>>,
) {
    let mut dir_buf = listing::DirBuffer::new();
    while let Some(dir) = folders.next() {
        let mut subfolders = Vec::new();
        let mut chunk = Vec::new();
        let mut last_send = Instant::now();
        let _ = listing::list_dir_with(&dir, &mut dir_buf, |entry| {
            if !scan.is_current() {
                return false;
            }
            if hidden(entry.name) {
                return true;
            }
            let path = dir.join(entry.name);
            match entry.kind {
                // Skip symlinks to avoid cycles.
                listing::Kind::Other => {}
                listing::Kind::Dir => {
                    if recursive && !is_package(&path) {
                        subfolders.push(path);
                    }
                }
                listing::Kind::File if is_image(&path) => {
                    let info = match entry.stat {
                        Some((modified, size)) => Some(FileInfo {
                            path,
                            modified,
                            size,
                            dates: None,
                        }),
                        None => file_info(&path),
                    };
                    chunk.extend(info);
                    if chunk.len() >= WALKER_CHUNK || last_send.elapsed() >= WALKER_CHUNK_TIME {
                        let _ = found.send(std::mem::take(&mut chunk));
                        last_send = Instant::now();
                    }
                }
                listing::Kind::File => {}
            }
            true
        });
        if !chunk.is_empty() {
            let _ = found.send(chunk);
        }
        folders.done(subfolders);
        if !scan.is_current() {
            folders.stop();
            return;
        }
    }
}

/// Collect image `(path, byte_size)` pairs under `root` using fast bulk directory reads,
/// skipping hidden entries, symlinks, and macOS bundle packages.
pub fn collect_images(
    root: &Path,
    recursive: bool,
    is_package: fn(&Path) -> bool,
) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut dir_buf = listing::DirBuffer::new();
    while let Some(dir) = stack.pop() {
        let mut subdirs = Vec::new();
        let _ = listing::list_dir_with(&dir, &mut dir_buf, |entry| {
            if hidden(entry.name) {
                return true;
            }
            let path = dir.join(entry.name);
            match entry.kind {
                listing::Kind::Other => {}
                listing::Kind::Dir => {
                    if recursive && !is_package(&path) {
                        subdirs.push(path);
                    }
                }
                listing::Kind::File if is_image(&path) => {
                    let size = match entry.stat {
                        Some((_, size)) => Some(size),
                        None => std::fs::symlink_metadata(&path)
                            .ok()
                            .filter(|m| m.is_file())
                            .map(|m| m.len()),
                    };
                    if let Some(size) = size {
                        out.push((path, size));
                    }
                }
                listing::Kind::File => {}
            }
            true
        });
        stack.extend(subdirs);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Shared work queue of directories remaining in a scan.
struct Folders {
    state: Mutex<FoldersState>,
    changed: Condvar,
}

struct FoldersState {
    waiting: Vec<PathBuf>,
    /// Number of walkers currently reading directories (may enqueue more).
    reading: usize,
    stopped: bool,
}

impl Folders {
    fn new(root: PathBuf) -> Self {
        Folders {
            state: Mutex::new(FoldersState {
                waiting: vec![root],
                reading: 0,
                stopped: false,
            }),
            changed: Condvar::new(),
        }
    }

    /// Pop the next directory, blocking while other walkers are active.
    fn next(&self) -> Option<PathBuf> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.stopped {
                return None;
            }
            if let Some(dir) = state.waiting.pop() {
                state.reading += 1;
                return Some(dir);
            }
            if state.reading == 0 {
                return None;
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    fn done(&self, subfolders: Vec<PathBuf>) {
        let mut state = self.state.lock().unwrap();
        state.reading -= 1;
        state.waiting.extend(subfolders);
        self.changed.notify_all();
    }

    fn stop(&self) {
        self.state.lock().unwrap().stopped = true;
        self.changed.notify_all();
    }
}
