//! Monochrome DNGs store their raw data with PhotometricInterpretation
//! BlackIsZero. rawler reads a CFA pattern for every DNG that is not
//! LinearRaw, so those files failed with "Couldn't find tag CFAPattern". The
//! data of a single plane BlackIsZero raw IFD is linear like LinearRaw, so a
//! copy of the file with the tag rewritten decodes correctly.

use std::borrow::Cow;
use std::collections::HashSet;

const NEW_SUBFILE_TYPE: u16 = 254;
const PHOTOMETRIC: u16 = 262;
const SUB_IFDS: u16 = 330;
const DNG_VERSION: u16 = 50706;

const SHORT: u16 = 3;
const LONG: u16 = 4;
const IFD: u16 = 13;

const BLACK_IS_ZERO: u16 = 1;
const LINEAR_RAW: u16 = 34892;

/// IFDs followed at most, against loops and hostile files.
const MAX_IFDS: usize = 64;

struct Tiff<'a> {
    data: &'a [u8],
    big_endian: bool,
}

impl Tiff<'_> {
    fn u16(&self, offset: usize) -> Option<u16> {
        let bytes: [u8; 2] = self.data.get(offset..offset + 2)?.try_into().ok()?;
        Some(if self.big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    fn u32(&self, offset: usize) -> Option<u32> {
        let bytes: [u8; 4] = self.data.get(offset..offset + 4)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    fn short_bytes(&self, value: u16) -> [u8; 2] {
        if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }

    /// The integer value of a single SHORT or LONG entry.
    fn entry_value(&self, entry: usize) -> Option<u32> {
        if self.u32(entry + 4)? != 1 {
            return None;
        }
        match self.u16(entry + 2)? {
            SHORT => self.u16(entry + 8).map(u32::from),
            LONG | IFD => self.u32(entry + 8),
            _ => None,
        }
    }

    fn sub_ifds(&self, entry: usize) -> Vec<usize> {
        let (Some(kind), Some(count)) = (self.u16(entry + 2), self.u32(entry + 4)) else {
            return Vec::new();
        };
        if !matches!(kind, LONG | IFD) || count == 0 || count > MAX_IFDS as u32 {
            return Vec::new();
        }
        let start = if count == 1 {
            entry + 8
        } else {
            match self.u32(entry + 8) {
                Some(offset) => offset as usize,
                None => return Vec::new(),
            }
        };
        (0..count as usize)
            .filter_map(|i| self.u32(start + i * 4).map(|v| v as usize))
            .collect()
    }
}

/// Offsets of the PhotometricInterpretation values of the full resolution
/// BlackIsZero IFDs of a DNG.
fn black_is_zero_entries(data: &[u8]) -> Vec<usize> {
    let big_endian = match data.get(..4) {
        Some(b"II*\0") => false,
        Some(b"MM\0*") => true,
        _ => return Vec::new(),
    };
    let tiff = Tiff { data, big_endian };
    let Some(first) = tiff.u32(4) else {
        return Vec::new();
    };
    let mut pending = vec![first as usize];
    let mut visited = HashSet::new();
    let mut is_dng = false;
    let mut found = Vec::new();
    while let Some(ifd) = pending.pop() {
        if ifd == 0 || visited.len() >= MAX_IFDS || !visited.insert(ifd) {
            continue;
        }
        let Some(count) = tiff.u16(ifd) else {
            continue;
        };
        let mut photometric = None;
        let mut reduced = false;
        for i in 0..count as usize {
            let entry = ifd + 2 + i * 12;
            let Some(tag) = tiff.u16(entry) else {
                break;
            };
            match tag {
                NEW_SUBFILE_TYPE => {
                    reduced = tiff.entry_value(entry).is_some_and(|v| v & 1 != 0);
                }
                PHOTOMETRIC => {
                    if tiff.u16(entry + 2) == Some(SHORT)
                        && tiff.entry_value(entry) == Some(BLACK_IS_ZERO as u32)
                    {
                        photometric = Some(entry + 8);
                    }
                }
                SUB_IFDS => pending.extend(tiff.sub_ifds(entry)),
                DNG_VERSION => is_dng = true,
                _ => {}
            }
        }
        if let (Some(value), false) = (photometric, reduced) {
            found.push(value);
        }
        if let Some(next) = tiff.u32(ifd + 2 + count as usize * 12) {
            pending.push(next as usize);
        }
    }
    if is_dng { found } else { Vec::new() }
}

/// The file with the full resolution BlackIsZero IFDs of a monochrome DNG
/// marked as LinearRaw, or the file itself if it has none.
pub fn as_linear_raw(data: &[u8]) -> Cow<'_, [u8]> {
    let entries = black_is_zero_entries(data);
    if entries.is_empty() {
        return Cow::Borrowed(data);
    }
    let big_endian = data.starts_with(b"MM");
    let tiff = Tiff { data, big_endian };
    let linear = tiff.short_bytes(LINEAR_RAW);
    let mut patched = data.to_vec();
    for value in entries {
        patched[value..value + 2].copy_from_slice(&linear);
    }
    Cow::Owned(patched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::test_dng::{self, BLACK_IS_ZERO as BIZ, CFA, LINEAR_RAW as LIN};

    fn photometric_of(file: &[u8]) -> u16 {
        let source = rawler::rawsource::RawSource::new_from_slice(file);
        let decoder = rawler::get_decoder(&source).unwrap();
        let raw = decoder
            .ifd(rawler::decoders::WellKnownIFD::Raw)
            .unwrap()
            .unwrap();
        raw.get_entry(rawler::tags::TiffCommonTag::PhotometricInt)
            .unwrap()
            .force_u16(0)
    }

    #[test]
    fn monochrome_dngs_become_linear_raw() {
        let file = test_dng::dng(2, 2, 1, BIZ, &[0; 4], vec![]);
        let patched = as_linear_raw(&file);
        assert!(matches!(patched, Cow::Owned(_)));
        assert_eq!(photometric_of(&patched), LIN);
        assert_eq!(patched.len(), file.len());
    }

    #[test]
    fn other_files_are_left_alone() {
        for photometric in [CFA, LIN] {
            let file = test_dng::dng(2, 2, 1, photometric, &[0; 4], vec![]);
            assert!(matches!(as_linear_raw(&file), Cow::Borrowed(_)));
        }
        // A reduced resolution BlackIsZero preview is not raw data
        let preview = test_dng::dng(
            2,
            2,
            1,
            BIZ,
            &[0; 4],
            vec![test_dng::longs(NEW_SUBFILE_TYPE, &[1])],
        );
        assert!(matches!(as_linear_raw(&preview), Cow::Borrowed(_)));
        // A plain TIFF without DNGVersion
        let mut tiff = test_dng::dng(2, 2, 1, BIZ, &[0; 4], vec![]);
        let at = tiff
            .windows(2)
            .position(|w| w == DNG_VERSION.to_le_bytes())
            .unwrap();
        tiff[at..at + 2].copy_from_slice(&50000u16.to_le_bytes());
        assert!(matches!(as_linear_raw(&tiff), Cow::Borrowed(_)));
        for junk in [
            &b""[..],
            b"II*\0",
            b"II*\0\xff\xff\xff\xff",
            b"MM\0*\0\0\0\x08\xff",
        ] {
            assert!(matches!(as_linear_raw(junk), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn raw_ifds_in_big_endian_sub_ifds_are_found() {
        // IFD0 is an RGB thumbnail with DNGVersion and a SubIFD holding the
        // BlackIsZero raw data
        let mut file = b"MM\0*".to_vec();
        file.extend_from_slice(&8u32.to_be_bytes());
        let entry = |tag: u16, kind: u16, value: u32| {
            let mut e = tag.to_be_bytes().to_vec();
            e.extend_from_slice(&kind.to_be_bytes());
            e.extend_from_slice(&1u32.to_be_bytes());
            if kind == SHORT {
                e.extend_from_slice(&(value as u16).to_be_bytes());
                e.extend_from_slice(&[0, 0]);
            } else {
                e.extend_from_slice(&value.to_be_bytes());
            }
            e
        };
        let sub_ifd = 8 + 2 + 3 * 12 + 4;
        file.extend_from_slice(&3u16.to_be_bytes());
        file.extend(entry(PHOTOMETRIC, SHORT, 2));
        file.extend(entry(SUB_IFDS, LONG, sub_ifd as u32));
        file.extend(entry(DNG_VERSION, 1, 0x0104_0000));
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&2u16.to_be_bytes());
        file.extend(entry(NEW_SUBFILE_TYPE, LONG, 0));
        file.extend(entry(PHOTOMETRIC, SHORT, BLACK_IS_ZERO as u32));
        file.extend_from_slice(&0u32.to_be_bytes());

        let patched = as_linear_raw(&file);
        let value = sub_ifd + 2 + 12 + 8;
        assert_eq!(patched[value..value + 2], LINEAR_RAW.to_be_bytes());
        // The thumbnail keeps its RGB interpretation
        assert_eq!(patched[8 + 2 + 8..8 + 2 + 10], 2u16.to_be_bytes());
    }
}
