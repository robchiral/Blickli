//! Single-image and recursive batch compression under a target file size cap.
//!
//! Prioritizes spatial downscaling at high quality before dropping quality,
//! preserves metadata and RGB ICC profiles, skips animations, and replaces files
//! via Trash for undo.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use blickli_core::history::Edit;
use blickli_core::Orientation;
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFMutableData, CFNumber, CFRetained, CFString, CFType, CGPoint,
    CGRect, CGSize, CFURL,
};
use objc2_core_graphics::{
    CGBitmapContextCreateImage, CGBitmapContextGetData, CGColorSpace, CGColorSpaceModel, CGContext,
    CGImage, CGImageAlphaInfo,
};
use objc2_image_io::{
    kCGImagePropertyIsIndexed, kCGImagePropertyOrientation, CGImageDestination, CGImageSource,
};

use crate::edit;

const SUPPORTED_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff",
];

/// Scale ladder (~15% area drop per step).
const SCALE_STEPS: [f64; 9] = [1.0, 0.92, 0.85, 0.78, 0.71, 0.65, 0.59, 0.54, 0.50];

const JPEG_TARGET_Q: f64 = 0.85;
const JPEG_FLOOR_Q: f64 = 0.78;
const HEIC_TARGET_Q: f64 = 0.85;
const HEIC_FLOOR_Q: f64 = 0.75;
const AVIF_TARGET_Q: f64 = 0.85;
const AVIF_FLOOR_Q: f64 = 0.75;
const WEBP_TARGET_Q: f32 = 85.0;
const WEBP_FLOOR_Q: f32 = 75.0;

const PHOTO_COLOR_THRESHOLD: usize = 4096;
const PHOTO_SAMPLE_TARGET: usize = 16_384;
const PNG_PALETTE_COLORS: [usize; 3] = [256, 192, 128];

/// Base-10 KB size cap presets (matching Finder).
pub const TARGET_KB_PRESETS: [(&str, isize); 6] = [
    ("0.5 MB", 500),
    ("1 MB", 1000),
    ("2 MB", 2000),
    ("3 MB", 3000),
    ("5 MB", 5000),
    ("10 MB", 10000),
];

pub const DEFAULT_TARGET_KB: isize = 2000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MaxDimensions {
    Original,
    #[default]
    Uhd4k,
    Qhd,
    FullHd,
}

impl MaxDimensions {
    pub const fn tag(self) -> isize {
        match self {
            MaxDimensions::Original => 0,
            MaxDimensions::Uhd4k => 1,
            MaxDimensions::Qhd => 2,
            MaxDimensions::FullHd => 3,
        }
    }

    pub const fn from_tag(tag: isize) -> Self {
        match tag {
            0 => MaxDimensions::Original,
            2 => MaxDimensions::Qhd,
            3 => MaxDimensions::FullHd,
            _ => MaxDimensions::Uhd4k,
        }
    }

    pub const fn bounds(self) -> Option<(usize, usize)> {
        match self {
            MaxDimensions::Original => None,
            MaxDimensions::Uhd4k => Some((3840, 2160)),
            MaxDimensions::Qhd => Some((2560, 1440)),
            MaxDimensions::FullHd => Some((1920, 1080)),
        }
    }
}

pub const MAX_DIMS_PRESETS: [(&str, isize); 4] = [
    ("4K (3840 × 2160)", MaxDimensions::Uhd4k.tag()),
    ("QHD (2560 × 1440)", MaxDimensions::Qhd.tag()),
    ("Full HD (1920 × 1080)", MaxDimensions::FullHd.tag()),
    ("Original size", MaxDimensions::Original.tag()),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FallbackFormat {
    #[default]
    Keep,
    Heic,
    Webp,
}

impl FallbackFormat {
    pub const fn tag(self) -> isize {
        match self {
            FallbackFormat::Keep => 0,
            FallbackFormat::Heic => 1,
            FallbackFormat::Webp => 2,
        }
    }

    pub const fn from_tag(tag: isize) -> Self {
        match tag {
            1 => FallbackFormat::Heic,
            2 => FallbackFormat::Webp,
            _ => FallbackFormat::Keep,
        }
    }
}

pub const FALLBACK_PRESETS: [(&str, isize); 3] = [
    ("Keep original format", FallbackFormat::Keep.tag()),
    ("Convert to HEIC if needed", FallbackFormat::Heic.tag()),
    ("Convert to WebP if needed", FallbackFormat::Webp.tag()),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompressOptions {
    /// Target size cap in base-10 KB (`2000` = `2 MB`).
    pub target_kb: isize,
    pub max_dims: MaxDimensions,
    pub fallback: FallbackFormat,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            target_kb: DEFAULT_TARGET_KB,
            max_dims: MaxDimensions::Uhd4k,
            fallback: FallbackFormat::Keep,
        }
    }
}

impl CompressOptions {
    pub fn max_bytes(self) -> u64 {
        (self.target_kb.max(100) as u64) * 1000
    }
}

pub fn normalize_target_kb(kb: isize) -> isize {
    if TARGET_KB_PRESETS.iter().any(|&(_, preset)| preset == kb) {
        kb
    } else {
        DEFAULT_TARGET_KB
    }
}

pub(crate) fn is_supported_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|ext| {
        SUPPORTED_EXTS
            .iter()
            .any(|supported| ext.eq_ignore_ascii_case(supported))
    })
}

/// Inspect explicit image paths, returning `(path, byte_size)` for supported files.
pub fn inspect_paths(paths: &[PathBuf]) -> Vec<(PathBuf, u64)> {
    paths
        .iter()
        .filter(|path| is_supported_path(path))
        .filter_map(|path| {
            let meta = std::fs::symlink_metadata(path).ok()?;
            meta.is_file().then(|| (path.clone(), meta.len()))
        })
        .collect()
}

/// Collect supported images in `root` (and subfolders when `recursive` is true)
/// using `blickli-core`'s bulk directory reader, skipping hidden entries, symlinks,
/// and macOS bundle packages.
pub fn collect_folder_candidates(
    root: &Path,
    recursive: bool,
    is_package: fn(&Path) -> bool,
) -> Vec<(PathBuf, u64)> {
    blickli_core::collect_images(root, recursive, is_package)
        .into_iter()
        .filter(|(path, _)| is_supported_path(path))
        .collect()
}

enum FileOutcome {
    Compressed {
        orig_path: PathBuf,
        edit: Edit,
        orig_bytes: u64,
        final_bytes: u64,
        over_limit: bool,
    },
    Skipped,
    Failed(String),
}

/// Summary of a parallel batch compression run.
#[derive(Debug, Default)]
pub struct BatchResult {
    pub edits: Vec<Edit>,
    pub orig_paths: Vec<PathBuf>,
    pub orig_bytes: u64,
    pub final_bytes: u64,
    pub over_limit: usize,
    pub failed: Vec<String>,
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct BatchProgress {
    pub done: usize,
    pub total: usize,
    pub saved_bytes: u64,
}

/// Compress `paths` in parallel across a bounded worker pool, committing each
/// improvement via Trash-backed replacement for undo.
pub fn compress_batch(
    paths: &[PathBuf],
    options: CompressOptions,
    cancel: &AtomicBool,
    on_progress: impl Fn(BatchProgress) + Send + Sync,
) -> BatchResult {
    let total = paths.len();
    if total == 0 {
        return BatchResult::default();
    }

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let saved_bytes = AtomicU64::new(0);
    let commit_lock = Mutex::new(());
    let results = Mutex::new(BatchResult::default());
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 6)
        .min(total);

    on_progress(BatchProgress {
        done: 0,
        total,
        saved_bytes: 0,
    });

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let next = &next;
            let done = &done;
            let saved_bytes = &saved_bytes;
            let commit_lock = &commit_lock;
            let results = &results;
            let on_progress = &on_progress;
            scope.spawn(move || loop {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let idx = next.fetch_add(1, Ordering::AcqRel);
                if idx >= paths.len() {
                    return;
                }
                let path = &paths[idx];
                let outcome = objc2::rc::autoreleasepool(|_| {
                    compress_one_locked(path, options, cancel, commit_lock)
                });
                if cancel.load(Ordering::Acquire) && matches!(outcome, FileOutcome::Skipped) {
                    return;
                }
                {
                    let mut guard = results.lock().unwrap();
                    match outcome {
                        FileOutcome::Compressed {
                            orig_path,
                            edit,
                            orig_bytes,
                            final_bytes,
                            over_limit,
                        } => {
                            let delta = orig_bytes.saturating_sub(final_bytes);
                            saved_bytes.fetch_add(delta, Ordering::AcqRel);
                            guard.orig_paths.push(orig_path);
                            guard.edits.push(edit);
                            guard.orig_bytes += orig_bytes;
                            guard.final_bytes += final_bytes;
                            if over_limit {
                                guard.over_limit += 1;
                            }
                        }
                        FileOutcome::Skipped => {}
                        FileOutcome::Failed(why) => {
                            guard.failed.push(why);
                        }
                    }
                }
                let finished = done.fetch_add(1, Ordering::AcqRel) + 1;
                on_progress(BatchProgress {
                    done: finished,
                    total,
                    saved_bytes: saved_bytes.load(Ordering::Acquire),
                });
            });
        }
    });

    let mut summary = results.into_inner().unwrap();
    summary.cancelled = cancel.load(Ordering::Acquire);
    summary
}

fn compress_one_locked(
    path: &Path,
    options: CompressOptions,
    cancel: &AtomicBool,
    commit_lock: &Mutex<()>,
) -> FileOutcome {
    let orig_size = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => meta.len(),
        _ => return FileOutcome::Skipped,
    };
    let limit = options.max_bytes();
    if orig_size <= limit {
        return FileOutcome::Skipped;
    }

    let prepared = match prepare_compressed(path, orig_size, options, cancel) {
        Ok(Some(encoded)) => encoded,
        Ok(None) => return FileOutcome::Skipped,
        Err(why) => return FileOutcome::Failed(why),
    };
    if cancel.load(Ordering::Acquire) {
        return FileOutcome::Skipped;
    }

    let orig_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let target_path = if prepared.ext.eq_ignore_ascii_case(&orig_ext) {
        path.to_path_buf()
    } else {
        path.with_extension(prepared.ext)
    };
    let final_bytes = prepared.bytes.len() as u64;
    let commit = {
        let _guard = commit_lock.lock().unwrap();
        edit::commit_bytes(path, &target_path, &prepared.bytes)
    };
    match commit {
        Ok(edit) => FileOutcome::Compressed {
            orig_path: path.to_path_buf(),
            edit,
            orig_bytes: orig_size,
            final_bytes,
            over_limit: prepared.over_limit,
        },
        Err(why) => FileOutcome::Failed(why),
    }
}

struct EncodedCandidate {
    bytes: Vec<u8>,
    ext: &'static str,
    over_limit: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Jpeg,
    Heic,
    Avif,
    Webp,
    Png,
    Uncompressed,
}

struct SourceMeta {
    uti: String,
    properties: CFRetained<CFDictionary<CFString, CFType>>,
    is_indexed: bool,
}

fn search_lossy_format(
    capped: &CFRetained<CGImage>,
    properties: &CFDictionary<CFString, CFType>,
    uti: &str,
    ext: &'static str,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<EncodedCandidate, String> {
    let (progressive, target_q, floor_q) = match uti {
        "public.jpeg" => (true, JPEG_TARGET_Q, JPEG_FLOOR_Q),
        "public.heic" => (false, HEIC_TARGET_Q, HEIC_FLOOR_Q),
        "public.avif" => (false, AVIF_TARGET_Q, AVIF_FLOOR_Q),
        _ => (false, JPEG_TARGET_Q, JPEG_FLOOR_Q),
    };
    let mut res = search_lossy_cg(
        capped,
        properties,
        uti,
        progressive,
        target_q,
        floor_q,
        limit,
        cancel,
    )?;
    res.ext = ext;
    Ok(res)
}

fn classify_source(uti: &str, ext: &str) -> Option<SourceKind> {
    match uti {
        "public.jpeg" => Some(SourceKind::Jpeg),
        "public.heic" | "public.heif" => Some(SourceKind::Heic),
        "public.avif" => Some(SourceKind::Avif),
        "org.webmproject.webp" => Some(SourceKind::Webp),
        "public.png" => Some(SourceKind::Png),
        "com.microsoft.bmp" | "public.tiff" => Some(SourceKind::Uncompressed),
        _ => match ext {
            "jpg" | "jpeg" => Some(SourceKind::Jpeg),
            "heic" | "heif" => Some(SourceKind::Heic),
            "avif" => Some(SourceKind::Avif),
            "webp" => Some(SourceKind::Webp),
            "png" => Some(SourceKind::Png),
            "bmp" | "tif" | "tiff" => Some(SourceKind::Uncompressed),
            _ => None,
        },
    }
}

/// Decode, cap dimensions, and search for the highest-quality encoding under `options.max_bytes()`.
/// Returns `Ok(None)` when the file should be skipped (animated, unsupported, or no size improvement).
fn prepare_compressed(
    path: &Path,
    orig_size: u64,
    options: CompressOptions,
    cancel: &AtomicBool,
) -> Result<Option<EncodedCandidate>, String> {
    if cancel.load(Ordering::Acquire) {
        return Ok(None);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !SUPPORTED_EXTS.contains(&ext.as_str()) {
        return Ok(None);
    }

    let Some((capped, source)) = open_upright_capped(path, options.max_dims)? else {
        return Ok(None);
    };
    let Some(kind) = classify_source(&source.uti, &ext) else {
        return Ok(None);
    };
    let limit = options.max_bytes() as usize;

    let candidate = match kind {
        SourceKind::Jpeg => {
            let out_ext = if ext == "jpeg" { "jpeg" } else { "jpg" };
            search_lossy_format(&capped, &source.properties, "public.jpeg", out_ext, limit, cancel)?
        }
        SourceKind::Heic => {
            let out_ext = if ext == "heif" { "heif" } else { "heic" };
            search_lossy_format(&capped, &source.properties, "public.heic", out_ext, limit, cancel)?
        }
        SourceKind::Avif => {
            search_lossy_format(&capped, &source.properties, "public.avif", "avif", limit, cancel)?
        }
        SourceKind::Webp => search_lossy_webp(
            &capped,
            None,
            WEBP_TARGET_Q,
            WEBP_FLOOR_Q,
            limit,
            cancel,
        )?,
        SourceKind::Png => encode_png_target(&capped, &source, limit, options.fallback, cancel)?,
        SourceKind::Uncompressed => match options.fallback {
            FallbackFormat::Keep => {
                search_lossy_format(&capped, &source.properties, "public.jpeg", "jpg", limit, cancel)?
            }
            FallbackFormat::Heic => {
                search_lossy_format(&capped, &source.properties, "public.heic", "heic", limit, cancel)?
            }
            FallbackFormat::Webp => search_lossy_webp(
                &capped,
                None,
                WEBP_TARGET_Q,
                WEBP_FLOOR_Q,
                limit,
                cancel,
            )?,
        },
    };

    if cancel.load(Ordering::Acquire) || candidate.bytes.is_empty() {
        return Ok(None);
    }
    if candidate.bytes.len() as u64 >= orig_size {
        return Ok(None);
    }
    Ok(Some(candidate))
}

// --- Image I/O decode, dimension capping, and encoding ---

/// Decode `path`, orient upright, and apply `max_dims` in a single render pass.
fn open_upright_capped(
    path: &Path,
    max_dims: MaxDimensions,
) -> Result<Option<(CFRetained<CGImage>, SourceMeta)>, String> {
    let url = CFURL::from_file_path(path).ok_or("Cannot read file")?;
    unsafe {
        let Some(image_source) = CGImageSource::with_url(&url, None) else {
            return Err("Cannot read image".to_owned());
        };
        if image_source.count() > 1 {
            return Ok(None);
        }
        let uti = image_source
            .r#type()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let primary = image_source.primary_image_index();
        let Some(props_raw) = image_source.properties_at_index(primary, None) else {
            return Err("Cannot read image metadata".to_owned());
        };
        let properties: CFRetained<CFDictionary<CFString, CFType>> =
            CFRetained::cast_unchecked(props_raw);
        let orientation = properties
            .get(kCGImagePropertyOrientation)
            .and_then(|v| v.downcast::<CFNumber>().ok())
            .and_then(|n| n.as_i64())
            .map_or(Orientation::UP, Orientation::from_exif);
        let is_indexed = properties
            .get(kCGImagePropertyIsIndexed)
            .and_then(|v| v.downcast::<CFBoolean>().ok())
            .is_some_and(|b| b.value());
        let Some(raw_img) = image_source.image_at_index(primary, None) else {
            return Err("Cannot decode image".to_owned());
        };
        let (stored_w, stored_h) = (CGImage::width(Some(&raw_img)), CGImage::height(Some(&raw_img)));
        let (shown_w, shown_h) = if orientation.swaps_sides() {
            (stored_h, stored_w)
        } else {
            (stored_w, stored_h)
        };
        let (nw, nh) = capped_dimensions(shown_w, shown_h, max_dims);
        let needs_resize = nw != shown_w || nh != shown_h;
        let capped = if orientation == Orientation::UP && !needs_resize {
            raw_img
        } else {
            edit::draw_oriented_scaled(&raw_img, orientation, needs_resize.then_some((nw, nh)), None)?
        };

        Ok(Some((
            capped,
            SourceMeta {
                uti,
                properties,
                is_indexed,
            },
        )))
    }
}

/// Orientation-aware dimension cap: landscape caps to `(long, short)` and portrait to `(short, long)`.
fn capped_dimensions(width: usize, height: usize, max_dims: MaxDimensions) -> (usize, usize) {
    let Some((long_cap, short_cap)) = max_dims.bounds() else {
        return (width, height);
    };
    if width == 0 || height == 0 {
        return (width, height);
    }
    let (max_w, max_h) = if width >= height {
        (long_cap, short_cap)
    } else {
        (short_cap, long_cap)
    };
    let scale = (max_w as f64 / width as f64)
        .min(max_h as f64 / height as f64)
        .min(1.0);
    if scale >= 1.0 {
        (width, height)
    } else {
        (
            ((width as f64 * scale).round() as usize).max(1),
            ((height as f64 * scale).round() as usize).max(1),
        )
    }
}

fn rescale_cg(image: &CFRetained<CGImage>, scale: f64) -> Result<CFRetained<CGImage>, String> {
    if scale >= 1.0 {
        return Ok(image.clone());
    }
    let w = CGImage::width(Some(image));
    let h = CGImage::height(Some(image));
    let nw = ((w as f64 * scale).round() as usize).max(1);
    let nh = ((h as f64 * scale).round() as usize).max(1);
    edit::resize_cg(image, nw, nh)
}

fn flatten_on_white(image: &CGImage) -> Result<CFRetained<CGImage>, String> {
    let w = CGImage::width(Some(image));
    let h = CGImage::height(Some(image));
    let space = edit::output_rgb_space(image);
    let (context, _) = edit::make_bitmap_context(w, h, 0, space, CGImageAlphaInfo::NoneSkipLast)
        .ok_or("Not enough memory")?;
    let ctx = Some(&*context);
    CGContext::set_rgb_fill_color(ctx, 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(
        ctx,
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)),
    );
    CGContext::draw_image(
        ctx,
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)),
        Some(image),
    );
    CGBitmapContextCreateImage(ctx).ok_or_else(|| "Cannot flatten image".to_owned())
}

/// Encode `image` to memory via Image I/O (`public.jpeg`, `public.heic`, `public.avif`),
/// preserving EXIF/TIFF/IPTC/GPS metadata and ICC color space.
fn encode_cg_to_memory(
    image: &CGImage,
    source_props: &CFDictionary<CFString, CFType>,
    uti: &str,
    quality: f64,
    progressive_jpeg: bool,
) -> Result<Vec<u8>, String> {
    let flattened;
    let src_img = if uti == "public.jpeg" && edit::has_alpha_channel(image) {
        flattened = flatten_on_white(image)?;
        &*flattened
    } else {
        image
    };

    let data = CFMutableData::new(None, 0).ok_or("Not enough memory")?;
    unsafe {
        let Some(destination) =
            CGImageDestination::with_data(&data, &CFString::from_str(uti), 1, None)
        else {
            return Err(format!("Format {uti} cannot be written"));
        };
        let props =
            edit::cleaned_encode_properties(source_props, src_img, Some(quality), false, progressive_jpeg);
        destination.add_image(src_img, Some(props.as_opaque()));
        if !destination.finalize() {
            return Err(format!("Failed to encode {uti}"));
        }
    }
    Ok(data.to_vec())
}

// --- Binary-search lossy & lossless scale optimization ---

/// Binary-search `SCALE_STEPS` at `target_q`, falling back to `floor_q` only if the
/// smallest scale at `target_q` still exceeds `limit`. Memoizes resized bitmaps across
/// both passes and avoids copying `Vec<u8>` on fitting probes.
fn search_lossy_generic<T, R, E>(
    target_q: f64,
    floor_q: f64,
    limit: usize,
    cancel: &AtomicBool,
    mut make_scaled: R,
    mut encode: E,
) -> Result<(Vec<u8>, bool), String>
where
    R: FnMut(usize, f64) -> Result<T, String>,
    E: FnMut(&T, f64) -> Result<Vec<u8>, String>,
{
    let n = SCALE_STEPS.len();
    let mut scaled_cache: Vec<Option<T>> = (0..n).map(|_| None).collect();
    let mut smallest: Option<Vec<u8>> = None;

    let mut track_over = |data: Vec<u8>| {
        if smallest.as_ref().is_none_or(|best| data.len() < best.len()) {
            smallest = Some(data);
        }
    };

    let mut eval = |idx: usize, q: f64| -> Result<Vec<u8>, String> {
        if scaled_cache[idx].is_none() {
            scaled_cache[idx] = Some(make_scaled(idx, SCALE_STEPS[idx])?);
        }
        encode(scaled_cache[idx].as_ref().unwrap(), q)
    };

    let qualities: &[f64] = if (target_q - floor_q).abs() < 1e-6 {
        &[target_q]
    } else {
        &[target_q, floor_q]
    };

    for (pass, &q) in qualities.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Ok((Vec::new(), true));
        }
        let mut lo = if pass == 0 {
            let full = eval(0, q)?;
            if full.len() <= limit {
                return Ok((full, false));
            }
            track_over(full);
            if n == 1 {
                continue;
            }
            1usize
        } else {
            // Target quality already failed at 0.50; probe 0.50 first before searching [0..n-1].
            0usize
        };

        let mut best_fit = eval(n - 1, q)?;
        if best_fit.len() > limit {
            track_over(best_fit);
            continue;
        }
        let mut hi = n - 1;
        while lo < hi {
            if cancel.load(Ordering::Acquire) {
                return Ok((Vec::new(), true));
            }
            let mid = lo + (hi - lo) / 2;
            let data = eval(mid, q)?;
            if data.len() <= limit {
                best_fit = data;
                hi = mid;
            } else {
                track_over(data);
                lo = mid + 1;
            }
        }
        return Ok((best_fit, false));
    }

    Ok((smallest.unwrap_or_default(), true))
}

#[allow(clippy::too_many_arguments)]
fn search_lossy_cg(
    capped: &CFRetained<CGImage>,
    source_props: &CFDictionary<CFString, CFType>,
    uti: &str,
    progressive_jpeg: bool,
    target_q: f64,
    floor_q: f64,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<EncodedCandidate, String> {
    let (bytes, over_limit) = search_lossy_generic(
        target_q,
        floor_q,
        limit,
        cancel,
        |_, scale| rescale_cg(capped, scale),
        |img, q| encode_cg_to_memory(img, source_props, uti, q, progressive_jpeg),
    )?;
    Ok(EncodedCandidate {
        bytes,
        ext: "",
        over_limit,
    })
}

// --- Straight RGB/RGBA pixel extraction for PNG & WebP ---

enum PixelBuf {
    /// Tightly packed RGB bytes (`width * height * 3`) when all pixels are opaque.
    Rgb(Vec<u8>),
    /// Straight RGBA bytes (`width * height * 4`) when at least one pixel has alpha < 255.
    Rgba(Vec<u8>),
}

struct StraightPixels {
    width: usize,
    height: usize,
    data: PixelBuf,
    /// ICC profile of the actual RGB `CGBitmapContext` these pixels were extracted from.
    icc_profile: Option<Vec<u8>>,
}

impl StraightPixels {
    fn has_transparency(&self) -> bool {
        matches!(self.data, PixelBuf::Rgba(_))
    }
}

fn extract_straight_pixels(image: &CGImage) -> Result<StraightPixels, String> {
    let (context, actual_space, width, height, stride, has_alpha) =
        edit::render_rgb_bitmap(image)?;
    let ptr = CGBitmapContextGetData(Some(&context)) as *const u8;
    if ptr.is_null() {
        return Err("Cannot access pixels".to_owned());
    }
    let raw = unsafe { std::slice::from_raw_parts(ptr, stride * height) };
    let has_transparency = has_alpha
        && (0..height).any(|y| {
            raw[y * stride..y * stride + width * 4]
                .chunks_exact(4)
                .any(|px| px[3] < 255)
        });

    let data = if has_transparency {
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            let src_row = &raw[y * stride..y * stride + width * 4];
            let dst_row = &mut rgba[y * width * 4..(y + 1) * width * 4];
            for (src, dst) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                let a = src[3];
                if a == 255 {
                    dst.copy_from_slice(src);
                } else if a == 0 {
                    dst.copy_from_slice(&[0, 0, 0, 0]);
                } else {
                    dst[0] = edit::unpremultiply_channel(src[0], a);
                    dst[1] = edit::unpremultiply_channel(src[1], a);
                    dst[2] = edit::unpremultiply_channel(src[2], a);
                    dst[3] = a;
                }
            }
        }
        PixelBuf::Rgba(rgba)
    } else {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            let src_row = &raw[y * stride..y * stride + width * 4];
            for px in src_row.chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
        }
        PixelBuf::Rgb(rgb)
    };

    // Match the ICC profile to the exact RGB context that produced these pixels.
    let icc_profile = CGImage::color_space(Some(image))
        .filter(|space| CGColorSpace::model(Some(space)) == CGColorSpaceModel::RGB)
        .and(actual_space)
        .and_then(|space| CGColorSpace::icc_data(Some(&space)))
        .map(|data| data.to_vec())
        .filter(|bytes| !bytes.is_empty());

    Ok(StraightPixels {
        width,
        height,
        data,
        icc_profile,
    })
}

// --- WebP encoding (lossy + ICC chunk preservation) ---

fn encode_webp(pixels: &StraightPixels, quality: f32) -> Result<Vec<u8>, String> {
    let mut config =
        webp::WebPConfig::new().map_err(|_| "Cannot initialize WebP encoder".to_owned())?;
    config.lossless = 0;
    config.quality = quality;
    config.method = 6;
    config.alpha_compression = 1;

    let raw = match &pixels.data {
        PixelBuf::Rgba(rgba) => {
            let enc = webp::Encoder::from_rgba(rgba, pixels.width as u32, pixels.height as u32);
            enc.encode_advanced(&config)
                .map_err(|e| format!("WebP encode failed: {e:?}"))?
                .to_vec()
        }
        PixelBuf::Rgb(rgb) => {
            let enc = webp::Encoder::from_rgb(rgb, pixels.width as u32, pixels.height as u32);
            enc.encode_advanced(&config)
                .map_err(|e| format!("WebP encode failed: {e:?}"))?
                .to_vec()
        }
    };

    Ok(match pixels.icc_profile.as_deref() {
        Some(profile) if !profile.is_empty() => mux_webp_with_icc(
            &raw,
            profile,
            pixels.width as u32,
            pixels.height as u32,
            pixels.has_transparency(),
        ),
        _ => raw,
    })
}

/// Attach an `ICCP` chunk to a RIFF WebP container via `VP8X` so Display P3 and
/// custom RGB color profiles survive WebP re-encoding.
fn mux_webp_with_icc(
    raw: &[u8],
    icc: &[u8],
    width: u32,
    height: u32,
    has_alpha: bool,
) -> Vec<u8> {
    if raw.len() < 20 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WEBP" {
        return raw.to_vec();
    }
    let icc_pad = icc.len() & 1;
    let mut out = Vec::with_capacity(raw.len() + 32 + icc.len() + icc_pad);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"WEBP");

    if &raw[12..16] == b"VP8X" && raw.len() >= 30 {
        out.extend_from_slice(&raw[12..30]);
        out[20] |= 0x20;
        write_riff_chunk(&mut out, *b"ICCP", icc);
        out.extend_from_slice(&raw[30..]);
    } else {
        let mut vp8x = [0u8; 10];
        vp8x[0] = 0x20 | if has_alpha { 0x10 } else { 0 };
        let w = width.saturating_sub(1);
        let h = height.saturating_sub(1);
        vp8x[4] = (w & 0xff) as u8;
        vp8x[5] = ((w >> 8) & 0xff) as u8;
        vp8x[6] = ((w >> 16) & 0xff) as u8;
        vp8x[7] = (h & 0xff) as u8;
        vp8x[8] = ((h >> 8) & 0xff) as u8;
        vp8x[9] = ((h >> 16) & 0xff) as u8;
        write_riff_chunk(&mut out, *b"VP8X", &vp8x);
        write_riff_chunk(&mut out, *b"ICCP", icc);
        out.extend_from_slice(&raw[12..]);
    }

    let riff_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    out
}

fn write_riff_chunk(out: &mut Vec<u8>, tag: [u8; 4], payload: &[u8]) {
    out.extend_from_slice(&tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() & 1 == 1 {
        out.push(0);
    }
}

fn search_lossy_webp(
    capped: &CFRetained<CGImage>,
    mut pre_extracted_full: Option<StraightPixels>,
    target_q: f32,
    floor_q: f32,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<EncodedCandidate, String> {
    let (bytes, over_limit) = search_lossy_generic(
        f64::from(target_q),
        f64::from(floor_q),
        limit,
        cancel,
        |idx, scale| {
            if idx == 0 {
                if let Some(full) = pre_extracted_full.take() {
                    return Ok(full);
                }
            }
            let scaled = rescale_cg(capped, scale)?;
            extract_straight_pixels(&scaled)
        },
        |pixels, q| encode_webp(pixels, q as f32),
    )?;
    Ok(EncodedCandidate {
        bytes,
        ext: "webp",
        over_limit,
    })
}

// --- PNG optimization (exact palette, lossless, graphic NeuQuant, binary-search scale, fallback) ---

fn encode_png_target(
    capped: &CFRetained<CGImage>,
    source: &SourceMeta,
    limit: usize,
    fallback: FallbackFormat,
    cancel: &AtomicBool,
) -> Result<EncodedCandidate, String> {
    let mut full_px = Some(extract_straight_pixels(capped)?);
    let icc = full_px
        .as_ref()
        .and_then(|p| p.icc_profile.clone());

    // 1. Check fast exact lossless indexed palette first (aborts immediately at 257 colors).
    //    If exact palette succeeds, skip slower 24/32-bit truecolor deflate at 1.0x.
    let (mut smallest_png, exact_colors) = {
        let px = full_px.as_ref().unwrap();
        if let Some((exact, count)) = encode_png_exact_palette(px, icc.as_deref())? {
            if exact.len() <= limit {
                return Ok(EncodedCandidate {
                    bytes: exact,
                    ext: "png",
                    over_limit: false,
                });
            }
            (Some(exact), Some(count))
        } else {
            let lossless = encode_png_lossless(px, icc.as_deref())?;
            if lossless.len() <= limit {
                return Ok(EncodedCandidate {
                    bytes: lossless,
                    ext: "png",
                    over_limit: false,
                });
            }
            (Some(lossless), None)
        }
    };

    // 2. Palette quantization is restricted to graphics (never photographs) and only tests
    //    palette sizes strictly smaller than any exact palette count already encoded.
    {
        let px = full_px.as_ref().unwrap();
        if !is_photographic_pixels(px, source.is_indexed) {
            let mut rgba_cache: Option<Vec<u8>> = None;
            for colors in PNG_PALETTE_COLORS {
                if exact_colors.is_some_and(|n| colors >= n) {
                    continue;
                }
                if cancel.load(Ordering::Acquire) {
                    return Ok(EncodedCandidate {
                        bytes: Vec::new(),
                        ext: "png",
                        over_limit: true,
                    });
                }
                let rgba: &[u8] = match &px.data {
                    PixelBuf::Rgba(buf) => buf,
                    PixelBuf::Rgb(rgb) => rgba_cache.get_or_insert_with(|| {
                        let mut buf = Vec::with_capacity(px.width * px.height * 4);
                        for p in rgb.chunks_exact(3) {
                            buf.extend_from_slice(p);
                            buf.push(255);
                        }
                        buf
                    }),
                };
                let data = encode_png_quantized(
                    px.width,
                    px.height,
                    rgba,
                    px.has_transparency(),
                    colors,
                    icc.as_deref(),
                )?;
                if data.len() <= limit {
                    return Ok(EncodedCandidate {
                        bytes: data,
                        ext: "png",
                        over_limit: false,
                    });
                }
                if smallest_png.as_ref().is_none_or(|b| data.len() < b.len()) {
                    smallest_png = Some(data);
                }
            }
        }
    }

    // Free full-res pixel buffer before downscale search unless WebP fallback will reuse it.
    if fallback != FallbackFormat::Webp {
        full_px = None;
    }

    // 3. Binary-search lossless PNG downscaling across SCALE_STEPS[1..].
    let last = SCALE_STEPS.len() - 1;
    let eval_scale = |idx: usize| -> Result<Vec<u8>, String> {
        let scaled = rescale_cg(capped, SCALE_STEPS[idx])?;
        let px = extract_straight_pixels(&scaled)?;
        encode_png_lossless(&px, px.icc_profile.as_deref())
    };

    if !cancel.load(Ordering::Acquire) && last >= 1 {
        let mut best_fit = eval_scale(last)?;
        if best_fit.len() <= limit {
            let mut lo = 1usize;
            let mut hi = last;
            while lo < hi {
                if cancel.load(Ordering::Acquire) {
                    return Ok(EncodedCandidate {
                        bytes: Vec::new(),
                        ext: "png",
                        over_limit: true,
                    });
                }
                let mid = lo + (hi - lo) / 2;
                let data = eval_scale(mid)?;
                if data.len() <= limit {
                    best_fit = data;
                    hi = mid;
                } else {
                    if smallest_png.as_ref().is_none_or(|b| data.len() < b.len()) {
                        smallest_png = Some(data);
                    }
                    lo = mid + 1;
                }
            }
            return Ok(EncodedCandidate {
                bytes: best_fit,
                ext: "png",
                over_limit: false,
            });
        }
        if smallest_png.as_ref().is_none_or(|b| best_fit.len() < b.len()) {
            smallest_png = Some(best_fit);
        }
    }

    if cancel.load(Ordering::Acquire) {
        return Ok(EncodedCandidate {
            bytes: Vec::new(),
            ext: "png",
            over_limit: true,
        });
    }

    // 4. Optional format fallback for oversized photographic PNGs.
    let pick_smaller = |res: EncodedCandidate, smallest_png: Option<Vec<u8>>| {
        if res.over_limit && !res.bytes.is_empty() {
            if let Some(png) = smallest_png {
                if png.len() <= res.bytes.len() {
                    return EncodedCandidate {
                        bytes: png,
                        ext: "png",
                        over_limit: true,
                    };
                }
            }
        }
        res
    };
    match fallback {
        FallbackFormat::Keep => Ok(EncodedCandidate {
            bytes: smallest_png.unwrap_or_default(),
            ext: "png",
            over_limit: true,
        }),
        FallbackFormat::Heic => {
            let res = search_lossy_format(
                capped,
                &source.properties,
                "public.heic",
                "heic",
                limit,
                cancel,
            )?;
            Ok(pick_smaller(res, smallest_png))
        }
        FallbackFormat::Webp => {
            let res = search_lossy_webp(
                capped,
                full_px,
                WEBP_TARGET_Q,
                WEBP_FLOOR_Q,
                limit,
                cancel,
            )?;
            Ok(pick_smaller(res, smallest_png))
        }
    }
}

/// Sample original pixels directly on a uniform stride (without interpolating new colors)
/// to distinguish graphics from photographs cleanly and without extra allocations.
fn is_photographic_pixels(px: &StraightPixels, is_indexed: bool) -> bool {
    if is_indexed {
        return false;
    }
    let (buf, stride) = match &px.data {
        PixelBuf::Rgb(b) => (b.as_slice(), 3usize),
        PixelBuf::Rgba(b) => (b.as_slice(), 4usize),
    };
    let total_pixels = px.width * px.height;
    let step = (total_pixels / PHOTO_SAMPLE_TARGET).max(1);
    let mut seen: HashSet<u32> = HashSet::with_capacity(PHOTO_COLOR_THRESHOLD + 1);
    for idx in (0..total_pixels).step_by(step) {
        let base = idx * stride;
        let rgb = ((buf[base] as u32) << 16)
            | ((buf[base + 1] as u32) << 8)
            | (buf[base + 2] as u32);
        seen.insert(rgb);
        if seen.len() > PHOTO_COLOR_THRESHOLD {
            return true;
        }
    }
    false
}

fn encode_png_lossless(pixels: &StraightPixels, icc: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut info = png::Info::with_size(pixels.width as u32, pixels.height as u32);
    let raw = match &pixels.data {
        PixelBuf::Rgba(rgba) => {
            info.color_type = png::ColorType::Rgba;
            rgba.as_slice()
        }
        PixelBuf::Rgb(rgb) => {
            info.color_type = png::ColorType::Rgb;
            rgb.as_slice()
        }
    };
    info.bit_depth = png::BitDepth::Eight;
    if let Some(profile) = icc {
        info.icc_profile = Some(Cow::Borrowed(profile));
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::with_info(&mut out, info)
            .map_err(|e| format!("PNG encoder setup failed: {e}"))?;
        encoder.set_compression(png::Compression::Best);
        encoder.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG header failed: {e}"))?;
        writer
            .write_image_data(raw)
            .map_err(|e| format!("PNG encode failed: {e}"))?;
    }
    Ok(out)
}

/// If the image has `<= 256` unique RGBA colors, encode a mathematically lossless indexed PNG.
fn encode_png_exact_palette(
    pixels: &StraightPixels,
    icc: Option<&[u8]>,
) -> Result<Option<(Vec<u8>, usize)>, String> {
    let mut table: HashMap<u32, u8> = HashMap::with_capacity(257);
    let mut palette = Vec::with_capacity(256 * 3);
    let mut trns = Vec::new();
    // Start small so photographs (which bail after ~257 pixels) don't allocate 8+ MB upfront.
    let mut indices = Vec::with_capacity(4096.min(pixels.width * pixels.height));

    match &pixels.data {
        PixelBuf::Rgba(rgba) => {
            trns.reserve(256);
            for px in rgba.chunks_exact(4) {
                let key = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                let idx = if let Some(&existing) = table.get(&key) {
                    existing
                } else {
                    if table.len() >= 256 {
                        return Ok(None);
                    }
                    let next = table.len() as u8;
                    table.insert(key, next);
                    palette.extend_from_slice(&px[..3]);
                    trns.push(px[3]);
                    next
                };
                indices.push(idx);
            }
        }
        PixelBuf::Rgb(rgb) => {
            for px in rgb.chunks_exact(3) {
                let key = u32::from_le_bytes([px[0], px[1], px[2], 255]);
                let idx = if let Some(&existing) = table.get(&key) {
                    existing
                } else {
                    if table.len() >= 256 {
                        return Ok(None);
                    }
                    let next = table.len() as u8;
                    table.insert(key, next);
                    palette.extend_from_slice(px);
                    next
                };
                indices.push(idx);
            }
        }
    }

    let bytes = write_indexed_png(
        pixels.width as u32,
        pixels.height as u32,
        &palette,
        pixels.has_transparency().then_some(&trns),
        &indices,
        icc,
    )?;
    Ok(Some((bytes, table.len())))
}

fn encode_png_quantized(
    width: usize,
    height: usize,
    rgba: &[u8],
    has_transparency: bool,
    colors: usize,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    // Scale NeuQuant sample factor with pixel count so multi-megapixel graphics stay fast.
    let pixel_count = width * height;
    let samplefac = (pixel_count / 250_000).clamp(1, 10) as i32;
    let nq = color_quant::NeuQuant::new(samplefac, colors, rgba);
    let mut palette = Vec::with_capacity(colors * 3);
    let mut trns = Vec::with_capacity(colors);
    for entry in nq.color_map_rgba().chunks_exact(4) {
        palette.extend_from_slice(&entry[..3]);
        trns.push(entry[3]);
    }
    let indices: Vec<u8> = rgba
        .chunks_exact(4)
        .map(|px| nq.index_of(px) as u8)
        .collect();
    write_indexed_png(
        width as u32,
        height as u32,
        &palette,
        has_transparency.then_some(&trns),
        &indices,
        icc,
    )
}

fn write_indexed_png(
    width: u32,
    height: u32,
    palette: &[u8],
    trns: Option<&[u8]>,
    indices: &[u8],
    icc: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut info = png::Info::with_size(width, height);
    info.color_type = png::ColorType::Indexed;
    info.bit_depth = png::BitDepth::Eight;
    info.palette = Some(Cow::Borrowed(palette));
    if let Some(alpha) = trns {
        // Trim trailing opaque entries per PNG tRNS spec.
        let keep = alpha.iter().rposition(|&a| a < 255).map_or(0, |i| i + 1);
        if keep > 0 {
            info.trns = Some(Cow::Borrowed(&alpha[..keep]));
        }
    }
    if let Some(profile) = icc {
        info.icc_profile = Some(Cow::Borrowed(profile));
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::with_info(&mut out, info)
            .map_err(|e| format!("PNG palette encoder setup failed: {e}"))?;
        encoder.set_compression(png::Compression::Best);
        encoder.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG palette header failed: {e}"))?;
        writer
            .write_image_data(indices)
            .map_err(|e| format!("PNG palette encode failed: {e}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CFData;
    use objc2_core_graphics::{kCGColorSpaceSRGB, CGBitmapContextCreate};
    use objc2_image_io::kCGImageDestinationLossyCompressionQuality;

    fn make_cg_image(rgba: &[u8], width: usize, height: usize, alpha: bool) -> CFRetained<CGImage> {
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
        let info = if alpha {
            CGImageAlphaInfo::PremultipliedLast
        } else {
            CGImageAlphaInfo::NoneSkipLast
        };
        let ctx = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                width,
                height,
                8,
                width * 4,
                space.as_deref(),
                info.0,
            )
        }
        .unwrap();
        let ptr = CGBitmapContextGetData(Some(&ctx)) as *mut u8;
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, width * height * 4) };
        dst.copy_from_slice(rgba);
        CGBitmapContextCreateImage(Some(&ctx)).unwrap()
    }

    fn write_cg(image: &CGImage, uti: &str, path: &Path, quality: f64) {
        let url = CFURL::from_file_path(path).unwrap();
        unsafe {
            let dest =
                CGImageDestination::with_url(&url, &CFString::from_str(uti), 1, None).unwrap();
            let q = CFNumber::new_f64(quality);
            let props = CFDictionary::<CFString, CFType>::from_slices(
                &[kCGImageDestinationLossyCompressionQuality],
                &[q.as_ref()],
            );
            dest.add_image(image, Some(props.as_opaque()));
            assert!(dest.finalize());
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blickli-compress-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dimension_cap_respects_orientation() {
        assert_eq!(capped_dimensions(6000, 4000, MaxDimensions::Uhd4k), (3240, 2160));
        assert_eq!(capped_dimensions(4000, 6000, MaxDimensions::Uhd4k), (2160, 3240));
        assert_eq!(capped_dimensions(2160, 3840, MaxDimensions::Uhd4k), (2160, 3840));
        assert_eq!(capped_dimensions(5000, 3000, MaxDimensions::Original), (5000, 3000));
    }

    #[test]
    fn binary_search_matches_linear_with_fewer_encodes() {
        let cancel = AtomicBool::new(false);
        let synth_size = |scale: f64, q: f64| ((scale * scale * q * 1000.0) as usize).max(1);

        for limit in [200usize, 300, 450, 600, 800, 950] {
            let mut calls = 0usize;
            let (got, over) = search_lossy_generic(
                0.85,
                0.78,
                limit,
                &cancel,
                |_, s| Ok(s),
                |&s, q| {
                    calls += 1;
                    Ok(vec![0u8; synth_size(s, q)])
                },
            )
            .unwrap();

            let mut expected = None;
            let mut smallest = None::<Vec<u8>>;
            'outer: for q in [0.85, 0.78] {
                for &s in &SCALE_STEPS {
                    let sz = synth_size(s, q);
                    if smallest.as_ref().is_none_or(|b| sz < b.len()) {
                        smallest = Some(vec![0u8; sz]);
                    }
                    if sz <= limit {
                        expected = Some((sz, false));
                        break 'outer;
                    }
                }
            }
            let (exp_len, exp_over) = expected.unwrap_or((smallest.unwrap().len(), true));
            assert_eq!(got.len(), exp_len, "limit={limit}");
            assert_eq!(over, exp_over, "limit={limit}");
            assert!(calls <= 6, "binary search used {calls} calls for limit={limit}");
        }
    }

    #[test]
    fn graphic_png_uses_exact_or_quantized_palette_and_shrinks() {
        let (w, h) = (512, 512);
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let block = ((x / 64) + (y / 64)) % 4;
                let color = match block {
                    0 => [240, 60, 60, 255],
                    1 => [60, 180, 90, 255],
                    2 => [40, 110, 220, 255],
                    _ => [250, 220, 70, 255],
                };
                rgba[i..i + 4].copy_from_slice(&color);
            }
        }
        let img = make_cg_image(&rgba, w, h, false);
        let px = extract_straight_pixels(&img).unwrap();
        let (exact, count) = encode_png_exact_palette(&px, None).unwrap().unwrap();
        assert_eq!(count, 4);
        let full = encode_png_lossless(&px, None).unwrap();
        assert!(exact.len() <= full.len());
        assert!(!is_photographic_pixels(&px, false));
    }

    #[test]
    fn webp_with_icc_profile_roundtrips_through_macos_imageio() {
        let (w, h) = (64, 48);
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                rgba[i] = (x * 4) as u8;
                rgba[i + 1] = (y * 5) as u8;
                rgba[i + 2] = 160;
                rgba[i + 3] = if x < 16 { 128 } else { 255 };
            }
        }
        let icc = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            .and_then(|s| CGColorSpace::icc_data(Some(&s)))
            .map(|d| d.to_vec())
            .unwrap();
        let px = StraightPixels {
            width: w,
            height: h,
            data: PixelBuf::Rgba(rgba),
            icc_profile: Some(icc),
        };

        let encoded = encode_webp(&px, 85.0).unwrap();
        let cf = CFData::from_bytes(&encoded);
        let src = unsafe { CGImageSource::with_data(&cf, None) }.unwrap();
        let decoded = unsafe { src.image_at_index(0, None) }.unwrap();
        assert_eq!(CGImage::width(Some(&decoded)), w);
        assert_eq!(CGImage::height(Some(&decoded)), h);
    }

    #[test]
    fn jpeg_and_png_fallback_compress_under_cap_and_collect_recursive() {
        let dir = temp_dir("integration");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let (w, h) = (640, 480);
        let mut rgba = vec![0u8; w * h * 4];
        let mut state: u32 = 0x1234_5678;
        for px in rgba.chunks_exact_mut(4) {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            px[0] = (state >> 24) as u8;
            px[1] = (state >> 16) as u8;
            px[2] = (state >> 8) as u8;
            px[3] = 255;
        }
        let img = make_cg_image(&rgba, w, h, false);

        let jpg = dir.join("photo.jpg");
        let png = sub.join("photo.png");
        write_cg(&img, "public.jpeg", &jpg, 1.0);
        write_cg(&img, "public.png", &png, 1.0);

        let recursive = collect_folder_candidates(&dir, true, |_| false);
        assert_eq!(recursive.len(), 2);
        let flat = collect_folder_candidates(&dir, false, |_| false);
        assert_eq!(flat.len(), 1);

        let cancel = AtomicBool::new(false);
        let jpg_orig = std::fs::metadata(&jpg).unwrap().len();
        let opts = CompressOptions {
            target_kb: 100,
            max_dims: MaxDimensions::Uhd4k,
            fallback: FallbackFormat::Keep,
        };
        let encoded_jpg = prepare_compressed(&jpg, jpg_orig, opts, &cancel)
            .unwrap()
            .expect("jpeg compressed");
        assert_eq!(encoded_jpg.ext, "jpg");
        assert!(encoded_jpg.bytes.len() < jpg_orig as usize);

        let png_orig = std::fs::metadata(&png).unwrap().len();
        let webp_opts = CompressOptions {
            target_kb: 100,
            max_dims: MaxDimensions::Uhd4k,
            fallback: FallbackFormat::Webp,
        };
        let encoded_webp = prepare_compressed(&png, png_orig, webp_opts, &cancel)
            .unwrap()
            .expect("photographic png fell back to webp");
        assert_eq!(encoded_webp.ext, "webp");
        assert!(encoded_webp.bytes.len() < png_orig as usize);

        let bmp = dir.join("raw.bmp");
        write_cg(&img, "com.microsoft.bmp", &bmp, 1.0);
        let bmp_orig = std::fs::metadata(&bmp).unwrap().len();
        let encoded_bmp = prepare_compressed(&bmp, bmp_orig, opts, &cancel)
            .unwrap()
            .expect("bmp converted to jpeg");
        assert_eq!(encoded_bmp.ext, "jpg");
        assert!(encoded_bmp.bytes.len() < bmp_orig as usize);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "uses hardware VideoToolbox HEIC encoder via Image I/O"]
    fn heic_fallback_encodes_photographic_png() {
        let dir = temp_dir("heic");
        let (w, h) = (640, 480);
        let mut rgba = vec![0u8; w * h * 4];
        let mut state: u32 = 0xCAFE_BABE;
        for px in rgba.chunks_exact_mut(4) {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            px[0] = (state >> 24) as u8;
            px[1] = (state >> 16) as u8;
            px[2] = (state >> 8) as u8;
            px[3] = 255;
        }
        let img = make_cg_image(&rgba, w, h, false);
        let png = dir.join("photo.png");
        write_cg(&img, "public.png", &png, 1.0);
        let png_orig = std::fs::metadata(&png).unwrap().len();
        let opts = CompressOptions {
            target_kb: 100,
            max_dims: MaxDimensions::Uhd4k,
            fallback: FallbackFormat::Heic,
        };
        let cancel = AtomicBool::new(false);
        let encoded = prepare_compressed(&png, png_orig, opts, &cancel)
            .unwrap()
            .expect("photographic png fell back to heic");
        assert_eq!(encoded.ext, "heic");
        assert!(encoded.bytes.len() < png_orig as usize);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
