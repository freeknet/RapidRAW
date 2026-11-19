//! Builds small uncompressed DNG files for tests.

pub const CFA: u16 = 32803;
pub const LINEAR_RAW: u16 = 34892;

pub const OPCODE_LIST1: u16 = 51008;
pub const OPCODE_LIST2: u16 = 51009;
pub const OPCODE_LIST3: u16 = 51022;

const BYTE: u16 = 1;
const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const UNDEFINED: u16 = 7;

pub struct Tag {
    id: u16,
    kind: u16,
    count: u32,
    data: Vec<u8>,
}

pub fn shorts(id: u16, values: &[u16]) -> Tag {
    Tag {
        id,
        kind: SHORT,
        count: values.len() as u32,
        data: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
    }
}

pub fn longs(id: u16, values: &[u32]) -> Tag {
    Tag {
        id,
        kind: LONG,
        count: values.len() as u32,
        data: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
    }
}

pub fn bytes(id: u16, values: &[u8]) -> Tag {
    Tag {
        id,
        kind: BYTE,
        count: values.len() as u32,
        data: values.to_vec(),
    }
}

pub fn undefined(id: u16, values: &[u8]) -> Tag {
    Tag {
        id,
        kind: UNDEFINED,
        count: values.len() as u32,
        data: values.to_vec(),
    }
}

fn ascii(id: u16, value: &str) -> Tag {
    let mut data = value.as_bytes().to_vec();
    data.push(0);
    Tag {
        id,
        kind: ASCII,
        count: data.len() as u32,
        data,
    }
}

/// A little endian DNG with one uncompressed 16 bit strip of `pixels`. Tags in
/// `extra` replace the default ones with the same id.
pub fn dng(
    width: u32,
    height: u32,
    cpp: u16,
    photometric: u16,
    pixels: &[u16],
    extra: Vec<Tag>,
) -> Vec<u8> {
    let pixel_bytes: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut tags = vec![
        longs(254, &[0]),
        longs(256, &[width]),
        longs(257, &[height]),
        shorts(258, &vec![16; cpp as usize]),
        shorts(259, &[1]),
        shorts(262, &[photometric]),
        ascii(271, "Test"),
        ascii(272, "Synthetic"),
        longs(273, &[0]),
        shorts(274, &[1]),
        shorts(277, &[cpp]),
        longs(278, &[height]),
        longs(279, &[pixel_bytes.len() as u32]),
        bytes(50706, &[1, 4, 0, 0]),
    ];
    tags.retain(|tag| extra.iter().all(|e| e.id != tag.id));
    tags.extend(extra);
    tags.sort_by_key(|tag| tag.id);

    let ifd_size = 2 + tags.len() * 12 + 4;
    let mut data_offset = 8 + ifd_size;
    let mut entries = Vec::new();
    let mut data = Vec::new();
    let mut strip_entry = None;
    for tag in &tags {
        let mut entry = Vec::new();
        entry.extend_from_slice(&tag.id.to_le_bytes());
        entry.extend_from_slice(&tag.kind.to_le_bytes());
        entry.extend_from_slice(&tag.count.to_le_bytes());
        if tag.id == 273 {
            strip_entry = Some(entries.len() + entry.len());
        }
        if tag.data.len() <= 4 {
            let mut value = tag.data.clone();
            value.resize(4, 0);
            entry.extend_from_slice(&value);
        } else {
            entry.extend_from_slice(&(data_offset as u32).to_le_bytes());
            data.extend_from_slice(&tag.data);
            if data.len() % 2 == 1 {
                data.push(0);
            }
            data_offset = 8 + ifd_size + data.len();
        }
        entries.extend_from_slice(&entry);
    }
    let strip_offset = (8 + ifd_size + data.len()) as u32;
    let strip_entry = strip_entry.expect("StripOffsets tag");
    entries[strip_entry..strip_entry + 4].copy_from_slice(&strip_offset.to_le_bytes());

    let mut file = b"II*\0".to_vec();
    file.extend_from_slice(&8u32.to_le_bytes());
    file.extend_from_slice(&(tags.len() as u16).to_le_bytes());
    file.extend_from_slice(&entries);
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&data);
    file.extend_from_slice(&pixel_bytes);
    file
}
