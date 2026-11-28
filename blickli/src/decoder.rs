//! Image I/O backend for `ImageDecoder`. Runs on worker threads; only finished
//! `CGImage` values cross threads.

use std::path::Path;
use std::time::SystemTime;

use blickli_core::{FileDates, ImageDecoder};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL,
};
use objc2_core_graphics::CGImage;
use objc2_image_io::{
    kCGImagePropertyColorModel, kCGImagePropertyExifApertureValue,
    kCGImagePropertyExifDateTimeOriginal, kCGImagePropertyExifDictionary,
    kCGImagePropertyExifExposureTime, kCGImagePropertyExifFNumber, kCGImagePropertyExifFocalLength,
    kCGImagePropertyExifISOSpeedRatings, kCGImagePropertyExifLensModel,
    kCGImagePropertyGIFDelayTime, kCGImagePropertyGIFDictionary,
    kCGImagePropertyGIFUnclampedDelayTime, kCGImagePropertyGPSDictionary,
    kCGImagePropertyGPSLatitude, kCGImagePropertyGPSLatitudeRef, kCGImagePropertyGPSLongitude,
    kCGImagePropertyGPSLongitudeRef, kCGImagePropertyOrientation, kCGImagePropertyPixelHeight,
    kCGImagePropertyPixelWidth, kCGImagePropertyProfileName, kCGImagePropertyTIFFDictionary,
    kCGImagePropertyTIFFMake, kCGImagePropertyTIFFModel, kCGImagePropertyWebPDelayTime,
    kCGImagePropertyWebPDictionary, kCGImagePropertyWebPUnclampedDelayTime,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailFromImageIfAbsent,
    kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceShouldCache,
    kCGImageSourceShouldCacheImmediately, kCGImageSourceThumbnailMaxPixelSize, CGImageSource,
};

/// Oriented pixel dimensions read from the image header without decoding pixels.
pub fn image_dimensions(path: &Path) -> Option<(i64, i64)> {
    let url = CFURL::from_file_path(path)?;
    unsafe {
        let src = CGImageSource::with_url(&url, None)?;
        source_dimensions(&src)
    }
}

fn source_dimensions(src: &CGImageSource) -> Option<(i64, i64)> {
    unsafe {
        let props = src.properties_at_index(src.primary_image_index(), None)?;
        let props = CFRetained::cast_unchecked(props);
        shown_dimensions(&props)
    }
}

/// Swap width and height for EXIF orientations 5–8 to match decoded output.
fn shown_dimensions(props: &CFDictionary<CFString, CFType>) -> Option<(i64, i64)> {
    let width = number(props, unsafe { kCGImagePropertyPixelWidth })?.as_i64()?;
    let height = number(props, unsafe { kCGImagePropertyPixelHeight })?.as_i64()?;
    let on_its_side = number(props, unsafe { kCGImagePropertyOrientation })
        .and_then(|n| n.as_i64())
        .is_some_and(|orientation| (5..=8).contains(&orientation));
    Some(if on_its_side {
        (height, width)
    } else {
        (width, height)
    })
}

/// Key-value metadata rows from the image header for the Info panel and EXIF overlay.
pub fn image_info(path: &Path) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let Some(url) = CFURL::from_file_path(path) else {
        return rows;
    };
    unsafe {
        let Some(src) = CGImageSource::with_url(&url, None) else {
            return rows;
        };
        let Some(props) = src.properties_at_index(src.primary_image_index(), None) else {
            return rows;
        };
        let props: CFRetained<CFDictionary<CFString, CFType>> = CFRetained::cast_unchecked(props);

        if let Some((width, height)) = shown_dimensions(&props) {
            rows.push(("Dimensions".into(), format!("{width} × {height}")));
        }
        if let Some(model) = text(&props, kCGImagePropertyColorModel) {
            rows.push(("Color model".into(), model));
        }
        if let Some(profile) = text(&props, kCGImagePropertyProfileName) {
            rows.push(("Color profile".into(), profile));
        }

        if let Some(tiff) = sub_dictionary(&props, kCGImagePropertyTIFFDictionary) {
            let make = text(&tiff, kCGImagePropertyTIFFMake);
            let model = text(&tiff, kCGImagePropertyTIFFModel);
            let camera = match (make, model) {
                (Some(make), Some(model)) if model.starts_with(&make) => model,
                (Some(make), Some(model)) => format!("{make} {model}"),
                (Some(make), None) => make,
                (None, Some(model)) => model,
                (None, None) => String::new(),
            };
            if !camera.is_empty() {
                rows.push(("Camera".into(), camera));
            }
        }

        if let Some(exif) = sub_dictionary(&props, kCGImagePropertyExifDictionary) {
            if let Some(lens) = text(&exif, kCGImagePropertyExifLensModel) {
                rows.push(("Lens".into(), lens));
            }
            if let Some(taken) = text(&exif, kCGImagePropertyExifDateTimeOriginal) {
                let shown = crate::exif_date_text(&taken).unwrap_or(taken);
                rows.push(("Taken".into(), shown));
            }
            if let Some(seconds) =
                number(&exif, kCGImagePropertyExifExposureTime).and_then(|n| n.as_f64())
            {
                rows.push(("Exposure".into(), exposure(seconds)));
            }
            let aperture = number(&exif, kCGImagePropertyExifFNumber)
                .and_then(|n| n.as_f64())
                .or_else(|| {
                    number(&exif, kCGImagePropertyExifApertureValue)
                        .and_then(|n| n.as_f64())
                        .map(|av| 2.0_f64.powf(av / 2.0))
                });
            if let Some(aperture) = aperture {
                rows.push(("Aperture".into(), format!("f/{aperture:.1}")));
            }
            if let Some(iso) = first_number(&exif, kCGImagePropertyExifISOSpeedRatings) {
                rows.push(("ISO".into(), format!("{iso}")));
            }
            if let Some(focal) =
                number(&exif, kCGImagePropertyExifFocalLength).and_then(|n| n.as_f64())
            {
                rows.push(("Focal length".into(), format!("{focal:.0} mm")));
            }
        }

        if let Some(gps) = sub_dictionary(&props, kCGImagePropertyGPSDictionary) {
            let lat = number(&gps, kCGImagePropertyGPSLatitude).and_then(|n| n.as_f64());
            let lon = number(&gps, kCGImagePropertyGPSLongitude).and_then(|n| n.as_f64());
            if let (Some(lat), Some(lon)) = (lat, lon) {
                let lat_ref = text(&gps, kCGImagePropertyGPSLatitudeRef).unwrap_or_default();
                let lon_ref = text(&gps, kCGImagePropertyGPSLongitudeRef).unwrap_or_default();
                rows.push((
                    "Location".into(),
                    format!("{lat:.5}° {lat_ref}, {lon:.5}° {lon_ref}"),
                ));
            }
        }
    }
    rows
}

fn exposure(seconds: f64) -> String {
    if seconds <= 0.0 {
        return String::new();
    }
    if seconds >= 1.0 {
        format!("{seconds:.1} s")
    } else {
        format!("1/{:.0} s", 1.0 / seconds)
    }
}

fn text(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<String> {
    let value = dict.get(key)?.downcast::<CFString>().ok()?.to_string();
    (!value.trim().is_empty()).then_some(value)
}

fn number(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<CFRetained<CFNumber>> {
    dict.get(key)?.downcast::<CFNumber>().ok()
}

/// ISO arrives as an array of one number.
fn first_number(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<i64> {
    let value = dict.get(key)?;
    if let Ok(number) = value.clone().downcast::<CFNumber>() {
        return number.as_i64();
    }
    let array = value.downcast::<objc2_core_foundation::CFArray>().ok()?;
    let array: CFRetained<objc2_core_foundation::CFArray<CFType>> =
        unsafe { CFRetained::cast_unchecked(array) };
    array
        .iter()
        .next()
        .and_then(|first| first.downcast::<CFNumber>().ok())
        .and_then(|number| number.as_i64())
}

fn sub_dictionary(
    dict: &CFDictionary<CFString, CFType>,
    key: &CFString,
) -> Option<CFRetained<CFDictionary<CFString, CFType>>> {
    let value = dict.get(key)?.downcast::<CFDictionary>().ok()?;
    Some(unsafe { CFRetained::cast_unchecked(value) })
}

pub struct ImageIoDecoder;

/// EXIF capture date from the header, no decode.
fn exif_date(path: &Path) -> Option<SystemTime> {
    let url = CFURL::from_file_path(path)?;
    unsafe {
        let src = CGImageSource::with_url(&url, None)?;
        let props = src.properties_at_index(src.primary_image_index(), None)?;
        let props: CFRetained<CFDictionary<CFString, CFType>> = CFRetained::cast_unchecked(props);
        let exif = sub_dictionary(&props, kCGImagePropertyExifDictionary)?;
        let value = text(&exif, kCGImagePropertyExifDateTimeOriginal)?;
        blickli_core::parse_exif_datetime(&value)
    }
}

/// Finder's "date added".
fn date_added(path: &Path) -> Option<SystemTime> {
    use objc2_foundation::{NSArray, NSURLAddedToDirectoryDateKey, NSURL};
    let url = NSURL::from_file_path(path)?;
    let keys = NSArray::from_slice(&[unsafe { NSURLAddedToDirectoryDateKey }]);
    let values = url.resourceValuesForKeys_error(&keys).ok()?;
    let date = values.objectForKey(unsafe { NSURLAddedToDirectoryDateKey })?;
    let date = date.downcast::<objc2_foundation::NSDate>().ok()?;
    let secs = date.timeIntervalSince1970();
    std::time::Duration::try_from_secs_f64(secs)
        .ok()
        .map(|d| std::time::UNIX_EPOCH + d)
}

fn open_source_uncached(url: &CFURL) -> Option<CFRetained<CGImageSource>> {
    unsafe {
        let no: &CFBoolean = CFBoolean::new(false);
        let opts: CFRetained<CFDictionary<CFString, CFType>> =
            CFDictionary::from_slices(&[kCGImageSourceShouldCache], &[no.as_ref()]);
        CGImageSource::with_url(url, Some(opts.as_ref()))
    }
}

/// A capped, oriented decode from an open `CGImageSource`. `from_image` is
/// `Always` for a real decode and `IfAbsent` to accept the file's embedded preview.
fn thumbnail_from_source(
    src: &CGImageSource,
    max_px: i32,
    from_image: &'static CFString,
) -> Option<CFRetained<CGImage>> {
    unsafe {
        let idx = src.primary_image_index();
        let yes: &CFBoolean = CFBoolean::new(true);
        let size = CFNumber::new_i32(max_px);
        let opts: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
            &[
                from_image,
                kCGImageSourceThumbnailMaxPixelSize,
                kCGImageSourceCreateThumbnailWithTransform,
                kCGImageSourceShouldCacheImmediately,
            ],
            &[yes.as_ref(), size.as_ref(), yes.as_ref(), yes.as_ref()],
        );
        src.thumbnail_at_index(idx, Some(opts.as_ref()))
    }
}

impl ImageDecoder for ImageIoDecoder {
    type Image = CFRetained<CGImage>;

    fn decode(&self, path: &Path, max_px: i32) -> Option<Self::Image> {
        objc2::rc::autoreleasepool(|_| {
            let url = CFURL::from_file_path(path)?;
            let src = open_source_uncached(&url)?;
            thumbnail_from_source(&src, max_px, unsafe {
                kCGImageSourceCreateThumbnailFromImageAlways
            })
        })
    }

    /// Use the embedded thumbnail if it is at least half `max_px` (or already full-size);
    /// otherwise decode from the same open `CGImageSource`.
    fn decode_thumb(&self, path: &Path, max_px: i32) -> Option<Self::Image> {
        objc2::rc::autoreleasepool(|_| {
            let url = CFURL::from_file_path(path)?;
            let src = open_source_uncached(&url)?;
            let fast = thumbnail_from_source(&src, max_px, unsafe {
                kCGImageSourceCreateThumbnailFromImageIfAbsent
            });
            if let Some(image) = &fast {
                let long_edge = CGImage::width(Some(image)).max(CGImage::height(Some(image)));
                if long_edge * 2 >= max_px as usize {
                    return fast;
                }
                // Avoid double-decoding small images that are already at native size.
                if let Some((w, h)) = source_dimensions(&src) {
                    if long_edge >= w.max(h).max(0) as usize {
                        return fast;
                    }
                }
            }
            thumbnail_from_source(&src, max_px, unsafe {
                kCGImageSourceCreateThumbnailFromImageAlways
            })
            .or(fast)
        })
    }

    fn file_dates(&self, path: &Path, want_exif: bool) -> FileDates {
        objc2::rc::autoreleasepool(|_| FileDates {
            exif_date: want_exif.then(|| exif_date(path)).flatten(),
            added: date_added(path),
            exif_loaded: want_exif,
            added_loaded: true,
        })
    }

    /// Decode all frames of an animated GIF/WebP up front; falls back to still if over budget.
    fn decode_frames(&self, path: &Path) -> Option<Vec<(Self::Image, f64)>> {
        if !is_animated_ext(path) {
            return None;
        }
        objc2::rc::autoreleasepool(|_| {
            let url = CFURL::from_file_path(path)?;
            unsafe {
                let src = CGImageSource::with_url(&url, None)?;
                let count = src.count();
                if count < 2 {
                    return None;
                }
                const FRAME_BUDGET_BYTES: usize = 256 * 1024 * 1024;
                let (w, h) = source_dimensions(&src)?;
                let estimated_pixels = usize::try_from(w)
                    .ok()?
                    .checked_mul(usize::try_from(h).ok()?)?;
                let frame_table = count.checked_mul(std::mem::size_of::<(Self::Image, f64)>())?;
                let mut remaining = FRAME_BUDGET_BYTES.checked_sub(frame_table)?;
                if estimated_pixels.checked_mul(4)?.checked_mul(count)? > remaining {
                    return None;
                }
                // Decode now, on this thread, not lazily at first draw.
                let yes: &CFBoolean = CFBoolean::new(true);
                let decode_now: CFRetained<CFDictionary<CFString, CFType>> =
                    CFDictionary::from_slices(
                        &[kCGImageSourceShouldCacheImmediately],
                        &[yes.as_ref()],
                    );
                let mut frames = Vec::with_capacity(count);
                for index in 0..count {
                    let image = src.image_at_index(index, Some(decode_now.as_ref()))?;
                    remaining = remaining.checked_sub(crate::image_cost(&image))?;
                    frames.push((image, frame_delay(&src, index)));
                }
                Some(frames)
            }
        })
    }
}

pub fn is_animated_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gif") || ext.eq_ignore_ascii_case("webp"))
}

/// A frame's delay in seconds; zero or missing means 0.1 s, as in
/// browsers.
fn frame_delay(src: &CGImageSource, index: usize) -> f64 {
    let delay = unsafe {
        src.properties_at_index(index, None).and_then(|props| {
            let props: CFRetained<CFDictionary<CFString, CFType>> =
                CFRetained::cast_unchecked(props);
            if let Some(gif) = sub_dictionary(&props, kCGImagePropertyGIFDictionary) {
                number(&gif, kCGImagePropertyGIFUnclampedDelayTime)
                    .and_then(|n| n.as_f64())
                    .filter(|&d| d > 0.0)
                    .or_else(|| {
                        number(&gif, kCGImagePropertyGIFDelayTime).and_then(|n| n.as_f64())
                    })
            } else if let Some(webp) = sub_dictionary(&props, kCGImagePropertyWebPDictionary) {
                number(&webp, kCGImagePropertyWebPUnclampedDelayTime)
                    .and_then(|n| n.as_f64())
                    .filter(|&d| d > 0.0)
                    .or_else(|| {
                        number(&webp, kCGImagePropertyWebPDelayTime).and_then(|n| n.as_f64())
                    })
            } else {
                None
            }
        })
    };
    match delay {
        Some(d) if d > 0.011 => d,
        _ => 0.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_gifs_decode_every_frame() {
        let path = std::env::temp_dir().join("blickli-anim-selfcheck.gif");
        std::fs::write(&path, crate::e2e::ANIMATED_GIF).unwrap();
        let frames = ImageIoDecoder.decode_frames(&path).expect("animated");
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|(_, delay)| *delay > 0.0));
        assert!(ImageIoDecoder
            .decode_frames(Path::new("/x/a.jpg"))
            .is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_photo_on_its_side_reports_its_shown_size() {
        // Stored 4×2 with EXIF orientation 6: shown 2×4.
        let path = std::env::temp_dir().join("blickli-rotated-selfcheck.jpg");
        std::fs::write(&path, include_bytes!("../testdata/rotated-6.jpg")).unwrap();
        assert_eq!(image_dimensions(&path), Some((2, 4)));
        let decoded = ImageIoDecoder.decode(&path, 100).expect("decodes");
        let size = (
            CGImage::width(Some(&decoded)),
            CGImage::height(Some(&decoded)),
        );
        assert_eq!(size, (2, 4));
        let _ = std::fs::remove_file(&path);
    }
}
