//! DNG opcode list processing.
//!
//! OpcodeList1 is applied to the raw data as stored, OpcodeList2 to the
//! linearized raw data inside the ActiveArea after black subtraction and
//! scaling so the white level is 1.0, and OpcodeList3 to the demosaiced image
//! of the ActiveArea before the default crop.
//!
//! rawler only applies MapPolynomial to proxy images, so lossy and JPEG XL DNGs
//! that store their tone curve as per plane MapPolynomial opcodes came out
//! pink, phone and drone DNGs lost their lens shading GainMaps, and the lens
//! distortion and vignetting corrections of phone DNGs were never applied.

use image::Rgb32FImage;
use rawler::{
    RawImage, RawImageData,
    decoders::{Decoder, FormatHint, WellKnownIFD},
    imgop::{Dim2, Point, Rect},
    rawimage::{BlackLevel, RawPhotometricInterpretation, WhiteLevel},
    tags::DngTag,
};
use rayon::prelude::*;
use std::collections::HashSet;

const WARP_RECTILINEAR: u32 = 1;
const FIX_VIGNETTE_RADIAL: u32 = 3;
const FIX_BAD_PIXELS_CONSTANT: u32 = 4;
const FIX_BAD_PIXELS_LIST: u32 = 5;
const MAP_TABLE: u32 = 7;
const MAP_POLYNOMIAL: u32 = 8;
const GAIN_MAP: u32 = 9;
const DELTA_PER_ROW: u32 = 10;
const DELTA_PER_COLUMN: u32 = 11;
const SCALE_PER_ROW: u32 = 12;
const SCALE_PER_COLUMN: u32 = 13;

/// Opcode flag for opcodes that only apply to previews.
const FLAG_PREVIEW_ONLY: u32 = 2;

/// Bad pixel lists larger than this are not applied.
const MAX_BAD_PIXELS: usize = 1 << 24;

/// White level of integer data after the opcodes, one stop below u16::MAX so
/// that gains above 1.0 keep their headroom.
const OUTPUT_WHITE: u32 = 32767;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Area {
    top: usize,
    left: usize,
    bottom: usize,
    right: usize,
    plane: usize,
    planes: usize,
    row_pitch: usize,
    col_pitch: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct GainMap {
    points_v: usize,
    points_h: usize,
    spacing_v: f64,
    spacing_h: f64,
    origin_v: f64,
    origin_h: f64,
    map_planes: usize,
    gains: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Line {
    Row,
    Column,
}

#[derive(Debug, Clone, PartialEq)]
enum Opcode {
    MapTable {
        area: Area,
        table: Vec<u16>,
    },
    MapPolynomial {
        area: Area,
        coefficients: Vec<f64>,
    },
    GainMap {
        area: Area,
        map: GainMap,
    },
    /// DeltaPerRow and DeltaPerColumn.
    Delta {
        area: Area,
        line: Line,
        values: Vec<f32>,
    },
    /// ScalePerRow and ScalePerColumn.
    Scale {
        area: Area,
        line: Line,
        values: Vec<f32>,
    },
    FixBadPixelsConstant {
        constant: u32,
        phase: usize,
    },
    FixBadPixelsList {
        phase: usize,
        points: Vec<(usize, usize)>,
        /// Top, left, bottom and right of each bad rectangle.
        rects: Vec<[usize; 4]>,
    },
    WarpRectilinear {
        /// kr0..kr3 and kt0, kt1 of one or of each plane.
        planes: Vec<[f64; 6]>,
        /// Optical centre, relative to the image size.
        center: [f64; 2],
    },
    FixVignetteRadial {
        k: [f64; 5],
        center: [f64; 2],
    },
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.pos.checked_add(N)?;
        let bytes = self.buf.get(self.pos..end)?.try_into().ok()?;
        self.pos = end;
        Some(bytes)
    }
    fn u16(&mut self) -> Option<u16> {
        self.bytes().map(u16::from_be_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.bytes().map(u32::from_be_bytes)
    }
    fn usize(&mut self) -> Option<usize> {
        self.u32().map(|v| v as usize)
    }
    fn f32(&mut self) -> Option<f32> {
        self.bytes().map(f32::from_be_bytes)
    }
    fn f64(&mut self) -> Option<f64> {
        self.bytes().map(f64::from_be_bytes)
    }
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Reads `count` values of `size` bytes, refusing counts the data can't hold.
    fn many<T>(
        &mut self,
        count: usize,
        size: usize,
        read: impl Fn(&mut Self) -> Option<T>,
    ) -> Option<Vec<T>> {
        if self.remaining() < count.checked_mul(size)? {
            return None;
        }
        (0..count).map(|_| read(self)).collect()
    }

    fn finite_f64(&mut self) -> Option<f64> {
        self.f64().filter(|v| v.is_finite())
    }

    fn area(&mut self) -> Option<Area> {
        Some(Area {
            top: self.usize()?,
            left: self.usize()?,
            bottom: self.usize()?,
            right: self.usize()?,
            plane: self.usize()?,
            planes: self.usize()?,
            row_pitch: self.usize()?.max(1),
            col_pitch: self.usize()?.max(1),
        })
    }
}

fn parse_opcode(id: u32, params: &[u8]) -> Option<Opcode> {
    let mut r = Reader {
        buf: params,
        pos: 0,
    };
    match id {
        MAP_TABLE => {
            let area = r.area()?;
            let size = r.usize()?;
            if size == 0 || size > 65536 {
                return None;
            }
            let table = r.many(size, 2, Reader::u16)?;
            Some(Opcode::MapTable { area, table })
        }
        MAP_POLYNOMIAL => {
            let area = r.area()?;
            let degree = r.usize()?;
            // The specification limits the degree to 8
            if degree > 8 {
                return None;
            }
            let coefficients = r.many(degree + 1, 8, Reader::finite_f64)?;
            Some(Opcode::MapPolynomial { area, coefficients })
        }
        GAIN_MAP => {
            let area = r.area()?;
            let points_v = r.usize()?;
            let points_h = r.usize()?;
            let spacing_v = r.finite_f64()?;
            let spacing_h = r.finite_f64()?;
            let origin_v = r.finite_f64()?;
            let origin_h = r.finite_f64()?;
            let map_planes = r.usize()?;
            let count = points_v.checked_mul(points_h)?.checked_mul(map_planes)?;
            if count == 0 {
                return None;
            }
            let gains = r.many(count, 4, Reader::f32)?;
            Some(Opcode::GainMap {
                area,
                map: GainMap {
                    points_v,
                    points_h,
                    spacing_v,
                    spacing_h,
                    origin_v,
                    origin_h,
                    map_planes,
                    gains,
                },
            })
        }
        DELTA_PER_ROW | DELTA_PER_COLUMN | SCALE_PER_ROW | SCALE_PER_COLUMN => {
            let area = r.area()?;
            let count = r.usize()?;
            let values = r.many(count, 4, Reader::f32)?;
            if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
                return None;
            }
            let line = if matches!(id, DELTA_PER_ROW | SCALE_PER_ROW) {
                Line::Row
            } else {
                Line::Column
            };
            Some(if matches!(id, DELTA_PER_ROW | DELTA_PER_COLUMN) {
                Opcode::Delta { area, line, values }
            } else {
                Opcode::Scale { area, line, values }
            })
        }
        FIX_BAD_PIXELS_CONSTANT => Some(Opcode::FixBadPixelsConstant {
            constant: r.u32()?,
            phase: r.usize()? % 4,
        }),
        FIX_BAD_PIXELS_LIST => {
            let phase = r.usize()? % 4;
            let point_count = r.usize()?;
            let rect_count = r.usize()?;
            let points = r.many(point_count, 8, |r| Some((r.usize()?, r.usize()?)))?;
            let rects = r.many(rect_count, 16, |r| {
                Some([r.usize()?, r.usize()?, r.usize()?, r.usize()?])
            })?;
            Some(Opcode::FixBadPixelsList {
                phase,
                points,
                rects,
            })
        }
        WARP_RECTILINEAR => {
            let count = r.usize()?;
            if count != 1 && count != 3 {
                return None;
            }
            let planes = r.many(count, 48, |r| {
                Some([
                    r.finite_f64()?,
                    r.finite_f64()?,
                    r.finite_f64()?,
                    r.finite_f64()?,
                    r.finite_f64()?,
                    r.finite_f64()?,
                ])
            })?;
            let center = [r.finite_f64()?, r.finite_f64()?];
            Some(Opcode::WarpRectilinear { planes, center })
        }
        FIX_VIGNETTE_RADIAL => {
            let k = [
                r.finite_f64()?,
                r.finite_f64()?,
                r.finite_f64()?,
                r.finite_f64()?,
                r.finite_f64()?,
            ];
            let center = [r.finite_f64()?, r.finite_f64()?];
            Some(Opcode::FixVignetteRadial { k, center })
        }
        _ => None,
    }
}

/// Parses the opcodes of an opcode list this module can apply, skipping all
/// others. A malformed list yields the opcodes before the damage.
fn parse(buf: &[u8]) -> Vec<Opcode> {
    let mut r = Reader { buf, pos: 0 };
    let Some(count) = r.u32() else {
        return Vec::new();
    };
    let mut opcodes = Vec::new();
    for _ in 0..count {
        let (Some(id), Some(_version), Some(flags), Some(size)) =
            (r.u32(), r.u32(), r.u32(), r.usize())
        else {
            break;
        };
        let Some(params) = r.pos.checked_add(size).and_then(|end| buf.get(r.pos..end)) else {
            break;
        };
        r.pos += size;
        if flags & FLAG_PREVIEW_ONLY != 0 {
            continue;
        }
        match parse_opcode(id, params) {
            Some(opcode) => opcodes.push(opcode),
            None => log::debug!("DNG opcode list: skipping opcode {id}"),
        }
    }
    opcodes
}

impl GainMap {
    fn interpolate(position: f64, origin: f64, spacing: f64, points: usize) -> (usize, usize, f32) {
        let index = if spacing > 0.0 {
            ((position - origin) / spacing).clamp(0.0, (points - 1) as f64)
        } else {
            0.0
        };
        let i0 = index.floor() as usize;
        let i1 = (i0 + 1).min(points - 1);
        (i0, i1, (index - i0 as f64) as f32)
    }

    fn gain(&self, v: f64, h: f64, plane: usize) -> f32 {
        let (v0, v1, fv) = Self::interpolate(v, self.origin_v, self.spacing_v, self.points_v);
        let (h0, h1, fh) = Self::interpolate(h, self.origin_h, self.spacing_h, self.points_h);
        let at = |v: usize, h: usize| self.gains[(v * self.points_h + h) * self.map_planes + plane];
        let top = at(v0, h0) * (1.0 - fh) + at(v0, h1) * fh;
        let bottom = at(v1, h0) * (1.0 - fh) + at(v1, h1) * fh;
        top * (1.0 - fv) + bottom * fv
    }
}

/// Geometry of the raw data an opcode list is applied to.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    /// Pixels per row of the raw data.
    width: usize,
    cpp: usize,
    /// The part of the data the opcode coordinates are relative to, the whole
    /// image for OpcodeList1 and the ActiveArea for OpcodeList2.
    bounds: Rect,
    /// Whether the data is a single plane Bayer mosaic.
    bayer: bool,
}

/// Geometry and levels of the raw data the opcodes are applied to.
struct Layout {
    geometry: Geometry,
    black: BlackLevel,
    white: Vec<f32>,
}

impl Layout {
    fn black(&self, row: usize, col: usize, c: usize) -> f32 {
        let b = &self.black;
        let (w, h, cpp) = (b.width.max(1), b.height.max(1), b.cpp.max(1));
        let index = ((row % h) * w + col % w) * cpp + c % cpp;
        b.levels
            .get(index)
            .or(b.levels.first())
            .map_or(0.0, |l| l.as_f32())
    }

    fn white(&self, c: usize) -> f32 {
        self.white
            .get(c)
            .or(self.white.first())
            .copied()
            .unwrap_or(u16::MAX as f32)
    }
}

fn apply_area_opcode(opcode: &Opcode, geometry: &Geometry, normalized: &mut [f32]) {
    let area = match opcode {
        Opcode::MapTable { area, .. }
        | Opcode::MapPolynomial { area, .. }
        | Opcode::GainMap { area, .. }
        | Opcode::Delta { area, .. }
        | Opcode::Scale { area, .. } => area,
        _ => return,
    };
    let bounds = geometry.bounds;
    let bottom = area.bottom.min(bounds.d.h);
    let right = area.right.min(bounds.d.w);
    let planes = area.plane..area.plane.saturating_add(area.planes).min(geometry.cpp);
    if area.top >= bottom || area.left >= right || planes.is_empty() {
        return;
    }
    let stride = geometry.width * geometry.cpp;
    let (height, width) = (bounds.d.h as f64, bounds.d.w as f64);
    normalized
        .par_chunks_exact_mut(stride)
        .enumerate()
        .skip(bounds.p.y + area.top)
        .take(bottom - area.top)
        .filter(|(row, _)| (row - bounds.p.y - area.top).is_multiple_of(area.row_pitch))
        .for_each(|(row, line)| {
            let row = row - bounds.p.y;
            let v = row as f64 / height;
            let row_index = (row - area.top) / area.row_pitch;
            for col in (area.left..right).step_by(area.col_pitch) {
                let h = col as f64 / width;
                let col_index = (col - area.left) / area.col_pitch;
                let base = (bounds.p.x + col) * geometry.cpp;
                for c in planes.clone() {
                    let x = &mut line[base + c];
                    *x = match opcode {
                        Opcode::MapTable { table, .. } => {
                            let index = (x.clamp(0.0, 1.0) * 65535.0).round() as usize;
                            table[index.min(table.len() - 1)] as f32 / 65535.0
                        }
                        Opcode::MapPolynomial { coefficients, .. } => {
                            let x = *x as f64;
                            let y = coefficients.iter().rev().fold(0.0, |acc, c| acc * x + c);
                            y.max(0.0) as f32
                        }
                        Opcode::GainMap { map, .. } => {
                            let plane = (c - area.plane).min(map.map_planes - 1);
                            *x * map.gain(v, h, plane)
                        }
                        Opcode::Delta { line, values, .. } | Opcode::Scale { line, values, .. } => {
                            let index = match line {
                                Line::Row => row_index,
                                Line::Column => col_index,
                            };
                            let value = values[index.min(values.len() - 1)];
                            if matches!(opcode, Opcode::Delta { .. }) {
                                *x + value
                            } else {
                                *x * value
                            }
                        }
                        _ => *x,
                    };
                }
            }
        });
}

/// Neighbours of the same colour used to replace a bad pixel.
fn neighbours(geometry: &Geometry, phase: usize, row: usize, col: usize) -> &'static [(i64, i64)] {
    const ADJACENT: [(i64, i64); 8] = [
        (-1, 0),
        (1, 0),
        (0, -1),
        (0, 1),
        (-1, -1),
        (-1, 1),
        (1, -1),
        (1, 1),
    ];
    const GREEN: [(i64, i64); 8] = [
        (-1, -1),
        (-1, 1),
        (1, -1),
        (1, 1),
        (-2, 0),
        (2, 0),
        (0, -2),
        (0, 2),
    ];
    const RED_BLUE: [(i64, i64); 8] = [
        (-2, 0),
        (2, 0),
        (0, -2),
        (0, 2),
        (-2, -2),
        (-2, 2),
        (2, -2),
        (2, 2),
    ];
    if !geometry.bayer {
        return &ADJACENT;
    }
    // Phase 0 and 3 start with red or blue, 1 and 2 with green
    let green_parity = usize::from(phase == 0 || phase == 3);
    if (row + col) % 2 == green_parity {
        &GREEN
    } else {
        &RED_BLUE
    }
}

/// Replaces bad pixels by the mean of their good neighbours of the same colour.
/// Coordinates are relative to the bounds of the geometry.
fn fix_bad_pixels(
    data: &mut [f32],
    geometry: &Geometry,
    phase: usize,
    bad: impl IntoIterator<Item = (usize, usize)>,
    is_bad: impl Fn(usize, usize, f32) -> bool,
) {
    let Geometry {
        width, cpp, bounds, ..
    } = *geometry;
    let (h, w) = (bounds.d.h as i64, bounds.d.w as i64);
    let index = |row: usize, col: usize, c: usize| {
        ((bounds.p.y + row) * width + bounds.p.x + col) * cpp + c
    };
    let mut fixes = Vec::new();
    for (row, col) in bad {
        if row >= bounds.d.h || col >= bounds.d.w {
            continue;
        }
        for c in 0..cpp {
            let (mut sum, mut count) = (0.0f32, 0u32);
            for &(dy, dx) in neighbours(geometry, phase, row, col) {
                let (y, x) = (row as i64 + dy, col as i64 + dx);
                if y < 0 || x < 0 || y >= h || x >= w {
                    continue;
                }
                let (y, x) = (y as usize, x as usize);
                let value = data[index(y, x, c)];
                if !is_bad(y, x, value) {
                    sum += value;
                    count += 1;
                }
            }
            if count > 0 {
                fixes.push((index(row, col, c), sum / count as f32));
            }
        }
    }
    for (i, value) in fixes {
        data[i] = value;
    }
}

fn apply_bad_pixel_opcode(
    opcode: &Opcode,
    geometry: &Geometry,
    data: &mut [f32],
    constant_value: impl Fn(u32) -> f32,
) {
    let bounds = geometry.bounds;
    match opcode {
        Opcode::FixBadPixelsConstant { constant, phase } => {
            let constant = constant_value(*constant);
            let stride = geometry.width * geometry.cpp;
            let value = |row: usize, col: usize| {
                data[(bounds.p.y + row) * stride + (bounds.p.x + col) * geometry.cpp]
            };
            let bad: Vec<(usize, usize)> = (0..bounds.d.h)
                .flat_map(|row| (0..bounds.d.w).map(move |col| (row, col)))
                .filter(|&(row, col)| value(row, col) == constant)
                .take(MAX_BAD_PIXELS)
                .collect();
            if !bad.is_empty() {
                log::debug!("DNG FixBadPixelsConstant: fixing {} pixels", bad.len());
                fix_bad_pixels(data, geometry, *phase, bad, |_, _, v| v == constant);
            }
        }
        Opcode::FixBadPixelsList {
            phase,
            points,
            rects,
        } => {
            let mut bad: HashSet<(usize, usize)> = points.iter().copied().collect();
            for &[top, left, bottom, right] in rects {
                let (bottom, right) = (bottom.min(bounds.d.h), right.min(bounds.d.w));
                let size = bottom.saturating_sub(top) * right.saturating_sub(left);
                if bad.len().saturating_add(size) > MAX_BAD_PIXELS {
                    return;
                }
                bad.extend((top..bottom).flat_map(|row| (left..right).map(move |col| (row, col))));
            }
            let listed = bad.clone();
            fix_bad_pixels(data, geometry, *phase, bad, |row, col, _| {
                listed.contains(&(row, col))
            });
        }
        _ => {}
    }
}

fn apply_opcode(opcode: &Opcode, geometry: &Geometry, normalized: &mut [f32]) {
    match opcode {
        Opcode::FixBadPixelsConstant { .. } | Opcode::FixBadPixelsList { .. } => {
            // Constants refer to 16 bit data
            apply_bad_pixel_opcode(opcode, geometry, normalized, |c| c as f32 / 65535.0)
        }
        _ => apply_area_opcode(opcode, geometry, normalized),
    }
}

/// Applies the opcodes to integer raw data, which is rescaled to black 0 and
/// white [`OUTPUT_WHITE`], and returns the new levels.
fn apply_integer(
    opcodes: &[Opcode],
    layout: &Layout,
    data: &mut [u16],
) -> (BlackLevel, WhiteLevel) {
    let geometry = &layout.geometry;
    let stride = geometry.width * geometry.cpp;
    let mut normalized: Vec<f32> = data
        .par_chunks_exact(stride)
        .enumerate()
        .flat_map_iter(|(row, line)| {
            line.iter().enumerate().map(move |(i, &v)| {
                let (col, c) = (i / geometry.cpp, i % geometry.cpp);
                let black = layout.black(row, col, c);
                (v as f32 - black) / (layout.white(c) - black).max(1.0)
            })
        })
        .collect();
    for opcode in opcodes {
        apply_opcode(opcode, geometry, &mut normalized);
    }
    data.par_iter_mut()
        .zip(normalized.par_iter())
        .for_each(|(v, &x)| {
            *v = (x * OUTPUT_WHITE as f32)
                .round()
                .clamp(0.0, u16::MAX as f32) as u16;
        });
    (
        BlackLevel::zero(1, 1, geometry.cpp),
        WhiteLevel::new(vec![OUTPUT_WHITE; geometry.cpp]),
    )
}

/// Applies the opcodes to floating point raw data in place, keeping its levels.
fn apply_float(opcodes: &[Opcode], layout: &Layout, data: &mut [f32]) {
    let geometry = &layout.geometry;
    let stride = geometry.width * geometry.cpp;
    let levels = |row: usize, i: usize| {
        let (col, c) = (i / geometry.cpp, i % geometry.cpp);
        let black = layout.black(row, col, c);
        (black, (layout.white(c) - black).max(f32::MIN_POSITIVE))
    };
    data.par_chunks_exact_mut(stride)
        .enumerate()
        .for_each(|(row, line)| {
            for (i, v) in line.iter_mut().enumerate() {
                let (black, range) = levels(row, i);
                *v = (*v - black) / range;
            }
        });
    for opcode in opcodes {
        apply_opcode(opcode, geometry, data);
    }
    data.par_chunks_exact_mut(stride)
        .enumerate()
        .for_each(|(row, line)| {
            for (i, v) in line.iter_mut().enumerate() {
                let (black, range) = levels(row, i);
                *v = *v * range + black;
            }
        });
}

/// The geometry of a decoded raw image, or None if it can't hold its bounds.
fn geometry(raw_image: &RawImage, bounds: Rect) -> Option<Geometry> {
    if raw_image.cpp == 0
        || raw_image.width == 0
        || bounds.p.x + bounds.d.w > raw_image.width
        || bounds.p.y + bounds.d.h > raw_image.height
    {
        return None;
    }
    Some(Geometry {
        width: raw_image.width,
        cpp: raw_image.cpp,
        bounds,
        bayer: raw_image.cpp == 1
            && matches!(raw_image.photometric, RawPhotometricInterpretation::Cfa(_)),
    })
}

fn samples(raw_image: &RawImage) -> usize {
    raw_image.width * raw_image.height * raw_image.cpp
}

fn full_bounds(raw_image: &RawImage) -> Rect {
    Rect::new(Point::zero(), Dim2::new(raw_image.width, raw_image.height))
}

/// Applies the bad pixel opcodes of OpcodeList1 to the stored values.
fn apply_list1(opcodes: &[Opcode], raw_image: &mut RawImage) {
    let Some(geometry) = geometry(raw_image, full_bounds(raw_image)) else {
        return;
    };
    let samples = samples(raw_image);
    let opcodes = opcodes.iter().filter(|opcode| {
        matches!(
            opcode,
            Opcode::FixBadPixelsConstant { .. } | Opcode::FixBadPixelsList { .. }
        )
    });
    match &mut raw_image.data {
        RawImageData::Integer(data) if data.len() >= samples => {
            let mut values: Vec<f32> = data[..samples].par_iter().map(|&v| v as f32).collect();
            for opcode in opcodes {
                apply_bad_pixel_opcode(opcode, &geometry, &mut values, |c| c as f32);
            }
            data[..samples]
                .par_iter_mut()
                .zip(values.par_iter())
                .for_each(|(v, &x)| *v = x.round().clamp(0.0, u16::MAX as f32) as u16);
        }
        RawImageData::Float(data) if data.len() >= samples => {
            for opcode in opcodes {
                apply_bad_pixel_opcode(opcode, &geometry, &mut data[..samples], |c| c as f32);
            }
        }
        _ => {}
    }
}

fn apply_list2(opcodes: &[Opcode], raw_image: &mut RawImage) {
    let bounds = raw_image
        .active_area
        .unwrap_or_else(|| full_bounds(raw_image));
    let Some(geometry) = geometry(raw_image, bounds) else {
        return;
    };
    let samples = samples(raw_image);
    let layout = Layout {
        geometry,
        black: raw_image.blacklevel.clone(),
        white: raw_image.whitelevel.as_vec(),
    };
    match &mut raw_image.data {
        RawImageData::Integer(data) if data.len() >= samples => {
            let (black, white) = apply_integer(opcodes, &layout, &mut data[..samples]);
            raw_image.blacklevel = black;
            raw_image.whitelevel = white;
        }
        RawImageData::Float(data) if data.len() >= samples => {
            apply_float(opcodes, &layout, &mut data[..samples]);
        }
        _ => {}
    }
}

fn opcode_list(decoder: &dyn Decoder, tag: DngTag) -> Vec<Opcode> {
    if decoder.format_hint() != FormatHint::DNG {
        return Vec::new();
    }
    let Ok(Some(ifd)) = decoder.ifd(WellKnownIFD::VirtualDngRawTags) else {
        return Vec::new();
    };
    ifd.get_entry(tag)
        .map(|entry| parse(entry.value.get_data()))
        .unwrap_or_default()
}

/// Applies the opcodes of a DNG's OpcodeList1 and OpcodeList2 that work on
/// raw data to its decoded raw image: bad pixel fixes, MapTable,
/// MapPolynomial, GainMap and the per row and per column deltas and scales.
pub fn apply_raw_opcodes(decoder: &dyn Decoder, raw_image: &mut RawImage) {
    let list1 = opcode_list(decoder, DngTag::OpcodeList1);
    if !list1.is_empty() {
        log::debug!("DNG OpcodeList1: applying {} opcodes", list1.len());
        apply_list1(&list1, raw_image);
    }
    let list2 = opcode_list(decoder, DngTag::OpcodeList2);
    if !list2.is_empty() {
        log::debug!("DNG OpcodeList2: applying {} opcodes", list2.len());
        apply_list2(&list2, raw_image);
    }
}

/// Where a developed image lies in the image OpcodeList3 refers to, the
/// demosaiced ActiveArea, in the pixels of the developed image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Position of the first pixel of the developed image.
    pub origin: [f64; 2],
    /// Size of the whole ActiveArea.
    pub size: [f64; 2],
}

impl Placement {
    /// A developed image showing `crop` of an ActiveArea of `size`.
    pub fn new(crop: Rect, size: Dim2) -> Self {
        Self {
            origin: [crop.p.x as f64, crop.p.y as f64],
            size: [size.w as f64, size.h as f64],
        }
    }
}

/// The radial frame of an opcode: the optical centre in pixels and the
/// distance from it to the farthest corner of the image.
struct Radial {
    center: [f64; 2],
    radius: f64,
}

impl Radial {
    fn new(center: [f64; 2], placement: &Placement) -> Option<Self> {
        let [w, h] = placement.size.map(|s| (s - 1.0).max(0.0));
        let center = [center[0] * w, center[1] * h];
        let radius = [[0.0, 0.0], [w, 0.0], [0.0, h], [w, h]]
            .iter()
            .map(|[x, y]| (x - center[0]).hypot(y - center[1]))
            .fold(0.0, f64::max);
        (radius > 0.0).then_some(Self { center, radius })
    }

    /// Offset of a developed image pixel from the centre, in radii.
    fn offset(&self, placement: &Placement, x: usize, y: usize) -> [f64; 2] {
        [
            (x as f64 + placement.origin[0] - self.center[0]) / self.radius,
            (y as f64 + placement.origin[1] - self.center[1]) / self.radius,
        ]
    }
}

fn sample_bilinear(image: &Rgb32FImage, x: f64, y: f64, c: usize) -> f32 {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let x = x.clamp(0.0, (w - 1) as f64);
    let y = y.clamp(0.0, (h - 1) as f64);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
    let data = image.as_raw();
    let at = |x: usize, y: usize| data[(y * w + x) * 3 + c];
    let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
    let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
    top * (1.0 - fy) + bottom * fy
}

fn warp_rectilinear(
    image: &Rgb32FImage,
    placement: &Placement,
    planes: &[[f64; 6]],
    center: [f64; 2],
) -> Option<Rgb32FImage> {
    let radial = Radial::new(center, placement)?;
    let (w, h) = (image.width() as usize, image.height() as usize);
    let mut out = vec![0.0f32; w * h * 3];
    out.par_chunks_exact_mut(w * 3)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..w {
                let [dx, dy] = radial.offset(placement, x, y);
                let r2 = dx * dx + dy * dy;
                for c in 0..3 {
                    let [kr0, kr1, kr2, kr3, kt0, kt1] = planes[c.min(planes.len() - 1)];
                    let ratio = kr0 + r2 * (kr1 + r2 * (kr2 + r2 * kr3));
                    let sx = dx * ratio + kt1 * (r2 + 2.0 * dx * dx) + 2.0 * kt0 * dx * dy;
                    let sy = dy * ratio + kt0 * (r2 + 2.0 * dy * dy) + 2.0 * kt1 * dx * dy;
                    let src_x = radial.center[0] + sx * radial.radius - placement.origin[0];
                    let src_y = radial.center[1] + sy * radial.radius - placement.origin[1];
                    row[x * 3 + c] = sample_bilinear(image, src_x, src_y, c);
                }
            }
        });
    Rgb32FImage::from_raw(w as u32, h as u32, out)
}

fn fix_vignette_radial(
    image: &mut Rgb32FImage,
    placement: &Placement,
    k: [f64; 5],
    center: [f64; 2],
) {
    let Some(radial) = Radial::new(center, placement) else {
        return;
    };
    let w = image.width() as usize;
    image
        .par_chunks_exact_mut(w * 3)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, pixel) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let [dx, dy] = radial.offset(placement, x, y);
                let r2 = dx * dx + dy * dy;
                let gain = 1.0 + r2 * (k[0] + r2 * (k[1] + r2 * (k[2] + r2 * (k[3] + r2 * k[4]))));
                pixel.iter_mut().for_each(|v| *v *= gain as f32);
            }
        });
}

/// The WarpRectilinear and FixVignetteRadial opcodes of a DNG's OpcodeList3.
#[derive(Debug, Clone, PartialEq)]
pub struct OpcodeList3 {
    opcodes: Vec<Opcode>,
}

impl OpcodeList3 {
    pub fn read(decoder: &dyn Decoder) -> Option<Self> {
        let opcodes: Vec<Opcode> = opcode_list(decoder, DngTag::OpcodeList3)
            .into_iter()
            .filter(|opcode| {
                matches!(
                    opcode,
                    Opcode::WarpRectilinear { .. } | Opcode::FixVignetteRadial { .. }
                )
            })
            .collect();
        (!opcodes.is_empty()).then_some(Self { opcodes })
    }

    /// Applies the opcodes to a developed image that is not yet rotated to
    /// its orientation.
    pub fn apply(&self, mut image: Rgb32FImage, placement: Placement) -> Rgb32FImage {
        if image.width() == 0 || image.height() == 0 {
            return image;
        }
        for opcode in &self.opcodes {
            match opcode {
                Opcode::WarpRectilinear { planes, center } => {
                    if let Some(warped) = warp_rectilinear(&image, &placement, planes, *center) {
                        image = warped;
                    }
                }
                Opcode::FixVignetteRadial { k, center } => {
                    fix_vignette_radial(&mut image, &placement, *k, *center);
                }
                _ => {}
            }
        }
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opcode_with_flags(id: u32, flags: u32, params: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [id, 0x0103_0000, flags, params.len() as u32] {
            v.extend_from_slice(&x.to_be_bytes());
        }
        v.extend_from_slice(params);
        v
    }

    fn opcode(id: u32, params: &[u8]) -> Vec<u8> {
        opcode_with_flags(id, 1, params)
    }

    fn words(a: &[u32]) -> Vec<u8> {
        a.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    fn doubles(a: &[f64]) -> Vec<u8> {
        a.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    fn polynomial(a: [u32; 8], coefficients: &[f64]) -> Vec<u8> {
        let mut p = words(&a);
        p.extend_from_slice(&(coefficients.len() as u32 - 1).to_be_bytes());
        p.extend(doubles(coefficients));
        opcode(MAP_POLYNOMIAL, &p)
    }

    fn table(a: [u32; 8], values: &[u16]) -> Vec<u8> {
        let mut p = words(&a);
        p.extend_from_slice(&(values.len() as u32).to_be_bytes());
        values
            .iter()
            .for_each(|v| p.extend_from_slice(&v.to_be_bytes()));
        opcode(MAP_TABLE, &p)
    }

    fn gain_map(a: [u32; 8], points: (u32, u32), spacing: (f64, f64), gains: &[f32]) -> Vec<u8> {
        let mut p = words(&a);
        p.extend(words(&[points.0, points.1]));
        p.extend(doubles(&[spacing.0, spacing.1, 0.0, 0.0]));
        p.extend_from_slice(&1u32.to_be_bytes());
        gains
            .iter()
            .for_each(|g| p.extend_from_slice(&g.to_be_bytes()));
        opcode(GAIN_MAP, &p)
    }

    fn per_line(id: u32, a: [u32; 8], values: &[f32]) -> Vec<u8> {
        let mut p = words(&a);
        p.extend_from_slice(&(values.len() as u32).to_be_bytes());
        values
            .iter()
            .for_each(|g| p.extend_from_slice(&g.to_be_bytes()));
        opcode(id, &p)
    }

    fn bad_pixel_list(phase: u32, points: &[[u32; 2]], rects: &[[u32; 4]]) -> Vec<u8> {
        let mut p = words(&[phase, points.len() as u32, rects.len() as u32]);
        points.iter().for_each(|pt| p.extend(words(pt)));
        rects.iter().for_each(|r| p.extend(words(r)));
        opcode(FIX_BAD_PIXELS_LIST, &p)
    }

    fn warp(planes: &[[f64; 6]], center: [f64; 2]) -> Vec<u8> {
        let mut p = words(&[planes.len() as u32]);
        planes.iter().for_each(|k| p.extend(doubles(k)));
        p.extend(doubles(&center));
        opcode(WARP_RECTILINEAR, &p)
    }

    fn vignette(k: [f64; 5], center: [f64; 2]) -> Vec<u8> {
        let mut p = doubles(&k);
        p.extend(doubles(&center));
        opcode(FIX_VIGNETTE_RADIAL, &p)
    }

    pub(crate) fn list(ops: &[Vec<u8>]) -> Vec<u8> {
        let mut v = (ops.len() as u32).to_be_bytes().to_vec();
        ops.iter().for_each(|o| v.extend_from_slice(o));
        v
    }

    fn geometry(width: usize, height: usize, cpp: usize, bayer: bool) -> Geometry {
        Geometry {
            width,
            cpp,
            bounds: Rect::new(Point::zero(), Dim2::new(width, height)),
            bayer,
        }
    }

    fn layout(width: usize, height: usize, cpp: usize, black: u16, white: u32) -> Layout {
        Layout {
            geometry: geometry(width, height, cpp, false),
            black: BlackLevel::new(&[black], 1, 1, 1),
            white: vec![white as f32],
        }
    }

    fn to_white(x: f32) -> u16 {
        (x * OUTPUT_WHITE as f32).round() as u16
    }

    #[test]
    fn parses_known_opcodes_and_skips_others() {
        let all = [0, 0, 2, 2, 0, 1, 1, 1];
        let ops = parse(&list(&[
            opcode(2, &[9; 12]),
            polynomial(all, &[0.0, 2.0]),
            table(all, &[0, 65535]),
            gain_map(all, (1, 1), (1.0, 1.0), &[2.0]),
            opcode_with_flags(MAP_POLYNOMIAL, FLAG_PREVIEW_ONLY, &[0; 44]),
            opcode(FIX_BAD_PIXELS_CONSTANT, &words(&[0, 1])),
            warp(&[[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]], [0.5, 0.5]),
        ]));
        assert_eq!(ops.len(), 5);
        assert!(
            matches!(&ops[0], Opcode::MapPolynomial { coefficients, .. } if coefficients == &[0.0, 2.0])
        );
        assert!(matches!(&ops[1], Opcode::MapTable { table, .. } if table == &[0, 65535]));
        assert!(matches!(&ops[2], Opcode::GainMap { map, .. } if map.gains == [2.0]));
        assert_eq!(
            ops[3],
            Opcode::FixBadPixelsConstant {
                constant: 0,
                phase: 1
            }
        );
        assert!(matches!(&ops[4], Opcode::WarpRectilinear { planes, center }
            if planes.len() == 1 && center == &[0.5, 0.5]));
    }

    #[test]
    fn per_plane_polynomials_rescale_to_output_white() {
        // Two pixels of three planes, 8 bit data with a different curve per plane
        let all = |plane| [0, 0, 1, 2, plane, 1, 1, 1];
        let ops = parse(&list(&[
            polynomial(all(0), &[0.0, 0.0, 1.0]),
            polynomial(all(1), &[0.0, 1.0]),
            polynomial(all(2), &[0.5]),
        ]));
        let mut data = vec![255, 255, 0, 51, 102, 255];
        let mut layout = layout(2, 1, 3, 0, 255);
        layout.black = BlackLevel::new(&[0u16, 0, 0], 1, 1, 3);
        let (black, white) = apply_integer(&ops, &layout, &mut data);
        assert_eq!(
            data,
            vec![
                to_white(1.0),
                to_white(1.0),
                to_white(0.5),
                to_white(0.04),
                to_white(0.4),
                to_white(0.5)
            ]
        );
        assert_eq!(white.0, vec![OUTPUT_WHITE; 3]);
        assert_eq!(black.as_vec(), vec![0.0; 3]);
    }

    #[test]
    fn subtracts_black_before_mapping() {
        let ops = parse(&list(&[polynomial([0, 0, 1, 2, 0, 1, 1, 1], &[0.0, 0.5])]));
        let mut data = vec![1000, 9000];
        apply_integer(&ops, &layout(2, 1, 1, 1000, 9000), &mut data);
        assert_eq!(data, vec![0, to_white(0.5)]);
    }

    #[test]
    fn table_maps_sixteen_bit_values() {
        let ops = parse(&list(&[table(
            [0, 0, 1, 2, 0, 1, 1, 1],
            &[0, 32768, 65535],
        )]));
        // Values beyond the table use its last entry
        let mut data = vec![0, 65535];
        apply_integer(&ops, &layout(2, 1, 1, 0, 65535), &mut data);
        assert_eq!(data, vec![0, OUTPUT_WHITE as u16]);
    }

    #[test]
    fn gain_map_follows_pitch_and_interpolates() {
        // 4x4 Bayer image, the gain map covers only the top left pixel of each 2x2 cell
        // and rises from 1.0 on the left to 2.0 on the right
        let ops = parse(&list(&[gain_map(
            [0, 0, 4, 4, 0, 1, 2, 2],
            (1, 2),
            (1.0, 0.5),
            &[1.0, 2.0],
        )]));
        let mut data = vec![1000u16; 16];
        apply_integer(&ops, &layout(4, 4, 1, 0, 4000), &mut data);
        let quarter = to_white(0.25);
        // Columns 0 and 2 map to 0 and 0.5 of the width, the gain at 0.5 is past the
        // second map point and clamps to 2.0
        assert_eq!(data[0], quarter);
        assert_eq!(data[2], to_white(0.5));
        assert_eq!(data[8], quarter);
        assert_eq!(data[10], to_white(0.5));
        for i in [1, 3, 4, 5, 6, 7, 9, 11, 12, 13, 14, 15] {
            assert_eq!(data[i], quarter, "pixel {i} is outside the pitch");
        }
    }

    #[test]
    fn gain_map_gains_above_one_keep_headroom() {
        let ops = parse(&list(&[gain_map(
            [0, 0, 1, 1, 0, 1, 1, 1],
            (1, 1),
            (1.0, 1.0),
            &[1.8],
        )]));
        let mut data = vec![4000u16];
        apply_integer(&ops, &layout(1, 1, 1, 0, 4000), &mut data);
        assert_eq!(data, vec![to_white(1.8)]);
    }

    #[test]
    fn per_row_and_column_deltas_and_scales() {
        let all = [0, 0, 2, 2, 0, 1, 1, 1];
        let ops = parse(&list(&[
            per_line(DELTA_PER_ROW, all, &[0.0, 0.25]),
            per_line(SCALE_PER_COLUMN, all, &[1.0, 2.0]),
        ]));
        let mut data = vec![25u16; 4];
        apply_integer(&ops, &layout(2, 2, 1, 0, 100), &mut data);
        assert_eq!(
            data,
            vec![to_white(0.25), to_white(0.5), to_white(0.5), to_white(1.0)]
        );
    }

    #[test]
    fn coordinates_are_relative_to_the_active_area() {
        let ops = parse(&list(&[polynomial([0, 0, 1, 1, 0, 1, 1, 1], &[1.0])]));
        let mut layout = layout(3, 2, 1, 0, 100);
        layout.geometry.bounds = Rect::new(Point::new(1, 1), Dim2::new(2, 1));
        let mut data = vec![0u16; 6];
        apply_integer(&ops, &layout, &mut data);
        assert_eq!(data, vec![0, 0, 0, 0, OUTPUT_WHITE as u16, 0]);
    }

    #[test]
    fn float_data_keeps_its_scale() {
        let ops = parse(&list(&[polynomial(
            [0, 0, 1, 2, 0, 1, 1, 1],
            &[0.0, 0.0, 1.0],
        )]));
        let mut data = vec![0.5f32, 2.0];
        apply_float(&ops, &layout(2, 1, 1, 0, 2), &mut data);
        assert_eq!(data, vec![0.125, 2.0]);
    }

    #[test]
    fn bad_bayer_pixels_use_neighbours_of_their_colour() {
        // RGGB 6x6 mosaic, red 100, green 50 and blue 10, with a dead red pixel
        // at 2,2 and a dead green pixel at 2,3
        let mosaic = |row: usize, col: usize| match (row % 2, col % 2) {
            (0, 0) => 100.0,
            (1, 1) => 10.0,
            _ => 50.0,
        };
        let geometry = geometry(6, 6, 1, true);
        let mut data: Vec<f32> = (0..36).map(|i| mosaic(i / 6, i % 6)).collect();
        data[2 * 6 + 2] = 0.0;
        data[2 * 6 + 3] = 0.0;
        let op = &parse(&list(&[opcode(FIX_BAD_PIXELS_CONSTANT, &words(&[0, 0]))]))[0];
        apply_bad_pixel_opcode(op, &geometry, &mut data, |c| c as f32);
        assert_eq!(data[2 * 6 + 2], 100.0);
        assert_eq!(data[2 * 6 + 3], 50.0);

        // The same pixels from a list, with phase 3 (blue first) the colours swap
        // but the green pixel is still at 2,3
        let mut data: Vec<f32> = (0..36).map(|i| mosaic(i / 6, i % 6)).collect();
        data[2 * 6 + 3] = 999.0;
        data[3 * 6 + 3] = 999.0;
        let op = &parse(&list(&[bad_pixel_list(3, &[[2, 3]], &[[3, 3, 4, 4]])]))[0];
        apply_bad_pixel_opcode(op, &geometry, &mut data, |c| c as f32);
        assert_eq!(data[2 * 6 + 3], 50.0);
        assert_eq!(data[3 * 6 + 3], 10.0);
    }

    #[test]
    fn bad_pixel_list_is_relative_to_the_active_area() {
        let mut geometry = geometry(4, 4, 3, false);
        geometry.bounds = Rect::new(Point::new(1, 1), Dim2::new(3, 3));
        let mut data = vec![0.5f32; 4 * 4 * 3];
        let bad = (2 * 4 + 2) * 3;
        data[bad..bad + 3].copy_from_slice(&[9.0, 9.0, 9.0]);
        let op = &parse(&list(&[bad_pixel_list(0, &[[1, 1]], &[])]))[0];
        apply_bad_pixel_opcode(op, &geometry, &mut data, |c| c as f32);
        assert_eq!(&data[bad..bad + 3], &[0.5, 0.5, 0.5]);
    }

    #[test]
    fn list1_fixes_stored_values() {
        let op = parse(&list(&[opcode(FIX_BAD_PIXELS_CONSTANT, &words(&[0, 0]))]));
        let camera = rawler::decoders::Camera::default();
        let mut data = vec![700u16; 16];
        data[5] = 0;
        let mut raw_image = RawImage::new_with_data(
            camera,
            RawImageData::Integer(data),
            4,
            4,
            1,
            [1.0; 4],
            RawPhotometricInterpretation::LinearRaw,
            Some(BlackLevel::new(&[0u16], 1, 1, 1)),
            Some(WhiteLevel::new(vec![1000])),
            false,
        );
        apply_list1(&op, &mut raw_image);
        assert_eq!(raw_image.pixels_u16(), &[700; 16]);
    }

    fn gradient(w: u32, h: u32) -> Rgb32FImage {
        Rgb32FImage::from_fn(w, h, |x, y| {
            image::Rgb([x as f32, y as f32, (x + y) as f32])
        })
    }

    #[test]
    fn identity_warp_keeps_the_image() {
        let list3 = OpcodeList3 {
            opcodes: parse(&list(&[warp(
                &[[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]],
                [0.5, 0.5],
            )])),
        };
        let image = gradient(9, 7);
        let placement = Placement::new(Rect::new(Point::zero(), Dim2::new(9, 7)), Dim2::new(9, 7));
        let warped = list3.apply(image.clone(), placement);
        for (a, b) in warped.as_raw().iter().zip(image.as_raw()) {
            assert!((a - b).abs() < 1e-4);
        }
    }

    #[test]
    fn warp_scales_around_the_optical_centre() {
        // kr0 0.5 samples every pixel from halfway to the centre, a 2x zoom
        let planes = [[0.5, 0.0, 0.0, 0.0, 0.0, 0.0]];
        let image = gradient(9, 9);
        let full = Placement::new(Rect::new(Point::zero(), Dim2::new(9, 9)), Dim2::new(9, 9));
        let warped = warp_rectilinear(&image, &full, &planes, [0.5, 0.5]).unwrap();
        let at = |img: &Rgb32FImage, x: u32, y: u32| img.get_pixel(x, y).0;
        assert_eq!(at(&warped, 4, 4), [4.0, 4.0, 8.0]);
        assert_eq!(at(&warped, 0, 0), [2.0, 2.0, 4.0]);
        assert_eq!(at(&warped, 8, 4), [6.0, 4.0, 10.0]);

        // A crop of the same image warps the same way, the centre stays where the
        // whole ActiveArea puts it
        let crop = image::imageops::crop_imm(&image, 2, 2, 5, 5).to_image();
        let placement = Placement::new(
            Rect::new(Point::new(2, 2), Dim2::new(5, 5)),
            Dim2::new(9, 9),
        );
        let warped_crop = warp_rectilinear(&crop, &placement, &planes, [0.5, 0.5]).unwrap();
        assert_eq!(at(&warped_crop, 2, 2), [4.0, 4.0, 8.0]);
        assert_eq!(at(&warped_crop, 0, 0), [3.0, 3.0, 6.0]);
    }

    #[test]
    fn warp_planes_are_independent() {
        let planes = [
            [1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            [0.5, 0.0, 0.0, 0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ];
        let image = gradient(9, 9);
        let full = Placement::new(Rect::new(Point::zero(), Dim2::new(9, 9)), Dim2::new(9, 9));
        let warped = warp_rectilinear(&image, &full, &planes, [0.5, 0.5]).unwrap();
        assert_eq!(warped.get_pixel(0, 0).0, [0.0, 2.0, 0.0]);
    }

    #[test]
    fn vignette_gain_grows_with_the_radius() {
        let list3 = OpcodeList3 {
            opcodes: parse(&list(&[vignette([1.0, 0.0, 0.0, 0.0, 0.0], [0.5, 0.5])])),
        };
        let image = Rgb32FImage::from_pixel(5, 5, image::Rgb([1.0, 1.0, 1.0]));
        let placement = Placement::new(Rect::new(Point::zero(), Dim2::new(5, 5)), Dim2::new(5, 5));
        let fixed = list3.apply(image, placement);
        assert_eq!(fixed.get_pixel(2, 2).0, [1.0; 3]);
        // The corners are one radius away
        assert_eq!(fixed.get_pixel(0, 0).0, [2.0; 3]);
        assert_eq!(fixed.get_pixel(4, 4).0, [2.0; 3]);
    }

    fn decode(file: &[u8]) -> (RawImage, Option<OpcodeList3>) {
        let source = rawler::rawsource::RawSource::new_from_slice(file);
        let decoder = rawler::get_decoder(&source).unwrap();
        let params = rawler::decoders::RawDecodeParams::default();
        let mut raw_image = decoder.raw_image(&source, &params, false).unwrap();
        apply_raw_opcodes(decoder.as_ref(), &mut raw_image);
        (raw_image, OpcodeList3::read(decoder.as_ref()))
    }

    #[test]
    fn lossy_dng_planes_get_their_own_polynomial() {
        // A lossy DNG stores linear RGB with its tone curve as one MapPolynomial
        // per plane in OpcodeList2 (#1542)
        use crate::decode::test_dng::*;
        let area = |plane| [0, 0, 2, 2, plane, 1, 1, 1];
        let opcodes = list(&[
            polynomial(area(0), &[0.0, 1.0]),
            polynomial(area(1), &[0.0, 0.5]),
            polynomial(area(2), &[0.0, 0.0, 1.0]),
        ]);
        let file = dng(
            2,
            2,
            3,
            LINEAR_RAW,
            &[200; 12],
            vec![
                shorts(50714, &[0, 0, 0]),
                longs(50717, &[400]),
                undefined(OPCODE_LIST2, &opcodes),
            ],
        );
        let (raw_image, list3) = decode(&file);
        assert_eq!(
            raw_image.pixels_u16()[..3],
            [to_white(0.5), to_white(0.25), to_white(0.25)]
        );
        assert_eq!(raw_image.whitelevel.0, vec![OUTPUT_WHITE; 3]);
        assert!(list3.is_none());
    }

    #[test]
    fn phone_dng_gain_maps_and_bad_pixels_apply_per_bayer_channel() {
        // Phone DNGs carry one lens shading GainMap per Bayer channel in
        // OpcodeList2, and some a bad pixel list in OpcodeList1
        use crate::decode::test_dng::*;
        let mut pixels = vec![1100u16; 16];
        pixels[5] = 64000;
        let list1 = list(&[bad_pixel_list(0, &[[1, 1]], &[])]);
        let list2 = list(&[
            gain_map([0, 0, 4, 4, 0, 1, 2, 2], (1, 1), (1.0, 1.0), &[2.0]),
            gain_map([1, 1, 4, 4, 0, 1, 2, 2], (1, 1), (1.0, 1.0), &[0.5]),
        ]);
        let file = dng(
            4,
            4,
            1,
            CFA,
            &pixels,
            vec![
                shorts(33421, &[2, 2]),
                bytes(33422, &[0, 1, 1, 2]),
                shorts(50714, &[100]),
                longs(50717, &[2100]),
                undefined(OPCODE_LIST1, &list1),
                undefined(OPCODE_LIST2, &list2),
            ],
        );
        let (raw_image, _) = decode(&file);
        let half = to_white(0.5);
        let data = raw_image.pixels_u16();
        // Red is doubled, blue (at the stuck pixel) halved, green kept
        assert_eq!(data[0], to_white(1.0));
        assert_eq!(data[5], to_white(0.25));
        assert_eq!([data[1], data[4]], [half, half]);
        assert_eq!(raw_image.blacklevel.as_vec(), vec![0.0]);
    }

    #[test]
    fn opcode_list3_is_read_from_the_raw_ifd() {
        use crate::decode::test_dng::*;
        let opcodes = list(&[
            warp(&[[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]], [0.5, 0.5]),
            opcode(GAIN_MAP, &[0; 8]),
        ]);
        let file = dng(
            2,
            2,
            1,
            LINEAR_RAW,
            &[0; 4],
            vec![undefined(OPCODE_LIST3, &opcodes)],
        );
        let (_, list3) = decode(&file);
        assert_eq!(list3.map(|list3| list3.opcodes.len()), Some(1));
    }

    #[test]
    fn malformed_lists_are_ignored() {
        let good = polynomial([0, 0, 1, 1, 0, 1, 1, 1], &[0.0]);
        let mut huge = opcode(MAP_POLYNOMIAL, &[]);
        huge[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        let cases = [
            vec![],
            vec![0, 0, 0, 5],
            list(std::slice::from_ref(&good))[..20].to_vec(),
            list(&[huge]),
            list(&[polynomial([0, 0, 1, 1, 0, 1, 1, 1], &[0.0; 12])]),
            list(&[polynomial([0, 0, 1, 1, 0, 1, 1, 1], &[f64::NAN])]),
            list(&[gain_map(
                [0, 0, 1, 1, 0, 1, 1, 1],
                (u32::MAX, u32::MAX),
                (1.0, 1.0),
                &[1.0],
            )]),
            list(&[table([0, 0, 1, 1, 0, 1, 1, 1], &[])]),
            list(&[opcode(
                FIX_BAD_PIXELS_LIST,
                &words(&[0, u32::MAX, u32::MAX]),
            )]),
            list(&[warp(&[[1.0; 6], [1.0; 6]], [0.5, 0.5])]),
            list(&[per_line(DELTA_PER_ROW, [0, 0, 1, 1, 0, 1, 1, 1], &[])]),
        ];
        for case in cases {
            assert!(parse(&case).is_empty());
        }
        // Areas past the image and planes past cpp are clipped
        let ops = parse(&list(&[polynomial(
            [0, 0, u32::MAX, u32::MAX, 0, u32::MAX, 0, 0],
            &[0.0],
        )]));
        let mut data = vec![500u16; 4];
        apply_integer(&ops, &layout(2, 2, 1, 0, 1000), &mut data);
        assert_eq!(data, vec![0; 4]);
    }
}
