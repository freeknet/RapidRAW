//! Import of Lightroom / Camera Raw presets, including the profiles (looks)
//! they reference and their AI masks, from the user's own Adobe install.

pub mod convert;
pub mod focus;
pub mod library;
pub mod tables;
pub mod xmp;

use crate::library::presets::Preset;
use library::ProfileIndex;

/// Converts one XMP preset. Looks resolve against `profiles` when given.
pub fn convert_xmp_to_preset(
    xmp_content: &str,
    profiles: Option<&ProfileIndex>,
) -> Result<Preset, String> {
    let doc = xmp::parse_crs(xmp_content)?;
    Ok(convert::convert(&doc, profiles).preset)
}
