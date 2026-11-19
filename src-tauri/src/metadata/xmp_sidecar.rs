//! Reading and surgical editing of XMP sidecars written by other applications
//! (Lightroom, Bridge, darktable, digiKam, ...).
//!
//! Only the rating, the color label and the keyword properties are ever touched.
//! Edits are applied as byte range replacements on the original text, so every
//! other byte of the file (develop settings, history stacks, unknown namespaces,
//! formatting) is kept exactly as it was. A file that cannot be parsed is never
//! modified, and the edited result is parsed again before it is handed back.

use quick_xml::Reader;
use quick_xml::escape::{escape, unescape};
use quick_xml::events::{BytesStart, Event};
use regex::regex;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;

pub const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
pub const DC_NS: &str = "http://purl.org/dc/elements/1.1/";

/// The color labels RapidRAW knows, in darktable's `darktable:colorlabels` order.
pub const COLOR_LABELS: [&str; 5] = ["red", "yellow", "green", "blue", "purple"];

const RATING: &str = "xmp:Rating";
const LABEL: &str = "xmp:Label";
const SUBJECT: &str = "dc:subject";
const HIERARCHICAL_SUBJECT: &str = "lr:hierarchicalSubject";
const DARKTABLE_COLOR_LABELS: &str = "darktable:colorlabels";
const MICROSOFT_RATING: &str = "MicrosoftPhoto:Rating";

#[derive(Debug)]
struct Attribute {
    name: String,
    /// The whole attribute including the whitespace in front of it.
    span: Range<usize>,
    /// The attribute value between the quotes, still escaped.
    value: Range<usize>,
}

#[derive(Debug)]
struct Property {
    name: String,
    span: Range<usize>,
    /// The content between the start and the end tag, `None` for `<x/>`.
    inner: Option<Range<usize>>,
    has_container: bool,
    /// The content of each `rdf:li` of an `rdf:Bag`, `rdf:Seq` or `rdf:Alt`.
    items: Vec<Range<usize>>,
}

#[derive(Debug)]
struct Description {
    start_tag: Range<usize>,
    self_closing: bool,
    end_tag: Option<usize>,
    attributes: Vec<Attribute>,
    properties: Vec<Property>,
}

enum Frame {
    Rdf,
    Description(usize),
    Property(usize, usize),
    Container(usize, usize),
    Item(usize, usize, usize),
    Other,
}

/// A parsed XMP packet that remembers where each top level property lives.
pub struct XmpDocument<'a> {
    src: &'a str,
    descriptions: Vec<Description>,
}

/// The changes to apply to a sidecar. `None` leaves a property as it is.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct XmpUpdate {
    pub rating: Option<i8>,
    /// `Some(None)` removes the label.
    pub label: Option<Option<String>>,
    /// The complete new keyword list.
    pub keywords: Option<Vec<String>>,
}

fn element_name(e: &BytesStart) -> Option<String> {
    std::str::from_utf8(e.name().as_ref())
        .ok()
        .map(str::to_string)
}

fn parse_attributes(src: &str, tag: &Range<usize>) -> Vec<Attribute> {
    let re = regex!(r#"(\s+)([A-Za-z_][\w.\-]*:[\w.\-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#);
    let slice = &src[tag.clone()];
    re.captures_iter(slice)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            let value = caps.get(3).or_else(|| caps.get(4))?;
            Some(Attribute {
                name: caps.get(2)?.as_str().to_string(),
                span: tag.start + whole.start()..tag.start + whole.end(),
                value: tag.start + value.start()..tag.start + value.end(),
            })
        })
        .collect()
}

fn unescape_text(raw: &str) -> String {
    unescape(raw).map_or_else(|_| raw.to_string(), Cow::into_owned)
}

/// The whitespace between the previous line break and `pos`.
fn indent_before(src: &str, pos: usize) -> &str {
    let line_start = src[..pos].rfind('\n').map_or(0, |i| i + 1);
    let indent = &src[line_start..pos];
    if indent.chars().all(|c| c == ' ' || c == '\t') {
        indent
    } else {
        ""
    }
}

/// Moves `pos` back over any whitespace in front of it.
fn skip_whitespace_back(src: &str, pos: usize) -> usize {
    src[..pos].trim_end().len()
}

fn render_array(name: &str, container: &str, items: &[String], indent: &str) -> String {
    if items.is_empty() {
        return format!("<{name}>\n{indent} <{container}/>\n{indent}</{name}>");
    }
    let mut out = format!("<{name}>\n{indent} <{container}>\n");
    for item in items {
        out.push_str(&format!(
            "{indent}  <rdf:li>{}</rdf:li>\n",
            escape(item.as_str())
        ));
    }
    out.push_str(&format!("{indent} </{container}>\n{indent}</{name}>"));
    out
}

fn parse_rating(value: &str) -> Option<i8> {
    let value = value.trim();
    let rating = value
        .parse::<i32>()
        .ok()
        .or_else(|| value.parse::<f64>().ok().map(|f| f.round() as i32))?;
    Some(rating.clamp(-1, 5) as i8)
}

fn microsoft_rating(rating: i8) -> Option<&'static str> {
    match rating {
        0 => Some("0"),
        1 => Some("1"),
        2 => Some("25"),
        3 => Some("50"),
        4 => Some("75"),
        5 => Some("99"),
        _ => None,
    }
}

/// Maps an `xmp:Label` value to one of [`COLOR_LABELS`].
pub fn normalize_color_label(label: &str) -> Option<String> {
    let label = label.trim().to_lowercase();
    COLOR_LABELS.contains(&label.as_str()).then_some(label)
}

impl<'a> XmpDocument<'a> {
    pub fn parse(src: &'a str) -> Option<Self> {
        let body = src.strip_prefix('\u{feff}').unwrap_or(src);
        let base = src.len() - body.len();
        let mut reader = Reader::from_str(body);
        let mut stack: Vec<Frame> = Vec::new();
        let mut descriptions: Vec<Description> = Vec::new();
        let mut saw_rdf = false;

        loop {
            let before = base + usize::try_from(reader.buffer_position()).ok()?;
            let event = reader.read_event().ok()?;
            let after = base + usize::try_from(reader.buffer_position()).ok()?;
            match event {
                Event::Eof => break,
                Event::Start(e) => {
                    let name = element_name(&e)?;
                    let frame = Self::open(
                        src,
                        &name,
                        before..after,
                        false,
                        stack.last(),
                        &mut descriptions,
                    );
                    saw_rdf |= matches!(frame, Frame::Rdf);
                    stack.push(frame);
                }
                Event::Empty(e) => {
                    let name = element_name(&e)?;
                    let frame = Self::open(
                        src,
                        &name,
                        before..after,
                        true,
                        stack.last(),
                        &mut descriptions,
                    );
                    saw_rdf |= matches!(frame, Frame::Rdf);
                    match frame {
                        Frame::Property(d, p) => {
                            descriptions[d].properties[p].span = before..after;
                        }
                        Frame::Item(d, p, _) => {
                            descriptions[d].properties[p].items.push(after..after)
                        }
                        _ => {}
                    }
                }
                Event::End(_) => match stack.pop()? {
                    Frame::Description(d) => descriptions[d].end_tag = Some(before),
                    Frame::Property(d, p) => {
                        let property = &mut descriptions[d].properties[p];
                        property.inner = Some(property.span.end..before);
                        property.span.end = after;
                    }
                    Frame::Item(d, p, start) => {
                        descriptions[d].properties[p].items.push(start..before);
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        if !stack.is_empty() || !saw_rdf {
            return None;
        }
        Some(Self { src, descriptions })
    }

    fn open(
        src: &str,
        name: &str,
        tag: Range<usize>,
        empty: bool,
        parent: Option<&Frame>,
        descriptions: &mut Vec<Description>,
    ) -> Frame {
        match (parent, name) {
            (_, "rdf:RDF") => Frame::Rdf,
            (Some(Frame::Rdf), "rdf:Description") => {
                descriptions.push(Description {
                    attributes: parse_attributes(src, &tag),
                    start_tag: tag,
                    self_closing: empty,
                    end_tag: None,
                    properties: Vec::new(),
                });
                Frame::Description(descriptions.len() - 1)
            }
            (Some(Frame::Description(d)), _) => {
                let properties = &mut descriptions[*d].properties;
                properties.push(Property {
                    name: name.to_string(),
                    span: tag,
                    inner: None,
                    has_container: false,
                    items: Vec::new(),
                });
                Frame::Property(*d, properties.len() - 1)
            }
            (Some(Frame::Property(d, p)), "rdf:Bag" | "rdf:Seq" | "rdf:Alt") => {
                descriptions[*d].properties[*p].has_container = true;
                Frame::Container(*d, *p)
            }
            (Some(Frame::Container(d, p)), "rdf:li") => Frame::Item(*d, *p, tag.end),
            _ => Frame::Other,
        }
    }

    fn attributes(&self, name: &'a str) -> impl Iterator<Item = &Attribute> + '_ {
        self.descriptions
            .iter()
            .flat_map(|d| d.attributes.iter())
            .filter(move |a| a.name == name)
    }

    fn properties(&self, name: &'a str) -> impl Iterator<Item = &Property> + '_ {
        self.descriptions
            .iter()
            .flat_map(|d| d.properties.iter())
            .filter(move |p| p.name == name)
    }

    /// The text of a simple property, written either as an attribute or as an element.
    fn simple_value(&self, name: &'a str) -> Option<String> {
        if let Some(attr) = self.attributes(name).next() {
            return Some(unescape_text(&self.src[attr.value.clone()]));
        }
        self.properties(name)
            .filter(|p| !p.has_container)
            .find_map(|p| {
                let inner = &self.src[p.inner.clone()?];
                (!inner.contains('<')).then(|| unescape_text(inner.trim()))
            })
    }

    fn array_items(&self, name: &'a str) -> Option<Vec<String>> {
        let property = self.properties(name).next()?;
        Some(
            property
                .items
                .iter()
                .map(|range| &self.src[range.clone()])
                .filter(|raw| !raw.contains('<'))
                .map(unescape_text)
                .filter(|item| !item.trim().is_empty())
                .collect(),
        )
    }

    pub fn rating(&self) -> Option<i8> {
        self.simple_value(RATING).as_deref().and_then(parse_rating)
    }

    /// The raw `xmp:Label` value.
    pub fn label(&self) -> Option<String> {
        self.simple_value(LABEL)
    }

    /// The color label as one of [`COLOR_LABELS`]. darktable keeps its labels in
    /// `darktable:colorlabels`, which wins over a possibly stale `xmp:Label`.
    pub fn color_label(&self) -> Option<String> {
        if let Some(items) = self.array_items(DARKTABLE_COLOR_LABELS) {
            return items
                .iter()
                .filter_map(|i| i.trim().parse::<usize>().ok())
                .find_map(|i| COLOR_LABELS.get(i))
                .map(|c| c.to_string());
        }
        self.label().as_deref().and_then(normalize_color_label)
    }

    pub fn keywords(&self) -> Vec<String> {
        let mut keywords: Vec<String> = Vec::new();
        for keyword in self.array_items(SUBJECT).unwrap_or_default() {
            if !keywords.contains(&keyword) {
                keywords.push(keyword);
            }
        }
        keywords
    }

    fn declares(&self, prefix: &str) -> Option<usize> {
        let decl = format!("xmlns:{prefix}");
        self.descriptions
            .iter()
            .position(|d| d.attributes.iter().any(|a| a.name == decl))
    }

    /// The description new properties of a namespace go into, and whether the
    /// namespace still has to be declared on it.
    fn target(&self, prefix: &str) -> Option<(usize, bool)> {
        if let Some(d) = self.declares(prefix) {
            return Some((d, false));
        }
        if self.descriptions.is_empty() {
            return None;
        }
        // Declared on rdf:RDF or x:xmpmeta, which are ancestors of every description.
        let decl = format!("xmlns:{prefix}=");
        let first_description = self.descriptions[0].start_tag.start;
        Some((0, !self.src[..first_description].contains(&decl)))
    }

    /// Applies `update` and returns the new text, or `None` when the document
    /// could not be edited safely. Unchanged properties keep their exact bytes.
    pub fn apply(&self, update: &XmpUpdate) -> Option<String> {
        let mut edits = Edits::default();

        if let Some(rating) = update.rating {
            let value = rating.to_string();
            self.set_simple(&mut edits, RATING, "xmp", XMP_NS, Some(&value), true)?;
            if let Some(ms) = microsoft_rating(rating) {
                self.set_simple(
                    &mut edits,
                    MICROSOFT_RATING,
                    "MicrosoftPhoto",
                    "",
                    Some(ms),
                    false,
                )?;
            }
        }

        if let Some(label) = &update.label {
            let darktable = self.properties(DARKTABLE_COLOR_LABELS).next();
            if let Some(property) = darktable {
                let items: Vec<String> = label
                    .as_deref()
                    .and_then(|l| COLOR_LABELS.iter().position(|c| c.eq_ignore_ascii_case(l)))
                    .map(|i| vec![i.to_string()])
                    .unwrap_or_default();
                let indent = indent_before(self.src, property.span.start);
                edits.replace(
                    property.span.clone(),
                    render_array(DARKTABLE_COLOR_LABELS, "rdf:Seq", &items, indent),
                );
            }
            let display = label.as_deref().map(capitalize);
            self.set_simple(
                &mut edits,
                LABEL,
                "xmp",
                XMP_NS,
                display.as_deref(),
                darktable.is_none(),
            )?;
        }

        if let Some(keywords) = &update.keywords {
            let current = self.keywords();
            if &current != keywords {
                self.set_array(&mut edits, SUBJECT, "dc", DC_NS, keywords)?;
                let removed: Vec<&String> =
                    current.iter().filter(|k| !keywords.contains(k)).collect();
                self.remove_hierarchical_leaves(&mut edits, &removed);
            }
        }

        let result = edits.apply(self.src)?;
        XmpDocument::parse(&result)?;
        Some(result)
    }

    fn set_simple(
        &self,
        edits: &mut Edits,
        name: &'a str,
        prefix: &str,
        ns: &str,
        value: Option<&str>,
        insert: bool,
    ) -> Option<()> {
        let mut found = false;
        for attr in self.attributes(name) {
            found = true;
            match value {
                Some(v) => {
                    if unescape_text(&self.src[attr.value.clone()]) != v {
                        edits.replace(attr.value.clone(), escape(v).into_owned());
                    }
                }
                None => edits.replace(attr.span.clone(), String::new()),
            }
        }
        for property in self.properties(name).filter(|p| !p.has_container) {
            found = true;
            match (value, &property.inner) {
                (Some(v), Some(inner)) => {
                    let raw = &self.src[inner.clone()];
                    if raw.contains('<') {
                        return None;
                    }
                    if unescape_text(raw.trim()) != v {
                        edits.replace(inner.clone(), escape(v).into_owned());
                    }
                }
                (Some(v), None) => edits.replace(
                    property.span.clone(),
                    format!("<{name}>{}</{name}>", escape(v)),
                ),
                (None, _) => {
                    let start = skip_whitespace_back(self.src, property.span.start);
                    edits.replace(start..property.span.end, String::new());
                }
            }
        }
        if found || !insert {
            return Some(());
        }
        let Some(v) = value else {
            return Some(());
        };
        let (d, needs_decl) = self.target(prefix)?;
        if needs_decl {
            edits.add_attribute(d, format!("xmlns:{prefix}"), ns.to_string());
        }
        edits.add_attribute(d, name.to_string(), v.to_string());
        Some(())
    }

    fn set_array(
        &self,
        edits: &mut Edits,
        name: &'a str,
        prefix: &str,
        ns: &str,
        items: &[String],
    ) -> Option<()> {
        let mut existing = self.properties(name);
        if let Some(property) = existing.next() {
            if items.is_empty() {
                let start = skip_whitespace_back(self.src, property.span.start);
                edits.replace(start..property.span.end, String::new());
            } else {
                let indent = indent_before(self.src, property.span.start);
                edits.replace(
                    property.span.clone(),
                    render_array(name, "rdf:Bag", items, indent),
                );
            }
            // A second copy of the same property would be invalid RDF.
            for duplicate in existing {
                let start = skip_whitespace_back(self.src, duplicate.span.start);
                edits.replace(start..duplicate.span.end, String::new());
            }
            return Some(());
        }
        if items.is_empty() {
            return Some(());
        }

        let (d, needs_decl) = self.target(prefix)?;
        if needs_decl {
            edits.add_attribute(d, format!("xmlns:{prefix}"), ns.to_string());
        }
        let description = &self.descriptions[d];
        let outer_indent = indent_before(self.src, description.start_tag.start).to_string();
        let child_indent = description
            .properties
            .first()
            .map(|p| indent_before(self.src, p.span.start).to_string())
            .filter(|i| !i.is_empty())
            .unwrap_or_else(|| format!("{outer_indent} "));
        let element = render_array(name, "rdf:Bag", items, &child_indent);

        match description.end_tag {
            Some(end_tag) => {
                let at = skip_whitespace_back(self.src, end_tag);
                edits.replace(at..at, format!("\n{child_indent}{element}"));
            }
            None => {
                // `<rdf:Description .../>` has to become an element with children.
                let close = description.start_tag.end - 2;
                if !description.self_closing || &self.src[close..description.start_tag.end] != "/>"
                {
                    return None;
                }
                edits.replace(
                    close..description.start_tag.end,
                    format!(">\n{child_indent}{element}\n{outer_indent}</rdf:Description>"),
                );
            }
        }
        Some(())
    }

    /// Lightroom rebuilds `dc:subject` from `lr:hierarchicalSubject`, so a removed
    /// keyword also has to go from there or it comes back.
    fn remove_hierarchical_leaves(&self, edits: &mut Edits, removed: &[&String]) {
        if removed.is_empty() {
            return;
        }
        let Some(property) = self.properties(HIERARCHICAL_SUBJECT).next() else {
            return;
        };
        let Some(items) = self.array_items(HIERARCHICAL_SUBJECT) else {
            return;
        };
        let kept: Vec<String> = items
            .iter()
            .filter(|item| {
                let leaf = item.rsplit('|').next().unwrap_or(item);
                !removed.iter().any(|r| r.as_str() == leaf)
            })
            .cloned()
            .collect();
        if kept.len() == items.len() {
            return;
        }
        if kept.is_empty() {
            let start = skip_whitespace_back(self.src, property.span.start);
            edits.replace(start..property.span.end, String::new());
        } else {
            let indent = indent_before(self.src, property.span.start);
            edits.replace(
                property.span.clone(),
                render_array(HIERARCHICAL_SUBJECT, "rdf:Bag", &kept, indent),
            );
        }
    }

    fn attribute_insert_point(&self, d: usize) -> (usize, String) {
        let description = &self.descriptions[d];
        match description.attributes.last() {
            Some(last) => {
                let leading = &self.src[last.span.clone()];
                let ws_len = leading.len() - leading.trim_start().len();
                (last.span.end, leading[..ws_len].to_string())
            }
            None => (
                description.start_tag.start + "<rdf:Description".len(),
                " ".to_string(),
            ),
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[derive(Default)]
struct Edits {
    replacements: Vec<(Range<usize>, String)>,
    attributes: BTreeMap<usize, Vec<(String, String)>>,
}

impl Edits {
    fn replace(&mut self, range: Range<usize>, text: String) {
        self.replacements.push((range, text));
    }

    fn add_attribute(&mut self, description: usize, name: String, value: String) {
        let list = self.attributes.entry(description).or_default();
        if !list.iter().any(|(n, _)| n == &name) {
            list.push((name, value));
        }
    }

    fn apply(mut self, src: &str) -> Option<String> {
        if !self.attributes.is_empty() {
            let doc = XmpDocument::parse(src)?;
            for (d, attributes) in std::mem::take(&mut self.attributes) {
                let (at, ws) = doc.attribute_insert_point(d);
                let text: String = attributes
                    .iter()
                    .map(|(name, value)| format!("{ws}{name}=\"{}\"", escape(value.as_str())))
                    .collect();
                self.replacements.push((at..at, text));
            }
        }

        // Apply from the end so earlier offsets stay valid. For the same start,
        // the longer replacement goes first so an insertion lands in front of it.
        self.replacements
            .sort_by(|a, b| b.0.start.cmp(&a.0.start).then(b.0.end.cmp(&a.0.end)));
        let mut out = src.to_string();
        let mut limit = usize::MAX;
        for (range, text) in self.replacements {
            if range.end > limit || range.start > range.end {
                return None;
            }
            limit = range.start;
            out.replace_range(range, &text);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIGHTROOM: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00        ">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:tiff="http://ns.adobe.com/tiff/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
   xmp:Rating="3"
   xmp:Label="Red"
   tiff:Make="Canon"
   crs:Exposure2012="+0.35"
   crs:HasSettings="True">
   <crs:Look>
    <rdf:Description
     crs:Name="Adobe Color"
     crs:Amount="1"/>
   </crs:Look>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>Paris</rdf:li>
     <rdf:li>Tom &amp; Jerry</rdf:li>
    </rdf:Bag>
   </dc:subject>
   <lr:hierarchicalSubject>
    <rdf:Bag>
     <rdf:li>Places|Paris</rdf:li>
     <rdf:li>Tom &amp; Jerry</rdf:li>
    </rdf:Bag>
   </lr:hierarchicalSubject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

    const DARKTABLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
   darktable:history_end="1">
   <xmp:Rating>2</xmp:Rating>
   <darktable:colorlabels>
    <rdf:Seq>
     <rdf:li>2</rdf:li>
    </rdf:Seq>
   </darktable:colorlabels>
   <darktable:history>
    <rdf:Seq>
     <rdf:li
      darktable:operation="exposure"
      darktable:enabled="1"
      darktable:params="0000000000000000"/>
    </rdf:Seq>
   </darktable:history>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

    const DIGIKAM: &str = "\u{feff}<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"XMP Core 4.4.0-Exiv2\">
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">
  <rdf:Description rdf:about=\"\"
    xmlns:MicrosoftPhoto=\"http://ns.microsoft.com/photo/1.0/\"
    xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"
    xmlns:digiKam=\"http://www.digikam.org/ns/1.0/\"
   MicrosoftPhoto:Rating=\"25\"
   xmp:Rating=\"2\"
   digiKam:ColorLabel=\"0\">
   <exif:Flash xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"
    exif:Fired=\"False\"/>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end=\"w\"?>";

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn apply(src: &str, update: XmpUpdate) -> String {
        XmpDocument::parse(src).unwrap().apply(&update).unwrap()
    }

    #[test]
    fn reads_lightroom_attribute_form() {
        let doc = XmpDocument::parse(LIGHTROOM).unwrap();
        assert_eq!(doc.rating(), Some(3));
        assert_eq!(doc.color_label().as_deref(), Some("red"));
        assert_eq!(doc.keywords(), strings(&["Paris", "Tom & Jerry"]));
    }

    #[test]
    fn reads_darktable_element_form() {
        let doc = XmpDocument::parse(DARKTABLE).unwrap();
        assert_eq!(doc.rating(), Some(2));
        assert_eq!(doc.color_label().as_deref(), Some("green"));
        assert!(doc.keywords().is_empty());
    }

    #[test]
    fn reads_digikam_with_bom() {
        let doc = XmpDocument::parse(DIGIKAM).unwrap();
        assert_eq!(doc.rating(), Some(2));
        assert_eq!(doc.color_label(), None);
    }

    #[test]
    fn empty_update_keeps_every_byte() {
        for src in [LIGHTROOM, DARKTABLE, DIGIKAM] {
            assert_eq!(apply(src, XmpUpdate::default()), src);
            let doc = XmpDocument::parse(src).unwrap();
            let same = XmpUpdate {
                rating: doc.rating(),
                keywords: Some(doc.keywords()),
                ..Default::default()
            };
            assert_eq!(apply(src, same), src);
        }
    }

    #[test]
    fn lightroom_round_trip_only_touches_synced_properties() {
        let out = apply(
            LIGHTROOM,
            XmpUpdate {
                rating: Some(5),
                label: Some(Some("blue".into())),
                keywords: Some(strings(&["Paris", "A<B"])),
            },
        );
        let doc = XmpDocument::parse(&out).unwrap();
        assert_eq!(doc.rating(), Some(5));
        assert_eq!(doc.label().as_deref(), Some("Blue"));
        assert_eq!(doc.keywords(), strings(&["Paris", "A<B"]));
        assert!(out.contains("<rdf:li>A&lt;B</rdf:li>"));
        // The removed keyword is gone from the hierarchy too, the rest is untouched.
        assert!(out.contains("Places|Paris"));
        assert!(!out.contains("Jerry"));
        assert_eq!(out.matches("xmp:Rating").count(), 1);
        assert_eq!(out.matches("xmp:Label").count(), 1);
        for kept in [
            "crs:Exposure2012=\"+0.35\"",
            "   <crs:Look>\n    <rdf:Description\n     crs:Name=\"Adobe Color\"\n     crs:Amount=\"1\"/>\n   </crs:Look>\n",
            "x:xmptk=\"Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00        \"",
        ] {
            assert!(out.contains(kept), "lost {kept}");
        }
        let prefix_len = LIGHTROOM.find("   xmp:Rating").unwrap();
        assert_eq!(out[..prefix_len], LIGHTROOM[..prefix_len]);
    }

    #[test]
    fn removing_label_and_keywords() {
        let out = apply(
            LIGHTROOM,
            XmpUpdate {
                label: Some(None),
                keywords: Some(Vec::new()),
                ..Default::default()
            },
        );
        assert!(!out.contains("xmp:Label"));
        assert!(!out.contains("dc:subject>"));
        assert!(!out.contains("Tom &amp; Jerry"));
        // Every leaf was removed, so the whole hierarchy goes too.
        assert!(!out.contains("lr:hierarchicalSubject>"));
        assert!(out.contains("xmlns:lr=\"http://ns.adobe.com/lightroom/1.0/\""));
        assert!(out.contains("crs:HasSettings=\"True\">\n   <crs:Look>"));
        XmpDocument::parse(&out).unwrap();
    }

    #[test]
    fn darktable_round_trip_keeps_history() {
        let out = apply(
            DARKTABLE,
            XmpUpdate {
                rating: Some(4),
                label: Some(Some("purple".into())),
                keywords: Some(strings(&["holiday & sun"])),
            },
        );
        let doc = XmpDocument::parse(&out).unwrap();
        assert_eq!(doc.rating(), Some(4));
        assert_eq!(doc.color_label().as_deref(), Some("purple"));
        assert_eq!(doc.keywords(), strings(&["holiday & sun"]));
        assert!(out.contains("<xmp:Rating>4</xmp:Rating>"));
        // darktable keeps its labels in its own property, no xmp:Label is added.
        assert!(!out.contains("xmp:Label"));
        let history_start = DARKTABLE.find("   <darktable:history>").unwrap();
        let history_end = DARKTABLE.find("</darktable:history>").unwrap();
        assert!(out.contains(&DARKTABLE[history_start..history_end]));
        assert!(out.contains("<rdf:li>holiday &amp; sun</rdf:li>"));
        assert_eq!(out.matches("<dc:subject>").count(), 1);
    }

    #[test]
    fn digikam_rating_updates_microsoft_rating() {
        let out = apply(
            DIGIKAM,
            XmpUpdate {
                rating: Some(4),
                label: Some(Some("yellow".into())),
                keywords: Some(strings(&["cat"])),
            },
        );
        assert!(out.starts_with('\u{feff}'));
        assert!(out.contains("MicrosoftPhoto:Rating=\"75\""));
        assert!(out.contains("xmp:Rating=\"4\""));
        assert!(out.contains("   digiKam:ColorLabel=\"0\"\n   xmp:Label=\"Yellow\""));
        // The dc namespace was not declared yet.
        assert!(out.contains("xmlns:dc=\"http://purl.org/dc/elements/1.1/\""));
        assert!(out.contains(
            "<exif:Flash xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n    exif:Fired=\"False\"/>"
        ));
        assert!(out.ends_with("<?xpacket end=\"w\"?>"));
        let doc = XmpDocument::parse(&out).unwrap();
        assert_eq!(doc.keywords(), strings(&["cat"]));
    }

    #[test]
    fn single_quoted_attributes_are_updated_not_duplicated() {
        let src = "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='1'/></rdf:RDF></x:xmpmeta>";
        let out = apply(
            src,
            XmpUpdate {
                rating: Some(3),
                keywords: Some(strings(&["a"])),
                ..Default::default()
            },
        );
        assert!(out.contains("xmp:Rating='3'"));
        assert_eq!(out.matches("Rating").count(), 1);
        let doc = XmpDocument::parse(&out).unwrap();
        assert_eq!(doc.keywords(), strings(&["a"]));
        assert_eq!(doc.rating(), Some(3));
    }

    #[test]
    fn malformed_files_are_left_alone() {
        assert!(XmpDocument::parse("<x:xmpmeta><rdf:RDF><rdf:Description></rdf:RDF>").is_none());
        assert!(XmpDocument::parse("not xml at all").is_none());
    }
}
