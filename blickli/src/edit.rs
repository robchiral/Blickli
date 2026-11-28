//! Image I/O rotation and crop persistence (runs on the background edit worker).
//!
//! Rotations on JPEG, HEIC, PNG, and TIFF rewrite only the EXIF orientation tag.
//! Other formats and crops re-encode the image and move the original to Trash for undo.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use blickli_core::history::Edit;
use blickli_core::messages::reason;
use blickli_core::{transfer, Orientation};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFError, CFMutableDictionary, CFNumber, CFRetained,
    CFString, CFType, CGAffineTransform, CGPoint, CGRect, CGSize, CFURL,
};
use objc2_core_graphics::{
    kCGColorSpaceDisplayP3, kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage,
    CGBitmapContextGetBytesPerRow, CGColorSpace, CGColorSpaceModel, CGContext, CGImage,
    CGImageAlphaInfo, CGInterpolationQuality,
};
use objc2_foundation::{
    NSFileManager, NSFileManagerItemReplacementOptions, NSSearchPathDirectory,
    NSSearchPathDomainMask, NSURL,
};
use objc2_image_io::{
    kCGImageDestinationEmbedThumbnail, kCGImageDestinationLossyCompressionQuality,
    kCGImageDestinationOrientation, kCGImagePropertyExifDictionary,
    kCGImagePropertyExifPixelXDimension, kCGImagePropertyExifPixelYDimension,
    kCGImagePropertyJFIFDictionary, kCGImagePropertyJFIFIsProgressive,
    kCGImagePropertyOrientation, kCGImagePropertyTIFFDictionary, kCGImagePropertyTIFFOrientation,
    CGImageDestination, CGImageSource,
};

/// Normalized crop rectangle (top-left origin) in displayed coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub enum Cropped {
    /// Original moved to Trash and replaced with the cropped file.
    Replaced(Edit),
    /// Saved as a new file beside the original.
    Copy(PathBuf),
}

const QUALITY: f64 = 0.92;

/// UTIs supporting lossless EXIF orientation tag updates in Image I/O.
const LOSSLESS_ORIENTATION: &[&str] = &["public.jpeg", "public.heic", "public.heif", "public.png", "public.tiff"];

struct Source {
    image_source: CFRetained<CGImageSource>,
    uti: String,
    orientation: Orientation,
    frames: usize,
    properties: CFRetained<CFDictionary<CFString, CFType>>,
}

fn open(path: &Path) -> Result<Source, String> {
    let unreadable = || "Cannot read file".to_owned();
    let url = CFURL::from_file_path(path).ok_or_else(unreadable)?;
    unsafe {
        let image_source = CGImageSource::with_url(&url, None).ok_or_else(unreadable)?;
        let uti = image_source.r#type().ok_or_else(unreadable)?.to_string();
        let properties = image_source
            .properties_at_index(image_source.primary_image_index(), None)
            .ok_or_else(unreadable)?;
        let properties: CFRetained<CFDictionary<CFString, CFType>> =
            CFRetained::cast_unchecked(properties);
        let orientation = properties
            .get(kCGImagePropertyOrientation)
            .and_then(|value| value.downcast::<CFNumber>().ok())
            .and_then(|number| number.as_i64())
            .map_or(Orientation::UP, Orientation::from_exif);
        Ok(Source {
            frames: image_source.count(),
            image_source,
            uti,
            orientation,
            properties,
        })
    }
}

/// Apply `view` rotation/flip to `path` on disk and return an undo record.
pub fn rotate_file(path: &Path, view: Orientation) -> Result<Edit, String> {
    rotate_file_with(path, view, &trash_one)
}

fn destination_uti(uti: &str) -> &str {
    // Image I/O writes HEIF containers under `public.heic`.
    if uti == "public.heif" {
        "public.heic"
    } else {
        uti
    }
}

fn rotate_file_with(
    path: &Path,
    view: Orientation,
    trash: &dyn Fn(&Path) -> Result<PathBuf, String>,
) -> Result<Edit, String> {
    let source = open(path)?;
    if source.frames > 1 {
        return Err("Multi-frame images cannot be edited".to_owned());
    }
    let after = source.orientation.then(view);
    let dest_uti = destination_uti(&source.uti);
    if LOSSLESS_ORIENTATION.contains(&source.uti.as_str()) {
        match write_orientation_staged(&source, path, after.exif()) {
            Ok(staging) => {
                // Replacement failures are filesystem errors; do not fall through to lossy re-encode.
                replace_contents(path, &staging.file)?;
                return Ok(Edit::Oriented {
                    path: path.to_path_buf(),
                    before: source.orientation.exif(),
                    after: after.exif(),
                });
            }
            // Fall through to re-encoding if lossless copy fails for a writable format.
            Err(error) if !writable(dest_uti) => return Err(error),
            Err(_) => {}
        }
    }
    if !writable(dest_uti) {
        return Err(format!("{} files cannot be saved", kind_name(&source.uti, path)));
    }
    let edited = Staging::new(path)?;
    render_to(&source, view, None, dest_uti, &edited.file)?;
    swap_in(path, &edited.file, path, trash)
}

/// Rewrite a file's EXIF orientation tag directly (used for undo/redo).
pub fn write_orientation(path: &Path, tag: u8) -> Result<(), String> {
    let staging = write_orientation_staged(&open(path)?, path, tag)?;
    replace_contents(path, &staging.file)
}

fn write_orientation_staged(source: &Source, path: &Path, tag: u8) -> Result<Staging, String> {
    let staging = Staging::new(path)?;
    let url = CFURL::from_file_path(&staging.file).ok_or("Cannot write to folder")?;
    unsafe {
        let uti = CFString::from_str(destination_uti(&source.uti));
        let destination = CGImageDestination::with_url(&url, &uti, 1, None)
            .ok_or("Cannot create output file")?;
        let tag = CFNumber::new_i32(i32::from(tag));
        let options: CFRetained<CFDictionary<CFString, CFType>> =
            CFDictionary::from_slices(&[kCGImageDestinationOrientation], &[tag.as_ref()]);
        let mut error: *mut CFError = std::ptr::null_mut();
        let copied =
            destination.copy_image_source(&source.image_source, Some(options.as_opaque()), &mut error);
        if let Some(error) = std::ptr::NonNull::new(error) {
            let error = CFRetained::from_raw(error);
            if !copied {
                let text = error.description().map(|d| d.to_string()).unwrap_or_default();
                return Err(reason(&text));
            }
        }
        if !copied {
            return Err("Cannot write orientation".to_owned());
        }
    }
    Ok(staging)
}

/// Render and save a crop of `path`, either replacing the original or creating a copy.
pub fn crop_file(
    path: &Path,
    view: Orientation,
    crop: CropRect,
    replace: bool,
) -> Result<Cropped, String> {
    crop_file_with(path, view, crop, replace, &trash_one)
}

fn writable_target(source: &Source, path: &Path) -> (String, String) {
    let dest_uti = destination_uti(&source.uti);
    if writable(dest_uti) {
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        (dest_uti.to_owned(), extension)
    } else {
        ("public.png".to_owned(), "png".to_owned())
    }
}

fn place_or_replace(
    path: &Path,
    staged: &Path,
    extension: &str,
    copy_suffix: &str,
    replace: bool,
    trash: &dyn Fn(&Path) -> Result<PathBuf, String>,
) -> Result<Cropped, String> {
    let folder = path.parent().ok_or("Invalid file path")?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dotted = |name: String| {
        if extension.is_empty() {
            name
        } else {
            format!("{name}.{extension}")
        }
    };
    if replace {
        let same_ext = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(extension));
        let target = if same_ext {
            path.to_path_buf()
        } else {
            folder.join(dotted(stem))
        };
        swap_in(path, staged, &target, trash).map(Cropped::Replaced)
    } else {
        let name = dotted(format!("{stem} {copy_suffix}"));
        transfer::place_as(staged, folder, &name)
            .map(|placed| Cropped::Copy(placed.target))
            .map_err(|error| reason(&error.to_string()))
    }
}

fn crop_file_with(
    path: &Path,
    view: Orientation,
    crop: CropRect,
    replace: bool,
    trash: &dyn Fn(&Path) -> Result<PathBuf, String>,
) -> Result<Cropped, String> {
    let source = open(path)?;
    // Fall back to lossless PNG for read-only formats.
    let (uti, extension) = writable_target(&source, path);
    let edited = Staging::new(path)?;
    render_to(&source, view, Some(crop), &uti, &edited.file)?;
    place_or_replace(path, &edited.file, &extension, "cropped", replace, trash)
}

/// Decode `path` upright with `view` applied, refusing multi-frame animations.
pub fn decode_oriented(path: &Path, view: Orientation) -> Result<CFRetained<CGImage>, String> {
    let source = open(path)?;
    if source.frames > 1 {
        return Err("files with several frames or pages can't be upscaled".to_owned());
    }
    render(&source, view, None)
}

/// Save an upscaled bitmap beside `path` (`<stem> upscaled.<ext>`) or in its place.
pub fn save_upscaled(path: &Path, image: &CGImage, replace: bool) -> Result<Cropped, String> {
    save_upscaled_with(path, image, replace, &trash_one)
}

fn save_upscaled_with(
    path: &Path,
    image: &CGImage,
    replace: bool,
    trash: &dyn Fn(&Path) -> Result<PathBuf, String>,
) -> Result<Cropped, String> {
    let source = open(path)?;
    if source.frames > 1 {
        return Err("files with several frames or pages can't be upscaled".to_owned());
    }
    let (uti, extension) = writable_target(&source, path);
    let edited = Staging::new(path)?;
    encode(image, &source, &uti, &edited.file)?;
    place_or_replace(path, &edited.file, &extension, "upscaled", replace, trash)
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Save raw encoded `bytes` to `target`, moving `orig` to Trash for undo.
pub(crate) fn commit_bytes(orig: &Path, target: &Path, bytes: &[u8]) -> Result<Edit, String> {
    if !same_file(target, orig) && std::fs::symlink_metadata(target).is_ok() {
        let name = crate::info::file_name(target);
        return Err(format!("{name} already exists"));
    }
    // Stage near `orig` (which exists on disk so ItemReplacementDirectory succeeds).
    let staged = Staging::new(orig)?;
    std::fs::write(&staged.file, bytes).map_err(|error| reason(&error.to_string()))?;
    swap_in(orig, &staged.file, target, &trash_one)
}

/// Swap `current` to Trash and restore `stashed` to `home`.
pub fn swap_back(current: &Path, stashed: &Path, home: &Path) -> Result<Edit, String> {
    swap_in(current, stashed, home, &trash_one)
}

/// Trash `out` and move `incoming` to `to`, rolling back if the move fails.
fn swap_in(
    out: &Path,
    incoming: &Path,
    to: &Path,
    trash: &dyn Fn(&Path) -> Result<PathBuf, String>,
) -> Result<Edit, String> {
    if incoming.as_os_str().is_empty() || !incoming.exists() {
        return Err("Original file is no longer in Trash".to_owned());
    }
    // Avoid clobbering an existing file (or dangling symlink) when saving under a new extension.
    if !same_file(to, out) && std::fs::symlink_metadata(to).is_ok() {
        let name = crate::info::file_name(to);
        return Err(format!("{name} already exists"));
    }
    let landed = trash(out)?;
    // Once `out` has been trashed, always finish moving `incoming` into place even if
    // the volume didn't report a Trash URL (undo will simply refuse instead of losing both files).
    match transfer::move_exact(incoming, to) {
        Ok(()) => Ok(Edit::Replaced {
            current: to.to_path_buf(),
            stashed: landed,
            home: out.to_path_buf(),
        }),
        Err(error) => {
            if !landed.as_os_str().is_empty() {
                let _ = transfer::move_exact(&landed, out);
            }
            Err(reason(&error.to_string()))
        }
    }
}

fn trash_one(path: &Path) -> Result<PathBuf, String> {
    let trashed = crate::trash::move_to_trash(&[path.to_path_buf()]);
    match (trashed.moved.into_iter().next(), trashed.failed.into_iter().next()) {
        (Some((_, landed)), _) => Ok(landed),
        (None, Some(why)) => Err(why),
        (None, None) => Err("Cannot move original to Trash".to_owned()),
    }
}

fn render_to(
    source: &Source,
    view: Orientation,
    crop: Option<CropRect>,
    uti: &str,
    out: &Path,
) -> Result<(), String> {
    // Refuse multi-frame images because re-encoding would discard frames.
    if source.frames > 1 {
        return Err("Multi-frame images cannot be edited".to_owned());
    }
    let image = render(source, view, crop)?;
    encode(&image, source, uti, out)
}

fn render(
    source: &Source,
    view: Orientation,
    crop: Option<CropRect>,
) -> Result<CFRetained<CGImage>, String> {
    let image = unsafe {
        source
            .image_source
            .image_at_index(source.image_source.primary_image_index(), None)
    }
    .ok_or("Cannot decode image")?;
    draw_oriented(&image, source.orientation.then(view), crop)
}

pub(crate) fn has_alpha_channel(image: &CGImage) -> bool {
    !matches!(
        CGImage::alpha_info(Some(image)),
        CGImageAlphaInfo::None | CGImageAlphaInfo::NoneSkipFirst | CGImageAlphaInfo::NoneSkipLast
    )
}

/// Pick a non-extended 8-bit output-capable RGB `CGColorSpace` for `image`.
/// Extended-range HDR spaces (`ExtendedSRGB`, `ExtendedDisplayP3`, etc.) report
/// `supports_output == true` yet fail 8-bit `CGBitmapContextCreate`, so map them
/// to Display P3 (when wide-gamut) or sRGB.
pub(crate) fn output_rgb_space(image: &CGImage) -> Option<CFRetained<CGColorSpace>> {
    if let Some(space) = CGImage::color_space(Some(image)) {
        if CGColorSpace::model(Some(&space)) == CGColorSpaceModel::RGB {
            if space.supports_output() && !space.uses_extended_range() {
                return Some(space);
            }
            if space.is_wide_gamut_rgb() {
                if let Some(p3) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceDisplayP3 })) {
                    return Some(p3);
                }
            }
        }
    }
    CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
}

pub(crate) fn bitmap_format(
    image: &CGImage,
) -> (Option<CFRetained<CGColorSpace>>, CGImageAlphaInfo) {
    let alpha = has_alpha_channel(image);
    // Preserve a standard grayscale space when supported by 8-bit CGBitmapContext.
    if !alpha {
        if let Some(space) = CGImage::color_space(Some(image)) {
            if CGColorSpace::model(Some(&space)) == CGColorSpaceModel::Monochrome
                && space.supports_output()
                && !space.uses_extended_range()
            {
                return (Some(space), CGImageAlphaInfo::None);
            }
        }
    }
    let info = if alpha {
        CGImageAlphaInfo::PremultipliedLast
    } else {
        CGImageAlphaInfo::NoneSkipLast
    };
    (output_rgb_space(image), info)
}

/// Create an 8-bit `CGBitmapContext`, retrying in sRGB if CoreGraphics rejects `space`.
/// Returns both the context and the actual `CGColorSpace` backing it.
pub(crate) fn make_bitmap_context(
    width: usize,
    height: usize,
    bytes_per_row: usize,
    space: Option<CFRetained<CGColorSpace>>,
    info: CGImageAlphaInfo,
) -> Option<(CFRetained<CGContext>, Option<CFRetained<CGColorSpace>>)> {
    let ctx = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            bytes_per_row,
            space.as_deref(),
            info.0,
        )
    };
    if let Some(ctx) = ctx {
        return Some((ctx, space));
    }
    let fallback_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
    let fallback_info = if info == CGImageAlphaInfo::None {
        CGImageAlphaInfo::NoneSkipLast
    } else {
        info
    };
    let fallback_bpr = if bytes_per_row == 0 {
        0
    } else {
        width.checked_mul(4)?
    };
    let ctx = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            fallback_bpr,
            fallback_space.as_deref(),
            fallback_info.0,
        )
    }?;
    Some((ctx, fallback_space))
}

/// Render `image` into an 8-bit RGB/RGBA `CGBitmapContext`, returning the context,
/// actual color space, dimensions, row stride in bytes, and whether the source has alpha.
#[allow(clippy::type_complexity)]
pub(crate) fn render_rgb_bitmap(
    image: &CGImage,
) -> Result<(CFRetained<CGContext>, Option<CFRetained<CGColorSpace>>, usize, usize, usize, bool), String> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        return Err("Empty image".to_owned());
    }
    let has_alpha = has_alpha_channel(image);
    let space = output_rgb_space(image);
    let info = if has_alpha {
        CGImageAlphaInfo::PremultipliedLast
    } else {
        CGImageAlphaInfo::NoneSkipLast
    };
    let req_bpr = width.checked_mul(4).ok_or("Image too large")?;
    let (context, actual_space) =
        make_bitmap_context(width, height, req_bpr, space, info).ok_or("Not enough memory")?;
    let stride = CGBitmapContextGetBytesPerRow(Some(&context));
    if stride < req_bpr {
        return Err("Cannot access pixels".to_owned());
    }
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::None);
    CGContext::draw_image(
        Some(&context),
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(width as f64, height as f64),
        ),
        Some(image),
    );
    Ok((context, actual_space, width, height, stride, has_alpha))
}

/// Recover a straight 8-bit color channel from premultiplied `v` with non-zero alpha `a`.
#[inline]
pub(crate) fn unpremultiply_channel(v: u8, a: u8) -> u8 {
    let a = u32::from(a);
    ((u32::from(v) * 255 + a / 2) / a).min(255) as u8
}

/// Draw `image` with `orientation` and optional `crop` baked into pixel coordinates.
pub(crate) fn draw_oriented(
    image: &CGImage,
    orientation: Orientation,
    crop: Option<CropRect>,
) -> Result<CFRetained<CGImage>, String> {
    draw_oriented_scaled(image, orientation, None, crop)
}

/// Draw `image` with `orientation`, optional target output size, and optional `crop`
/// in a single Core Graphics render pass.
pub(crate) fn draw_oriented_scaled(
    image: &CGImage,
    orientation: Orientation,
    target_size: Option<(usize, usize)>,
    crop: Option<CropRect>,
) -> Result<CFRetained<CGImage>, String> {
    let (stored_w, stored_h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    if stored_w == 0 || stored_h == 0 {
        return Err("Empty image".to_owned());
    }
    let (shown_w, shown_h) = if orientation.swaps_sides() {
        (stored_h, stored_w)
    } else {
        (stored_w, stored_h)
    };
    let (x, y, crop_w, crop_h) = pixel_rect(crop, shown_w, shown_h);
    let (out_w, out_h) = target_size.unwrap_or((crop_w, crop_h));
    let (space, info) = bitmap_format(image);
    let (context, _) =
        make_bitmap_context(out_w, out_h, 0, space, info).ok_or("Not enough memory")?;
    let context = Some(&*context);
    let quality = if target_size.is_some() && (out_w != crop_w || out_h != crop_h) {
        CGInterpolationQuality::High
    } else {
        CGInterpolationQuality::None
    };
    CGContext::set_interpolation_quality(context, quality);
    if out_w != crop_w || out_h != crop_h {
        CGContext::scale_ctm(
            context,
            out_w as f64 / crop_w as f64,
            out_h as f64 / crop_h as f64,
        );
    }
    // CGContext uses a y-up origin; translate the crop's bottom-left corner to (0, 0).
    CGContext::translate_ctm(
        context,
        -(x as f64),
        -((shown_h - y - crop_h) as f64),
    );
    CGContext::translate_ctm(context, shown_w as f64 / 2.0, shown_h as f64 / 2.0);
    // Use exact trig values for 90-degree multiples to prevent rounding drift.
    let (cos, sin) = match orientation.turns {
        1 => (0.0, -1.0),
        2 => (-1.0, 0.0),
        3 => (0.0, 1.0),
        _ => (1.0, 0.0),
    };
    CGContext::concat_ctm(
        context,
        CGAffineTransform {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            tx: 0.0,
            ty: 0.0,
        },
    );
    if orientation.mirrored {
        CGContext::scale_ctm(context, -1.0, 1.0);
    }
    CGContext::draw_image(
        context,
        CGRect::new(
            CGPoint::new(-(stored_w as f64) / 2.0, -(stored_h as f64) / 2.0),
            CGSize::new(stored_w as f64, stored_h as f64),
        ),
        Some(image),
    );
    CGBitmapContextCreateImage(context).ok_or_else(|| "Cannot render image".to_owned())
}

/// Resize a `CGImage` using high-quality Core Graphics resampling, preserving color space and alpha.
pub(crate) fn resize_cg(
    image: &CGImage,
    width: usize,
    height: usize,
) -> Result<CFRetained<CGImage>, String> {
    let (space, info) = bitmap_format(image);
    let (context, _) = make_bitmap_context(width, height, 0, space, info)
        .ok_or("Not enough memory to resize image")?;
    let ctx = Some(&*context);
    CGContext::set_interpolation_quality(ctx, CGInterpolationQuality::High);
    CGContext::draw_image(
        ctx,
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(width as f64, height as f64),
        ),
        Some(image),
    );
    CGBitmapContextCreateImage(ctx).ok_or_else(|| "Cannot resize image".to_owned())
}

/// Convert normalized `crop` to integer pixel bounds clamped to `[1..=width, 1..=height]`.
fn pixel_rect(crop: Option<CropRect>, width: usize, height: usize) -> (usize, usize, usize, usize) {
    let Some(crop) = crop else {
        return (0, 0, width, height);
    };
    let span = |start: f64, extent: f64, whole: usize| {
        if whole == 0 {
            return (0, 0);
        }
        let whole_f = whole as f64;
        let first = (start.clamp(0.0, 1.0) * whole_f).round() as usize;
        let last = ((start + extent).clamp(0.0, 1.0) * whole_f).round() as usize;
        let first = first.min(whole.saturating_sub(1));
        (first, last.clamp(first + 1, whole) - first)
    };
    let (x, w) = span(crop.x, crop.width, width);
    let (y, h) = span(crop.y, crop.height, height);
    (x, y, w, h)
}

/// Encode `image` with preserved metadata, reset orientation (1), and updated dimensions.
fn encode(image: &CGImage, source: &Source, uti: &str, out: &Path) -> Result<(), String> {
    let url = CFURL::from_file_path(out).ok_or("Cannot write to folder")?;
    unsafe {
        let destination = CGImageDestination::with_url(&url, &CFString::from_str(uti), 1, None)
            .ok_or("Cannot create output file")?;
        let properties =
            cleaned_encode_properties(&source.properties, image, Some(QUALITY), true, false);
        destination.add_image(image, Some(properties.as_opaque()));
        if !destination.finalize() {
            return Err("Cannot write output file".to_owned());
        }
    }
    Ok(())
}

/// Copy `source` Image I/O properties for re-encoding `image`, resetting orientation
/// to upright (1), updating EXIF dimensions, and stripping format-specific containers.
pub(crate) fn cleaned_encode_properties(
    source: &CFDictionary<CFString, CFType>,
    image: &CGImage,
    quality: Option<f64>,
    embed_thumbnail: bool,
    progressive_jpeg: bool,
) -> CFRetained<CFMutableDictionary<CFString, CFType>> {
    let (width, height) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let up = CFNumber::new_i32(1);
    let properties = CFMutableDictionary::<CFString, CFType>::with_capacity(0);
    let (keys, values) = source.to_vecs();
    unsafe {
        for (key, value) in keys.iter().zip(values.iter()) {
            let name = key.to_string();
            // The encoder works these out from the image itself; strip container-specific dicts.
            if [
                "PixelWidth",
                "PixelHeight",
                "Depth",
                "ColorModel",
                "ProfileName",
                "HasAlpha",
                "IsIndexed",
                "{PNG}",
                "{JFIF}",
                "{GIF}",
                "{WebP}",
            ]
            .contains(&name.as_str())
            {
                continue;
            }
            if name == kCGImagePropertyTIFFDictionary.to_string() {
                if let Some(tiff) = value_dictionary(value) {
                    let tiff = mutable_copy(&tiff);
                    tiff.set(kCGImagePropertyTIFFOrientation, &up);
                    properties.set(key, &tiff);
                }
                continue;
            }
            if name == kCGImagePropertyExifDictionary.to_string() {
                if let Some(exif) = value_dictionary(value) {
                    let exif = mutable_copy(&exif);
                    exif.set(
                        kCGImagePropertyExifPixelXDimension,
                        &CFNumber::new_i64(width as i64),
                    );
                    exif.set(
                        kCGImagePropertyExifPixelYDimension,
                        &CFNumber::new_i64(height as i64),
                    );
                    properties.set(key, &exif);
                }
                continue;
            }
            properties.set(key, value);
        }
        properties.set(kCGImagePropertyOrientation, &up);
        if let Some(q) = quality {
            properties.set(
                kCGImageDestinationLossyCompressionQuality,
                &CFNumber::new_f64(q),
            );
        }
        properties.set(
            kCGImageDestinationEmbedThumbnail,
            CFBoolean::new(embed_thumbnail),
        );
        if progressive_jpeg {
            let jfif = CFMutableDictionary::<CFString, CFType>::with_capacity(1);
            jfif.set(kCGImagePropertyJFIFIsProgressive, CFBoolean::new(true));
            properties.set(kCGImagePropertyJFIFDictionary, &jfif);
        }
    }
    properties
}

fn value_dictionary(
    value: &CFRetained<CFType>,
) -> Option<CFRetained<CFDictionary<CFString, CFType>>> {
    let dictionary = value.clone().downcast::<CFDictionary>().ok()?;
    Some(unsafe { CFRetained::cast_unchecked(dictionary) })
}

fn mutable_copy(
    dictionary: &CFDictionary<CFString, CFType>,
) -> CFRetained<CFMutableDictionary<CFString, CFType>> {
    let copy = CFMutableDictionary::<CFString, CFType>::with_capacity(0);
    let (keys, values) = dictionary.to_vecs();
    for (key, value) in keys.iter().zip(values.iter()) {
        copy.set(key, value);
    }
    copy
}

fn writable(uti: &str) -> bool {
    static TYPES: OnceLock<Vec<String>> = OnceLock::new();
    TYPES
        .get_or_init(|| unsafe {
            let types: CFRetained<CFArray<CFString>> =
                CFRetained::cast_unchecked(CGImageDestination::type_identifiers());
            types.iter().map(|uti| uti.to_string()).collect()
        })
        .iter()
        .any(|known| known == uti)
}

fn kind_name(uti: &str, path: &Path) -> String {
    match uti {
        "org.webmproject.webp" => "WebP".to_owned(),
        "public.jpeg-xl" => "JPEG XL".to_owned(),
        "public.avif" => "AVIF".to_owned(),
        _ => path
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .unwrap_or_else(|| "Unsupported".to_owned()),
    }
}

/// Atomically replace `path` with `edited`, preserving file metadata and Finder tags.
fn replace_contents(path: &Path, edited: &Path) -> Result<(), String> {
    let (Some(original), Some(edited)) = (NSURL::from_file_path(path), NSURL::from_file_path(edited))
    else {
        return Err("Cannot replace file".to_owned());
    };
    NSFileManager::defaultManager()
        .replaceItemAtURL_withItemAtURL_backupItemName_options_resultingItemURL_error(
            &original,
            &edited,
            None,
            NSFileManagerItemReplacementOptions::empty(),
            None,
        )
        .map_err(|error| reason(&error.localizedDescription().to_string()))
}

/// Temporary directory on the same volume so watchers only see the finished file.
struct Staging {
    directory: PathBuf,
    file: PathBuf,
}

static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

impl Staging {
    fn new(near: &Path) -> Result<Self, String> {
        let manager = NSFileManager::defaultManager();
        let system = NSURL::from_file_path(near).and_then(|url| {
            manager
                .URLForDirectory_inDomain_appropriateForURL_create_error(
                    NSSearchPathDirectory::ItemReplacementDirectory,
                    NSSearchPathDomainMask::UserDomainMask,
                    Some(&url),
                    true,
                )
                .ok()
                .and_then(|url| url.to_file_path())
        });
        let directory = match system {
            Some(directory) => directory,
            None => {
                let folder = near.parent().ok_or("Invalid file path")?;
                let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
                let directory =
                    folder.join(format!(".blickli-edit-{}-{seq}", std::process::id()));
                std::fs::create_dir_all(&directory)
                    .map_err(|_| "Cannot write to folder".to_owned())?;
                directory
            }
        };
        let name = near
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_owned());
        Ok(Self {
            file: directory.join(name),
            directory,
        })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::ImageIoDecoder;
    use blickli_core::ImageDecoder;

    /// Distinct test colors on a 3×2 grid, row-major from top-left.
    const COLOURS: [[u8; 3]; 6] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
        [255, 0, 255],
    ];

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blickli-edit-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rgba_context(width: usize, height: usize) -> CFRetained<CGContext> {
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
        unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                width,
                height,
                8,
                width * 4,
                space.as_deref(),
                CGImageAlphaInfo::NoneSkipLast.0,
            )
        }
        .unwrap()
    }

    /// The test picture as a lossless file of type `uti`.
    fn write_picture(path: &Path, uti: &str) {
        let context = rgba_context(3, 2);
        let bytes = objc2_core_graphics::CGBitmapContextGetData(Some(&context)) as *mut u8;
        for (index, colour) in COLOURS.iter().enumerate() {
            // Bitmap rows run top to bottom in memory.
            let pixel = unsafe { std::slice::from_raw_parts_mut(bytes.add(index * 4), 4) };
            pixel[..3].copy_from_slice(colour);
            pixel[3] = 255;
        }
        let image = CGBitmapContextCreateImage(Some(&context)).unwrap();
        let url = CFURL::from_file_path(path).unwrap();
        unsafe {
            let destination =
                CGImageDestination::with_url(&url, &CFString::from_str(uti), 1, None).unwrap();
            destination.add_image(&image, None);
            assert!(destination.finalize());
        }
    }

    /// Pixels row by row from the top left.
    fn pixels(image: &CGImage) -> (usize, usize, Vec<[u8; 3]>) {
        let (width, height) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
        let context = rgba_context(width, height);
        CGContext::draw_image(
            Some(&context),
            CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(width as f64, height as f64),
            ),
            Some(image),
        );
        let bytes = objc2_core_graphics::CGBitmapContextGetData(Some(&context)) as *const u8;
        let bytes = unsafe { std::slice::from_raw_parts(bytes, width * height * 4) };
        let colours = bytes.chunks(4).map(|p| [p[0], p[1], p[2]]).collect();
        (width, height, colours)
    }

    fn shown(path: &Path) -> (usize, usize, Vec<[u8; 3]>) {
        pixels(&ImageIoDecoder.decode(path, 64).expect("decodes"))
    }

    const R: [u8; 3] = COLOURS[0];
    const G: [u8; 3] = COLOURS[1];
    const B: [u8; 3] = COLOURS[2];
    const Y: [u8; 3] = COLOURS[3];
    const C: [u8; 3] = COLOURS[4];
    const M: [u8; 3] = COLOURS[5];

    /// A fake trash for tests: a folder of their own.
    fn fake_trash(dir: &Path) -> impl Fn(&Path) -> Result<PathBuf, String> + '_ {
        move |path: &Path| {
            let landed = dir.join(path.file_name().unwrap());
            std::fs::rename(path, &landed).map_err(|e| e.to_string())?;
            Ok(landed)
        }
    }

    #[test]
    fn a_quarter_turn_moves_every_pixel() {
        // R G B      Y R
        // Y C M  ->  C G
        //            M B
        let image = {
            let dir = scratch("turn-src");
            let path = dir.join("a.png");
            write_picture(&path, "public.png");
            render(&open(&path).unwrap(), Orientation::from_view(90, false), None).unwrap()
        };
        assert_eq!(pixels(&image), (2, 3, vec![Y, R, C, G, M, B]));
    }

    #[test]
    fn a_flip_then_a_crop_takes_the_right_corner() {
        // Flipped: B G R / M C Y. The right two columns of the top row.
        let dir = scratch("crop-src");
        let path = dir.join("a.png");
        write_picture(&path, "public.png");
        let crop = CropRect {
            x: 1.0 / 3.0,
            y: 0.0,
            width: 2.0 / 3.0,
            height: 0.5,
        };
        let image = render(&open(&path).unwrap(), Orientation::from_view(0, true), Some(crop)).unwrap();
        assert_eq!(pixels(&image), (2, 1, vec![G, R]));
    }

    #[test]
    fn a_png_rotation_only_rewrites_the_tag() {
        let dir = scratch("png-rotate");
        let path = dir.join("a.png");
        write_picture(&path, "public.png");
        let edit = rotate_file_with(&path, Orientation::from_view(90, false), &|_| {
            panic!("a lossless rotation never trashes the original")
        })
        .unwrap();
        assert_eq!(
            edit,
            Edit::Oriented {
                path: path.clone(),
                before: 1,
                after: 6
            }
        );
        assert_eq!(shown(&path), (2, 3, vec![Y, R, C, G, M, B]));
        // Undo writes the old tag back.
        write_orientation(&path, 1).unwrap();
        assert_eq!(shown(&path), (3, 2, COLOURS.to_vec()));
        // Nothing was left behind in the folder.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_bmp_rotation_redraws_and_keeps_the_original() {
        let dir = scratch("bmp-rotate");
        let trash = scratch("bmp-trash");
        let path = dir.join("a.bmp");
        write_picture(&path, "com.microsoft.bmp");
        let edit = rotate_file_with(&path, Orientation::from_view(180, false), &fake_trash(&trash))
            .unwrap();
        let Edit::Replaced { current, stashed, home } = edit else {
            panic!("a BMP has no orientation tag to set");
        };
        assert_eq!((&current, &home), (&path, &path));
        assert_eq!(shown(&path), (3, 2, vec![M, C, Y, B, G, R]));
        assert_eq!(shown(&stashed), (3, 2, COLOURS.to_vec()));
    }

    #[test]
    fn a_crop_saves_a_copy_or_takes_the_originals_place() {
        let dir = scratch("crop-file");
        let trash = scratch("crop-trash");
        let path = dir.join("a.png");
        write_picture(&path, "public.png");
        let bottom_row = CropRect {
            x: 0.0,
            y: 0.5,
            width: 1.0,
            height: 0.5,
        };
        let Cropped::Copy(copy) =
            crop_file_with(&path, Orientation::UP, bottom_row, false, &fake_trash(&trash)).unwrap()
        else {
            panic!("asked for a copy");
        };
        assert_eq!(copy, dir.join("a cropped.png"));
        assert_eq!(shown(&copy), (3, 1, vec![Y, C, M]));
        assert_eq!(shown(&path), (3, 2, COLOURS.to_vec()));

        let Cropped::Replaced(Edit::Replaced { current, stashed, .. }) =
            crop_file_with(&path, Orientation::UP, bottom_row, true, &fake_trash(&trash)).unwrap()
        else {
            panic!("asked to replace");
        };
        assert_eq!(current, path);
        assert_eq!(shown(&path), (3, 1, vec![Y, C, M]));
        assert_eq!(shown(&stashed), (3, 2, COLOURS.to_vec()));
    }

    #[test]
    fn a_camera_jpeg_on_its_side_keeps_its_metadata_and_comes_out_upright() {
        // Stored 4×2 with orientation 6, so shown 2×4.
        let dir = scratch("jpeg-6");
        let trash = scratch("jpeg-6-trash");
        let path = dir.join("camera.jpg");
        std::fs::write(&path, include_bytes!("../testdata/rotated-6.jpg")).unwrap();

        // A quarter turn more: the tag goes from 6 to 3, nothing re-encoded.
        let before = std::fs::read(&path).unwrap().len();
        let edit = rotate_file(&path, Orientation::from_view(90, false)).unwrap();
        assert_eq!(
            edit,
            Edit::Oriented {
                path: path.clone(),
                before: 6,
                after: 3
            }
        );
        assert_eq!(crate::decoder::image_dimensions(&path), Some((4, 2)));
        assert!(std::fs::read(&path).unwrap().len().abs_diff(before) < 256);
        write_orientation(&path, 6).unwrap();

        // A crop bakes the orientation in and says so.
        let all = CropRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        let Cropped::Copy(copy) =
            crop_file_with(&path, Orientation::UP, all, false, &fake_trash(&trash)).unwrap()
        else {
            panic!("asked for a copy");
        };
        assert_eq!(crate::decoder::image_dimensions(&copy), Some((2, 4)));
        assert_eq!(open(&copy).unwrap().orientation, Orientation::UP);
    }

    #[test]
    fn crops_round_to_whole_pixels_inside_the_picture() {
        let whole = |x, y, width, height| Some(CropRect { x, y, width, height });
        assert_eq!(pixel_rect(None, 30, 20), (0, 0, 30, 20));
        assert_eq!(pixel_rect(whole(0.1, 0.25, 0.5, 0.5), 30, 20), (3, 5, 15, 10));
        // Never empty, never outside.
        assert_eq!(pixel_rect(whole(0.99, 0.0, 0.0, 1.0), 30, 20), (29, 0, 1, 20));
        assert_eq!(pixel_rect(whole(-0.5, 0.5, 9.0, 9.0), 30, 20), (0, 10, 30, 10));
    }

    #[test]
    fn upscale_saves_a_copy_or_replaces_with_trash_backup() {
        let dir = scratch("upscale-file");
        let trash = scratch("upscale-trash");
        let path = dir.join("a.png");
        write_picture(&path, "public.png");
        let big = resize_cg(&decode_oriented(&path, Orientation::UP).unwrap(), 12, 8).unwrap();

        let Cropped::Copy(copy) =
            save_upscaled_with(&path, &big, false, &fake_trash(&trash)).unwrap()
        else {
            panic!("expected a copy");
        };
        assert_eq!(copy, dir.join("a upscaled.png"));
        assert_eq!(crate::decoder::image_dimensions(&copy), Some((12, 8)));

        let Cropped::Replaced(Edit::Replaced { current, stashed, .. }) =
            save_upscaled_with(&path, &big, true, &fake_trash(&trash)).unwrap()
        else {
            panic!("expected replacement");
        };
        assert_eq!(current, path);
        assert_eq!(crate::decoder::image_dimensions(&path), Some((12, 8)));
        assert_eq!(crate::decoder::image_dimensions(&stashed), Some((3, 2)));
    }
}
