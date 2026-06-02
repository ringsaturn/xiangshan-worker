// XSCI compact index format parser.
//
// Binary layout (all integers little-endian):
//   Header  24 bytes
//   Division records  29 bytes × div_count
//   Coarse grid cells  variable
//   Fine grid cells    variable
//   Country preindex   8 bytes × preindex_count
use std::collections::HashMap;

const MAGIC: &[u8; 4] = b"XSCI";
const VERSION: u8 = 1;

pub struct XsIndex {
    pub subtypes: Vec<u8>,
    pub bboxes: Vec<[f32; 4]>, // [xmin, xmax, ymin, ymax]
    pub poly_offsets: Vec<u64>,
    pub poly_lengths: Vec<u32>,
    pub grid_coarse: HashMap<[i16; 2], Vec<u32>>,
    pub grid_fine: HashMap<[i16; 2], Vec<u32>>,
    pub country_preindex: HashMap<[i16; 2], u32>,
}

pub fn parse(data: &[u8]) -> Result<XsIndex, String> {
    if data.len() < 24 {
        return Err("XSCI: file too short".into());
    }
    if &data[0..4] != MAGIC {
        return Err(format!("XSCI: bad magic {:?}", &data[0..4]));
    }
    if data[4] != VERSION {
        return Err(format!("XSCI: unsupported version {}", data[4]));
    }

    let div_count = u32_le(&data[8..]) as usize;
    let coarse_count = u32_le(&data[12..]) as usize;
    let fine_count = u32_le(&data[16..]) as usize;
    let preindex_count = u32_le(&data[20..]) as usize;
    let mut pos = 24;

    // Each division record: 1 (subtype) + 16 (bbox) + 8 (offset) + 4 (length) = 29 bytes
    const RECORD_LEN: usize = 29;
    let records_end = pos + div_count * RECORD_LEN;
    if records_end > data.len() {
        return Err("XSCI: truncated division section".into());
    }

    let mut subtypes = Vec::with_capacity(div_count);
    let mut bboxes = Vec::with_capacity(div_count);
    let mut poly_offsets = Vec::with_capacity(div_count);
    let mut poly_lengths = Vec::with_capacity(div_count);

    for _ in 0..div_count {
        subtypes.push(data[pos]);
        bboxes.push([
            f32_le(&data[pos + 1..]),
            f32_le(&data[pos + 5..]),
            f32_le(&data[pos + 9..]),
            f32_le(&data[pos + 13..]),
        ]);
        poly_offsets.push(u64_le(&data[pos + 17..]));
        poly_lengths.push(u32_le(&data[pos + 25..]));
        pos += RECORD_LEN;
    }

    let (grid_coarse, consumed) = parse_grid(&data[pos..], coarse_count)?;
    pos += consumed;
    let (grid_fine, consumed) = parse_grid(&data[pos..], fine_count)?;
    pos += consumed;
    let country_preindex = parse_preindex(&data[pos..], preindex_count)?;

    Ok(XsIndex {
        subtypes,
        bboxes,
        poly_offsets,
        poly_lengths,
        grid_coarse,
        grid_fine,
        country_preindex,
    })
}

fn parse_grid(
    data: &[u8],
    count: usize,
) -> Result<(HashMap<[i16; 2], Vec<u32>>, usize), String> {
    let mut map = HashMap::with_capacity(count);
    let mut pos = 0;

    for _ in 0..count {
        if pos + 6 > data.len() {
            return Err("XSCI: truncated grid cell header".into());
        }
        let lng = i16::from_le_bytes([data[pos], data[pos + 1]]);
        let lat = i16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        let n = u16_le(&data[pos + 4..]) as usize;
        pos += 6;

        if pos + n * 4 > data.len() {
            return Err("XSCI: truncated grid indices".into());
        }
        let mut indices = Vec::with_capacity(n);
        for j in 0..n {
            indices.push(u32_le(&data[pos + j * 4..]));
        }
        pos += n * 4;
        map.insert([lng, lat], indices);
    }

    Ok((map, pos))
}

fn parse_preindex(data: &[u8], count: usize) -> Result<HashMap<[i16; 2], u32>, String> {
    if data.len() < count * 8 {
        return Err("XSCI: truncated preindex".into());
    }
    let mut map = HashMap::with_capacity(count);
    for i in 0..count {
        let base = i * 8;
        let lng = i16::from_le_bytes([data[base], data[base + 1]]);
        let lat = i16::from_le_bytes([data[base + 2], data[base + 3]]);
        let idx = u32_le(&data[base + 4..]);
        map.insert([lng, lat], idx);
    }
    Ok(map)
}

#[inline]
fn u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
#[inline]
fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
#[inline]
fn u64_le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}
#[inline]
fn f32_le(b: &[u8]) -> f32 {
    f32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
