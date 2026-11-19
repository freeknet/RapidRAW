//! DNG OpcodeList2 processing (MapTable, MapPolynomial and GainMap).
//!
//! OpcodeList2 is applied to the linearized raw data inside the ActiveArea,
//! after black subtraction and scaling so the white level is 1.0. rawler only
//! applies MapPolynomial to proxy images, so lossy and JPEG XL DNGs that store
//! their tone curve as per plane MapPolynomial opcodes came out pink, and phone
//! and drone DNGs lost their lens shading GainMaps.

use rawler::{
    RawImage, RawImageData,
    decoders::{Decoder, FormatHint, WellKnownIFD},
    imgop::{Dim2, Point, Rect},
    rawimage::{BlackLevel, WhiteLevel},
    tags::DngTag,
};
use rayon::prelude::*;

const MAP_TABLE: u32 = 7;
const MAP_POLYNOMIAL: u32 = 8;
const GAIN_MAP: u32 = 9;

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

#[derive(Debug, Clone, PartialEq)]
enum Opcode {
    MapTable { area: Area, table: Vec<u16> },
    MapPolynomial { area: Area, coefficients: Vec<f64> },
    GainMap { area: Area, map: GainMap },
}

impl Opcode {
    fn area(&self) -> &Area {
        match self {
            Opcode::MapTable { area, .. }
            | Opcode::MapPolynomial { area, .. }
            | Opcode::GainMap { area, .. } => area,
        }
    }
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
    let area = r.area()?;
    match id {
        MAP_TABLE => {
            let size = r.usize()?;
            if size == 0 || size > 65536 || r.remaining() < size * 2 {
                return None;
            }
            let table = (0..size).map(|_| r.u16()).collect::<Option<_>>()?;
            Some(Opcode::MapTable { area, table })
        }
        MAP_POLYNOMIAL => {
            let degree = r.usize()?;
            // The specification limits the degree to 8
            if degree > 8 {
                return None;
            }
            let coefficients = (0..=degree).map(|_| r.f64()).collect::<Option<_>>()?;
            Some(Opcode::MapPolynomial { area, coefficients })
        }
        GAIN_MAP => {
            let points_v = r.usize()?;
            let points_h = r.usize()?;
            let spacing_v = r.f64()?;
            let spacing_h = r.f64()?;
            let origin_v = r.f64()?;
            let origin_h = r.f64()?;
            let map_planes = r.usize()?;
            let count = points_v.checked_mul(points_h)?.checked_mul(map_planes)?;
            let finite = [spacing_v, spacing_h, origin_v, origin_h]
                .iter()
                .all(|v| v.is_finite());
            if count == 0 || !finite || r.remaining() < count.checked_mul(4)? {
                return None;
            }
            let gains = (0..count).map(|_| r.f32()).collect::<Option<_>>()?;
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
        let (Some(id), Some(_version), Some(_flags), Some(size)) =
            (r.u32(), r.u32(), r.u32(), r.usize())
        else {
            break;
        };
        let Some(params) = r.pos.checked_add(size).and_then(|end| buf.get(r.pos..end)) else {
            break;
        };
        r.pos += size;
        match parse_opcode(id, params) {
            Some(opcode) => opcodes.push(opcode),
            None => log::debug!("DNG OpcodeList2: skipping opcode {id}"),
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

/// Geometry and levels of the raw data the opcodes are applied to.
struct Layout {
    /// Pixels per row of the raw data.
    width: usize,
    cpp: usize,
    /// The ActiveArea, OpcodeList2 coordinates are relative to its origin.
    bounds: Rect,
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

fn apply_opcode(opcode: &Opcode, layout: &Layout, normalized: &mut [f32]) {
    let area = opcode.area();
    let bounds = layout.bounds;
    let bottom = area.bottom.min(bounds.d.h);
    let right = area.right.min(bounds.d.w);
    let planes = area.plane..area.plane.saturating_add(area.planes).min(layout.cpp);
    if area.top >= bottom || area.left >= right || planes.is_empty() {
        return;
    }
    let stride = layout.width * layout.cpp;
    let (height, width) = (bounds.d.h as f64, bounds.d.w as f64);
    normalized
        .par_chunks_exact_mut(stride)
        .enumerate()
        .skip(bounds.p.y + area.top)
        .take(bottom - area.top)
        .filter(|(row, _)| (row - bounds.p.y - area.top) % area.row_pitch == 0)
        .for_each(|(row, line)| {
            let v = (row - bounds.p.y) as f64 / height;
            for col in (area.left..right).step_by(area.col_pitch) {
                let h = col as f64 / width;
                let base = (bounds.p.x + col) * layout.cpp;
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
                    };
                }
            }
        });
}

/// Applies the opcodes to integer raw data, which is rescaled to black 0 and
/// white [`OUTPUT_WHITE`], and returns the new levels.
fn apply_integer(
    opcodes: &[Opcode],
    layout: &Layout,
    data: &mut [u16],
) -> (BlackLevel, WhiteLevel) {
    let stride = layout.width * layout.cpp;
    let mut normalized: Vec<f32> = data
        .par_chunks_exact(stride)
        .enumerate()
        .flat_map_iter(|(row, line)| {
            line.iter().enumerate().map(move |(i, &v)| {
                let (col, c) = (i / layout.cpp, i % layout.cpp);
                let black = layout.black(row, col, c);
                (v as f32 - black) / (layout.white(c) - black).max(1.0)
            })
        })
        .collect();
    for opcode in opcodes {
        apply_opcode(opcode, layout, &mut normalized);
    }
    data.par_iter_mut()
        .zip(normalized.par_iter())
        .for_each(|(v, &x)| {
            *v = (x * OUTPUT_WHITE as f32)
                .round()
                .clamp(0.0, u16::MAX as f32) as u16;
        });
    (
        BlackLevel::zero(1, 1, layout.cpp),
        WhiteLevel::new(vec![OUTPUT_WHITE; layout.cpp]),
    )
}

/// Applies the opcodes to floating point raw data in place, keeping its levels.
fn apply_float(opcodes: &[Opcode], layout: &Layout, data: &mut [f32]) {
    let stride = layout.width * layout.cpp;
    let levels = |row: usize, i: usize| {
        let (col, c) = (i / layout.cpp, i % layout.cpp);
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
        apply_opcode(opcode, layout, data);
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

fn apply(opcodes: &[Opcode], raw_image: &mut RawImage) {
    let full = Rect::new(Point::zero(), Dim2::new(raw_image.width, raw_image.height));
    let bounds = raw_image.active_area.unwrap_or(full);
    let samples = raw_image.width * raw_image.height * raw_image.cpp;
    if opcodes.is_empty()
        || raw_image.cpp == 0
        || raw_image.width == 0
        || bounds.p.x + bounds.d.w > raw_image.width
        || bounds.p.y + bounds.d.h > raw_image.height
    {
        return;
    }
    let layout = Layout {
        width: raw_image.width,
        cpp: raw_image.cpp,
        bounds,
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

/// Applies the MapTable, MapPolynomial and GainMap opcodes of a DNG's
/// OpcodeList2 to its decoded raw image.
pub fn apply_opcode_list2(decoder: &dyn Decoder, raw_image: &mut RawImage) {
    if decoder.format_hint() != FormatHint::DNG {
        return;
    }
    let Ok(Some(ifd)) = decoder.ifd(WellKnownIFD::VirtualDngRawTags) else {
        return;
    };
    let Some(entry) = ifd.get_entry(DngTag::OpcodeList2) else {
        return;
    };
    let opcodes = parse(entry.value.get_data());
    if !opcodes.is_empty() {
        log::debug!("DNG OpcodeList2: applying {} opcodes", opcodes.len());
        apply(&opcodes, raw_image);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opcode(id: u32, params: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [id, 0x0103_0000, 1, params.len() as u32] {
            v.extend_from_slice(&x.to_be_bytes());
        }
        v.extend_from_slice(params);
        v
    }

    fn area(a: [u32; 8]) -> Vec<u8> {
        a.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    fn polynomial(a: [u32; 8], coefficients: &[f64]) -> Vec<u8> {
        let mut p = area(a);
        p.extend_from_slice(&(coefficients.len() as u32 - 1).to_be_bytes());
        coefficients
            .iter()
            .for_each(|c| p.extend_from_slice(&c.to_be_bytes()));
        opcode(MAP_POLYNOMIAL, &p)
    }

    fn table(a: [u32; 8], values: &[u16]) -> Vec<u8> {
        let mut p = area(a);
        p.extend_from_slice(&(values.len() as u32).to_be_bytes());
        values
            .iter()
            .for_each(|v| p.extend_from_slice(&v.to_be_bytes()));
        opcode(MAP_TABLE, &p)
    }

    fn gain_map(a: [u32; 8], points: (u32, u32), spacing: (f64, f64), gains: &[f32]) -> Vec<u8> {
        let mut p = area(a);
        p.extend_from_slice(&points.0.to_be_bytes());
        p.extend_from_slice(&points.1.to_be_bytes());
        for f in [spacing.0, spacing.1, 0.0, 0.0] {
            p.extend_from_slice(&f.to_be_bytes());
        }
        p.extend_from_slice(&1u32.to_be_bytes());
        gains
            .iter()
            .for_each(|g| p.extend_from_slice(&g.to_be_bytes()));
        opcode(GAIN_MAP, &p)
    }

    fn list(ops: &[Vec<u8>]) -> Vec<u8> {
        let mut v = (ops.len() as u32).to_be_bytes().to_vec();
        ops.iter().for_each(|o| v.extend_from_slice(o));
        v
    }

    fn layout(width: usize, height: usize, cpp: usize, black: u16, white: u32) -> Layout {
        Layout {
            width,
            cpp,
            bounds: Rect::new(Point::zero(), Dim2::new(width, height)),
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
            opcode(1, &[9; 12]),
            polynomial(all, &[0.0, 2.0]),
            table(all, &[0, 65535]),
            gain_map(all, (1, 1), (1.0, 1.0), &[2.0]),
        ]));
        assert_eq!(ops.len(), 3);
        assert!(
            matches!(&ops[0], Opcode::MapPolynomial { coefficients, .. } if coefficients == &[0.0, 2.0])
        );
        assert!(matches!(&ops[1], Opcode::MapTable { table, .. } if table == &[0, 65535]));
        assert!(matches!(&ops[2], Opcode::GainMap { map, .. } if map.gains == [2.0]));
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
    fn coordinates_are_relative_to_the_active_area() {
        let ops = parse(&list(&[polynomial([0, 0, 1, 1, 0, 1, 1, 1], &[1.0])]));
        let mut layout = layout(3, 2, 1, 0, 100);
        layout.bounds = Rect::new(Point::new(1, 1), Dim2::new(2, 1));
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
    fn malformed_lists_are_ignored() {
        let good = polynomial([0, 0, 1, 1, 0, 1, 1, 1], &[0.0]);
        let mut huge = opcode(MAP_POLYNOMIAL, &[]);
        huge[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        let cases = [
            vec![],
            vec![0, 0, 0, 5],
            list(&[good.clone()])[..20].to_vec(),
            list(&[huge]),
            list(&[polynomial([0, 0, 1, 1, 0, 1, 1, 1], &[0.0; 12])]),
            list(&[gain_map(
                [0, 0, 1, 1, 0, 1, 1, 1],
                (u32::MAX, u32::MAX),
                (1.0, 1.0),
                &[1.0],
            )]),
            list(&[table([0, 0, 1, 1, 0, 1, 1, 1], &[])]),
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
