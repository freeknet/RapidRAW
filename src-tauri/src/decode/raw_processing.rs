use crate::decode::as_shot::AsShot;
use crate::decode::demosaic::{Mosaic, white_level};
use crate::decode::develop::{self, Camera, Finish};
use crate::decode::dng_opcodes::{OpcodeList3, Placement};
use crate::decode::hot_pixels::repair_hot_pixels;
use crate::decode::white_balance::WhiteBalance;
use crate::render::color_space::{GamutCompression, fit_gamut, gamut_limits, srgb_to_working};
use crate::render::geometry::apply_orientation;
use anyhow::{Result, anyhow};
use glam::Vec3;
use image::{DynamicImage, ImageBuffer, Rgb, Rgb32FImage};
use rawler::{
    decoders::{Decoder, FormatHint, Orientation, RawDecodeParams},
    imgop::{
        Dim2, Point, Rect,
        develop::{DemosaicAlgorithm, Intermediate, ProcessingStep, RawDevelop},
        xyz::{FlatColorMatrix, Illuminant},
    },
    pixarray::Color2D,
    rawimage::{RawImage, RawImageData, RawPhotometricInterpretation},
    rawsource::RawSource,
};
use rayon::prelude::*;
use std::borrow::Cow;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn metadata_orientation(decoder: &dyn Decoder, source: &RawSource) -> Result<Orientation> {
    let metadata = decoder.raw_metadata(source, &RawDecodeParams::default())?;
    Ok(metadata
        .exif
        .orientation
        .map(Orientation::from_u16)
        .unwrap_or(Orientation::Normal))
}

const HIGHLIGHT_HEADROOM: f32 = 1000.0;

thread_local! {
    static READING_PREFIX: Cell<bool> = const { Cell::new(false) };
    static PREFIX_WARNED: Cell<bool> = const { Cell::new(false) };
}

pub fn is_reading_prefix() -> bool {
    READING_PREFIX.with(Cell::get)
}

pub fn swallow_prefix_log(metadata: &log::Metadata) -> bool {
    if !metadata.target().starts_with("rawler") || !is_reading_prefix() {
        return false;
    }
    if metadata.level() <= log::Level::Warn {
        PREFIX_WARNED.with(|warned| warned.set(true));
    }
    true
}

pub fn read_from_prefix<T>(
    file_bytes: &[u8],
    read: impl Fn(&[u8], bool) -> Option<T>,
) -> Option<T> {
    let mut len = 256 * 1024;
    while len < file_bytes.len() && len <= 16 * 1024 * 1024 {
        READING_PREFIX.with(|reading| reading.set(true));
        PREFIX_WARNED.with(|warned| warned.set(false));
        let found = catch_unwind(AssertUnwindSafe(|| read(&file_bytes[..len], false)));
        READING_PREFIX.with(|reading| reading.set(false));
        if let Ok(Some(found)) = found
            && !PREFIX_WARNED.with(Cell::get)
        {
            return Some(found);
        }
        len *= 4;
    }
    read(file_bytes, true)
}

pub fn extract_embedded_preview(file_bytes: &[u8]) -> Option<DynamicImage> {
    read_from_prefix(file_bytes, |bytes, whole_file| {
        let source = RawSource::new_from_slice(bytes);
        let decoder = rawler::get_decoder(&source).ok()?;
        let orientation = match metadata_orientation(decoder.as_ref(), &source) {
            Ok(orientation) => orientation,
            Err(_) if whole_file => Orientation::Normal,
            Err(_) => return None,
        };
        let preview = decoder
            .full_image(&source, &RawDecodeParams::default())
            .ok()??;
        Some(apply_orientation(preview, orientation))
    })
}

fn is_linear_raw_format(raw_image: &RawImage) -> bool {
    matches!(
        raw_image.photometric,
        RawPhotometricInterpretation::LinearRaw
    )
}

#[inline]
fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(3.0)
    }
}

#[inline]
fn smootherstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[inline]
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn recover_clipped_pixel(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max_c = r.max(g).max(b);

    if max_c <= 0.50 {
        return (r, g, b);
    }

    let mut cur_r = r;
    let mut cur_g = g;
    let mut cur_b = b;

    let outer_blend = smootherstep(0.50, 1.5, max_c);

    let magenta = (cur_r.min(cur_b) - cur_g).max(0.0);
    if magenta > 0.0 {
        let target_g = cur_r.min(cur_b) * 0.80 + ((cur_r + cur_b) * 0.5) * 0.20;
        let correction = (target_g - cur_g).max(0.0);
        let magenta_weight = smoothstep(0.0, 0.25, magenta / max_c);
        cur_g += correction * outer_blend * magenta_weight;
    }

    let residual = (cur_r.min(cur_b) - cur_g).max(0.0);
    if residual > 0.0 {
        cur_g += residual * outer_blend;
    }

    let new_max = cur_r.max(cur_g).max(cur_b);
    let min_c = cur_r.min(cur_g).min(cur_b);

    let knee = smoothstep(0.50, 1.5, new_max);

    if knee > 0.0 {
        let neutrality = (min_c / new_max.max(1e-5)).clamp(0.0, 1.0);

        let core_burn = smoothstep(0.60, 3.0, new_max);

        let desat = (knee * (neutrality * 0.85 + core_burn * 0.15)).clamp(0.0, 1.0);
        let smooth_desat = desat * desat * (3.0 - 2.0 * desat);

        let neutral_value = min_c + (new_max - min_c) * 1.0;

        cur_r = cur_r * (1.0 - smooth_desat) + neutral_value * smooth_desat;
        cur_g = cur_g * (1.0 - smooth_desat) + neutral_value * smooth_desat;
        cur_b = cur_b * (1.0 - smooth_desat) + neutral_value * smooth_desat;
    }

    (cur_r, cur_g, cur_b)
}

fn primary_color_matrix(raw_image: &RawImage) -> Option<&FlatColorMatrix> {
    let matrices = &raw_image.color_matrix;
    matrices
        .get(&Illuminant::D65)
        .or_else(|| matrices.values().next())
}

struct CameraToWorking {
    matrix: [[f32; 4]; 3],
    compression: [Option<GamutCompression>; 3],
}

impl CameraToWorking {
    fn new(raw_image: &RawImage) -> Option<Self> {
        let color_matrix = as_shot_color(raw_image)
            .map(|(_, color_matrix)| color_matrix)
            .or_else(|| primary_color_matrix(raw_image).cloned())?;
        if color_matrix.len() % 3 != 0 || color_matrix.len() > 12 {
            return None;
        }
        let primaries = crate::decode::white_balance::camera_to_working(
            &color_matrix,
            &camera_neutral(raw_image),
        )?;

        let mut matrix = [[0.0f32; 4]; 3];
        for (channel, primary) in primaries.iter().enumerate() {
            for (row, value) in matrix.iter_mut().zip(primary) {
                row[channel] = *value;
            }
        }

        Some(Self {
            matrix,
            compression: gamut_limits(&primaries).map(GamutCompression::new),
        })
    }

    #[inline]
    fn pixel<const N: usize>(&self, camera: &[f32; N]) -> [f32; 3] {
        let rgb = self
            .matrix
            .map(|row| row.iter().zip(camera).map(|(m, c)| m * c).sum::<f32>());
        fit_gamut(rgb, &self.compression)
    }

    fn camera(&self, wb: [f32; 3], clamp_limit: f32) -> Camera {
        Camera {
            wb,
            matrix: self.matrix.map(|row| [row[0], row[1], row[2]]),
            compression: self.compression,
            clamp_limit,
        }
    }

    fn convert_pixels<const N: usize>(&self, pixels: &Color2D<f32, N>) -> Color2D<f32, 3> {
        Color2D::new_with(
            pixels.data.par_iter().map(|p| self.pixel(p)).collect(),
            pixels.width,
            pixels.height,
        )
    }

    fn apply(&self, intermediate: Intermediate) -> Intermediate {
        match intermediate {
            Intermediate::ThreeColor(pixels) => {
                Intermediate::ThreeColor(self.convert_pixels(&pixels))
            }
            Intermediate::FourColor(pixels) => {
                Intermediate::ThreeColor(self.convert_pixels(&pixels))
            }
            monochrome => monochrome,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn develop_raw_image(
    file_bytes: &[u8],
    fast_demosaic: bool,
    _highlight_compression: f32,
    linear_mode: String,
    denoise: f32,
    sharpen: f32,
    remove_hot_pixels: bool,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    let check_cancel = || -> Result<()> {
        if let Some((tracker, generation)) = &cancel_token
            && tracker.load(Ordering::SeqCst) != *generation
        {
            return Err(anyhow!("Load cancelled"));
        }
        Ok(())
    };

    check_cancel()?;

    let decodable = crate::decode::dng_monochrome::as_linear_raw(file_bytes);
    let file_bytes: &[u8] = &decodable;
    let source = RawSource::new_from_slice(file_bytes);
    let decoder = rawler::get_decoder(&source)?;

    check_cancel()?;
    let mut raw_image: RawImage = decoder.raw_image(&source, &RawDecodeParams::default(), false)?;
    crate::decode::dng_opcodes::apply_raw_opcodes(decoder.as_ref(), &mut raw_image);
    if decoder.format_hint() == FormatHint::RAF {
        raw_image.active_area =
            extend_active_area_to_crop(raw_image.active_area, raw_image.crop_area, raw_image.dim());
    }
    if remove_hot_pixels {
        check_cancel()?;
        repair_hot_pixels(&mut raw_image);
    }

    let orientation = metadata_orientation(decoder.as_ref(), &source)?;
    // OpcodeList3 refers to the unrotated image, so the rotation waits for it
    let list3 = OpcodeList3::read(decoder.as_ref());
    let finish_orientation = if list3.is_some() {
        Orientation::Normal
    } else {
        orientation
    };
    let finish_list3 = |image: Rgb32FImage, placement: Placement| match &list3 {
        Some(list3) => apply_orientation(
            DynamicImage::ImageRgb32F(list3.apply(image, placement)),
            orientation,
        ),
        None => DynamicImage::ImageRgb32F(image),
    };

    let is_linear_format = is_linear_raw_format(&raw_image);
    let is_floating_point = matches!(raw_image.data, RawImageData::Float(_));

    let (apply_ungamma, apply_calibration) = match linear_mode.as_str() {
        "gamma" => (true, true),
        "skip_calib" => (false, false),
        "gamma_skip_calib" => (true, false),
        _ => (false, true),
    };

    let original_white_level = white_level(&raw_image);
    let original_black_level = raw_image
        .blacklevel
        .levels
        .first()
        .map(|r| r.as_f32())
        .unwrap_or(0.0);

    raw_image.wb_coeffs = crate::decode::multi_exposure::neutralize_wb_if_multiexposure(
        raw_image.wb_coeffs,
        file_bytes,
    );

    let camera_to_working = (apply_calibration || !is_linear_format)
        .then(|| CameraToWorking::new(&raw_image))
        .flatten();
    let cfa_balance = (!is_linear_format && camera_to_working.is_some())
        .then(|| cfa_white_balance(&raw_image))
        .flatten();
    let (clamp_limit, denoise, sharpen) = if fast_demosaic {
        (1.0, 0.0, 0.0)
    } else {
        (HIGHLIGHT_HEADROOM, denoise, sharpen)
    };
    let finish = |camera: Option<Camera>| Finish {
        camera,
        orientation: finish_orientation,
        denoise,
        sharpen,
    };

    if let (Some(wb), Some(camera_to_working)) = (cfa_balance, &camera_to_working)
        && let Some(mosaic) = Mosaic::new(&raw_image, wb)
    {
        check_cancel()?;
        let finish = finish(Some(camera_to_working.camera(wb, clamp_limit)));
        let crop = |dim| {
            default_crop(
                raw_image.crop_area,
                raw_image.active_area,
                raw_image.dim(),
                dim,
            )
        };
        let (image, placement) = if fast_demosaic {
            let (data, dim) = mosaic.bin();
            check_cancel()?;
            let crop = crop(dim);
            let image = develop::finish_rgb(
                &crop_rows(&data, dim.w, crop),
                crop.d.w as u32,
                crop.d.h as u32,
                &finish,
            );
            (image, Placement::new(crop, dim))
        } else {
            let domain = mosaic.domain();
            let crop = crop(domain.d);
            let placement = Placement::new(crop, domain.d);
            let crop = Rect::new(
                Point::new(domain.p.x + crop.p.x, domain.p.y + crop.p.y),
                crop.d,
            );
            (mosaic.develop(crop, &finish, check_cancel)?, placement)
        };
        return Ok(finish_list3(image, placement));
    }

    for level in raw_image.whitelevel.0.iter_mut() {
        *level = u32::MAX;
    }

    let mut developer = RawDevelop::default();
    developer
        .steps
        .retain(|&step| step != ProcessingStep::SRgb && step != ProcessingStep::Calibrate);

    if is_linear_format {
        developer
            .steps
            .retain(|&step| step != ProcessingStep::Demosaic);
    } else if fast_demosaic {
        developer.demosaic_algorithm = DemosaicAlgorithm::Speed;
    }

    if cfa_balance.is_some() {
        developer
            .steps
            .retain(|&step| step != ProcessingStep::CropDefault);
    }

    check_cancel()?;
    let mut developed_intermediate = developer.develop_intermediate(&raw_image)?;

    let (crop_area, active_area, raw_dim) =
        (raw_image.crop_area, raw_image.active_area, raw_image.dim());
    drop(raw_image);
    let list3_placement = if is_linear_format {
        linear_placement(crop_area, active_area, raw_dim)
    } else {
        let dim = developed_intermediate.dim();
        Placement::new(Rect::new(Point::zero(), dim), dim)
    };

    if cfa_balance.is_none()
        && let Some(camera_to_working) = &camera_to_working
    {
        check_cancel()?;
        developed_intermediate = camera_to_working.apply(developed_intermediate);
    }

    let denominator = (original_white_level - original_black_level).max(1.0);
    let rescale_factor = (u32::MAX as f32 - original_black_level) / denominator;

    let (width, height) = {
        let dim = developed_intermediate.dim();
        (dim.w as u32, dim.h as u32)
    };

    check_cancel()?;

    match &mut developed_intermediate {
        Intermediate::Monochrome(pixels) => {
            pixels.data.par_iter_mut().for_each(|p| {
                let mut linear_val = *p * rescale_factor;
                if is_linear_format && apply_ungamma {
                    linear_val = srgb_to_linear(linear_val.max(0.0));
                }
                *p = linear_val.clamp(0.0, clamp_limit);
            });
        }
        Intermediate::ThreeColor(pixels) if cfa_balance.is_some() => {
            let wb = cfa_balance.unwrap();
            let camera = camera_to_working.as_ref().unwrap().camera(wb, clamp_limit);
            let crop = default_crop(crop_area, active_area, raw_dim, pixels.dim());
            let placement = Placement::new(crop, pixels.dim());
            let mut data = crop_rows(&pixels.data, pixels.width, crop).into_owned();
            data.par_iter_mut().for_each(|p| {
                for c in 0..3 {
                    p[c] = (p[c] * rescale_factor).max(0.0) * wb[c];
                }
            });
            check_cancel()?;

            let image = develop::finish_rgb(
                &data,
                crop.d.w as u32,
                crop.d.h as u32,
                &finish(Some(camera)),
            );
            return Ok(finish_list3(image, placement));
        }
        Intermediate::ThreeColor(pixels) => {
            let uncalibrated_to_working = camera_to_working.is_none().then(srgb_to_working);
            pixels.data.par_iter_mut().for_each(|p| {
                let mut r = (p[0] * rescale_factor).max(0.0);
                let mut g = (p[1] * rescale_factor).max(0.0);
                let mut b = (p[2] * rescale_factor).max(0.0);

                if is_linear_format && apply_ungamma {
                    r = srgb_to_linear(r.max(0.0));
                    g = srgb_to_linear(g.max(0.0));
                    b = srgb_to_linear(b.max(0.0));
                }

                let (rec_r, rec_g, rec_b) = if is_floating_point {
                    (r, g, b)
                } else {
                    recover_clipped_pixel(r, g, b)
                };
                let recovered = Vec3::new(rec_r, rec_g, rec_b);
                let rgb = uncalibrated_to_working.map_or(recovered, |matrix| matrix * recovered);

                *p = rgb.clamp(Vec3::ZERO, Vec3::splat(clamp_limit)).to_array();
            });
        }
        Intermediate::FourColor(pixels) => {
            pixels.data.par_iter_mut().for_each(|p| {
                p.iter_mut().for_each(|c| {
                    let mut linear_val = *c * rescale_factor;
                    if is_linear_format && apply_ungamma {
                        linear_val = srgb_to_linear(linear_val.max(0.0));
                    }
                    *c = linear_val.clamp(0.0, clamp_limit);
                });
            });
        }
    }

    check_cancel()?;

    let dynamic_image = match developed_intermediate {
        Intermediate::ThreeColor(pixels) => {
            DynamicImage::ImageRgb32F(rgb_pixels_to_rgb(pixels.data, width, height)?)
        }
        Intermediate::Monochrome(pixels) => {
            DynamicImage::ImageRgb32F(mono_pixels_to_rgb(&pixels.data, width, height)?)
        }
        _ => {
            return Err(anyhow!("Unsupported intermediate format for conversion"));
        }
    };

    let dynamic_image = match (&list3, dynamic_image) {
        (Some(list3), DynamicImage::ImageRgb32F(image)) => {
            DynamicImage::ImageRgb32F(list3.apply(image, list3_placement))
        }
        (_, image) => image,
    };

    Ok(develop::enhance(
        apply_orientation(dynamic_image, orientation),
        denoise,
        sharpen,
    ))
}

/// Where a linear raw image developed by rawler lies in its ActiveArea. rawler
/// crops linear images to the crop area of the whole image, without
/// demosaicing to the ActiveArea first.
fn linear_placement(
    crop_area: Option<Rect>,
    active_area: Option<Rect>,
    raw_dim: Dim2,
) -> Placement {
    let full = Rect::new(Point::zero(), raw_dim);
    let shown = crop_area.filter(|crop| !crop.is_empty()).unwrap_or(full);
    let area = active_area.unwrap_or(full);
    Placement {
        origin: [
            shown.p.x as f64 - area.p.x as f64,
            shown.p.y as f64 - area.p.y as f64,
        ],
        size: [area.d.w as f64, area.d.h as f64],
    }
}

fn crop_rows(data: &[[f32; 3]], stride: usize, crop: Rect) -> Cow<'_, [[f32; 3]]> {
    let rows = &data[crop.p.y * stride..(crop.p.y + crop.d.h) * stride];
    if crop.d.w == stride {
        return Cow::Borrowed(rows);
    }
    Cow::Owned(
        rows.par_chunks_exact(stride)
            .flat_map_iter(|row| row[crop.p.x..crop.p.x + crop.d.w].iter().copied())
            .collect(),
    )
}

fn default_crop(
    crop_area: Option<Rect>,
    active_area: Option<Rect>,
    raw_dim: Dim2,
    dim: Dim2,
) -> Rect {
    let full = Rect::new(Point::zero(), dim);
    let Some(mut crop) = crop_area else {
        return full;
    };
    if let Some(active_area) = active_area {
        crop = crop.intersection(&active_area);
        if crop.is_empty() {
            return full;
        }
        crop = crop.adapt(&active_area);
    }
    let original_width = active_area.map(|area| area.d.w).unwrap_or(raw_dim.w);
    if original_width > 0 {
        let scale_factor = dim.w as f32 / original_width as f32;
        if (scale_factor - 1.0).abs() > 1e-6 {
            crop.scale(scale_factor);
        }
    }
    let in_bounds = crop.p.x + crop.d.w <= dim.w && crop.p.y + crop.d.h <= dim.h;
    if crop.is_empty() || crop.d == dim || !in_bounds {
        full
    } else {
        crop
    }
}

/// Grows the active area so it holds the crop of the file.
///
/// The camera definitions give RAF active areas as borders of the uncompressed
/// raw width, while compressed files decode to a narrower width. The border
/// then cut into the crop the file itself declares, which lost 96 pixels on
/// the right of compressed X-Pro2 files.
fn extend_active_area_to_crop(
    active_area: Option<Rect>,
    crop_area: Option<Rect>,
    dim: Dim2,
) -> Option<Rect> {
    let (Some(active), Some(crop)) = (active_area, crop_area) else {
        return active_area;
    };
    let [crop_l, crop_t, crop_r, crop_b] = crop.as_ltrb();
    if crop.is_empty() || crop_r > dim.w || crop_b > dim.h {
        return active_area;
    }
    let [l, t, r, b] = active.as_ltrb();
    Some(Rect::new_with_points(
        Point::new(l.min(crop_l), t.min(crop_t)),
        Point::new(r.max(crop_r), b.max(crop_b)),
    ))
}

fn cfa_white_balance(raw_image: &RawImage) -> Option<[f32; 3]> {
    let RawPhotometricInterpretation::Cfa(config) = &raw_image.photometric else {
        return None;
    };
    if raw_image.cpp != 1 || !config.cfa.is_rgb() || config.cfa.unique_colors() != 3 {
        return None;
    }
    if raw_image.wb_coeffs[0].is_nan() {
        Some([1.0; 3])
    } else {
        Some([
            raw_image.wb_coeffs[0],
            raw_image.wb_coeffs[1],
            raw_image.wb_coeffs[2],
        ])
    }
}

pub fn read_as_shot(file_bytes: &[u8]) -> Option<AsShot> {
    let decodable = crate::decode::dng_monochrome::as_linear_raw(file_bytes);
    let file_bytes: &[u8] = &decodable;
    let source = RawSource::new_from_slice(file_bytes);
    let decoder = rawler::get_decoder(&source).ok()?;
    let mut raw_image = decoder
        .raw_image(&source, &RawDecodeParams::default(), true)
        .ok()?;
    raw_image.wb_coeffs = crate::decode::multi_exposure::neutralize_wb_if_multiexposure(
        raw_image.wb_coeffs,
        file_bytes,
    );
    let is_monochrome = raw_image.cpp == 1
        && !matches!(raw_image.photometric, RawPhotometricInterpretation::Cfa(_));

    Some(AsShot {
        white_balance: (!is_monochrome)
            .then(|| as_shot_color(&raw_image))
            .flatten()
            .map(|(white_balance, _)| white_balance)
            .unwrap_or_else(WhiteBalance::reference),
        exposure: crate::decode::baseline_exposure::read(
            decoder.as_ref(),
            file_bytes,
            is_monochrome,
        ),
        source_peak: 1.0,
    })
}

fn camera_neutral(raw_image: &RawImage) -> [f32; 4] {
    if raw_image.wb_coeffs[0].is_nan() {
        [1.0; 4]
    } else {
        raw_image.wb_coeffs.map(|c| 1.0 / c)
    }
}

fn as_shot_color(raw_image: &RawImage) -> Option<(WhiteBalance, FlatColorMatrix)> {
    let neutral = camera_neutral(raw_image);

    let matrices = &raw_image.color_matrix;
    if let (Some(matrix_a), Some(matrix_d65)) =
        (matrices.get(&Illuminant::A), matrices.get(&Illuminant::D65))
    {
        return WhiteBalance::from_dual_illuminant_camera_neutral(matrix_a, matrix_d65, &neutral);
    }

    let color_matrix = primary_color_matrix(raw_image)?;
    WhiteBalance::from_camera_neutral(color_matrix, &neutral)
        .map(|white_balance| (white_balance, color_matrix.clone()))
}

fn rgb_pixels_to_rgb(
    data: Vec<[f32; 3]>,
    width: u32,
    height: u32,
) -> Result<ImageBuffer<Rgb<f32>, Vec<f32>>> {
    ImageBuffer::from_raw(width, height, data.into_flattened())
        .ok_or_else(|| anyhow!("Developed image size does not match its dimensions"))
}

fn mono_pixels_to_rgb(
    data: &[f32],
    width: u32,
    height: u32,
) -> Result<ImageBuffer<Rgb<f32>, Vec<f32>>> {
    let mut rgb = vec![0.0f32; data.len() * 3];
    rgb.par_chunks_exact_mut(3)
        .zip(data.par_iter())
        .for_each(|(dst, &p)| dst.fill(p));
    ImageBuffer::from_raw(width, height, rgb)
        .ok_or_else(|| anyhow!("Developed image size does not match its dimensions"))
}

pub fn raw_dimensions(file_bytes: &[u8]) -> Option<(usize, usize)> {
    let file_bytes = crate::decode::dng_monochrome::as_linear_raw(file_bytes);
    let source = RawSource::new_from_slice(&file_bytes);
    let decoder = rawler::get_decoder(&source).ok()?;
    let raw_img = decoder
        .raw_image(&source, &RawDecodeParams::default(), true)
        .ok()?;
    Some((raw_img.width, raw_img.height))
}

pub fn get_fast_demosaic_scale_factor(
    file_bytes: &[u8],
    decoded_width: u32,
    decoded_height: u32,
) -> f32 {
    if let Some((raw_width, raw_height)) = raw_dimensions(file_bytes) {
        let max_orig = (raw_width as f32).max(raw_height as f32);
        let max_comp = (decoded_width as f32).max(decoded_height as f32);
        if max_orig > 0.0 {
            let ratio = max_comp / max_orig;
            if ratio > 0.1 && ratio < 0.29 {
                return 0.25;
            } else if (0.29..0.4).contains(&ratio) {
                return 1.0 / 3.0;
            } else if (0.4..0.75).contains(&ratio) {
                return 0.5;
            }
        }
    }
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(l: usize, t: usize, r: usize, b: usize) -> Rect {
        Rect::new_with_points(Point::new(l, t), Point::new(r, b))
    }

    #[test]
    fn compressed_raf_crop_is_kept_whole() {
        // Compressed X-Pro2: 6048 wide, the camera border leaves 5920 columns,
        // the file declares a 6000x4000 crop at 16:21
        let dim = Dim2::new(6048, 4038);
        let active = extend_active_area_to_crop(
            Some(rect(0, 0, 5920, 4038)),
            Some(rect(16, 21, 6016, 4021)),
            dim,
        );
        assert_eq!(active, Some(rect(0, 0, 6016, 4038)));
        let domain = Dim2::new(6016, 4038);
        let crop = default_crop(Some(rect(16, 21, 6016, 4021)), active, dim, domain);
        assert_eq!(crop, rect(16, 21, 6016, 4021));
    }

    #[test]
    fn active_area_holding_the_crop_is_unchanged() {
        let dim = Dim2::new(6160, 4032);
        let active = Some(rect(0, 0, 6032, 4032));
        let crop = Some(rect(16, 21, 6016, 4021));
        assert_eq!(extend_active_area_to_crop(active, crop, dim), active);
        assert_eq!(extend_active_area_to_crop(active, None, dim), active);
        assert_eq!(extend_active_area_to_crop(None, crop, dim), None);
        // A crop past the image is not trusted
        let outside = Some(rect(16, 21, 6200, 4021));
        assert_eq!(extend_active_area_to_crop(active, outside, dim), active);
    }

    #[test]
    fn crop_outside_the_active_area_falls_back_to_full() {
        let dim = Dim2::new(100, 100);
        let crop = default_crop(
            Some(rect(0, 0, 10, 10)),
            Some(rect(50, 50, 100, 100)),
            dim,
            Dim2::new(50, 50),
        );
        assert_eq!(crop, rect(0, 0, 50, 50));
    }

    fn develop(file: &[u8]) -> Rgb32FImage {
        develop_raw_image(file, false, 1.0, "linear".into(), 0.0, 0.0, false, None)
            .unwrap()
            .into_rgb32f()
    }

    #[test]
    fn black_is_zero_monochrome_dngs_develop() {
        // Monochrome DNGs store their data as BlackIsZero, which rawler failed
        // to decode because it looked for a CFA pattern
        use crate::decode::test_dng::*;
        let file = dng(
            8,
            8,
            1,
            BLACK_IS_ZERO,
            &[600; 64],
            vec![shorts(50714, &[100]), longs(50717, &[1100])],
        );
        let image = develop(&file);
        assert_eq!(image.dimensions(), (8, 8));
        for value in image.as_raw() {
            assert!((value - 0.5).abs() < 1e-3, "{value}");
        }
    }

    fn linear_gradient(orientation: u16, list3: Option<&[u8]>) -> Vec<u8> {
        use crate::decode::test_dng::*;
        let pixels: Vec<u16> = (0..7u16)
            .flat_map(|y| (0..9u16).flat_map(move |x| [50 * x + 10 * y; 3]))
            .collect();
        let mut tags = vec![
            shorts(274, &[orientation]),
            shorts(50714, &[0, 0, 0]),
            longs(50717, &[1000]),
        ];
        if let Some(list3) = list3 {
            tags.push(undefined(OPCODE_LIST3, list3));
        }
        dng(9, 7, 3, LINEAR_RAW, &pixels, tags)
    }

    fn warp_list(kr0: f64, center: [f64; 2]) -> Vec<u8> {
        let mut params = 1u32.to_be_bytes().to_vec();
        for value in [kr0, 0.0, 0.0, 0.0, 0.0, 0.0, center[0], center[1]] {
            params.extend_from_slice(&value.to_be_bytes());
        }
        let mut list = 1u32.to_be_bytes().to_vec();
        for word in [1, 0x0103_0000, 0, params.len() as u32] {
            list.extend_from_slice(&u32::to_be_bytes(word));
        }
        list.extend_from_slice(&params);
        list
    }

    fn assert_close(a: &Rgb32FImage, b: &Rgb32FImage) {
        assert_eq!(a.dimensions(), b.dimensions());
        for (x, y) in a.as_raw().iter().zip(b.as_raw()) {
            assert!((x - y).abs() < 1e-4, "{x} != {y}");
        }
    }

    #[test]
    fn opcode_list3_warp_applies_before_the_rotation() {
        let warp = warp_list(0.8, [0.25, 0.5]);
        let plain = develop(&linear_gradient(1, None));
        let warped = develop(&linear_gradient(1, Some(&warp)));
        // The optical centre stays, the pixels around it are pulled towards it
        assert_eq!(warped.get_pixel(2, 3), plain.get_pixel(2, 3));
        assert!(warped.get_pixel(8, 3)[0] < plain.get_pixel(8, 3)[0]);

        let rotated = develop(&linear_gradient(6, Some(&warp)));
        let expected = apply_orientation(DynamicImage::ImageRgb32F(warped), Orientation::Rotate90)
            .into_rgb32f();
        assert_close(&rotated, &expected);
    }

    #[test]
    fn linear_placement_is_relative_to_the_active_area() {
        let placement = linear_placement(
            Some(rect(10, 12, 90, 70)),
            Some(rect(8, 8, 92, 72)),
            Dim2::new(100, 80),
        );
        assert_eq!(placement.origin, [2.0, 4.0]);
        assert_eq!(placement.size, [84.0, 64.0]);
        let whole = linear_placement(None, None, Dim2::new(100, 80));
        assert_eq!(whole.origin, [0.0, 0.0]);
        assert_eq!(whole.size, [100.0, 80.0]);
    }
}
