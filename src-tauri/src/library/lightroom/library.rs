//! Finds the presets and profiles of a local Lightroom / Camera Raw install,
//! and imports them into the RapidRAW preset library grouped like Lightroom.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::convert::convert;
use super::tables::{ProfileTable, bake_cube, decode_table};
use super::xmp::{CrsDoc, LookRef, parse_crs};
use crate::library::presets::{
    Preset, PresetFolder, PresetImportFailure, PresetItem, load_presets, save_presets,
};

pub const LUT_SIZE: usize = 33;
/// Bump when `bake_cube` changes what it writes, so existing cubes are baked again.
const BAKE_VERSION: u32 = 2;
static BAKE_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const IMPORTED_FOLDER_PREFIX: &str = "lr-group-";
const IMPORTED_PRESET_PREFIX: &str = "lr-";

/// Lightroom lists its built-in groups in a fixed order rather than
/// alphabetically. Groups not listed here follow, sorted by name.
const GROUP_ORDER: &[&str] = &[
    "Adaptive: Portrait",
    "Adaptive: Sky",
    "Adaptive: Subject",
    "Adaptive: Landscape",
    "Adaptive: Blur Background",
    "Portraits: Deep Skin",
    "Portraits: Medium Skin",
    "Portraits: Light Skin",
    "Portraits: Black & White",
    "Portraits: Group",
    "Portraits: Edgy",
    "Auto+: Retro",
    "Seasons: Spring",
    "Seasons: Summer",
    "Seasons: Autumn",
    "Seasons: Winter",
    "Style: Cinematic",
    "Style: Cinematic II",
    "Style: Futuristic",
    "Style: Vintage",
    "Style: Black & White",
    "Style: Film-Inspired",
    "Subject: Food",
    "Subject: Concerts",
    "Subject: Landscape",
    "Subject: Lifestyle",
    "Subject: Travel",
    "Subject: Travel II",
    "Subject: Urban Architecture",
    "Video: Creative",
    "Color",
    "Creative",
    "B&W",
    "Portraits",
    "Curve",
    "Grain",
    "Optics",
    "Sharpening",
    "Vignetting",
    "Defaults",
];

pub fn default_settings_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(program_data) = std::env::var_os("PROGRAMDATA") {
            roots.push(PathBuf::from(program_data).join("Adobe/CameraRaw/Settings"));
        }
        if let Some(app_data) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(app_data).join("Adobe/CameraRaw/Settings"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        roots.push(PathBuf::from(
            "/Library/Application Support/Adobe/CameraRaw/Settings",
        ));
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(
                PathBuf::from(home).join("Library/Application Support/Adobe/CameraRaw/Settings"),
            );
        }
    }
    roots.into_iter().filter(|r| r.is_dir()).collect()
}

fn collect_xmp(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Camera-matching profiles are per-camera DCP looks that presets
            // never reference; skipping them keeps the scan fast.
            let is_camera_profiles = path.file_name().is_some_and(|n| n == "Camera")
                && dir.file_name().is_some_and(|n| n == "Profiles");
            if !is_camera_profiles {
                collect_xmp(&path, out);
            }
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
        {
            out.push(path);
        }
    }
}

struct BakedLut {
    path: String,
    amount_range: (f64, f64),
}

pub struct ProfileIndex {
    by_uuid: HashMap<String, Arc<CrsDoc>>,
    by_name: HashMap<String, Arc<CrsDoc>>,
    lut_dir: Option<PathBuf>,
    baked: Mutex<HashMap<String, Option<BakedLut>>>,
}

impl ProfileIndex {
    pub fn new(lut_dir: Option<PathBuf>) -> Self {
        Self {
            by_uuid: HashMap::new(),
            by_name: HashMap::new(),
            lut_dir,
            baked: Mutex::new(HashMap::new()),
        }
    }

    fn add(&mut self, doc: CrsDoc) {
        let doc = Arc::new(doc);
        if let Some(uuid) = doc.uuid() {
            self.by_uuid.insert(uuid.to_uppercase(), doc.clone());
        }
        if let Some(name) = &doc.name {
            self.by_name.entry(name.to_lowercase()).or_insert(doc);
        }
    }

    pub fn len(&self) -> usize {
        self.by_uuid.len().max(self.by_name.len())
    }

    pub fn find(&self, look: &LookRef) -> Option<&CrsDoc> {
        look.uuid
            .as_ref()
            .and_then(|u| self.by_uuid.get(&u.to_uppercase()))
            .or_else(|| {
                look.name
                    .as_ref()
                    .and_then(|n| self.by_name.get(&n.to_lowercase()))
            })
            .map(|d| d.as_ref())
    }

    /// Bakes the profile's color tables to a `.cube` file (once per profile)
    /// and returns its path with the amount range the tables allow.
    pub fn lut_for(&self, profile: &CrsDoc) -> Option<(String, (f64, f64))> {
        let lut_dir = self.lut_dir.as_ref()?;
        let digests: Vec<&str> = ["LookTable", "RGBTable"]
            .iter()
            .filter_map(|k| profile.get(k))
            .filter(|d| !d.is_empty())
            .collect();
        if digests.is_empty() {
            return None;
        }
        let rgb_strength = profile
            .get("RGBTableAmount")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .map_or(1.0, |v| v.clamp(0.0, 1.0));
        let mut key = digests.join("+");
        if rgb_strength < 1.0 {
            key.push_str(&format!(" {}", (rgb_strength * 100.0).round()));
        }

        // A panic while baking must not leave every later lookup failing on a poisoned lock.
        let mut baked = self.baked.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = baked.get(&key) {
            return entry.as_ref().map(|b| (b.path.clone(), b.amount_range));
        }

        let entry = (|| {
            let tables: Vec<ProfileTable> = digests
                .iter()
                .filter_map(|d| profile.get(&format!("Table_{}", d)))
                .filter_map(|encoded| match decode_table(encoded) {
                    Ok(mut t) => {
                        t.set_rgb_strength(rgb_strength);
                        Some(t)
                    }
                    Err(e) => {
                        log::warn!("Skipping Lightroom profile table: {}", e);
                        None
                    }
                })
                .collect();
            if tables.is_empty() {
                return None;
            }
            let amount_range = tables
                .iter()
                .map(|t| t.amount_range())
                .fold((f64::MIN, f64::MAX), |acc, r| {
                    (acc.0.max(r.0), acc.1.min(r.1))
                });

            let name = profile.name.as_deref().unwrap_or("Profile");
            // Every input of the bake is in the name, so a changed table, strength or baking method
            // never reuses an old cube.
            let identity = format!("{} v{} n{}", key, BAKE_VERSION, LUT_SIZE);
            let file_name = format!("{} {}.cube", sanitize_file_name(name), name_hash(&identity));
            let path = lut_dir.join(file_name);
            if !path.exists() {
                let refs: Vec<&ProfileTable> = tables.iter().collect();
                let cube = bake_cube(name, &refs, LUT_SIZE);
                fs::create_dir_all(lut_dir).ok()?;
                // Write then rename, so a crash or a second baker never leaves a truncated cube behind.
                let n = BAKE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let tmp = path.with_extension(format!("cube.{}-{}.tmp", std::process::id(), n));
                let written = fs::write(&tmp, cube).and_then(|_| fs::rename(&tmp, &path));
                if let Err(e) = written {
                    let _ = fs::remove_file(&tmp);
                    if !path.exists() {
                        log::warn!("Failed to write baked profile LUT {:?}: {}", path, e);
                        return None;
                    }
                }
            }
            Some(BakedLut {
                path: path.to_string_lossy().into_owned(),
                amount_range,
            })
        })();

        let result = entry.as_ref().map(|b| (b.path.clone(), b.amount_range));
        baked.insert(key, entry);
        result
    }
}

fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn lut_dir(app_handle: &AppHandle) -> Option<PathBuf> {
    app_handle
        .path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("presets").join("lightroom-profiles"))
}

struct Scan {
    presets: Vec<(PathBuf, CrsDoc)>,
    profiles: ProfileIndex,
    failures: Vec<PresetImportFailure>,
}

fn scan(preset_roots: &[PathBuf], profile_roots: &[PathBuf], lut_dir: Option<PathBuf>) -> Scan {
    let mut profiles = ProfileIndex::new(lut_dir);
    let mut presets = Vec::new();
    let mut failures = Vec::new();
    let preset_set: HashSet<&PathBuf> = preset_roots.iter().collect();
    let mut seen = HashSet::new();

    for root in preset_roots.iter().chain(profile_roots) {
        let mut files = Vec::new();
        collect_xmp(root, &mut files);
        for file in files {
            if !seen.insert(file.clone()) {
                continue;
            }
            let doc = match fs::read_to_string(&file)
                .map_err(|e| e.to_string())
                .and_then(|c| parse_crs(&c))
            {
                Ok(doc) => doc,
                Err(error) => {
                    if preset_set.contains(root) {
                        failures.push(PresetImportFailure {
                            file_name: file.to_string_lossy().into_owned(),
                            error,
                        });
                    }
                    continue;
                }
            };
            match doc.preset_type() {
                "Look" => profiles.add(doc),
                "Normal" if preset_set.contains(root) => presets.push((file, doc)),
                _ => {}
            }
        }
    }
    Scan {
        presets,
        profiles,
        failures,
    }
}

/// Profiles from the default install, shared by single-file XMP imports.
pub fn shared_profile_index(app_handle: &AppHandle) -> Arc<ProfileIndex> {
    static INDEX: OnceLock<Arc<ProfileIndex>> = OnceLock::new();
    INDEX
        .get_or_init(|| {
            Arc::new(scan(&[], &default_settings_roots(), lut_dir(app_handle)).profiles)
        })
        .clone()
}

/// Splits a name into text and number runs so "Preset 10" sorts after "Preset 9".
fn natural_cmp(a: &str, b: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, run)) if *d == digit => run.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (ca, cb) = (chunks(&a.to_lowercase()), chunks(&b.to_lowercase()));
    for ((da, ra), (db, rb)) in ca.iter().zip(cb.iter()) {
        let ord = if *da && *db {
            ra.trim_start_matches('0')
                .len()
                .cmp(&rb.trim_start_matches('0').len())
                .then_with(|| ra.trim_start_matches('0').cmp(rb.trim_start_matches('0')))
        } else {
            ra.cmp(rb)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ca.len().cmp(&cb.len())
}

fn group_rank(name: &str) -> (usize, String) {
    let pos = GROUP_ORDER
        .iter()
        .position(|g| g.eq_ignore_ascii_case(name))
        .unwrap_or(GROUP_ORDER.len());
    (pos, name.to_lowercase())
}

/// Stable across Rust versions, unlike `DefaultHasher`, so folder ids survive app updates.
fn name_hash(name: &str) -> String {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{:08x}", hash as u32)
}

fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LightroomImportResult {
    pub presets: Vec<PresetItem>,
    pub imported: usize,
    pub groups: usize,
    pub profiles: usize,
    pub missing_profiles: Vec<String>,
    pub failures: Vec<PresetImportFailure>,
}

/// Converts every preset under the roots into folders named after their
/// Lightroom groups. Returns the folders in Lightroom's order.
fn build_folders(scan: &Scan) -> (Vec<PresetFolder>, usize, Vec<String>) {
    struct Entry<'a> {
        doc: &'a CrsDoc,
        file: &'a Path,
    }
    let mut groups: HashMap<String, Vec<Entry>> = HashMap::new();
    for (file, doc) in &scan.presets {
        let group = doc
            .group
            .clone()
            .filter(|g| !g.trim().is_empty())
            .or_else(|| {
                file.parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "Lightroom".to_string());
        groups.entry(group).or_default().push(Entry { doc, file });
    }

    let mut names: Vec<String> = groups.keys().cloned().collect();
    names.sort_by_key(|n| group_rank(n));

    let mut used_ids = HashSet::new();
    let mut used_folder_ids = HashSet::new();
    let mut missing = HashSet::new();
    let mut count = 0;
    let mut folders = Vec::new();
    for group in names {
        let mut entries = groups.remove(&group).unwrap_or_default();
        entries.sort_by(|a, b| {
            let key = |e: &Entry| {
                e.doc
                    .sort_name
                    .clone()
                    .or_else(|| e.doc.name.clone())
                    .unwrap_or_else(|| e.file.to_string_lossy().into_owned())
            };
            natural_cmp(&key(a), &key(b))
        });

        let mut children = Vec::new();
        for entry in entries {
            let conversion = convert(entry.doc, Some(&scan.profiles));
            if let Some(name) = conversion.missing_profile {
                missing.insert(name);
            }
            let mut preset = conversion.preset;
            preset.name = entry
                .doc
                .display_name()
                .map(str::to_string)
                .or_else(|| {
                    entry
                        .file
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                })
                .unwrap_or(preset.name);
            let base_id = entry
                .doc
                .uuid()
                .map(|u| format!("{}{}", IMPORTED_PRESET_PREFIX, u.to_lowercase()))
                .unwrap_or_else(|| preset.id.clone());
            let mut id = base_id.clone();
            let mut n = 1;
            while !used_ids.insert(id.clone()) {
                n += 1;
                id = format!("{}-{}", base_id, n);
            }
            preset.id = id;
            children.push(preset);
            count += 1;
        }

        // Non-Latin names slug to nothing, and names differing only in case or punctuation slug alike.
        let slugged = slug(&group);
        let mut folder_id = if slugged.is_empty() {
            format!("{}{}", IMPORTED_FOLDER_PREFIX, name_hash(&group))
        } else {
            format!("{}{}", IMPORTED_FOLDER_PREFIX, slugged)
        };
        if !used_folder_ids.insert(folder_id.clone()) {
            folder_id = format!("{}-{}", folder_id, name_hash(&group));
            used_folder_ids.insert(folder_id.clone());
        }
        folders.push(PresetFolder {
            id: folder_id,
            name: group,
            children,
        });
    }
    let mut missing: Vec<String> = missing.into_iter().collect();
    missing.sort();
    (folders, count, missing)
}

/// Replaces previously imported folders with the fresh conversion while keeping what the user did to them:
/// presets they added to an imported folder stay in it, and imported presets they moved elsewhere are
/// refreshed where they are instead of coming back as a second copy with the same id.
fn merge_with_existing(current: &mut Vec<PresetItem>, folders: &mut [PresetFolder]) {
    let replaced: HashSet<String> = folders.iter().map(|f| f.id.clone()).collect();
    let mut user_added: HashMap<String, Vec<Preset>> = HashMap::new();
    current.retain(|item| match item {
        PresetItem::Folder(f) if replaced.contains(&f.id) => {
            let kept: Vec<Preset> = f
                .children
                .iter()
                .filter(|p| !p.id.starts_with(IMPORTED_PRESET_PREFIX))
                .cloned()
                .collect();
            if !kept.is_empty() {
                user_added.insert(f.id.clone(), kept);
            }
            false
        }
        _ => true,
    });

    let mut fresh: HashMap<String, Preset> = HashMap::new();
    for folder in folders.iter() {
        for p in &folder.children {
            fresh.insert(p.id.clone(), p.clone());
        }
    }
    let mut moved: HashSet<String> = HashSet::new();
    let mut refresh = |p: &mut Preset| {
        if let Some(new) = fresh.get(&p.id) {
            let name = std::mem::take(&mut p.name);
            *p = new.clone();
            p.name = name;
            moved.insert(p.id.clone());
        }
    };
    for item in current.iter_mut() {
        match item {
            PresetItem::Preset(p) => refresh(p),
            PresetItem::Folder(f) => f.children.iter_mut().for_each(&mut refresh),
        }
    }

    for folder in folders.iter_mut() {
        folder.children.retain(|p| !moved.contains(&p.id));
        if let Some(kept) = user_added.remove(&folder.id) {
            folder.children.extend(kept);
        }
    }
}

fn import(
    roots: Option<Vec<String>>,
    app_handle: &AppHandle,
) -> Result<LightroomImportResult, String> {
    let defaults = default_settings_roots();
    let preset_roots: Vec<PathBuf> = match roots {
        Some(r) if !r.is_empty() => r.into_iter().map(PathBuf::from).collect(),
        _ => defaults.clone(),
    };
    if preset_roots.is_empty() {
        return Err("NO_LIGHTROOM_INSTALL".to_string());
    }

    let lut_dir = lut_dir(app_handle);
    let scan = scan(&preset_roots, &defaults, lut_dir);
    if scan.presets.is_empty() {
        return Err("NO_LIGHTROOM_PRESETS".to_string());
    }
    let (folders, imported, missing_profiles) = build_folders(&scan);

    let mut current = load_presets(app_handle.clone())?;
    let mut folders = folders;
    merge_with_existing(&mut current, &mut folders);
    let mut taken: HashSet<String> = current
        .iter()
        .map(|item| match item {
            PresetItem::Preset(p) => p.name.clone(),
            PresetItem::Folder(f) => f.name.clone(),
        })
        .collect();

    let groups = folders.len();
    for mut folder in folders {
        if taken.contains(&folder.name) {
            folder.name = format!("{} (Lightroom)", folder.name);
        }
        taken.insert(folder.name.clone());
        current.push(PresetItem::Folder(folder));
    }

    save_presets(current.clone(), app_handle.clone())?;

    Ok(LightroomImportResult {
        presets: current,
        imported,
        groups,
        profiles: scan.profiles.len(),
        missing_profiles,
        failures: scan.failures,
    })
}

#[tauri::command]
pub async fn import_lightroom_presets(
    root_paths: Option<Vec<String>>,
    app_handle: AppHandle,
) -> Result<LightroomImportResult, String> {
    tauri::async_runtime::spawn_blocking(move || import(root_paths, &app_handle))
        .await
        .map_err(|e| e.to_string())?
}

/// Bump when the converter output changes so existing installs pick up the fix on next launch.
const CONVERTER_VERSION: &str = "13";
const VERSION_MARKER: &str = "lightroom-import-version";

/// Imports the Lightroom install's presets on first launch, and again after a converter update.
/// Returns `None` when there is nothing to do or no Lightroom install was found.
#[tauri::command]
pub async fn auto_import_lightroom_presets(
    app_handle: AppHandle,
) -> Result<Option<LightroomImportResult>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if default_settings_roots().is_empty() {
            return Ok(None);
        }
        let marker = app_handle
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("presets")
            .join(VERSION_MARKER);
        if fs::read_to_string(&marker).is_ok_and(|v| v.trim() == CONVERTER_VERSION) {
            return Ok(None);
        }
        let result = match import(None, &app_handle) {
            Ok(result) => result,
            Err(e) if e == "NO_LIGHTROOM_PRESETS" => return Ok(None),
            Err(e) => return Err(e),
        };
        if let Some(parent) = marker.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&marker, CONVERTER_VERSION).map_err(|e| e.to_string())?;
        Ok(Some(result))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: &str, name: &str, exposure: f64) -> Preset {
        Preset {
            id: id.into(),
            name: name.into(),
            adjustments: serde_json::json!({ "exposure": exposure }),
            include_masks: None,
            include_crop_transform: None,
            preset_type: None,
        }
    }

    #[test]
    fn reimport_keeps_user_changes() {
        let mut current = vec![
            PresetItem::Folder(PresetFolder {
                id: "lr-group-color".into(),
                name: "Color".into(),
                children: vec![preset("lr-a", "A", 1.0), preset("mine", "Mine", 5.0)],
            }),
            PresetItem::Preset(preset("lr-b", "Moved B", 1.0)),
        ];
        let mut folders = vec![PresetFolder {
            id: "lr-group-color".into(),
            name: "Color".into(),
            children: vec![preset("lr-a", "A", 2.0), preset("lr-b", "B", 2.0)],
        }];
        merge_with_existing(&mut current, &mut folders);
        let ids: Vec<&str> = folders[0].children.iter().map(|p| p.id.as_str()).collect();
        // The user's own preset stays; the moved one is not duplicated back into the folder.
        assert_eq!(ids, ["lr-a", "mine"]);
        assert_eq!(folders[0].children[0].adjustments["exposure"], 2.0);
        let PresetItem::Preset(moved) = &current[0] else {
            panic!("moved preset lost")
        };
        assert_eq!(moved.name, "Moved B");
        assert_eq!(moved.adjustments["exposure"], 2.0);
    }

    #[test]
    fn non_latin_group_names_get_distinct_ids() {
        assert!(slug("人像").is_empty());
        assert_ne!(name_hash("人像"), name_hash("Портреты"));
        assert_eq!(name_hash("Color"), name_hash("Color"));
    }

    #[test]
    fn natural_sort_orders_numbers() {
        let mut v = vec!["Preset 10", "Preset 2", "Preset 1", "Adaptive Subject 02.7"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            ["Adaptive Subject 02.7", "Preset 1", "Preset 2", "Preset 10"]
        );
    }

    #[test]
    fn groups_follow_lightroom_order() {
        let mut v = vec![
            "Zebra",
            "Adaptive: Landscape",
            "Adaptive: Portrait",
            "Color",
        ];
        v.sort_by_key(|n| group_rank(n));
        assert_eq!(
            v,
            [
                "Adaptive: Portrait",
                "Adaptive: Landscape",
                "Color",
                "Zebra"
            ]
        );
    }

    /// Converts the real Lightroom install when one is present. Run with
    /// `cargo test lightroom_install -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn lightroom_install_converts() {
        let roots = default_settings_roots();
        assert!(!roots.is_empty(), "no Lightroom install found");
        let dir = std::env::temp_dir().join("rapidraw-lr-luts");
        let scan = scan(&roots, &roots, Some(dir.clone()));
        println!(
            "{} presets, {} profiles, {} failures",
            scan.presets.len(),
            scan.profiles.len(),
            scan.failures.len()
        );
        for f in &scan.failures {
            println!("FAIL {} {}", f.file_name, f.error);
        }
        let (folders, count, missing) = build_folders(&scan);
        println!(
            "{} presets in {} groups, missing profiles {:?}",
            count,
            folders.len(),
            missing
        );
        for f in &folders {
            let names: Vec<&str> = f.children.iter().map(|p| p.name.as_str()).collect();
            println!("[{}] {}", f.name, names.join(", "));
        }
        let dump: Vec<_> = folders
            .iter()
            .map(|f| (f.name.clone(), f.children.clone()))
            .collect();
        fs::write(
            dir.join("converted.json"),
            serde_json::to_string_pretty(&dump).unwrap(),
        )
        .unwrap();
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "cube") {
                crate::adjustments::lut_processing::parse_lut_file(&p.to_string_lossy())
                    .unwrap_or_else(|e| panic!("{:?}: {}", p, e));
            }
        }
        assert!(missing.is_empty(), "unresolved profiles: {:?}", missing);
        assert!(scan.failures.is_empty());
    }
}
