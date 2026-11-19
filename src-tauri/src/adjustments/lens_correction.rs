use crate::library::file_ops::read_file_mapped;
use crate::library::sidecars::parse_virtual_path;
use fuzzy_matcher::FuzzyMatcher;
#[cfg(target_os = "android")]
use include_dir::{Dir, include_dir};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
#[cfg(not(target_os = "android"))]
use std::fs;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
#[cfg(not(target_os = "android"))]
use tauri::Manager;
#[cfg(not(target_os = "android"))]
use walkdir::WalkDir;
#[cfg(target_os = "android")]
static LENS_DB_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/lensfun_db");

const MAX_CACHED_PROFILES: usize = 1024;
const DEFAULT_ASPECT_RATIO: f64 = 1.5;
const DEFAULT_DISTANCE: f32 = 1000.0;
const TCA_FOCAL_SCALED: [bool; 6] = [false, false, true, true, true, true];

static DATABASE: OnceLock<LensDatabase> = OnceLock::new();
static SAVED_LENSES: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Distortion {
    #[serde(rename = "@model")]
    pub model: String,
    #[serde(rename = "@focal")]
    pub focal: f32,
    #[serde(rename = "@k1")]
    pub k1: Option<f32>,
    #[serde(rename = "@k2")]
    pub k2: Option<f32>,
    #[serde(rename = "@a")]
    pub a: Option<f32>,
    #[serde(rename = "@b")]
    pub b: Option<f32>,
    #[serde(rename = "@c")]
    pub c: Option<f32>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Tca {
    #[serde(rename = "@model")]
    pub model: String,
    #[serde(rename = "@focal")]
    pub focal: f32,
    #[serde(rename = "@kr")]
    pub kr: Option<f32>,
    #[serde(rename = "@kb")]
    pub kb: Option<f32>,
    #[serde(rename = "@vr")]
    pub vr: Option<f32>,
    #[serde(rename = "@vb")]
    pub vb: Option<f32>,
    #[serde(rename = "@cr")]
    pub cr: Option<f32>,
    #[serde(rename = "@cb")]
    pub cb: Option<f32>,
    #[serde(rename = "@br")]
    pub br: Option<f32>,
    #[serde(rename = "@bb")]
    pub bb: Option<f32>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Vignetting {
    #[serde(rename = "@model")]
    pub model: String,
    #[serde(rename = "@focal")]
    pub focal: f32,
    #[serde(rename = "@aperture")]
    pub aperture: f32,
    #[serde(rename = "@distance")]
    pub distance: Option<f32>,
    #[serde(rename = "@k1")]
    pub k1: Option<f32>,
    #[serde(rename = "@k2")]
    pub k2: Option<f32>,
    #[serde(rename = "@k3")]
    pub k3: Option<f32>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationElement {
    Distortion(Distortion),
    Tca(Tca),
    Vignetting(Vignetting),
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Calibration {
    #[serde(rename = "$value", default)]
    pub elements: Vec<CalibrationElement>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Focal {
    #[serde(rename = "@value")]
    pub value: Option<f32>,
    #[serde(rename = "@min")]
    pub min: Option<f32>,
    #[serde(rename = "@max")]
    pub max: Option<f32>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct Lens {
    #[serde(default)]
    pub maker: Vec<MultiName>,
    #[serde(default)]
    pub model: Vec<MultiName>,
    #[serde(default)]
    pub mount: Vec<String>,
    pub cropfactor: Option<f32>,
    pub calibration: Option<Calibration>,
    pub focal: Option<Focal>,
    pub aspect_ratio: Option<String>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct Camera {
    pub maker: Vec<MultiName>,
    pub model: Vec<MultiName>,
    pub mount: String,
    pub cropfactor: f32,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct LensDatabase {
    #[serde(rename = "camera", default)]
    pub cameras: Vec<Camera>,
    #[serde(rename = "lens", default)]
    pub lenses: Vec<Lens>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct MultiName {
    #[serde(rename = "@lang")]
    lang: Option<String>,
    #[serde(rename = "$value")]
    value: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LensProfile {
    pub distortion: Option<[f64; 5]>,
    pub tca: Option<[f64; 6]>,
    pub vignetting: Option<[f64; 3]>,
    pub distortion_scale: f64,
    pub vignetting_scale: f64,
}

#[derive(Serialize, Clone)]
pub struct LensCorrectionInfo {
    maker: String,
    model: String,
    distortion: bool,
    tca: bool,
    vignetting: bool,
}

struct ResolvedLens {
    info: LensCorrectionInfo,
    profile: LensProfile,
}

type ProfileKey = (String, Option<(String, String)>);

#[derive(Default)]
struct Shot {
    make: String,
    model: String,
    lens_model: String,
    focal: Option<f32>,
    aperture: Option<f32>,
    distance: Option<f32>,
}

fn strip_maker_prefix(name: &str, maker: &str) -> String {
    if name.to_lowercase().starts_with(&maker.to_lowercase())
        && let Some(rest) = name.get(maker.len()..)
    {
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    name.to_string()
}

fn pick_name(names: &[MultiName], fallback: &str) -> String {
    names
        .iter()
        .find(|n| n.lang.as_deref() == Some("en"))
        .or_else(|| names.first())
        .map(|n| n.value.clone())
        .unwrap_or_else(|| fallback.to_string())
}

fn any_name_matches(names: &[MultiName], needle: &str) -> bool {
    names
        .iter()
        .any(|n| n.value.trim().eq_ignore_ascii_case(needle))
}

fn clean_exif_text(value: &str) -> &str {
    value.trim().trim_matches('"')
}

fn parse_aspect_ratio(value: &str) -> Option<f64> {
    match value.split_once(':') {
        Some((w, h)) => Some(w.trim().parse::<f64>().ok()? / h.trim().parse::<f64>().ok()?),
        None => value.trim().parse().ok(),
    }
}

fn exif_number(exif: &HashMap<String, String>, key: &str) -> Option<f32> {
    let text = exif.get(key)?.trim();
    let text = text.strip_prefix("f/").unwrap_or(text);
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    text[..end]
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

fn hermite(p0: f64, m0: f64, p1: f64, m1: f64, t: f64) -> f64 {
    let t2 = t * t;
    let t3 = t2 * t;
    (2.0 * t3 - 3.0 * t2 + 1.0) * p0
        + (t3 - 2.0 * t2 + t) * m0
        + (-2.0 * t3 + 3.0 * t2) * p1
        + (t3 - t2) * m1
}

fn interpolate_by_focal<const N: usize>(
    mut samples: Vec<(f32, [f64; N])>,
    focal: f32,
    focal_scaled: [bool; N],
) -> Option<[f64; N]> {
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    if let Some(exact) = samples.iter().find(|s| (s.0 - focal).abs() < 1e-4) {
        return Some(exact.1);
    }
    let upper = samples.partition_point(|s| s.0 < focal);
    if upper == 0 || upper == samples.len() {
        return samples
            .get(upper.min(samples.len().saturating_sub(1)))
            .map(|s| s.1);
    }

    let (lo, hi) = (&samples[upper - 1], &samples[upper]);
    let before = upper.checked_sub(2).map(|i| &samples[i]);
    let after = samples.get(upper + 1);
    let t = ((focal - lo.0) / (hi.0 - lo.0)) as f64;

    Some(std::array::from_fn(|i| {
        let value = |s: &(f32, [f64; N])| {
            if focal_scaled[i] {
                s.1[i] * s.0 as f64
            } else {
                s.1[i]
            }
        };
        let (y_lo, y_hi) = (value(lo), value(hi));
        let tangent_lo = before.map_or(y_hi - y_lo, |s| (y_hi - value(s)) * 0.5);
        let tangent_hi = after.map_or(y_hi - y_lo, |s| (value(s) - y_lo) * 0.5);
        let y = hermite(y_lo, tangent_lo, y_hi, tangent_hi, t);
        if focal_scaled[i] { y / focal as f64 } else { y }
    }))
}

fn distortion_terms(d: &Distortion) -> Option<[f64; 3]> {
    let v = |x: Option<f32>| x.unwrap_or(0.0) as f64;
    match d.model.as_str() {
        "poly3" => Some([v(d.k1), 0.0, 0.0]),
        "poly5" => Some([v(d.k1), v(d.k2), 0.0]),
        "ptlens" => Some([v(d.a), v(d.b), v(d.c)]),
        _ => None,
    }
}

fn distortion_polynomial(model: &str, [t0, t1, t2]: [f64; 3]) -> [f64; 5] {
    match model {
        "poly3" => [1.0 - t0, 0.0, t0, 0.0, 0.0],
        "poly5" => [1.0, 0.0, t0, 0.0, t1],
        _ => [1.0 - t0 - t1 - t2, t2, t1, t0, 0.0],
    }
}

fn tca_terms(t: &Tca) -> Option<[f64; 6]> {
    let v = |x: Option<f32>, default: f32| x.unwrap_or(default) as f64;
    match t.model.as_str() {
        "linear" => Some([v(t.kr, 1.0), v(t.kb, 1.0), 0.0, 0.0, 0.0, 0.0]),
        "poly3" => Some([
            v(t.vr, 1.0),
            v(t.vb, 1.0),
            v(t.cr, 0.0),
            v(t.cb, 0.0),
            v(t.br, 0.0),
            v(t.bb, 0.0),
        ]),
        _ => None,
    }
}

fn vignetting_terms(v: &Vignetting) -> [f64; 3] {
    [v.k1, v.k2, v.k3].map(|k| k.unwrap_or(0.0) as f64)
}

fn crop_score(lens_crop: f32, camera_crop: Option<f32>) -> Option<i64> {
    let Some(camera_crop) = camera_crop else {
        return Some(0);
    };
    let ratio = camera_crop / lens_crop;
    match ratio {
        r if r < 0.96 => None,
        r if r >= 1.41 => Some(2),
        r if r >= 1.31 => Some(4),
        r if r >= 1.21 => Some(6),
        r if r >= 1.11 => Some(8),
        r if r >= 1.01 => Some(10),
        r if r >= 1.0 => Some(5),
        _ => Some(3),
    }
}

fn crop_distance(lens_crop: f32, target_crop: Option<f32>) -> f32 {
    target_crop.map_or(0.0, |target| (lens_crop / target).ln().abs())
}

/// Splits a lens name into lowercase words the way Lensfun compares them: runs of
/// letters and runs of numbers, with punctuation dropped, so "f/3.5-5.6G" and
/// "F3.5-5.6 G" give the same words. Numbers lose trailing zeros ("4.0" == "4").
fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_alphabetic() {
                i += 1;
            }
            words.push(chars[start..i].iter().collect::<String>().to_lowercase());
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_digit()
                    || (chars[i] == '.' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())))
            {
                i += 1;
            }
            let number: String = chars[start..i].iter().collect();
            let number = if number.contains('.') {
                number
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string()
            } else {
                number
            };
            words.push(number);
        } else {
            i += 1;
        }
    }
    words
}

/// Lensfun's fuzzy name comparison: every word of the EXIF name has to appear in
/// the candidate, in any order. The score rewards candidates without extra words.
fn word_match_score(pattern: &[String], candidate: &str) -> Option<i64> {
    let mut remaining = name_words(candidate);
    if pattern.is_empty() || remaining.is_empty() {
        return None;
    }
    let total = pattern.len() + remaining.len();
    for word in pattern {
        let index = remaining.iter().position(|w| w == word)?;
        remaining.swap_remove(index);
    }
    Some((pattern.len() * 200 / total) as i64)
}

fn maker_matches(lens_maker: &str, camera_make: &str) -> bool {
    let camera = name_words(camera_make);
    name_words(lens_maker)
        .first()
        .is_some_and(|first| camera.contains(first))
}

impl Shot {
    fn from_exif(exif: &HashMap<String, String>) -> Self {
        let text = |key: &str| exif.get(key).map_or("", |v| clean_exif_text(v)).to_string();
        Self {
            make: text("Make"),
            model: text("Model"),
            lens_model: text("LensModel"),
            focal: exif_number(exif, "FocalLength"),
            aperture: exif_number(exif, "FNumber")
                .or_else(|| exif_number(exif, "ApertureValue").map(|av| 2f32.powf(av / 2.0))),
            distance: exif_number(exif, "SubjectDistance"),
        }
    }

    fn read(source_path: &str) -> Self {
        let path = std::path::Path::new(source_path);
        crate::metadata::exif_processing::read_rrexif_sidecar(path)
            .or_else(|| {
                let bytes = read_file_mapped(path).ok()?;
                Some(crate::metadata::exif_processing::read_exif_data_from_bytes(
                    source_path,
                    &bytes,
                ))
            })
            .map(|exif| Self::from_exif(&exif))
            .unwrap_or_default()
    }
}

impl Lens {
    pub fn get_full_model_name(&self) -> String {
        pick_name(&self.model, "Unknown Model")
    }

    pub fn get_canonical_model_name(&self) -> String {
        self.model
            .iter()
            .find(|m| m.lang.is_none())
            .or_else(|| self.model.first())
            .map(|m| m.value.clone())
            .unwrap_or_else(|| "Unknown Model".to_string())
    }

    pub fn get_name(&self) -> String {
        strip_maker_prefix(&self.get_full_model_name(), &self.get_maker())
    }

    pub fn get_maker(&self) -> String {
        pick_name(&self.maker, "Misc")
    }

    pub fn get_display_name(&self, all_maker_lenses: &[&Lens]) -> String {
        let my_short = self.get_name();
        let short_count = all_maker_lenses
            .iter()
            .filter(|l| l.get_name() == my_short)
            .count();

        if short_count <= 1 {
            return my_short;
        }

        let maker = self.get_maker();
        let my_canonical_short = strip_maker_prefix(&self.get_canonical_model_name(), &maker);

        let canonical_short_count = all_maker_lenses
            .iter()
            .filter(|l| {
                strip_maker_prefix(&l.get_canonical_model_name(), &l.get_maker())
                    == my_canonical_short
            })
            .count();

        if canonical_short_count <= 1 {
            return my_canonical_short;
        }

        let my_canonical = self.get_canonical_model_name();
        let canonical_count = all_maker_lenses
            .iter()
            .filter(|l| l.get_canonical_model_name() == my_canonical)
            .count();

        if canonical_count <= 1 {
            return my_canonical;
        }

        if let Some(cf) = self.cropfactor {
            format!("{} (crop {:.1}x)", my_canonical_short, cf)
        } else {
            my_canonical_short
        }
    }

    fn calibration_crop(&self) -> f32 {
        self.cropfactor.filter(|c| *c > 0.0).unwrap_or(1.0)
    }

    fn aspect_ratio(&self) -> f64 {
        self.aspect_ratio
            .as_deref()
            .and_then(parse_aspect_ratio)
            .filter(|r| *r >= 1.0)
            .unwrap_or(DEFAULT_ASPECT_RATIO)
    }

    fn calibrations(&self) -> impl Iterator<Item = &CalibrationElement> {
        self.calibration.iter().flat_map(|c| &c.elements)
    }

    fn focal_range(&self) -> (f32, f32) {
        let declared = self.focal.as_ref().and_then(|f| {
            let min = f.min.or(f.value)?;
            Some((min, f.max.or(f.value).unwrap_or(min)))
        });
        declared.unwrap_or_else(|| {
            self.calibrations()
                .map(|c| match c {
                    CalibrationElement::Distortion(d) => d.focal,
                    CalibrationElement::Tca(t) => t.focal,
                    CalibrationElement::Vignetting(v) => v.focal,
                })
                .fold((f32::MAX, f32::MIN), |(lo, hi), f| (lo.min(f), hi.max(f)))
        })
    }

    fn distortion_at(&self, focal: f32) -> Option<[f64; 5]> {
        let mut model = None;
        let samples = self
            .calibrations()
            .filter_map(|c| match c {
                CalibrationElement::Distortion(d) => Some(d),
                _ => None,
            })
            .filter_map(|d| {
                let terms = distortion_terms(d)?;
                (*model.get_or_insert(d.model.as_str()) == d.model).then_some((d.focal, terms))
            })
            .collect();
        let terms = interpolate_by_focal(samples, focal, [true; 3])?;
        let polynomial = distortion_polynomial(model?, terms);
        (polynomial[0].abs() > 1e-6 && polynomial[1..].iter().any(|c| c.abs() > 1e-9))
            .then_some(polynomial)
    }

    fn tca_at(&self, focal: f32) -> Option<[f64; 6]> {
        let mut model = None;
        let samples = self
            .calibrations()
            .filter_map(|c| match c {
                CalibrationElement::Tca(t) => Some(t),
                _ => None,
            })
            .filter_map(|t| {
                let terms = tca_terms(t)?;
                (*model.get_or_insert(t.model.as_str()) == t.model).then_some((t.focal, terms))
            })
            .collect();
        let terms = interpolate_by_focal(samples, focal, TCA_FOCAL_SCALED)?;
        let identity = [1.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        terms
            .iter()
            .zip(identity)
            .any(|(t, i)| (t - i).abs() > 1e-9)
            .then_some(terms)
    }

    fn vignetting_at(&self, focal: f32, aperture: f32, distance: f32) -> Option<[f64; 3]> {
        let (min_focal, max_focal) = self.focal_range();
        let focal_span = max_focal - min_focal;
        let normalized = |f: f32| {
            let offset = (f - min_focal) as f64;
            if focal_span != 0.0 {
                offset / focal_span as f64
            } else {
                offset
            }
        };

        let mut weighted = [0.0f64; 3];
        let mut total_weight = 0.0f64;
        let mut nearest = f64::MAX;
        for v in self.calibrations().filter_map(|c| match c {
            CalibrationElement::Vignetting(v) if v.model == "pa" => Some(v),
            _ => None,
        }) {
            let calibration_distance = v.distance.unwrap_or(0.0) as f64;
            let separation = ((normalized(v.focal) - normalized(focal)).powi(2)
                + (4.0 / v.aperture as f64 - 4.0 / aperture as f64).powi(2)
                + (0.1 / calibration_distance - 0.1 / distance as f64).powi(2))
            .sqrt();
            if separation < 1e-4 {
                return Some(vignetting_terms(v));
            }
            nearest = nearest.min(separation);
            let weight = 1.0 / separation.powf(3.5);
            if weight.is_finite() && weight > 0.0 {
                for (acc, term) in weighted.iter_mut().zip(vignetting_terms(v)) {
                    *acc += weight * term;
                }
                total_weight += weight;
            }
        }

        (nearest <= 1.0 && total_weight > 0.0)
            .then(|| weighted.map(|t| t / total_weight))
            .filter(|terms| terms.iter().any(|k| k.abs() > 1e-9))
    }

    fn has_geometry(&self) -> bool {
        self.calibrations().any(|c| {
            matches!(
                c,
                CalibrationElement::Distortion(_) | CalibrationElement::Tca(_)
            )
        })
    }

    fn has_vignetting(&self) -> bool {
        self.calibrations()
            .any(|c| matches!(c, CalibrationElement::Vignetting(_)))
    }

    fn calibration_count(&self) -> usize {
        self.calibrations().count()
    }

    /// Lensfun keeps one `<lens>` entry per calibration sensor size. Entries with the
    /// same maker and model are the same physical lens.
    fn is_same_lens(&self, other: &Lens) -> bool {
        self.get_maker().eq_ignore_ascii_case(&other.get_maker())
            && self
                .get_canonical_model_name()
                .eq_ignore_ascii_case(&other.get_canonical_model_name())
    }

    fn crop_ratio(&self, camera_crop: Option<f32>) -> f64 {
        camera_crop.map_or(1.0, |crop| (self.calibration_crop() / crop) as f64)
    }

    /// Builds the profile from `geometry` (distortion and TCA) and `vignetting`, two
    /// entries of the same lens that may come from different calibration crops. Each
    /// set is scaled by its own calibration crop.
    fn profile(
        geometry: &Lens,
        vignetting: &Lens,
        shot: &Shot,
        camera_crop: Option<f32>,
    ) -> LensProfile {
        let focal = shot.focal.unwrap_or_else(|| geometry.focal_range().0);
        LensProfile {
            distortion: geometry.distortion_at(focal),
            tca: geometry.tca_at(focal),
            vignetting: shot.aperture.and_then(|aperture| {
                vignetting.vignetting_at(focal, aperture, shot.distance.unwrap_or(DEFAULT_DISTANCE))
            }),
            distortion_scale: geometry.aspect_ratio().hypot(1.0) * geometry.crop_ratio(camera_crop),
            vignetting_scale: vignetting.crop_ratio(camera_crop),
        }
    }
}

impl LensDatabase {
    fn lenses_for_maker(&self, maker: &str) -> Vec<&Lens> {
        self.lenses
            .iter()
            .filter(|l| l.get_maker() == maker)
            .collect()
    }

    fn lens_by_name(&self, maker: &str, model: &str) -> Option<&Lens> {
        let maker_lenses = self.lenses_for_maker(maker);
        maker_lenses
            .iter()
            .find(|l| l.get_display_name(&maker_lenses) == model)
            .copied()
    }

    fn camera_crop(&self, make: &str, model: &str) -> Option<f32> {
        self.cameras
            .iter()
            .find(|c| any_name_matches(&c.maker, make) && any_name_matches(&c.model, model))
            .map(|c| c.cropfactor)
            .filter(|c| *c > 0.01)
    }

    fn lens_on_camera_mount(&self, make: &str, model: &str) -> Option<&Lens> {
        if make.is_empty() || model.is_empty() {
            return None;
        }

        let mounts: Vec<&str> = self
            .cameras
            .iter()
            .filter(|c| any_name_matches(&c.maker, make) && any_name_matches(&c.model, model))
            .map(|c| c.mount.trim())
            .filter(|m| !m.is_empty())
            .collect();

        let mut hits = self.lenses.iter().filter(|lens| {
            lens.mount
                .iter()
                .any(|m| mounts.iter().any(|c| m.trim().eq_ignore_ascii_case(c)))
        });
        let lens = hits.next()?;
        if hits.next().is_some() {
            log::info!(
                "Mount fallback for {} {}: more than one lens on mount, giving up.",
                make,
                model
            );
            return None;
        }
        log::info!(
            "Mount fallback for {} {}: using fixed lens {} {}",
            make,
            model,
            lens.get_maker(),
            lens.get_full_model_name()
        );
        Some(lens)
    }

    /// Picks the best scoring lens. Equal scores go to the calibration crop closest to
    /// the camera, then to the entry with more calibration data, then to the first one.
    fn best_lens<'a>(
        candidates: impl Iterator<Item = &'a Lens>,
        camera_crop: Option<f32>,
        score: impl Fn(&Lens) -> Option<i64>,
    ) -> Option<&'a Lens> {
        let mut best: Option<(i64, f32, usize, &'a Lens)> = None;
        for lens in candidates {
            let Some(score) = score(lens) else {
                continue;
            };
            let distance = crop_distance(lens.calibration_crop(), camera_crop);
            let count = lens.calibration_count();
            let better = best.is_none_or(|(s, d, c, _)| {
                score > s || (score == s && (distance < d || (distance == d && count > c)))
            });
            if better {
                best = Some((score, distance, count, lens));
            }
        }
        best.map(|(.., lens)| lens)
    }

    fn word_score(lens: &Lens, pattern: &[String], make: &str) -> Option<i64> {
        let maker = lens.get_maker();
        let name = lens
            .model
            .iter()
            .flat_map(|m| {
                let with_maker = format!("{} {}", maker, m.value);
                [
                    word_match_score(pattern, &m.value),
                    word_match_score(pattern, &with_maker),
                ]
            })
            .flatten()
            .max()?;
        Some(name + i64::from(maker_matches(&maker, make)))
    }

    fn lens_by_words<'a>(
        candidates: impl Iterator<Item = &'a Lens>,
        shot: &Shot,
        camera_crop: Option<f32>,
    ) -> Option<&'a Lens> {
        let pattern = name_words(&shot.lens_model);
        Self::best_lens(candidates, camera_crop, |lens| {
            let crop = crop_score(lens.calibration_crop(), camera_crop)?;
            Some(Self::word_score(lens, &pattern, &shot.make)? * 4 + crop)
        })
    }

    fn lens_by_fuzzy_model<'a>(
        &'a self,
        maker: &str,
        model: &str,
        camera_crop: Option<f32>,
    ) -> Option<&'a Lens> {
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default().ignore_case();
        let score = |lens: &Lens, penalize_length: bool| {
            let crop = crop_score(lens.calibration_crop(), camera_crop)?;
            let english_name = lens.get_full_model_name();
            let canonical_name = lens.get_canonical_model_name();
            let score_english = matcher.fuzzy_match(&english_name, model).unwrap_or(0);
            let score_canonical = matcher.fuzzy_match(&canonical_name, model).unwrap_or(0);
            let score = score_english.max(score_canonical);
            if score <= 0 {
                return None;
            }
            let penalty = if penalize_length {
                let best_name = if score_canonical > score_english {
                    &canonical_name
                } else {
                    &english_name
                };
                (best_name.len() as i64 - model.len() as i64).max(0) / 2
            } else {
                0
            };
            Some(score - penalty + crop)
        };

        Self::best_lens(
            self.lenses
                .iter()
                .filter(|lens| maker_matches(&lens.get_maker(), maker)),
            camera_crop,
            |lens| score(lens, true),
        )
        .or_else(|| Self::best_lens(self.lenses.iter(), camera_crop, |lens| score(lens, false)))
    }

    fn saved_lenses(&self, saved: &[(String, String)]) -> Vec<&Lens> {
        saved
            .iter()
            .filter_map(|(maker, model)| self.lens_by_name(maker, model))
            .collect()
    }

    fn detect_lens(
        &self,
        shot: &Shot,
        camera_crop: Option<f32>,
        saved: &[(String, String)],
    ) -> Option<&Lens> {
        if shot.lens_model.is_empty() {
            return self.lens_on_camera_mount(&shot.make, &shot.model);
        }
        Self::lens_by_words(self.saved_lenses(saved).into_iter(), shot, camera_crop)
            .or_else(|| Self::lens_by_words(self.lenses.iter(), shot, camera_crop))
            .or_else(|| self.lens_by_fuzzy_model(&shot.make, &shot.lens_model, camera_crop))
            .or_else(|| self.lens_on_camera_mount(&shot.make, &shot.model))
    }

    /// The entry of the same lens that has the wanted calibration: `lens` itself when
    /// it has it, otherwise the calibration crop closest to the camera.
    fn calibration_source<'a>(
        &'a self,
        lens: &'a Lens,
        camera_crop: Option<f32>,
        has: impl Fn(&Lens) -> bool,
    ) -> &'a Lens {
        if has(lens) {
            return lens;
        }
        let target = camera_crop.or(Some(lens.calibration_crop()));
        Self::best_lens(
            self.lenses
                .iter()
                .filter(|other| has(other) && lens.is_same_lens(other)),
            target,
            |_| Some(0),
        )
        .unwrap_or(lens)
    }

    fn resolve(
        &self,
        shot: &Shot,
        selection: &Option<(String, String)>,
        saved: &[(String, String)],
    ) -> Option<ResolvedLens> {
        let camera_crop = self.camera_crop(&shot.make, &shot.model);
        let lens = match selection {
            Some((maker, model)) => self.lens_by_name(maker, model),
            None => self.detect_lens(shot, camera_crop, saved),
        }?;
        let geometry = self.calibration_source(lens, camera_crop, Lens::has_geometry);
        let vignetting = self.calibration_source(lens, camera_crop, Lens::has_vignetting);
        let profile = Lens::profile(geometry, vignetting, shot, camera_crop);
        let maker = lens.get_maker();
        let info = LensCorrectionInfo {
            model: lens.get_display_name(&self.lenses_for_maker(&maker)),
            maker,
            distortion: profile.distortion.is_some(),
            tca: profile.tca.is_some(),
            vignetting: profile.vignetting.is_some(),
        };
        Some(ResolvedLens { info, profile })
    }
}

fn database() -> Option<&'static LensDatabase> {
    DATABASE.get()
}

fn profile_cache() -> &'static Mutex<HashMap<ProfileKey, Option<Arc<ResolvedLens>>>> {
    static CACHE: OnceLock<Mutex<HashMap<ProfileKey, Option<Arc<ResolvedLens>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn source_key(path: &str) -> String {
    parse_virtual_path(path).0.to_string_lossy().into_owned()
}

fn lens_selection(adjustments: &Value) -> Option<Option<(String, String)>> {
    if adjustments
        .get("lensCorrectionMode")
        .and_then(Value::as_str)
        == Some("auto")
    {
        return Some(None);
    }
    let name = |key: &str| {
        adjustments
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Some((name("lensMaker")?, name("lensModel")?)))
}

fn resolve(path: &str, adjustments: &Value) -> Option<Arc<ResolvedLens>> {
    let selection = lens_selection(adjustments)?;
    let key = (source_key(path), selection);
    if let Some(cached) = profile_cache().lock().unwrap().get(&key) {
        return cached.clone();
    }
    let db = database()?;
    let saved = SAVED_LENSES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let resolved = db
        .resolve(&Shot::read(&key.0), &key.1, &saved)
        .map(Arc::new);
    let mut cache = profile_cache().lock().unwrap();
    if cache.len() >= MAX_CACHED_PROFILES {
        cache.clear();
    }
    cache.insert(key, resolved.clone());
    resolved
}

pub fn has_lens_selection(adjustments: &Value) -> bool {
    lens_selection(adjustments).is_some()
}

pub fn same_lens_selection(a: &Value, b: &Value) -> bool {
    lens_selection(a) == lens_selection(b)
}

pub fn lens_profile(path: &str, adjustments: &Value) -> Option<LensProfile> {
    resolve(path, adjustments).map(|r| r.profile.clone())
}

pub fn forget_lens_profiles(path: &str) {
    let source = source_key(path);
    profile_cache()
        .lock()
        .unwrap()
        .retain(|(cached, _), _| *cached != source);
}

/// Saved lenses ("My Lenses") are tried first by the automatic lens detection.
pub fn set_saved_lenses(lenses: Vec<(String, String)>) {
    let mut saved = SAVED_LENSES.write().unwrap_or_else(|e| e.into_inner());
    if *saved != lenses {
        *saved = lenses;
        drop(saved);
        profile_cache()
            .lock()
            .unwrap()
            .retain(|(_, selection), _| selection.is_some());
    }
}

pub fn init_lensfun_db(app_handle: &tauri::AppHandle) {
    let _ = DATABASE.set(load_lensfun_db(app_handle));
}

fn load_lensfun_db(app_handle: &tauri::AppHandle) -> LensDatabase {
    let mut combined_db = LensDatabase {
        cameras: Vec::new(),
        lenses: Vec::new(),
    };

    #[cfg(target_os = "android")]
    {
        let _ = app_handle;
        log::info!("Loading Lensfun DB from embedded assets (Android path)");

        for file in LENS_DB_DIR.files() {
            let is_xml = file
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("xml"))
                .unwrap_or(false);

            if is_xml {
                if let Some(xml_content) = file.contents_utf8() {
                    match quick_xml::de::from_str::<LensDatabase>(xml_content) {
                        Ok(mut db) => {
                            combined_db.cameras.append(&mut db.cameras);
                            combined_db.lenses.append(&mut db.lenses);
                        }
                        Err(e) => {
                            log::error!("Failed to parse embedded XML {:?}: {}", file.path(), e)
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        let resource_path = app_handle
            .path()
            .resolve("lensfun_db", tauri::path::BaseDirectory::Resource)
            .expect("failed to resolve lensfun_db directory");

        if !resource_path.exists() {
            log::error!("Lensfun DB directory not found at: {:?}", resource_path);
            return combined_db;
        }

        for entry in WalkDir::new(resource_path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "xml"))
        {
            let path = entry.path();
            log::info!("Processing file: {:?}", path);
            match fs::read_to_string(path) {
                Ok(xml_content) => match quick_xml::de::from_str::<LensDatabase>(&xml_content) {
                    Ok(mut db) => {
                        combined_db.cameras.append(&mut db.cameras);
                        combined_db.lenses.append(&mut db.lenses);
                    }
                    Err(e) => {
                        log::error!("Failed to parse Lensfun XML file {:?}: {}", path, e);
                    }
                },
                Err(e) => log::error!("Failed to read Lensfun XML file {:?}: {}", path, e),
            }
        }
    }

    log::info!(
        "Loaded {} lenses and {} cameras from Lensfun database.",
        combined_db.lenses.len(),
        combined_db.cameras.len()
    );
    combined_db
}

#[tauri::command]
pub fn get_lensfun_makers() -> Result<Vec<String>, String> {
    let db = database().ok_or("Lens database not loaded")?;
    let mut makers: Vec<String> = db.lenses.iter().map(|lens| lens.get_maker()).collect();
    makers.sort_unstable();
    makers.dedup();
    Ok(makers)
}

#[tauri::command]
pub fn get_lensfun_lenses_for_maker(maker: String) -> Result<Vec<String>, String> {
    let db = database().ok_or("Lens database not loaded")?;
    let maker_lenses = db.lenses_for_maker(&maker);
    let mut models: Vec<String> = maker_lenses
        .iter()
        .map(|lens| lens.get_display_name(&maker_lenses))
        .collect();
    models.sort_unstable();
    models.dedup();
    Ok(models)
}

#[tauri::command]
pub fn get_lens_correction_info(path: String, selection: Value) -> Option<LensCorrectionInfo> {
    resolve(&path, &selection).map(|r| r.info.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &str = r#"<lensdatabase version="2">
    <camera>
        <maker>Canon</maker>
        <model>Canon EOS M6 Mark II</model>
        <mount>Canon EF-M</mount>
        <cropfactor>1.613</cropfactor>
    </camera>
    <camera>
        <maker>Nikon Corporation</maker>
        <maker lang="en">Nikon</maker>
        <model>Nikon D600</model>
        <mount>Nikon F AF</mount>
        <cropfactor>1</cropfactor>
    </camera>
    <camera>
        <maker>Sony</maker>
        <model>ILCE-6000</model>
        <mount>Sony E</mount>
        <cropfactor>1.534</cropfactor>
    </camera>
    <lens>
        <maker>Canon</maker>
        <model>Canon EF 50mm f/1.8 STM</model>
        <mount>Canon EF</mount>
        <cropfactor>1.0</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="50" a="0.0061844" b="-0.0313122" c="0.0314815"/>
            <vignetting model="pa" focal="50" aperture="1.8" distance="1000" k1="-0.5" k2="0.1" k3="0"/>
        </calibration>
    </lens>
    <lens>
        <maker>Canon</maker>
        <model>Canon EF 50mm f/1.8 STM</model>
        <mount>Canon EF</mount>
        <cropfactor>1.613</cropfactor>
        <calibration>
            <vignetting model="pa" focal="50" aperture="1.8" distance="1000" k1="-0.3" k2="0.05" k3="0"/>
        </calibration>
    </lens>
    <lens>
        <maker>Nikon</maker>
        <model>Nikon AF-S Zoom-Nikkor 28-300mm f/3.5-5.6G ED VR</model>
        <model lang="en">Nikkor AF-S 28-300mm f/3.5-5.6G ED VR</model>
        <mount>Nikon F AF</mount>
        <cropfactor>1</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="28" a="0.02437" b="-0.07944" c="0.03221"/>
        </calibration>
    </lens>
    <lens>
        <maker>Nikon</maker>
        <model>Nikon AF-S DX Nikkor 18-300mm f/3.5-5.6G ED VR</model>
        <mount>Nikon F AF</mount>
        <cropfactor>1.523</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="28" a="0.01" b="-0.02" c="0.01"/>
        </calibration>
    </lens>
    <lens>
        <maker>Meike</maker>
        <model>Meike 35mm f/1.7</model>
        <mount>Sony E</mount>
        <cropfactor>1.528</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="35" a="-0.00688" b="0.00111" c="-0.00744"/>
        </calibration>
    </lens>
    <lens>
        <maker>Viltrox</maker>
        <model>Viltrox 35mm F1.7 E</model>
        <mount>Sony E</mount>
        <cropfactor>1.534</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="35" a="0.006113" b="-0.0140932" c="0.0129453"/>
        </calibration>
    </lens>
    <lens>
        <maker>Sigma</maker>
        <model>Sigma 35mm F1.4 DG HSM | A</model>
        <mount>Sony E</mount>
        <cropfactor>1.534</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="35" a="0.001" b="-0.002" c="0.001"/>
        </calibration>
    </lens>
    <lens>
        <maker>Viltrox</maker>
        <model>Viltrox AF 14mm f/4.0 Z/FE</model>
        <model lang="en">Viltrox AF 14mm f/4.0</model>
        <mount>Nikon Z</mount>
        <mount>Sony E</mount>
        <cropfactor>1.0</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="14" a="0.03431" b="-0.11264" c="0.09647"/>
        </calibration>
    </lens>
</lensdatabase>"#;

    fn db() -> LensDatabase {
        quick_xml::de::from_str(DB).expect("test database parses")
    }

    fn shot(make: &str, model: &str, lens: &str, focal: f32) -> Shot {
        Shot {
            make: make.into(),
            model: model.into(),
            lens_model: lens.into(),
            focal: Some(focal),
            aperture: Some(1.8),
            distance: None,
        }
    }

    fn detected(db: &LensDatabase, shot: &Shot, saved: &[(String, String)]) -> String {
        db.resolve(shot, &None, saved)
            .map(|r| format!("{} {}", r.info.maker, r.info.model))
            .unwrap_or_default()
    }

    #[test]
    fn name_words_ignore_punctuation_order_and_trailing_zeros() {
        assert_eq!(
            name_words("AF-S Nikkor 28-300mm f/3.5-5.6G"),
            [
                "af", "s", "nikkor", "28", "300", "mm", "f", "3.5", "5.6", "g"
            ]
        );
        assert_eq!(name_words("f/4.0"), name_words("F4"));
        assert_eq!(name_words("M.12-40mm"), ["m", "12", "40", "mm"]);
    }

    #[test]
    fn word_match_needs_every_exif_word_in_any_order() {
        let pattern = name_words("E 35mm F1.7");
        assert!(word_match_score(&pattern, "Viltrox 35mm F1.7 E").is_some());
        assert_eq!(word_match_score(&pattern, "Meike 35mm f/1.7"), None);
        let short = word_match_score(&pattern, "35mm F1.7 E").unwrap();
        let long = word_match_score(&pattern, "Viltrox 35mm F1.7 E").unwrap();
        assert!(short > long, "extra words lower the score");
    }

    #[test]
    fn maker_matching_tolerates_company_suffixes() {
        assert!(maker_matches("Nikon", "NIKON CORPORATION"));
        assert!(maker_matches("Olympus", "OLYMPUS IMAGING CORP."));
        assert!(!maker_matches("Sigma", "SONY"));
    }

    #[test]
    fn finds_lens_whose_exif_name_has_a_different_word_order() {
        let db = db();
        let shot = shot(
            "NIKON CORPORATION",
            "NIKON D600",
            "AF-S Nikkor 28-300mm f/3.5-5.6G ED VR",
            28.0,
        );
        let resolved = db.resolve(&shot, &None, &[]).unwrap();
        assert_eq!(resolved.info.maker, "Nikon");
        assert!(resolved.info.model.contains("28-300mm"));
        assert!(resolved.profile.distortion.is_some());
    }

    #[test]
    fn exif_name_without_maker_does_not_pick_an_unrelated_lens() {
        let db = db();
        let shot = shot("SONY", "ILCE-6000", "E 35mm F1.7", 35.0);
        assert_eq!(detected(&db, &shot, &[]), "Viltrox 35mm F1.7 E");
        assert_eq!(
            detected(
                &db,
                &self::shot("Nikon", "Z 6_3", "Viltrox AF 14mm f/4", 14.0),
                &[]
            ),
            "Viltrox AF 14mm f/4.0"
        );
    }

    #[test]
    fn saved_lenses_are_preferred_when_they_match() {
        let db = db();
        let shot = shot("SONY", "ILCE-6000", "35mm F1.4", 35.0);
        let sigma = "Sigma 35mm F1.4 DG HSM | A";
        assert_eq!(detected(&db, &shot, &[]), sigma);

        let saved = [("Viltrox".to_string(), "35mm F1.7 E".to_string())];
        // A saved lens that does not match the EXIF name is not forced on the image.
        assert_eq!(detected(&db, &shot, &saved), sigma);

        let shot = self::shot("SONY", "ILCE-6000", "35mm", 35.0);
        let saved = [("Viltrox".to_string(), "35mm F1.7 E".to_string())];
        assert_eq!(detected(&db, &shot, &saved), "Viltrox 35mm F1.7 E");
    }

    #[test]
    fn equal_scores_go_to_the_closest_crop_and_keep_the_distortion_of_the_sibling() {
        let db = db();
        let shot = shot("Canon", "Canon EOS M6 Mark II", "EF50mm f/1.8 STM", 50.0);
        let resolved = db.resolve(&shot, &None, &[]).unwrap();
        // The crop 1.613 entry matches the camera, but only has vignetting. The
        // distortion comes from the full frame entry, scaled to the smaller sensor.
        let profile = &resolved.profile;
        assert!(profile.distortion.is_some());
        let expected_scale = 1.5f64.hypot(1.0) * (1.0 / 1.613f32) as f64;
        assert!((profile.distortion_scale - expected_scale).abs() < 1e-6);
        assert!((profile.vignetting_scale - 1.0).abs() < 1e-6);
        assert!((profile.vignetting.unwrap()[0] + 0.3).abs() < 1e-6);

        // On an unknown body the tie goes to the entry with more calibration data,
        // not to whichever entry comes last in the file.
        let unknown = self::shot("Canon", "Unknown", "EF50mm f/1.8 STM", 50.0);
        let resolved = db.resolve(&unknown, &None, &[]).unwrap();
        assert!((resolved.profile.vignetting.unwrap()[0] + 0.5).abs() < 1e-6);
        assert!(resolved.profile.distortion.is_some());
    }

    #[test]
    fn manual_selection_of_a_vignetting_only_entry_still_gets_distortion() {
        let db = db();
        let shot = shot("Canon", "Canon EOS M6 Mark II", "", 50.0);
        let maker_lenses = db.lenses_for_maker("Canon");
        let crop_entry = maker_lenses
            .iter()
            .find(|l| l.cropfactor == Some(1.613))
            .unwrap()
            .get_display_name(&maker_lenses);
        let selection = Some(("Canon".to_string(), crop_entry));
        let resolved = db.resolve(&shot, &selection, &[]).unwrap();
        assert!(resolved.profile.distortion.is_some());
        assert!((resolved.profile.vignetting.unwrap()[0] + 0.3).abs() < 1e-6);
    }
}
