use crate::library::file_ops::write_file_atomic;
use crate::library::tagging::{COLOR_TAG_PREFIX, USER_TAG_PREFIX};
use crate::metadata::sidecar::{ImageFlag, ImageMetadata};
use crate::metadata::xmp_sidecar::{XmpDocument, XmpUpdate, normalize_color_label};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

const XMP_REJECTED_RATING: i8 = -1;

pub fn xmp_rating(metadata: &ImageMetadata) -> String {
    rating_value(metadata).to_string()
}

fn rating_value(metadata: &ImageMetadata) -> i8 {
    if metadata.flag == Some(ImageFlag::Reject) {
        XMP_REJECTED_RATING
    } else {
        i8::try_from(metadata.rating.min(5)).unwrap_or(5)
    }
}

pub fn xmp_label(tags: &[String]) -> Option<String> {
    tags.iter()
        .rev()
        .find_map(|t| t.strip_prefix(COLOR_TAG_PREFIX))
        .map(|color| {
            let mut c = color.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
}

pub fn xmp_subject_bag<S: AsRef<str>>(items: &[S]) -> String {
    let mut bag = String::from("<dc:subject>\n    <rdf:Bag>\n");
    for item in items {
        bag.push_str(&format!("     <rdf:li>{}</rdf:li>\n", item.as_ref()));
    }
    bag.push_str("    </rdf:Bag>\n   </dc:subject>");
    bag
}

pub fn extract_xmp_tags(content: &str) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(start_idx) = content.find("<dc:subject>")
        && let Some(end_idx) = content[start_idx..].find("</dc:subject>")
    {
        let subject_block = &content[start_idx..start_idx + end_idx];
        let mut current_idx = 0;
        while let Some(li_start) = subject_block[current_idx..].find("<rdf:li>") {
            let val_start = current_idx + li_start + 8;
            if let Some(li_end) = subject_block[val_start..].find("</rdf:li>") {
                tags.push(subject_block[val_start..val_start + li_end].to_string());
                current_idx = val_start + li_end + 9;
            } else {
                break;
            }
        }
    }
    tags
}

/// The rating, color label and keywords as they appear in an XMP sidecar.
///
/// The last state RapidRAW and the sidecar agreed on is stored in the `.rrdata`
/// file. Comparing against it tells which side changed a value since then, so a
/// change made in another application is picked up (including removals) and a
/// change made in RapidRAW is written back without reverting the other side.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct XmpSyncState {
    /// `xmp:Rating`, -1 for rejected.
    pub rating: i8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

impl XmpSyncState {
    pub fn is_empty(&self) -> bool {
        self.rating == 0 && self.label.is_none() && self.keywords.is_empty()
    }

    fn into_snapshot(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }

    fn from_document(doc: &XmpDocument) -> Self {
        Self {
            rating: doc.rating().unwrap_or(0),
            label: doc.color_label(),
            keywords: doc.keywords(),
        }
    }

    /// The state RapidRAW would write for `metadata`.
    pub fn from_metadata(metadata: &ImageMetadata) -> Self {
        let tags = metadata.tags.as_deref().unwrap_or_default();
        let label = tags
            .iter()
            .rev()
            .find_map(|t| t.strip_prefix(COLOR_TAG_PREFIX))
            .and_then(normalize_color_label);
        let mut keywords: Vec<String> = Vec::new();
        for tag in tags.iter().filter(|t| !t.starts_with(COLOR_TAG_PREFIX)) {
            let keyword = tag.strip_prefix(USER_TAG_PREFIX).unwrap_or(tag).trim();
            if !keyword.is_empty() && !keywords.iter().any(|k| k == keyword) {
                keywords.push(keyword.to_string());
            }
        }
        Self {
            rating: rating_value(metadata),
            label,
            keywords,
        }
    }
}

/// The sidecar of `image_path` written by another application, if any.
///
/// darktable and digiKam name it `IMG_0001.CR2.xmp`, Lightroom, Bridge and
/// Capture One `IMG_0001.xmp`. The per-file name wins because it cannot belong to
/// the other half of a RAW+JPEG pair.
pub fn resolve_xmp_path(image_path: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::with_capacity(4);
    if let Some(file_name) = image_path.file_name() {
        for ext in [".xmp", ".XMP"] {
            let mut name = file_name.to_os_string();
            name.push(ext);
            candidates.push(image_path.with_file_name(name));
        }
    }
    candidates.push(image_path.with_extension("xmp"));
    candidates.push(image_path.with_extension("XMP"));
    candidates
        .into_iter()
        .filter(|p| p != image_path)
        .find(|p| p.is_file())
}

fn is_primary(source_path: &Path, sidecar_path: &Path) -> bool {
    sidecar_path == crate::metadata::sidecar::get_primary_sidecar_path(source_path)
}

fn set_color_tag(tags: &mut Vec<String>, label: Option<&str>) {
    tags.retain(|t| !t.starts_with(COLOR_TAG_PREFIX));
    if let Some(label) = label {
        tags.push(format!("{COLOR_TAG_PREFIX}{label}"));
    }
}

fn has_keyword(tags: &[String], keyword: &str) -> bool {
    tags.iter()
        .any(|t| t == keyword || t.strip_prefix(USER_TAG_PREFIX) == Some(keyword))
}

fn add_keyword(tags: &mut Vec<String>, keyword: &str) {
    if !has_keyword(tags, keyword) {
        tags.push(format!("{USER_TAG_PREFIX}{keyword}"));
    }
}

fn apply_rating(metadata: &mut ImageMetadata, rating: i8) {
    if rating == XMP_REJECTED_RATING {
        metadata.flag = Some(ImageFlag::Reject);
    } else {
        metadata.rating = u8::try_from(rating.clamp(0, 5)).unwrap_or(0);
        if metadata.flag == Some(ImageFlag::Reject) {
            metadata.flag = None;
        }
    }
}

/// Merges the values read from a sidecar into `metadata`. Returns whether
/// `metadata` changed and has to be saved.
pub fn apply_xmp_state(metadata: &mut ImageMetadata, xmp: &XmpSyncState) -> bool {
    let before = (metadata.rating, metadata.flag, metadata.tags.clone());
    let previous = metadata.xmp_sync.clone();
    let mut tags = metadata.tags.clone().unwrap_or_default();

    match &previous {
        // Only values that changed in the sidecar since the last sync are taken over.
        Some(last) => {
            if xmp.rating != last.rating {
                apply_rating(metadata, xmp.rating);
            }
            if xmp.label != last.label {
                set_color_tag(&mut tags, xmp.label.as_deref());
            }
            for removed in last.keywords.iter().filter(|k| !xmp.keywords.contains(k)) {
                tags.retain(|t| {
                    t != removed && t.strip_prefix(USER_TAG_PREFIX) != Some(removed.as_str())
                });
            }
            for added in xmp.keywords.iter().filter(|k| !last.keywords.contains(k)) {
                add_keyword(&mut tags, added);
            }
        }
        // First sync: fill in what RapidRAW does not have yet, never drop anything.
        None => {
            if xmp.rating == XMP_REJECTED_RATING {
                if metadata.flag.is_none() {
                    metadata.flag = Some(ImageFlag::Reject);
                }
            } else if metadata.rating == 0 && xmp.rating > 0 {
                apply_rating(metadata, xmp.rating);
            }
            if let Some(label) = &xmp.label {
                set_color_tag(&mut tags, Some(label));
            }
            for keyword in &xmp.keywords {
                add_keyword(&mut tags, keyword);
            }
        }
    }

    if metadata.tags.is_some() || !tags.is_empty() {
        metadata.tags = (!tags.is_empty()).then_some(tags);
    }
    metadata.xmp_sync = xmp.clone().into_snapshot();

    before != (metadata.rating, metadata.flag, metadata.tags.clone())
        || metadata.xmp_sync != previous
}

/// What has to change in a sidecar that currently holds `current` so it shows
/// RapidRAW's `desired` state, given the `last` synced state. Values that only
/// changed in the sidecar are left alone so they can be picked up later.
pub fn plan_xmp_update(
    current: &XmpSyncState,
    raw_label: Option<&str>,
    desired: &XmpSyncState,
    last: Option<&XmpSyncState>,
) -> XmpUpdate {
    let rapidraw_changed = |f: &dyn Fn(&XmpSyncState) -> bool| last.is_none_or(|l| !f(l));

    let rating = (rapidraw_changed(&|l| l.rating == desired.rating)
        && current.rating != desired.rating)
        .then_some(desired.rating);

    let label = match (&desired.label, last) {
        (Some(label), _) if current.label.as_ref() != Some(label) => Some(Some(label.clone())),
        // An unknown label set elsewhere is only removed when RapidRAW removed its own.
        (None, Some(l)) if l.label.is_some() && raw_label.is_some() => Some(None),
        _ => None,
    }
    .filter(|_| rapidraw_changed(&|l| l.label == desired.label));

    let empty = Vec::new();
    let last_keywords = last.map_or(&empty, |l| &l.keywords);
    let mut keywords: Vec<String> = current
        .keywords
        .iter()
        .filter(|k| desired.keywords.contains(k) || !last_keywords.contains(k))
        .cloned()
        .collect();
    for keyword in &desired.keywords {
        if !current.keywords.contains(keyword) && !last_keywords.contains(keyword) {
            keywords.push(keyword.clone());
        }
    }
    let keywords = (keywords != current.keywords).then_some(keywords);

    XmpUpdate {
        rating,
        label,
        keywords,
    }
}

pub fn sync_metadata_from_xmp(
    source_path: &Path,
    sidecar_path: &Path,
    metadata: &mut ImageMetadata,
) -> bool {
    if !is_primary(source_path, sidecar_path) {
        return false;
    }
    let Some(xmp_file) = resolve_xmp_path(source_path) else {
        return false;
    };
    let Ok(content) = fs::read_to_string(&xmp_file) else {
        return false;
    };
    let Some(doc) = XmpDocument::parse(&content) else {
        log::warn!("Ignoring unreadable XMP sidecar {}", xmp_file.display());
        return false;
    };
    apply_xmp_state(metadata, &XmpSyncState::from_document(&doc))
}

const XMP_SKELETON: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RapidRAW">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

static XMP_LOCK: Mutex<()> = Mutex::new(());

/// Writes RapidRAW's rating, color label and keywords into the XMP sidecar,
/// editing only those properties and keeping everything else in the file.
pub fn sync_metadata_to_xmp(
    source_path: &Path,
    sidecar_path: &Path,
    metadata: &ImageMetadata,
    create_if_missing: bool,
) {
    if !is_primary(source_path, sidecar_path) {
        return;
    }
    let desired = XmpSyncState::from_metadata(metadata);

    {
        let _xmp = XMP_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let existing = resolve_xmp_path(source_path);
        let (xmp_file, content) = match &existing {
            Some(path) => match fs::read_to_string(path) {
                Ok(content) => (path.clone(), content),
                Err(e) => {
                    log::warn!("Not syncing unreadable XMP {}: {}", path.display(), e);
                    return;
                }
            },
            None if create_if_missing && !desired.is_empty() => {
                (source_path.with_extension("xmp"), XMP_SKELETON.to_string())
            }
            None => return,
        };

        let Some(doc) = XmpDocument::parse(&content) else {
            log::warn!(
                "Not touching XMP {} that could not be parsed",
                xmp_file.display()
            );
            return;
        };
        let current = XmpSyncState::from_document(&doc);
        let update = plan_xmp_update(
            &current,
            doc.label().as_deref(),
            &desired,
            metadata.xmp_sync.as_ref(),
        );
        let Some(updated) = doc.apply(&update) else {
            log::warn!("Could not update XMP {} safely", xmp_file.display());
            return;
        };
        if (existing.is_none() || updated != content)
            && let Err(e) = write_file_atomic(&xmp_file, updated.as_bytes())
        {
            log::error!("Failed to write XMP {}: {}", xmp_file.display(), e);
            return;
        }
    }

    let snapshot = desired.into_snapshot();
    if metadata.xmp_sync != snapshot {
        let _ = crate::metadata::sidecar::update_sidecar(sidecar_path, source_path, |current| {
            current.xmp_sync = snapshot;
        });
    }
}

/// Writes a sidecar for an image that has a rating, label or keywords but no
/// XMP file yet, used when "Create Missing XMP Files" is turned on after the
/// images were rated.
pub fn backfill_missing_xmp(source_path: &Path, sidecar_path: &Path, metadata: &ImageMetadata) {
    if is_primary(source_path, sidecar_path)
        && !XmpSyncState::from_metadata(metadata).is_empty()
        && resolve_xmp_path(source_path).is_none()
    {
        sync_metadata_to_xmp(source_path, sidecar_path, metadata, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(rating: i8, label: Option<&str>, keywords: &[&str]) -> XmpSyncState {
        XmpSyncState {
            rating,
            label: label.map(str::to_string),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
        }
    }

    fn metadata(rating: u8, tags: &[&str], last: Option<XmpSyncState>) -> ImageMetadata {
        ImageMetadata {
            rating,
            tags: (!tags.is_empty()).then(|| tags.iter().map(|t| t.to_string()).collect()),
            xmp_sync: last,
            ..Default::default()
        }
    }

    #[test]
    fn first_sync_merges_without_dropping() {
        let mut meta = metadata(0, &["user:mine", "dog"], None);
        let xmp = state(4, Some("red"), &["dog", "mine", "Paris"]);
        assert!(apply_xmp_state(&mut meta, &xmp));
        assert_eq!(meta.rating, 4);
        assert_eq!(
            meta.tags.unwrap(),
            vec!["user:mine", "dog", "color:red", "user:Paris"]
        );
        assert_eq!(meta.xmp_sync, Some(xmp));
    }

    #[test]
    fn external_changes_and_removals_are_picked_up() {
        let last = state(3, Some("red"), &["a", "b"]);
        let mut meta = metadata(
            3,
            &["color:red", "user:a", "user:b", "ai"],
            Some(last.clone()),
        );
        // Same count of keywords and a changed label: both were missed before.
        let xmp = state(2, Some("green"), &["a", "c"]);
        assert!(apply_xmp_state(&mut meta, &xmp));
        assert_eq!(meta.rating, 2);
        assert_eq!(
            meta.tags.unwrap(),
            vec!["user:a", "ai", "color:green", "user:c"]
        );

        // Nothing changed in the sidecar: RapidRAW's own edits stay.
        let mut meta = metadata(5, &["user:new"], Some(last.clone()));
        assert!(!apply_xmp_state(&mut meta, &last));
        assert_eq!(meta.rating, 5);
        assert_eq!(meta.tags.unwrap(), vec!["user:new"]);
    }

    #[test]
    fn reject_round_trips() {
        let mut meta = metadata(2, &[], Some(state(2, None, &[])));
        apply_xmp_state(&mut meta, &state(-1, None, &[]));
        assert_eq!(meta.flag, Some(ImageFlag::Reject));
        assert_eq!(XmpSyncState::from_metadata(&meta).rating, -1);
        apply_xmp_state(&mut meta, &state(1, None, &[]));
        assert_eq!((meta.rating, meta.flag), (1, None));
    }

    #[test]
    fn empty_sidecar_state_is_not_stored() {
        let mut meta = metadata(0, &[], None);
        assert!(!apply_xmp_state(&mut meta, &XmpSyncState::default()));
        assert_eq!(meta.xmp_sync, None);
        assert_eq!(meta.tags, None);
    }

    #[test]
    fn keywords_are_written_without_prefix_and_deduplicated() {
        let meta = metadata(
            1,
            &["user:Tom & Jerry", "Tom & Jerry", "color:blue", "cat"],
            None,
        );
        assert_eq!(
            XmpSyncState::from_metadata(&meta),
            state(1, Some("blue"), &["Tom & Jerry", "cat"])
        );
    }

    #[test]
    fn plan_keeps_changes_made_elsewhere() {
        let last = state(3, Some("red"), &["a", "b"]);
        // RapidRAW removed "b" and added "d"; the sidecar meanwhile got "c" and a new rating.
        let desired = state(3, Some("red"), &["a", "d"]);
        let current = state(1, Some("red"), &["a", "b", "c"]);
        let update = plan_xmp_update(&current, Some("Red"), &desired, Some(&last));
        assert_eq!(update.rating, None);
        assert_eq!(update.label, None);
        assert_eq!(
            update.keywords,
            Some(vec!["a".into(), "c".into(), "d".into()])
        );

        // Without a previous sync nothing in the sidecar is removed.
        let update = plan_xmp_update(&current, Some("To Do"), &state(5, None, &["z"]), None);
        assert_eq!(update.rating, Some(5));
        assert_eq!(update.label, None);
        assert_eq!(
            update.keywords,
            Some(vec!["a".into(), "b".into(), "c".into(), "z".into()])
        );
    }

    #[test]
    fn plan_removes_label_only_when_removed_in_rapidraw() {
        let last = state(0, Some("red"), &[]);
        let current = state(0, Some("red"), &[]);
        let update = plan_xmp_update(&current, Some("Red"), &state(0, None, &[]), Some(&last));
        assert_eq!(update.label, Some(None));
        // A label RapidRAW cannot show ("To Do") survives unrelated writes.
        let update = plan_xmp_update(
            &state(0, None, &[]),
            Some("To Do"),
            &state(2, None, &[]),
            Some(&state(0, None, &[])),
        );
        assert_eq!(update.label, None);
        assert_eq!(update.rating, Some(2));
    }

    #[test]
    fn full_round_trip_through_a_lightroom_sidecar() {
        let src = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
   xmp:Rating="2"
   crs:Temperature="5500">
   <dc:subject>
    <rdf:Bag>
     <rdf:li>Paris</rdf:li>
    </rdf:Bag>
   </dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;
        let doc = XmpDocument::parse(src).unwrap();
        let mut meta = ImageMetadata::default();
        apply_xmp_state(&mut meta, &XmpSyncState::from_document(&doc));
        assert_eq!(meta.rating, 2);
        assert_eq!(meta.tags.as_deref().unwrap(), ["user:Paris"]);

        meta.tags = Some(vec!["user:London".into(), "color:yellow".into()]);
        meta.rating = 4;
        let desired = XmpSyncState::from_metadata(&meta);
        let update = plan_xmp_update(
            &XmpSyncState::from_document(&doc),
            doc.label().as_deref(),
            &desired,
            meta.xmp_sync.as_ref(),
        );
        let out = doc.apply(&update).unwrap();
        let written = XmpSyncState::from_document(&XmpDocument::parse(&out).unwrap());
        assert_eq!(written, desired);
        assert!(out.contains("crs:Temperature=\"5500\""));
        assert!(!out.contains("user:"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_is_read_from_attribute_or_element_form() {
        assert_eq!(
            extract_xmp_rating(r#"<rdf:Description xmp:Rating="4" />"#),
            Some(4)
        );
        assert_eq!(extract_xmp_rating("<xmp:Rating>2</xmp:Rating>"), Some(2));
        assert_eq!(
            extract_xmp_rating(r#"<rdf:Description xmp:Rating="-1" />"#),
            Some(-1)
        );
    }

    #[test]
    fn missing_or_malformed_rating_is_none() {
        assert_eq!(extract_xmp_rating("<rdf:Description />"), None);
        assert_eq!(extract_xmp_rating(r#"xmp:Rating="five""#), None);
        assert_eq!(extract_xmp_rating(r#"xmp:Rating="3"#), None);
    }

    #[test]
    fn rejected_images_are_written_with_the_rejected_rating() {
        let mut metadata = ImageMetadata {
            rating: 3,
            ..Default::default()
        };
        assert_eq!(xmp_rating(&metadata), "3");
        metadata.flag = Some(ImageFlag::Reject);
        assert_eq!(xmp_rating(&metadata), "-1");
        metadata.flag = Some(ImageFlag::Pick);
        assert_eq!(xmp_rating(&metadata), "3");
    }

    #[test]
    fn label_is_read_from_attribute_or_element_form() {
        assert_eq!(
            extract_xmp_label(r#"<rdf:Description xmp:Label="Red" />"#).as_deref(),
            Some("Red")
        );
        assert_eq!(
            extract_xmp_label("<xmp:Label>Green</xmp:Label>").as_deref(),
            Some("Green")
        );
        assert_eq!(extract_xmp_label("<rdf:Description />"), None);
    }

    #[test]
    fn label_comes_from_the_last_color_tag_capitalized() {
        let tags = vec![
            "color:red".to_string(),
            "landscape".to_string(),
            "color:blue".to_string(),
        ];
        assert_eq!(xmp_label(&tags).as_deref(), Some("Blue"));
        assert_eq!(xmp_label(&["landscape".to_string()]), None);
        assert_eq!(xmp_label(&["color:".to_string()]).as_deref(), Some(""));
    }

    #[test]
    fn subject_bag_round_trips_through_tag_extraction() {
        let tags = ["landscape", "mountains", "sunset"];
        let bag = xmp_subject_bag(&tags);
        assert_eq!(extract_xmp_tags(&bag), tags);
        assert!(extract_xmp_tags(&xmp_subject_bag::<&str>(&[])).is_empty());
    }

    #[test]
    fn tags_outside_the_subject_block_are_ignored() {
        let content = "<dc:creator><rdf:Seq><rdf:li>Someone</rdf:li></rdf:Seq></dc:creator>\
                       <dc:subject><rdf:Bag><rdf:li>kept</rdf:li></rdf:Bag></dc:subject>";
        assert_eq!(extract_xmp_tags(content), vec!["kept".to_string()]);
        assert!(extract_xmp_tags("<rdf:li>loose</rdf:li>").is_empty());
    }
}
