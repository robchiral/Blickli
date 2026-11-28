//! Platform-independent engine for folder scanning, sorting, and background decoding.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub mod cache;
pub mod history;
mod listing;
pub mod messages;
mod order;
pub mod orientation;
pub mod playlist;
mod scan;
mod thumbnails;
pub mod transfer;

pub use cache::LruCache;
pub use order::{
    apply_inserts, file_info_cmp, merge_sorted_by, natural_str_cmp, sort_files, SortOrder,
};
pub use orientation::Orientation;
pub use playlist::Playlist;
pub use scan::{collect_images, in_folder_scan};
pub use thumbnails::ThumbnailRequest;

use scan::{file_info, is_image, walk, Scan};

/// Platform image decoder. Decodes cap the long edge at `max_px` and apply EXIF orientation.
pub trait ImageDecoder: Send + Sync + 'static {
    type Image: Send + 'static;
    fn decode(&self, path: &Path, max_px: i32) -> Option<Self::Image>;

    /// Thumbnail decode; override on platforms with an embedded-preview fast path.
    fn decode_thumb(&self, path: &Path, max_px: i32) -> Option<Self::Image> {
        self.decode(path, max_px)
    }

    /// Read date added (and EXIF capture date when `want_exif` is true) on demand.
    fn file_dates(&self, _path: &Path, _want_exif: bool) -> FileDates {
        FileDates::default()
    }

    /// Decode all frames and frame delays (seconds) of an animated image, or `None` for stills.
    fn decode_frames(&self, _path: &Path) -> Option<Vec<(Self::Image, f64)>> {
        None
    }
}

/// Engine events sent to the UI. File events carry a `scan` ID; ignore stale
/// events using `Engine::is_stale`.
pub enum Event<I> {
    /// Newly discovered files with their insertion indices in the name-sorted list.
    FilesInserted {
        scan: u64,
        files: Vec<(usize, FileInfo)>,
    },
    FilesRemoved {
        scan: u64,
        paths: Vec<PathBuf>,
    },
    /// File contents changed on disk; invalidate cached thumbnails and decodes.
    FileChanged {
        scan: u64,
        info: FileInfo,
    },
    FileDatesReady {
        scan: u64,
        files: Vec<FileInfo>,
    },
    ScanDone {
        scan: u64,
        total: usize,
    },
    Thumbnail {
        request: ThumbnailRequest,
        image: Option<I>,
    },
    SlideReady {
        path: PathBuf,
        image: I,
        generation: u64,
    },
    SlideFramesReady {
        path: PathBuf,
        frames: Vec<(I, f64)>,
        generation: u64,
    },
    DecodeFailed {
        path: PathBuf,
        generation: u64,
    },
}

#[derive(Clone, Debug)]
pub struct FileInfo {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub size: u64,
    pub dates: Option<FileDates>,
}

impl FileInfo {
    /// Whether `order` requires loading additional date metadata for this file.
    pub fn missing_dates_for(&self, order: SortOrder) -> bool {
        match (order, self.dates) {
            (SortOrder::ExifDate, Some(d)) => !d.exif_loaded,
            (SortOrder::Added, Some(d)) => !d.added_loaded,
            (SortOrder::ExifDate | SortOrder::Added, None) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileDates {
    pub exif_date: Option<SystemTime>,
    pub added: Option<SystemTime>,
    pub exif_loaded: bool,
    pub added_loaded: bool,
}

impl FileDates {
    /// Merge newly loaded fields onto previously loaded fields.
    pub fn merge(self, newer: FileDates) -> FileDates {
        FileDates {
            exif_date: if newer.exif_loaded {
                newer.exif_date
            } else {
                self.exif_date
            },
            added: if newer.added_loaded {
                newer.added
            } else {
                self.added
            },
            exif_loaded: self.exif_loaded || newer.exif_loaded,
            added_loaded: self.added_loaded || newer.added_loaded,
        }
    }
}

/// Minimum interval between date-metadata batches sent to the UI.
const DATES_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Worker thread count for thumbnails, date reads, and folder walks.
fn worker_count() -> usize {
    thread::available_parallelism()
        .map_or(2, |n| n.get() / 2)
        .clamp(2, 4)
}

struct SlideRequest {
    path: PathBuf,
    /// `None` uses the default slide size (and decodes animations); `Some` decodes a larger still for zoom.
    max_px: Option<i32>,
    /// High-priority requests (active slide and zoom re-decodes) run ahead of prefetches.
    priority: bool,
    generation: u64,
}

struct DateRequest {
    scan: u64,
    paths: Vec<PathBuf>,
    want_exif: bool,
}

/// Coordinates folder scanning, thumbnail workers, and a dedicated slide decode thread.
pub struct Engine<I> {
    thumbnails: Arc<thumbnails::ThumbnailQueue>,
    slide_tx: Sender<SlideRequest>,
    slide_gen: Arc<AtomicU64>,
    events_tx: Sender<Event<I>>,
    wakeup: Arc<dyn Fn() + Send + Sync>,
    /// Current folder contents in natural name order, shared by scanner and watcher.
    files: Arc<Mutex<Vec<FileInfo>>>,
    /// Active scan ID; incremented while holding `files`.
    scan: Arc<AtomicU64>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    dates_tx: Sender<DateRequest>,
}

impl<I: Send + 'static> Engine<I> {
    pub fn start<D: ImageDecoder<Image = I>>(
        decoder: D,
        max_slide_px: i32,
        wakeup: impl Fn() + Send + Sync + 'static,
    ) -> (Self, Receiver<Event<I>>) {
        let decoder = Arc::new(decoder);
        let wakeup: Arc<dyn Fn() + Send + Sync> = Arc::new(wakeup);
        let (events_tx, events_rx) = channel::<Event<I>>();
        let scan = Arc::new(AtomicU64::new(0));
        let slide_gen = Arc::new(AtomicU64::new(0));
        let (dates_tx, dates_rx) = channel();
        spawn_date_worker(
            dates_rx,
            Arc::clone(&decoder),
            Arc::clone(&scan),
            events_tx.clone(),
            Arc::clone(&wakeup),
        );

        let thumbnails = Arc::new(thumbnails::ThumbnailQueue::default());
        for _ in 0..worker_count() {
            spawn_thumbnail_worker(
                Arc::clone(&thumbnails),
                Arc::clone(&decoder),
                events_tx.clone(),
                Arc::clone(&wakeup),
            );
        }

        let (slide_tx, slide_rx) = channel::<SlideRequest>();
        spawn_slide_worker(
            slide_rx,
            Arc::clone(&slide_gen),
            decoder,
            max_slide_px,
            events_tx.clone(),
            Arc::clone(&wakeup),
        );

        (
            Engine {
                thumbnails,
                slide_tx,
                slide_gen,
                events_tx,
                wakeup,
                files: Arc::new(Mutex::new(Vec::new())),
                scan,
                watcher: Mutex::new(None),
                dates_tx,
            },
            events_rx,
        )
    }

    /// Notify the engine immediately of files modified by the app so the UI updates
    /// before the filesystem watcher fires. Returns the subset of `paths` that
    /// emitted `Event::FileChanged` (in-place updates of existing entries).
    pub fn notice(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        let scan = self.current();
        let mut changed = Vec::new();
        for path in paths {
            if scan.apply_change(path, true) {
                changed.push(path.clone());
            }
        }
        changed
    }

    /// Returns `true` if `event` belongs to a superseded scan or invalidated slide generation.
    pub fn is_stale(&self, event: &Event<I>) -> bool {
        match event {
            Event::FilesInserted { scan, .. }
            | Event::FilesRemoved { scan, .. }
            | Event::FileChanged { scan, .. }
            | Event::FileDatesReady { scan, .. }
            | Event::ScanDone { scan, .. } => *scan != self.scan.load(Ordering::SeqCst),
            Event::SlideReady { generation, .. }
            | Event::SlideFramesReady { generation, .. }
            | Event::DecodeFailed { generation, .. } => {
                *generation != self.slide_gen.load(Ordering::Acquire)
            }
            Event::Thumbnail { .. } => false,
        }
    }

    /// Invalidate any in-flight or queued slide decodes (called after file edits/changes).
    pub fn invalidate_slides(&self) {
        self.slide_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// Scan `root` (recursively or single-level) and watch it for changes.
    pub fn scan(&self, root: PathBuf, recursive: bool, is_package: fn(&Path) -> bool) {
        let scan = self.begin();
        self.start_watcher(&root, recursive, scan.clone(), is_package);
        thread::spawn(move || walk(&scan, root, recursive, is_package));
    }

    /// Display explicit file paths without scanning or watching their parent directory
    /// (used in the App Sandbox when Finder grants access only to individual files).
    pub fn show(&self, paths: Vec<PathBuf>) {
        let scan = self.begin();
        thread::spawn(move || {
            let mut pending: Vec<FileInfo> = paths
                .iter()
                .filter(|path| is_image(path))
                .filter_map(|path| file_info(path))
                .collect();
            if scan.insert(&mut pending) {
                scan.finish();
            }
        });
    }

    /// Clear state, advance the scan ID to cancel prior walks, and drop the old watcher.
    fn begin(&self) -> Scan<I> {
        {
            let mut files = self.files.lock().unwrap();
            files.clear();
            self.scan.fetch_add(1, Ordering::SeqCst);
        }
        *self.watcher.lock().unwrap() = None;
        self.current()
    }

    fn current(&self) -> Scan<I> {
        Scan {
            number: self.scan.load(Ordering::SeqCst),
            current: Arc::clone(&self.scan),
            files: Arc::clone(&self.files),
            events_tx: self.events_tx.clone(),
            wakeup: Arc::clone(&self.wakeup),
        }
    }

    fn start_watcher(
        &self,
        root: &Path,
        recursive: bool,
        scan: Scan<I>,
        is_package: fn(&Path) -> bool,
    ) {
        use notify::Watcher;
        let Ok(watched_root) = root.canonicalize() else {
            return;
        };
        let selected_root = root.to_path_buf();
        // A dropped watcher may still invoke this callback; `scan` ignores stale IDs.
        let handler = move |result: Result<notify::Event, notify::Error>| {
            let Ok(event) = result else { return };
            for path in event.paths {
                let Ok(relative) = path.strip_prefix(&watched_root) else {
                    continue;
                };
                if relative.as_os_str().is_empty() {
                    continue;
                }
                let path = selected_root.join(relative);
                if is_image(&path) && in_folder_scan(&selected_root, &path, recursive, is_package) {
                    scan.apply_change(&path, false);
                } else if recursive && !is_image(&path) {
                    let visible_dir = relative
                        .components()
                        .all(|part| !scan::hidden(part.as_os_str()))
                        && path
                            .ancestors()
                            .take_while(|p| *p != selected_root)
                            .all(|p| !is_package(p));
                    if visible_dir && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                        scan.insert_subtree(path, is_package);
                    } else {
                        scan.remove_prefix(&path);
                    }
                }
            }
        };
        let Ok(mut watcher) = notify::recommended_watcher(handler) else {
            return;
        };
        let mode = if recursive {
            notify::RecursiveMode::Recursive
        } else {
            notify::RecursiveMode::NonRecursive
        };
        if watcher.watch(root, mode).is_ok() {
            *self.watcher.lock().unwrap() = Some(watcher);
        }
    }

    pub fn request_thumbnails(&self, requests: Vec<ThumbnailRequest>) {
        self.thumbnails.replace(requests);
    }

    pub fn request_file_dates(&self, paths: Vec<PathBuf>, want_exif: bool) {
        if !paths.is_empty() {
            let scan = self.scan.load(Ordering::SeqCst);
            let _ = self.dates_tx.send(DateRequest {
                scan,
                paths,
                want_exif,
            });
        }
    }

    pub fn request_slide(&self, path: PathBuf) {
        let _ = self.slide_tx.send(SlideRequest {
            path,
            max_px: None,
            priority: true,
            generation: self.slide_gen.load(Ordering::Acquire),
        });
    }

    pub fn prefetch_slide(&self, path: PathBuf) {
        let _ = self.slide_tx.send(SlideRequest {
            path,
            max_px: None,
            priority: false,
            generation: self.slide_gen.load(Ordering::Acquire),
        });
    }

    /// Request a higher-resolution decode capped at `max_px` for a zoomed slide.
    pub fn request_slide_at(&self, path: PathBuf, max_px: i32) {
        let _ = self.slide_tx.send(SlideRequest {
            path,
            max_px: Some(max_px),
            priority: true,
            generation: self.slide_gen.load(Ordering::Acquire),
        });
    }
}

impl<I> Drop for Engine<I> {
    fn drop(&mut self) {
        self.thumbnails.close();
    }
}

/// Read file dates across a worker pool and stream batches periodically.
fn spawn_date_worker<I: Send + 'static, D: ImageDecoder<Image = I>>(
    requests: Receiver<DateRequest>,
    decoder: Arc<D>,
    current: Arc<AtomicU64>,
    events: Sender<Event<I>>,
    wakeup: Arc<dyn Fn() + Send + Sync>,
) {
    thread::spawn(move || {
        while let Ok(DateRequest {
            scan,
            paths,
            want_exif,
        }) = requests.recv()
        {
            let is_current = || current.load(Ordering::SeqCst) == scan;
            if !is_current() {
                continue;
            }
            let send = |files: Vec<FileInfo>| {
                if !files.is_empty()
                    && is_current()
                    && events.send(Event::FileDatesReady { scan, files }).is_ok()
                {
                    wakeup();
                }
            };
            if paths.len() <= 2 {
                let mut files = Vec::with_capacity(paths.len());
                for path in &paths {
                    if !is_current() {
                        break;
                    }
                    if let Some(mut info) = file_info(path) {
                        info.dates = Some(decoder.file_dates(path, want_exif));
                        files.push(info);
                    }
                }
                send(files);
                continue;
            }
            let next = AtomicUsize::new(0);
            let threads = worker_count().min(paths.len());
            thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        let mut files = Vec::new();
                        let mut last_flush = Instant::now();
                        while is_current() {
                            let Some(path) = paths.get(next.fetch_add(1, Ordering::Relaxed)) else {
                                break;
                            };
                            if let Some(mut info) = file_info(path) {
                                info.dates = Some(decoder.file_dates(path, want_exif));
                                files.push(info);
                            }
                            if last_flush.elapsed() >= DATES_FLUSH_INTERVAL {
                                send(std::mem::take(&mut files));
                                last_flush = Instant::now();
                            }
                        }
                        send(files);
                    });
                }
            });
        }
    });
}

fn spawn_thumbnail_worker<I: Send + 'static, D: ImageDecoder<Image = I>>(
    queue: Arc<thumbnails::ThumbnailQueue>,
    decoder: Arc<D>,
    events: Sender<Event<I>>,
    wakeup: Arc<dyn Fn() + Send + Sync>,
) {
    thread::spawn(move || {
        while let Some(request) = queue.next() {
            let image = decoder.decode_thumb(&request.path, request.max_px);
            if queue.wants(request.id) {
                if events.send(Event::Thumbnail { request, image }).is_err() {
                    return;
                }
                wakeup();
            }
        }
    });
}

fn spawn_slide_worker<I: Send + 'static, D: ImageDecoder<Image = I>>(
    jobs: Receiver<SlideRequest>,
    slide_gen: Arc<AtomicU64>,
    decoder: Arc<D>,
    max_slide_px: i32,
    events_tx: Sender<Event<I>>,
    wakeup: Arc<dyn Fn() + Send + Sync>,
) {
    thread::spawn(move || {
        let mut backlog = std::collections::VecDeque::<SlideRequest>::new();
        loop {
            if backlog.is_empty() {
                let Ok(first) = jobs.recv() else { return };
                backlog.push_back(first);
            }
            // Coalesce burst slide requests (e.g. rapid scrubbing): keep the newest
            // request for any duplicate path, prioritize active/zoom decodes ahead of
            // prefetches, and cap prefetch backlog to the latest 3.
            while let Ok(req) = jobs.try_recv() {
                backlog.retain(|queued| queued.path != req.path);
                if req.priority {
                    // Place newest active/zoom request ahead of queued prefetches.
                    let insert_at = backlog.iter().take_while(|q| q.priority).count();
                    backlog.insert(insert_at, req);
                } else {
                    backlog.push_back(req);
                }
            }
            let current_gen = slide_gen.load(Ordering::Acquire);
            backlog.retain(|req| req.generation == current_gen);
            while backlog.len() > 3 {
                if let Some(pos) = backlog.iter().rposition(|q| !q.priority) {
                    backlog.remove(pos);
                } else {
                    backlog.pop_front();
                }
            }
            let Some(job) = backlog.pop_front() else {
                continue;
            };
            if job.generation != slide_gen.load(Ordering::Acquire) {
                continue;
            }
            let generation = job.generation;
            let event = match job {
                SlideRequest {
                    path,
                    max_px: Some(max_px),
                    ..
                } => match decoder.decode(&path, max_px) {
                    Some(image) => Event::SlideReady {
                        path,
                        image,
                        generation,
                    },
                    None => Event::DecodeFailed { path, generation },
                },
                SlideRequest {
                    path, max_px: None, ..
                } => match decoder.decode_frames(&path) {
                    Some(frames) => Event::SlideFramesReady {
                        path,
                        frames,
                        generation,
                    },
                    None => match decoder.decode(&path, max_slide_px) {
                        Some(image) => Event::SlideReady {
                            path,
                            image,
                            generation,
                        },
                        None => Event::DecodeFailed { path, generation },
                    },
                },
            };
            if generation != slide_gen.load(Ordering::Acquire) {
                continue;
            }
            if events_tx.send(event).is_err() {
                return;
            }
            wakeup();
        }
    });
}

/// Return the target long edge for re-decoding a zoomed slide, or `None` if the
/// current decode is sufficient. Caps at `natural_long` and requires >20% gain to
/// avoid queuing a decode on every pinch frame.
pub fn zoom_decode_request(
    natural_long: i32,
    decoded_long: i32,
    zoom: f64,
    backing: f64,
) -> Option<i32> {
    if natural_long <= 0 || decoded_long <= 0 || !zoom.is_finite() || zoom <= 0.0 {
        return None;
    }
    if decoded_long >= natural_long {
        return None;
    }
    let raw_wanted = (natural_long as f64 * zoom * backing.max(1.0)).ceil();
    if raw_wanted <= decoded_long as f64 * 1.2 {
        return None;
    }
    Some((raw_wanted as i32).min(natural_long))
}

/// Parse an EXIF timestamp (`YYYY:MM:DD HH:MM:SS`) as UTC for consistent sorting.
pub fn parse_exif_datetime(s: &str) -> Option<SystemTime> {
    let mut it = s
        .split([':', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<i64>());
    let mut next = || it.next()?.ok();
    let (y, mo, d) = (next()?, next()?, next()?);
    let (h, mi, se) = (next()?, next()?, next()?);
    if !(1900..=9999).contains(&y)
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&mi)
        || !(0..=60).contains(&se)
    {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y_adj = y - i64::from(mo <= 2);
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + se;
    if secs >= 0 {
        std::time::UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        std::time::UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::file_cmp;
    use std::time::UNIX_EPOCH;

    const WAIT: Duration = Duration::from_secs(10);

    struct NoDecoder;

    impl ImageDecoder for NoDecoder {
        type Image = ();
        fn decode(&self, _path: &Path, _max_px: i32) -> Option<()> {
            None
        }
    }

    /// A fresh, empty folder holding `files` (relative paths).
    fn folder(tag: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("blickli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        root
    }

    /// Apply events until the current scan is done, as the grid does.
    fn listed(engine: &Engine<()>, events: &Receiver<Event<()>>) -> Vec<PathBuf> {
        let mut listed: Vec<FileInfo> = Vec::new();
        loop {
            let event = events.recv_timeout(WAIT).expect("the scan finishes");
            if engine.is_stale(&event) {
                continue;
            }
            match event {
                Event::FilesInserted { files, .. } => apply_inserts(&mut listed, files, |f| f),
                Event::ScanDone { total, .. } => {
                    assert_eq!(total, listed.len());
                    return listed.into_iter().map(|f| f.path).collect();
                }
                _ => {}
            }
        }
    }

    #[test]
    fn exif_datetimes_parse() {
        assert_eq!(parse_exif_datetime("1970:01:01 00:00:00"), Some(UNIX_EPOCH));
        let pre = parse_exif_datetime("1969:12:31 23:59:00").unwrap();
        assert_eq!(UNIX_EPOCH.duration_since(pre).unwrap().as_secs(), 60);
        let t = parse_exif_datetime("2023:07:14 10:30:05").unwrap();
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_secs(), 1689330605);
        assert_eq!(parse_exif_datetime("2023: 7:14  10:30:05"), Some(t));
        assert!(parse_exif_datetime("").is_none());
        assert!(parse_exif_datetime("2023:13:01 00:00:00").is_none());
        assert!(parse_exif_datetime("2023:01:01 -1:00:00").is_none());
        assert!(parse_exif_datetime("2023:01:01 25:00:00").is_none());
        assert!(parse_exif_datetime("2023:01:01 00:60:00").is_none());
        assert!(parse_exif_datetime("1899:12:31 23:59:59").is_none());
        assert!(parse_exif_datetime("not a date").is_none());
    }

    #[test]
    fn a_zoom_asks_for_pixels_only_when_they_pay() {
        // Fit on Retina: 2560px decode is enough.
        assert_eq!(zoom_decode_request(6000, 2560, 0.2, 2.0), None);
        // Require >20% gain before re-decoding.
        assert_eq!(zoom_decode_request(6000, 2560, 0.234, 2.0), None);
        assert_eq!(zoom_decode_request(6000, 2560, 0.3, 2.0), Some(3600));
        // Native resolution within 20% of current decode is still reachable when zoomed in.
        assert_eq!(zoom_decode_request(3600, 3024, 1.0, 2.0), Some(3600));
        // Cap at natural size.
        assert_eq!(zoom_decode_request(4000, 1000, 8.0, 2.0), Some(4000));
        assert_eq!(zoom_decode_request(4000, 4000, 8.0, 2.0), None);
        // Invalid inputs.
        assert_eq!(zoom_decode_request(0, 100, 2.0, 2.0), None);
        assert_eq!(zoom_decode_request(100, 50, f64::NAN, 2.0), None);
        assert_eq!(zoom_decode_request(100, 50, -1.0, 2.0), None);
    }

    #[test]
    fn scans_respect_depth_and_skip_hidden_files_packages_and_links() {
        let package = |path: &Path| path.extension().is_some_and(|ext| ext == "bundle");
        let wanted = ["a.JPG", "level 1/a.JPG", "level 1/level 2/level 3/a.JPG"];
        let skipped = [
            ".hidden/skip.jpg",
            "level 1/library.bundle/skip.jpg",
            "level 1/.hidden.jpg",
            "level 1/notes.txt",
        ];
        let root = folder("depth", &[&wanted[..], &skipped[..]].concat());
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        std::os::unix::fs::symlink(root.join("a.JPG"), root.join("alias.jpg")).unwrap();

        let (engine, events) = Engine::start(NoDecoder, 100, || {});
        for (recursive, expected) in [(true, &wanted[..]), (false, &wanted[..1])] {
            engine.scan(root.clone(), recursive, package);
            let expected: Vec<PathBuf> = expected.iter().map(|name| root.join(name)).collect();
            assert_eq!(listed(&engine, &events), expected);
        }
        for (index, relative) in wanted.iter().chain(&skipped[..3]).enumerate() {
            let path = root.join(relative);
            assert_eq!(in_folder_scan(&root, &path, true, package), index < 3);
            assert_eq!(in_folder_scan(&root, &path, false, package), index == 0);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn walkers_share_a_wide_and_deep_tree() {
        // Forty folders side by side and a chain twelve deep: every
        // walker gets work, and the last one done releases the rest.
        let mut files: Vec<String> = (0..40)
            .flat_map(|n| (0..5).map(move |m| format!("wide {n}/w{n}-{m}.jpg")))
            .collect();
        let mut deep = String::new();
        for level in 0..12 {
            deep.push_str(&format!("level {level}/"));
            files.push(format!("{deep}d{level}.jpg"));
        }
        let names: Vec<&str> = files.iter().map(String::as_str).collect();
        let root = folder("tree", &names);
        let mut expected: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
        expected.sort_by(|a, b| file_cmp(a, b));

        let (engine, events) = Engine::start(NoDecoder, 100, || {});
        for _ in 0..3 {
            engine.scan(root.clone(), true, |_| false);
            assert_eq!(listed(&engine, &events), expected);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_new_scan_drops_the_old_folder() {
        let names: Vec<String> = (0..300).map(|n| format!("big/{n}.jpg")).collect();
        let mut files: Vec<&str> = names.iter().map(String::as_str).collect();
        files.extend(["small/a.jpg", "small/b.jpg", "small/c.jpg"]);
        let root = folder("rescan", &files);
        let (engine, events) = Engine::start(NoDecoder, 100, || {});

        engine.scan(root.join("big"), false, |_| false);
        events.recv_timeout(WAIT).unwrap();
        engine.scan(root.join("small"), false, |_| false);
        let expected: Vec<PathBuf> = files[300..].iter().map(|f| root.join(f)).collect();
        assert_eq!(listed(&engine, &events), expected);
        // Long enough for the old walk to have finished, had it run on.
        thread::sleep(Duration::from_millis(300));
        let kept: Vec<PathBuf> = engine
            .files
            .lock()
            .unwrap()
            .iter()
            .map(|f| f.path.clone())
            .collect();
        assert_eq!(kept, expected);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn show_lists_only_the_given_images() {
        let root = folder("show", &["a.jpg", "b.jpg", "c.jpg", "notes.txt"]);
        let (engine, events) = Engine::start(NoDecoder, 100, || {});
        engine.show(
            ["c.jpg", "a.jpg", "notes.txt"]
                .map(|f| root.join(f))
                .to_vec(),
        );
        assert_eq!(
            listed(&engine, &events),
            [root.join("a.jpg"), root.join("c.jpg")]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Blocks in `file_dates` until told to go on.
    struct SlowDates {
        started: Sender<()>,
        resume: Mutex<Receiver<()>>,
    }

    impl ImageDecoder for SlowDates {
        type Image = ();
        fn decode(&self, _path: &Path, _max_px: i32) -> Option<()> {
            None
        }
        fn file_dates(&self, _path: &Path, want_exif: bool) -> FileDates {
            self.started.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            FileDates {
                exif_date: want_exif.then_some(UNIX_EPOCH),
                added: None,
                exif_loaded: want_exif,
                added_loaded: true,
            }
        }
    }

    #[test]
    fn dates_load_on_request_without_locking_the_list() {
        let root = folder("dates", &["a.jpg"]);
        let (started, reading) = channel();
        let (resume, waiting) = channel();
        let decoder = SlowDates {
            started,
            resume: Mutex::new(waiting),
        };
        let (engine, events) = Engine::start(decoder, 100, || {});
        engine.scan(root.clone(), true, |_| false);
        assert_eq!(listed(&engine, &events).len(), 1);
        assert!(reading.try_recv().is_err(), "no dates until asked");

        engine.request_file_dates(vec![root.join("a.jpg")], true);
        reading.recv_timeout(WAIT).unwrap();
        let unlocked = engine.files.try_lock().is_ok();
        resume.send(()).unwrap();
        assert!(unlocked);
        loop {
            if let Event::FileDatesReady { files, .. } = events.recv_timeout(WAIT).unwrap() {
                assert_eq!(files[0].dates.unwrap().exif_date, Some(UNIX_EPOCH));
                break;
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
