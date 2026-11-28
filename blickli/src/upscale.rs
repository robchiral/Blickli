//! Local Core AI (`.aimodel`) image upscaling with overlapping tiled inference.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow, CGBitmapContextGetData,
    CGColorSpace, CGImage, CGImageAlphaInfo,
};

use crate::edit;

/// Hard cap on upscaled pixel count (~64 MP) to avoid OOM aborts on huge inputs.
const MAX_UPSCALE_PIXELS: usize = 64_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpscaleScale {
    X2,
    #[default]
    X4,
}

impl UpscaleScale {
    pub const fn factor(self) -> usize {
        match self {
            UpscaleScale::X2 => 2,
            UpscaleScale::X4 => 4,
        }
    }

    pub const fn tag(self) -> isize {
        match self {
            UpscaleScale::X2 => 2,
            UpscaleScale::X4 => 4,
        }
    }

    pub const fn from_tag(tag: isize) -> Self {
        match tag {
            2 => UpscaleScale::X2,
            _ => UpscaleScale::X4,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            UpscaleScale::X2 => "2×",
            UpscaleScale::X4 => "4×",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpscaleSaveMode {
    #[default]
    Ask,
    Copy,
    Replace,
}

impl UpscaleSaveMode {
    pub const fn tag(self) -> isize {
        match self {
            UpscaleSaveMode::Ask => 0,
            UpscaleSaveMode::Copy => 1,
            UpscaleSaveMode::Replace => 2,
        }
    }

    pub const fn from_tag(tag: isize) -> Self {
        match tag {
            1 => UpscaleSaveMode::Copy,
            2 => UpscaleSaveMode::Replace,
            _ => UpscaleSaveMode::Ask,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    PreparingModel,
    Tile { done: usize, total: usize },
}

pub struct UpscaleResult {
    pub original: CFRetained<CGImage>,
    pub upscaled_native: CFRetained<CGImage>,
    pub native_scale: usize,
}

/// Find a Core AI upscaler (`.aimodel`) in `folder` or one subfolder level down (prefers SwinIR).
pub fn find_model(folder: &Path) -> Result<PathBuf, String> {
    if !folder.is_dir() {
        return Err("the selected models folder is missing".to_owned());
    }
    let mut candidates = Vec::new();
    collect_models(folder, 0, &mut candidates)?;
    if candidates.is_empty() {
        return Err(format!(
            "no Core AI model (.aimodel) found in {}",
            blickli_core::messages::folder_label(folder)
        ));
    }
    candidates.sort_by_key(|a| model_rank(a));
    Ok(candidates.remove(0))
}

/// Short human-readable summary of the detected model for Settings.
pub fn model_summary(folder: Option<&Path>) -> String {
    let Some(folder) = folder else {
        return "Choose a folder containing a Core AI upscaler (.aimodel), such as SwinIR-L.".to_owned();
    };
    match find_model(folder) {
        Ok(path) => format!("Using {}", crate::info::file_name(&path)),
        Err(why) => format!("No usable model: {why}"),
    }
}

fn collect_models(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|_| "the models folder can't be read".to_owned())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if is_aimodel(&path) {
            out.push(path);
        } else if depth < 1 && path.is_dir() {
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if !hidden {
                let _ = collect_models(&path, depth + 1, out);
            }
        }
    }
    Ok(())
}

fn is_aimodel(path: &Path) -> bool {
    path.is_dir()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("aimodel"))
}

fn model_rank(path: &Path) -> (u8, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let family = if name.contains("swinir") {
        0
    } else if name.contains("esrgan") || name.contains("upscale") {
        1
    } else {
        2
    };
    (family, name)
}

extern "C" {
    fn blickli_coreai_load(
        path: *const c_char,
        out_tile_w: *mut isize,
        out_tile_h: *mut isize,
        out_scale: *mut isize,
        out_err: *mut *mut c_char,
    ) -> *mut c_void;
    fn blickli_coreai_predict(
        handle: *mut c_void,
        in_rgb: *const u8,
        out_rgb: *mut f32,
        out_max: *mut f32,
        out_err: *mut *mut c_char,
    ) -> bool;
    fn blickli_coreai_free(handle: *mut c_void);
    fn blickli_coreai_free_error(ptr: *mut c_char);
}

fn take_c_error(ptr: *mut c_char, fallback: &str) -> String {
    if ptr.is_null() {
        return fallback.to_owned();
    }
    let msg = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { blickli_coreai_free_error(ptr) };
    msg
}

/// Loaded Core AI model cached on the upscale worker thread.
pub struct LoadedModel {
    pub source_path: PathBuf,
    handle: *mut c_void,
    tile_w: usize,
    tile_h: usize,
    scale: usize,
}

impl Drop for LoadedModel {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { blickli_coreai_free(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

impl LoadedModel {
    pub fn load(source_path: &Path) -> Result<Self, String> {
        let c_path = CString::new(source_path.to_string_lossy().as_bytes())
            .map_err(|_| "model path contains an invalid NUL byte".to_owned())?;
        let mut tile_w: isize = 0;
        let mut tile_h: isize = 0;
        let mut scale: isize = 0;
        let mut err: *mut c_char = std::ptr::null_mut();
        let handle = unsafe {
            blickli_coreai_load(
                c_path.as_ptr(),
                &mut tile_w,
                &mut tile_h,
                &mut scale,
                &mut err,
            )
        };
        if handle.is_null() {
            return Err(format!(
                "failed to load Core AI model: {}",
                take_c_error(err, "unknown error")
            ));
        }
        Ok(Self {
            source_path: source_path.to_path_buf(),
            handle,
            tile_w: tile_w.max(1) as usize,
            tile_h: tile_h.max(1) as usize,
            scale: scale.max(1) as usize,
        })
    }

    fn predict_tile_into(&self, tile_rgb: &[u8], out_rgb: &mut [f32]) -> Result<f32, String> {
        let mut tile_max: f32 = 0.0;
        let mut err: *mut c_char = std::ptr::null_mut();
        let ok = unsafe {
            blickli_coreai_predict(
                self.handle,
                tile_rgb.as_ptr(),
                out_rgb.as_mut_ptr(),
                &mut tile_max,
                &mut err,
            )
        };
        if !ok {
            return Err(format!(
                "Core AI inference failed: {}",
                take_c_error(err, "unknown error")
            ));
        }
        Ok(tile_max)
    }

    /// Upscale an upright `CGImage` using overlapping tiles and feather blending.
    pub fn upscale_image(
        &self,
        source: &CFRetained<CGImage>,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<UpscaleResult, String> {
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        // Check output pixel cap before extracting and copying source pixels.
        let src_w = CGImage::width(Some(source));
        let src_h = CGImage::height(Some(source));
        if src_w == 0 || src_h == 0 {
            return Err("image has zero size".to_owned());
        }
        let tile_scale = self.scale;
        let out_w = src_w
            .checked_mul(tile_scale)
            .ok_or("upscaled image is too large")?;
        let out_h = src_h
            .checked_mul(tile_scale)
            .ok_or("upscaled image is too large")?;
        let pixels = out_w
            .checked_mul(out_h)
            .ok_or("upscaled image is too large")?;
        if pixels > MAX_UPSCALE_PIXELS {
            return Err(format!("upscaled size ({out_w}×{out_h}) is too large"));
        }

        let ExtractedImage {
            width: src_w,
            height: src_h,
            rgb: src_rgb,
            alpha: src_alpha,
            color_space,
        } = extract_rgba(source)?;
        let (tile_w, tile_h) = (self.tile_w, self.tile_h);
        let overlap_x = choose_overlap(tile_w);
        let overlap_y = choose_overlap(tile_h);
        let xs = tile_starts(src_w, tile_w, overlap_x);
        let ys = tile_starts(src_h, tile_h, overlap_y);
        let total_tiles = xs.len() * ys.len();
        let pred_w = tile_w * tile_scale;
        let pred_h = tile_h * tile_scale;

        let mut tile_rgb = vec![0u8; tile_w * tile_h * 3];
        let mut pred_rgb = vec![0.0f32; pred_w * pred_h * 3];

        on_progress(Progress::Tile {
            done: 0,
            total: total_tiles,
        });

        // Single-tile fast path skips allocating accum_rgb and accum_w.
        if total_tiles == 1 {
            if cancel.load(Ordering::Acquire) {
                return Err("cancelled".to_owned());
            }
            extract_padded_tile_into(&src_rgb, src_w, src_h, 0, 0, tile_w, tile_h, &mut tile_rgb);
            drop(src_rgb);
            let tile_max = objc2::rc::autoreleasepool(|_| {
                self.predict_tile_into(&tile_rgb, &mut pred_rgb)
            })?;
            drop(tile_rgb);
            on_progress(Progress::Tile { done: 1, total: 1 });
            let alpha = src_alpha
                .as_deref()
                .map(|plane| resample_alpha_plane(plane, src_w, src_h, out_w, out_h));
            let value_scale = if tile_max > 8.0 { 1.0 / 255.0 } else { 1.0 };
            let upscaled_native = build_cg_image_with(
                out_w,
                out_h,
                alpha.as_deref(),
                color_space,
                |x, y| {
                    let s = (y * pred_w + x) * 3;
                    (
                        pred_rgb[s] * value_scale,
                        pred_rgb[s + 1] * value_scale,
                        pred_rgb[s + 2] * value_scale,
                    )
                },
            )?;
            return Ok(UpscaleResult {
                original: source.clone(),
                upscaled_native,
                native_scale: tile_scale,
            });
        }

        let rgb_len = pixels.checked_mul(3).ok_or("upscaled image is too large")?;
        let mut accum_rgb = Vec::new();
        accum_rgb
            .try_reserve_exact(rgb_len)
            .map_err(|_| "not enough memory for upscaled image".to_owned())?;
        accum_rgb.resize(rgb_len, 0.0f32);
        let mut accum_w = Vec::new();
        accum_w
            .try_reserve_exact(pixels)
            .map_err(|_| "not enough memory for upscaled image".to_owned())?;
        accum_w.resize(pixels, 0.0f32);
        let mut wx_buf = Vec::with_capacity(pred_w);
        let mut done = 0usize;
        let mut max_seen = 0.0f32;

        for (yi, &ty) in ys.iter().enumerate() {
            let has_top = yi > 0;
            let has_bottom = yi + 1 < ys.len();
            for (xi, &tx) in xs.iter().enumerate() {
                if cancel.load(Ordering::Acquire) {
                    return Err("cancelled".to_owned());
                }
                let has_left = xi > 0;
                let has_right = xi + 1 < xs.len();

                let valid_w = (src_w - tx).min(tile_w);
                let valid_h = (src_h - ty).min(tile_h);
                extract_padded_tile_into(
                    &src_rgb,
                    src_w,
                    src_h,
                    tx,
                    ty,
                    tile_w,
                    tile_h,
                    &mut tile_rgb,
                );

                let tile_max = objc2::rc::autoreleasepool(|_| {
                    self.predict_tile_into(&tile_rgb, &mut pred_rgb)
                })?;
                max_seen = max_seen.max(tile_max);

                let dst_x = tx * tile_scale;
                let dst_y = ty * tile_scale;
                let copy_w = valid_w * tile_scale;
                let copy_h = valid_h * tile_scale;
                let feather_x = (overlap_x * tile_scale).min(copy_w / 2);
                let feather_y = (overlap_y * tile_scale).min(copy_h / 2);

                blend_tile(
                    &pred_rgb,
                    pred_w,
                    copy_w,
                    copy_h,
                    &mut accum_rgb,
                    &mut accum_w,
                    &mut wx_buf,
                    out_w,
                    dst_x,
                    dst_y,
                    has_left,
                    has_right,
                    has_top,
                    has_bottom,
                    feather_x,
                    feather_y,
                );

                done += 1;
                on_progress(Progress::Tile {
                    done,
                    total: total_tiles,
                });
            }
        }

        // Release source and tile buffers before allocating the alpha plane and final bitmap.
        drop(src_rgb);
        drop(tile_rgb);
        drop(pred_rgb);
        let alpha = src_alpha
            .as_deref()
            .map(|plane| resample_alpha_plane(plane, src_w, src_h, out_w, out_h));
        let value_scale = if max_seen > 8.0 { 1.0 / 255.0 } else { 1.0 };
        let upscaled_native = build_cg_image_with(
            out_w,
            out_h,
            alpha.as_deref(),
            color_space,
            |x, y| {
                let p = y * out_w + x;
                let inv_w = accum_w[p].max(1e-6).recip() * value_scale;
                let s = p * 3;
                (
                    accum_rgb[s] * inv_w,
                    accum_rgb[s + 1] * inv_w,
                    accum_rgb[s + 2] * inv_w,
                )
            },
        )?;

        Ok(UpscaleResult {
            original: source.clone(),
            upscaled_native,
            native_scale: tile_scale,
        })
    }
}

/// Produce the final output `CGImage` for `scale` from a `native_scale` image.
/// When `scale == X2` and `native_scale == 4`, downsamples 0.5× with high-quality interpolation.
pub fn image_for_scale(
    upscaled_native: &CFRetained<CGImage>,
    native_scale: usize,
    scale: UpscaleScale,
) -> Result<CFRetained<CGImage>, String> {
    let target = scale.factor();
    if target == native_scale || native_scale == 0 {
        return Ok(upscaled_native.clone());
    }
    let src_w = CGImage::width(Some(upscaled_native));
    let src_h = CGImage::height(Some(upscaled_native));
    let dst_w = ((src_w * target + native_scale / 2) / native_scale).max(1);
    let dst_h = ((src_h * target + native_scale / 2) / native_scale).max(1);
    edit::resize_cg(upscaled_native, dst_w, dst_h)
}

// --- Image pixel extraction, tiling, blending, and reconstruction ---

struct ExtractedImage {
    width: usize,
    height: usize,
    /// Straight RGB bytes, length `width * height * 3`.
    rgb: Vec<u8>,
    /// Straight alpha bytes if source has transparency, length `width * height`.
    alpha: Option<Vec<u8>>,
    color_space: Option<CFRetained<CGColorSpace>>,
}

fn extract_rgba(image: &CGImage) -> Result<ExtractedImage, String> {
    let (context, actual_space, width, height, stride, has_alpha) =
        edit::render_rgb_bitmap(image)?;
    let ptr = CGBitmapContextGetData(Some(&context)) as *const u8;
    if ptr.is_null() {
        return Err("could not access image pixels".to_owned());
    }
    let raw = unsafe { std::slice::from_raw_parts(ptr, stride * height) };
    let pixels = width * height;
    let mut rgb = vec![0u8; pixels * 3];
    let mut alpha = None;

    if has_alpha {
        let mut alpha_plane = vec![255u8; pixels];
        let mut any_transparent = false;
        for y in 0..height {
            let src_row = &raw[y * stride..y * stride + width * 4];
            let dst_row = &mut rgb[y * width * 3..(y + 1) * width * 3];
            let alpha_row = &mut alpha_plane[y * width..(y + 1) * width];
            for ((src, dst), a_out) in src_row
                .chunks_exact(4)
                .zip(dst_row.chunks_exact_mut(3))
                .zip(alpha_row.iter_mut())
            {
                let a = src[3];
                *a_out = a;
                if a == 255 {
                    dst.copy_from_slice(&src[..3]);
                } else if a == 0 {
                    any_transparent = true;
                    dst.copy_from_slice(&[0, 0, 0]);
                } else {
                    any_transparent = true;
                    dst[0] = edit::unpremultiply_channel(src[0], a);
                    dst[1] = edit::unpremultiply_channel(src[1], a);
                    dst[2] = edit::unpremultiply_channel(src[2], a);
                }
            }
        }
        if any_transparent {
            // Fill fully transparent border pixels from adjacent opaque neighbors to prevent dark halos.
            bleed_transparent_edges(&mut rgb, &alpha_plane, width, height);
            alpha = Some(alpha_plane);
        }
    } else {
        for y in 0..height {
            let src_row = &raw[y * stride..y * stride + width * 4];
            let dst_row = &mut rgb[y * width * 3..(y + 1) * width * 3];
            for (src, dst) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(3)) {
                dst.copy_from_slice(&src[..3]);
            }
        }
    }

    Ok(ExtractedImage {
        width,
        height,
        rgb,
        alpha,
        color_space: actual_space,
    })
}

fn bleed_transparent_edges(rgb: &mut [u8], alpha: &[u8], width: usize, height: usize) {
    if !alpha.contains(&0) {
        return;
    }
    let orig = rgb.to_vec();
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            if alpha[idx] > 0 {
                continue;
            }
            let (mut sr, mut sg, mut sb, mut count) = (0u32, 0u32, 0u32, 0u32);
            for dy in -1isize..=1 {
                let ny = y as isize + dy;
                if ny < 0 || ny >= height as isize {
                    continue;
                }
                for dx in -1isize..=1 {
                    let nx = x as isize + dx;
                    if nx < 0 || nx >= width as isize {
                        continue;
                    }
                    let nidx = ny as usize * width + nx as usize;
                    if alpha[nidx] > 0 {
                        sr += orig[nidx * 3] as u32;
                        sg += orig[nidx * 3 + 1] as u32;
                        sb += orig[nidx * 3 + 2] as u32;
                        count += 1;
                    }
                }
            }
            if count > 0 {
                rgb[idx * 3] = (sr / count) as u8;
                rgb[idx * 3 + 1] = (sg / count) as u8;
                rgb[idx * 3 + 2] = (sb / count) as u8;
            }
        }
    }
}

#[cfg(test)]
fn rgb_bytes_to_cgimage(
    rgb: &[u8],
    width: usize,
    height: usize,
    color_space: Option<&CGColorSpace>,
) -> Result<CFRetained<CGImage>, String> {
    let space = color_space.map(|s| {
        // SAFETY: `s` is a valid borrowed CoreGraphics color space for the duration of this call.
        unsafe { CFRetained::retain(std::ptr::NonNull::from(s)) }
    });
    let (context, _) = edit::make_bitmap_context(
        width,
        height,
        width * 4,
        space,
        CGImageAlphaInfo::NoneSkipLast,
    )
    .ok_or("not enough memory for tile bitmap")?;
    let stride = CGBitmapContextGetBytesPerRow(Some(&context));
    let ptr = CGBitmapContextGetData(Some(&context)) as *mut u8;
    if ptr.is_null() || stride < width * 4 {
        return Err("could not access tile bitmap".to_owned());
    }
    let dst = unsafe { std::slice::from_raw_parts_mut(ptr, stride * height) };
    for y in 0..height {
        let dst_row = &mut dst[y * stride..y * stride + width * 4];
        for x in 0..width {
            let s = (y * width + x) * 3;
            let d = x * 4;
            dst_row[d] = rgb[s];
            dst_row[d + 1] = rgb[s + 1];
            dst_row[d + 2] = rgb[s + 2];
            dst_row[d + 3] = 255;
        }
    }
    CGBitmapContextCreateImage(Some(&context))
        .ok_or_else(|| "could not create tile image".to_owned())
}

fn build_cg_image_with(
    width: usize,
    height: usize,
    alpha: Option<&[u8]>,
    color_space: Option<CFRetained<CGColorSpace>>,
    mut sample_rgb: impl FnMut(usize, usize) -> (f32, f32, f32),
) -> Result<CFRetained<CGImage>, String> {
    let info = if alpha.is_some() {
        CGImageAlphaInfo::PremultipliedLast
    } else {
        CGImageAlphaInfo::NoneSkipLast
    };
    let req_bpr = width.checked_mul(4).ok_or("upscaled image is too large")?;
    let (context, _) = edit::make_bitmap_context(width, height, req_bpr, color_space, info)
        .ok_or("not enough memory for upscaled image")?;
    let stride = CGBitmapContextGetBytesPerRow(Some(&context));
    let ptr = CGBitmapContextGetData(Some(&context)) as *mut u8;
    if ptr.is_null() || stride < req_bpr {
        return Err("could not access upscaled bitmap".to_owned());
    }
    let dst = unsafe { std::slice::from_raw_parts_mut(ptr, stride * height) };

    if let Some(alpha_plane) = alpha {
        for y in 0..height {
            let dst_row = &mut dst[y * stride..y * stride + width * 4];
            let alpha_row = &alpha_plane[y * width..(y + 1) * width];
            for (x, &a) in alpha_row.iter().enumerate() {
                let (r, g, b) = sample_rgb(x, y);
                let af = a as f32;
                let d = x * 4;
                dst_row[d] = (r.clamp(0.0, 1.0) * af + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[d + 1] = (g.clamp(0.0, 1.0) * af + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[d + 2] = (b.clamp(0.0, 1.0) * af + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[d + 3] = a;
            }
        }
    } else {
        for y in 0..height {
            let dst_row = &mut dst[y * stride..y * stride + width * 4];
            for x in 0..width {
                let (r, g, b) = sample_rgb(x, y);
                let d = x * 4;
                dst_row[d] = (r.clamp(0.0, 1.0) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[d + 1] = (g.clamp(0.0, 1.0) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[d + 2] = (b.clamp(0.0, 1.0) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[d + 3] = 255;
            }
        }
    }

    CGBitmapContextCreateImage(Some(&context))
        .ok_or_else(|| "could not finalize upscaled image".to_owned())
}

/// Bilinear resampling for an 8-bit alpha plane.
fn resample_alpha_plane(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    let mut out = vec![255u8; dst_w * dst_h];
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return out;
    }
    let sx_ratio = src_w as f32 / dst_w as f32;
    let sy_ratio = src_h as f32 / dst_h as f32;
    for y in 0..dst_h {
        let fy = ((y as f32 + 0.5) * sy_ratio - 0.5).clamp(0.0, (src_h - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(src_h - 1);
        let ty = fy - y0 as f32;
        for x in 0..dst_w {
            let fx = ((x as f32 + 0.5) * sx_ratio - 0.5).clamp(0.0, (src_w - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(src_w - 1);
            let tx = fx - x0 as f32;
            let a00 = src[y0 * src_w + x0] as f32;
            let a10 = src[y0 * src_w + x1] as f32;
            let a01 = src[y1 * src_w + x0] as f32;
            let a11 = src[y1 * src_w + x1] as f32;
            let top = a00 + (a10 - a00) * tx;
            let bot = a01 + (a11 - a01) * tx;
            out[y * dst_w + x] = (top + (bot - top) * ty + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Choose a tile overlap (in input pixels) proportional to the tile size.
fn choose_overlap(tile_side: usize) -> usize {
    if tile_side < 32 {
        return 0;
    }
    let target = (tile_side / 8).clamp(8, 32);
    (target / 8) * 8
}

/// Compute tile start offsets covering `[0, length)` with at least `overlap` pixels
/// between adjacent tiles.
fn tile_starts(length: usize, tile: usize, overlap: usize) -> Vec<usize> {
    if length <= tile || tile == 0 {
        return vec![0];
    }
    let step = tile.saturating_sub(overlap).max(1);
    let mut starts = Vec::new();
    let mut pos = 0;
    while pos + tile < length {
        starts.push(pos);
        pos += step;
    }
    let last = length - tile;
    if starts.last().copied() != Some(last) {
        starts.push(last);
    }
    starts
}

/// Extract a `(tile_w, tile_h)` RGB tile starting at `(tx, ty)` into `tile`,
/// reflecting edges when the source image is smaller than the model's fixed tile size.
#[allow(clippy::too_many_arguments)]
fn extract_padded_tile_into(
    rgb: &[u8],
    src_w: usize,
    src_h: usize,
    tx: usize,
    ty: usize,
    tile_w: usize,
    tile_h: usize,
    tile: &mut [u8],
) {
    for y in 0..tile_h {
        let sy = reflect_index(ty + y, src_h);
        let dst_row = y * tile_w * 3;
        if tx + tile_w <= src_w {
            let src_start = (sy * src_w + tx) * 3;
            tile[dst_row..dst_row + tile_w * 3]
                .copy_from_slice(&rgb[src_start..src_start + tile_w * 3]);
        } else {
            for x in 0..tile_w {
                let sx = reflect_index(tx + x, src_w);
                let src = (sy * src_w + sx) * 3;
                let dst = dst_row + x * 3;
                tile[dst..dst + 3].copy_from_slice(&rgb[src..src + 3]);
            }
        }
    }
}

fn reflect_index(index: usize, limit: usize) -> usize {
    if limit <= 1 {
        return 0;
    }
    if index < limit {
        return index;
    }
    let period = 2 * (limit - 1);
    let rem = index % period;
    if rem < limit {
        rem
    } else {
        period - rem
    }
}

/// Linear feather ramp on sides shared with another tile; 1.0 on outer image borders.
fn feather_weight(pos: usize, len: usize, fade_start: bool, fade_end: bool, feather: usize) -> f32 {
    if feather == 0 {
        return 1.0;
    }
    let mut w = 1.0f32;
    if fade_start && pos < feather {
        w = w.min((pos as f32 + 0.5) / feather as f32);
    }
    if fade_end && pos + feather >= len {
        let dist = len.saturating_sub(1).saturating_sub(pos);
        w = w.min((dist as f32 + 0.5) / feather as f32);
    }
    w.max(1e-4)
}

#[allow(clippy::too_many_arguments)]
fn blend_tile(
    tile_rgb: &[f32],
    tile_stride_w: usize,
    copy_w: usize,
    copy_h: usize,
    accum_rgb: &mut [f32],
    accum_w: &mut [f32],
    wx: &mut Vec<f32>,
    out_w: usize,
    dst_x: usize,
    dst_y: usize,
    has_left: bool,
    has_right: bool,
    has_top: bool,
    has_bottom: bool,
    feather_x: usize,
    feather_y: usize,
) {
    wx.clear();
    wx.extend((0..copy_w).map(|x| feather_weight(x, copy_w, has_left, has_right, feather_x)));
    for y in 0..copy_h {
        let wy = feather_weight(y, copy_h, has_top, has_bottom, feather_y);
        let src_row = y * tile_stride_w;
        let dst_row = (dst_y + y) * out_w + dst_x;
        for (x, &w_x) in wx.iter().enumerate() {
            let w = wy * w_x;
            let s = (src_row + x) * 3;
            let d = dst_row + x;
            accum_w[d] += w;
            accum_rgb[d * 3] += tile_rgb[s] * w;
            accum_rgb[d * 3 + 1] += tile_rgb[s + 1] * w;
            accum_rgb[d * 3 + 2] += tile_rgb[s + 2] * w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_starts_cover_any_extent_with_overlap() {
        assert_eq!(tile_starts(100, 256, 32), vec![0]);
        assert_eq!(tile_starts(256, 256, 32), vec![0]);
        let starts = tile_starts(600, 256, 32);
        assert_eq!(starts, vec![0, 224, 344]);
        for pair in starts.windows(2) {
            let overlap = pair[0] + 256 - pair[1];
            assert!(overlap >= 32, "expected >= 32px overlap, got {overlap}");
        }
        assert_eq!(starts.last().unwrap() + 256, 600);
    }

    #[test]
    fn padded_tiles_reflect_without_out_of_bounds() {
        // 3×1 image: R, G, B
        let rgb = [255, 0, 0, 0, 255, 0, 0, 0, 255];
        let mut tile = vec![0u8; 5 * 2 * 3];
        extract_padded_tile_into(&rgb, 3, 1, 0, 0, 5, 2, &mut tile);
        // Columns: 0(R), 1(G), 2(B), 1(G), 0(R)
        assert_eq!(&tile[0..15], &[255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 255, 0, 255, 0, 0]);
        // Second row reflects row 0.
        assert_eq!(&tile[15..30], &tile[0..15]);
    }

    #[test]
    fn feather_blending_preserves_constant_color_without_seams() {
        let out_w = 20;
        let out_h = 12;
        let tile_w = 12;
        let tile_h = 12;
        let overlap = 4;
        let starts = tile_starts(out_w, tile_w, overlap);
        assert_eq!(starts, vec![0, 8]);

        let tile_rgb: Vec<f32> = (0..tile_w * tile_h)
            .flat_map(|_| [0.25f32, 0.5, 0.75])
            .collect();
        let mut accum_rgb = vec![0.0f32; out_w * out_h * 3];
        let mut accum_w = vec![0.0f32; out_w * out_h];
        let mut wx = Vec::new();

        for (i, &tx) in starts.iter().enumerate() {
            blend_tile(
                &tile_rgb,
                tile_w,
                tile_w,
                tile_h,
                &mut accum_rgb,
                &mut accum_w,
                &mut wx,
                out_w,
                tx,
                0,
                i > 0,
                i + 1 < starts.len(),
                false,
                false,
                overlap,
                0,
            );
        }

        for p in 0..(out_w * out_h) {
            let w = accum_w[p];
            assert!(w > 0.5);
            assert!((accum_rgb[p * 3] / w - 0.25).abs() < 1e-5);
            assert!((accum_rgb[p * 3 + 1] / w - 0.50).abs() < 1e-5);
            assert!((accum_rgb[p * 3 + 2] / w - 0.75).abs() < 1e-5);
        }
    }

    #[test]
    fn find_model_prefers_swinir_aimodel_and_ignores_other_formats() {
        let dir = std::env::temp_dir().join(format!("blickli-models-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("RealESRGAN-x4plus.mlpackage")).unwrap();
        assert!(find_model(&dir).is_err());

        std::fs::create_dir_all(dir.join("realesrgan-x4plus.aimodel")).unwrap();
        assert_eq!(
            find_model(&dir).unwrap().file_name().unwrap(),
            "realesrgan-x4plus.aimodel"
        );
        std::fs::create_dir_all(dir.join("nested").join("swinir-l-real-x4-128-float16.aimodel"))
            .unwrap();
        assert_eq!(
            find_model(&dir).unwrap().file_name().unwrap(),
            "swinir-l-real-x4-128-float16.aimodel"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn half_scale_downsamples_cleanly() {
        let rgb = [120u8, 180, 60].repeat(16 * 12);
        let img = rgb_bytes_to_cgimage(&rgb, 16, 12, None).unwrap();
        let half = image_for_scale(&img, 4, UpscaleScale::X2).unwrap();
        assert_eq!(CGImage::width(Some(&half)), 8);
        assert_eq!(CGImage::height(Some(&half)), 6);
        let extracted = extract_rgba(&half).unwrap();
        assert_eq!(&extracted.rgb[0..3], &[120, 180, 60]);
    }

    #[test]
    #[ignore = "runs real Core AI SwinIR-L inference using ../upscale"]
    fn real_swinir_aimodel_upscales_tiled_image() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../upscale");
        let model_path = find_model(&dir).expect("model in ../upscale");
        let t0 = std::time::Instant::now();
        let loaded = LoadedModel::load(&model_path).expect("Core AI model loads");
        let load_ms = t0.elapsed().as_millis();

        // 160x120 exercises 2x1 overlapping 128x128 tiles + feather blending + edge padding.
        let (w, h) = (160, 120);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = ((x * 255) / w) as u8;
                rgb[i + 1] = ((y * 255) / h) as u8;
                rgb[i + 2] = if (x / 16 + y / 16) % 2 == 0 { 200 } else { 40 };
            }
        }
        let src = rgb_bytes_to_cgimage(&rgb, w, h, None).unwrap();
        let cancel = AtomicBool::new(false);
        let mut tiles_seen = 0usize;
        let t1 = std::time::Instant::now();
        let res = loaded
            .upscale_image(&src, &cancel, &mut |p| {
                if let Progress::Tile { done, .. } = p {
                    tiles_seen = done;
                }
            })
            .expect("SwinIR-L inference succeeds");
        let run_ms = t1.elapsed().as_millis();
        println!(
            "SwinIR-L ({}) load={load_ms}ms, 160x120->{}x{} ({tiles_seen} tiles) in {run_ms}ms",
            model_path.file_name().unwrap().to_string_lossy(),
            CGImage::width(Some(&res.upscaled_native)),
            CGImage::height(Some(&res.upscaled_native)),
        );
        assert_eq!(res.native_scale, 4);
        assert_eq!(CGImage::width(Some(&res.upscaled_native)), 640);
        assert_eq!(CGImage::height(Some(&res.upscaled_native)), 480);
        let half = image_for_scale(&res.upscaled_native, res.native_scale, UpscaleScale::X2).unwrap();
        assert_eq!(CGImage::width(Some(&half)), 320);
        assert_eq!(CGImage::height(Some(&half)), 240);
        // Verify non-blank RGB output roughly matches the gradient.
        let out = extract_rgba(&res.upscaled_native).unwrap();
        let tl = &out.rgb[(8 * 640 + 8) * 3..(8 * 640 + 8) * 3 + 3];
        let br = &out.rgb[(470 * 640 + 630) * 3..(470 * 640 + 630) * 3 + 3];
        assert!(br[0] > tl[0] + 100, "red channel gradient preserved: tl={tl:?} br={br:?}");
        assert!(br[1] > tl[1] + 100, "green channel gradient preserved: tl={tl:?} br={br:?}");
    }
}
