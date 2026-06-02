// Minimal FlatBuffers reader for CompressedDivision slab chunks.
//
// FlatBuffers binary layout (all integers little-endian):
//   Size-prefixed root:
//     [4] buffer size (u32)
//     [4] forward offset to root table (u32)
//   Table at position T:
//     [4] signed backward offset to vtable (i32)
//   Vtable:
//     [2] vtable size (u16)
//     [2] object size (u16)
//     [2 × N] field offsets from T (u16, 0 = field absent)
//   String at position S: [4] len (u32) + bytes (no null in slice)
//   Vector at position V: [4] count (u32) + elements
//   Vector of tables: elements are u32 forward offsets from their own position

// ----- low-level helpers -----

fn root_table_pos(chunk: &[u8]) -> Option<usize> {
    let fwd = u32::from_le_bytes(chunk.get(4..8)?.try_into().ok()?) as usize;
    Some(4 + fwd)
}

fn field_data_pos(buf: &[u8], table_pos: usize, field_idx: u16) -> Option<usize> {
    let vtable_slot = 4 + field_idx as usize * 2;
    let soffset = i32::from_le_bytes(buf.get(table_pos..table_pos + 4)?.try_into().ok()?);
    let vtable_pos = (table_pos as isize - soffset as isize) as usize;
    let vt_size = u16::from_le_bytes(buf.get(vtable_pos..vtable_pos + 2)?.try_into().ok()?) as usize;
    if vtable_slot + 2 > vt_size {
        return None; // field not present in this vtable version
    }
    let field_off = u16::from_le_bytes(
        buf.get(vtable_pos + vtable_slot..vtable_pos + vtable_slot + 2)?
            .try_into()
            .ok()?,
    ) as usize;
    if field_off == 0 {
        return None; // field explicitly absent
    }
    Some(table_pos + field_off)
}

fn read_str<'a>(buf: &'a [u8], table_pos: usize, field_idx: u16) -> Option<&'a str> {
    let slot = field_data_pos(buf, table_pos, field_idx)?;
    let fwd = u32::from_le_bytes(buf.get(slot..slot + 4)?.try_into().ok()?) as usize;
    let str_pos = slot + fwd;
    let len = u32::from_le_bytes(buf.get(str_pos..str_pos + 4)?.try_into().ok()?) as usize;
    std::str::from_utf8(buf.get(str_pos + 4..str_pos + 4 + len)?).ok()
}

fn read_u32_field(buf: &[u8], table_pos: usize, field_idx: u16) -> u32 {
    field_data_pos(buf, table_pos, field_idx)
        .and_then(|p| buf.get(p..p + 4))
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

fn read_bytes_vec<'a>(buf: &'a [u8], table_pos: usize, field_idx: u16) -> Option<&'a [u8]> {
    let slot = field_data_pos(buf, table_pos, field_idx)?;
    let fwd = u32::from_le_bytes(buf.get(slot..slot + 4)?.try_into().ok()?) as usize;
    let vec_pos = slot + fwd;
    let len = u32::from_le_bytes(buf.get(vec_pos..vec_pos + 4)?.try_into().ok()?) as usize;
    buf.get(vec_pos + 4..vec_pos + 4 + len)
}

// Returns (vector data start, element count) for a vector-of-tables field.
fn read_table_vec(buf: &[u8], table_pos: usize, field_idx: u16) -> Option<(usize, usize)> {
    let slot = field_data_pos(buf, table_pos, field_idx)?;
    let fwd = u32::from_le_bytes(buf.get(slot..slot + 4)?.try_into().ok()?) as usize;
    let vec_pos = slot + fwd;
    let count = u32::from_le_bytes(buf.get(vec_pos..vec_pos + 4)?.try_into().ok()?) as usize;
    Some((vec_pos + 4, count))
}

// Returns absolute table position for element i in a vector of tables.
fn table_vec_elem(buf: &[u8], data_pos: usize, i: usize) -> Option<usize> {
    let elem_pos = data_pos + i * 4;
    let fwd = u32::from_le_bytes(buf.get(elem_pos..elem_pos + 4)?.try_into().ok()?) as usize;
    Some(elem_pos + fwd)
}

// Returns absolute table position for a table-valued field.
fn read_table_field(buf: &[u8], table_pos: usize, field_idx: u16) -> Option<usize> {
    let slot = field_data_pos(buf, table_pos, field_idx)?;
    let fwd = u32::from_le_bytes(buf.get(slot..slot + 4)?.try_into().ok()?) as usize;
    Some(slot + fwd)
}

// ----- delta-varint ring decoder -----

const COORD_SCALE: f64 = 100_000.0;

fn read_uvarint32(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return None;
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte < 0x80 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    u32::try_from(result).ok()
}

#[inline]
fn decode_zigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

// Decode a CompressedRing into a closed Vec<Point> ready for geometry_rs.
// Returns None if point_count < 3 or decoding fails.
fn decode_ring(data: &[u8], point_count: u32) -> Option<Vec<geometry_rs::Point>> {
    if point_count < 3 {
        return None;
    }
    let n = point_count as usize;
    let mut pts = Vec::with_capacity(n + 1);
    let mut cursor = 0usize;
    let mut lng_acc: i32 = 0;
    let mut lat_acc: i32 = 0;

    for _ in 0..n {
        let dlng = decode_zigzag(read_uvarint32(data, &mut cursor)?);
        let dlat = decode_zigzag(read_uvarint32(data, &mut cursor)?);
        lng_acc = lng_acc.wrapping_add(dlng);
        lat_acc = lat_acc.wrapping_add(dlat);
        pts.push(geometry_rs::Point {
            x: lng_acc as f64 / COORD_SCALE,
            y: lat_acc as f64 / COORD_SCALE,
        });
    }

    // Close the ring (geometry_rs expects first == last).
    let first = pts[0];
    pts.push(first);
    Some(pts)
}

fn decode_ring_at_table(buf: &[u8], ring_tp: usize) -> Option<Vec<geometry_rs::Point>> {
    let data = read_bytes_vec(buf, ring_tp, 0)?; // field 0 = data ([ubyte])
    let count = read_u32_field(buf, ring_tp, 1); // field 1 = point_count
    decode_ring(data, count)
}

// ----- PIP -----

fn polygon_table_contains_point(buf: &[u8], poly_tp: usize, lng: f64, lat: f64) -> bool {
    let ext_tp = match read_table_field(buf, poly_tp, 0) {
        Some(p) => p,
        None => return false,
    };
    let exterior = match decode_ring_at_table(buf, ext_tp) {
        Some(pts) => pts,
        None => return false,
    };

    let holes: Vec<Vec<geometry_rs::Point>> =
        if let Some((holes_data, holes_count)) = read_table_vec(buf, poly_tp, 1) {
            (0..holes_count)
                .filter_map(|i| {
                    let hole_tp = table_vec_elem(buf, holes_data, i)?;
                    decode_ring_at_table(buf, hole_tp)
                })
                .collect()
        } else {
            Vec::new()
        };

    let options = geometry_rs::PolygonBuildOptions {
        enable_rtree: false,
        enable_compressed_quad: false,
        enable_y_stripes: false,
        rtree_min_segments: 64,
    };
    geometry_rs::Polygon::new(exterior, holes, Some(options))
        .contains_point(geometry_rs::Point { x: lng, y: lat })
}

// Check whether (lng, lat) is inside any polygon of the CompressedDivision chunk.
pub fn contains_point(chunk: &[u8], lng: f64, lat: f64) -> bool {
    let tp = match root_table_pos(chunk) {
        Some(p) => p,
        None => return false,
    };
    let (poly_data, poly_count) = match read_table_vec(chunk, tp, 9) {
        Some(v) => v,
        None => return false,
    };
    for i in 0..poly_count {
        if let Some(poly_tp) = table_vec_elem(chunk, poly_data, i) {
            if polygon_table_contains_point(chunk, poly_tp, lng, lat) {
                return true;
            }
        }
    }
    false
}

// ----- metadata extraction -----

// Returns the `id` field (field 0) of a CompressedDivision.
pub fn get_id(chunk: &[u8]) -> Option<&str> {
    let tp = root_table_pos(chunk)?;
    read_str(chunk, tp, 0)
}

// Returns the resolved display name.
// If lang is non-empty, checks names_common JSON (field 10) before falling back
// to the primary name (field 2).
pub fn resolve_name(chunk: &[u8], lang: &str) -> Option<String> {
    let tp = root_table_pos(chunk)?;

    if !lang.is_empty() {
        if let Some(names_json) = read_str(chunk, tp, 10) {
            if let Some(name) = lookup_i18n(names_json, lang) {
                return Some(name);
            }
        }
    }

    read_str(chunk, tp, 2).map(|s| s.to_string())
}

fn lookup_i18n(json: &str, lang: &str) -> Option<String> {
    // names_common is a flat JSON object: {"zh":"…","en":"…"}
    // We do a lightweight parse rather than pulling in full serde overhead here.
    let key = format!("\"{}\"", lang);
    let start = json.find(&key)?;
    let rest = json[start + key.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
