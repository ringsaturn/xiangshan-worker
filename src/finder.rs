// XsFinder: in-memory index for geographic point-in-polygon queries.
//
// Holds the XSCI compact index (subtypes, bboxes, slab offsets, grid maps).
// Does not perform any I/O — slab chunk fetching is handled by the caller.
use std::collections::HashMap;

use crate::xsci::XsIndex;

// Subtype constants matching the FlatBuffers Subtype enum.
pub const SUBTYPE_COUNTRY: u8 = 0;
pub const SUBTYPE_DEPENDENCY: u8 = 1;
pub const SUBTYPE_MACRO_REGION: u8 = 2;
pub const SUBTYPE_REGION: u8 = 3;
pub const SUBTYPE_MACRO_COUNTY: u8 = 4;
pub const SUBTYPE_COUNTY: u8 = 5;
pub const SUBTYPE_LOCAL_ADMIN: u8 = 6;
pub const SUBTYPE_LOCALITY: u8 = 7;

pub struct XsFinder {
    subtypes: Vec<u8>,
    bboxes: Vec<[f32; 4]>, // [xmin, xmax, ymin, ymax]
    poly_offsets: Vec<u64>,
    poly_lengths: Vec<u32>,
    grid_coarse: HashMap<[i16; 2], Vec<u32>>,
    grid_fine: HashMap<[i16; 2], Vec<u32>>,
    country_preindex: HashMap<[i16; 2], u32>,
}

impl XsFinder {
    pub fn new(idx: XsIndex) -> Self {
        Self {
            subtypes: idx.subtypes,
            bboxes: idx.bboxes,
            poly_offsets: idx.poly_offsets,
            poly_lengths: idx.poly_lengths,
            grid_coarse: idx.grid_coarse,
            grid_fine: idx.grid_fine,
            country_preindex: idx.country_preindex,
        }
    }

    // Return (offset, length) in the xs-poly slab for a given division index.
    pub fn slab_range(&self, idx: u32) -> (u64, u64) {
        let i = idx as usize;
        (self.poly_offsets[i], self.poly_lengths[i] as u64)
    }

    pub fn subtype(&self, idx: u32) -> u8 {
        self.subtypes[idx as usize]
    }

    // Floor-of-degree grid key for the coarse (1°×1°) tier.
    pub fn coarse_key(lng: f64, lat: f64) -> [i16; 2] {
        [lng.floor() as i16, lat.floor() as i16]
    }

    // Quarter-degree grid key for the fine (0.25°×0.25°) tier.
    pub fn fine_key(lng: f64, lat: f64) -> [i16; 2] {
        [(lng * 4.0).floor() as i16, (lat * 4.0).floor() as i16]
    }

    // Skip polygon check when the point is well away from ±180°/±90° boundaries.
    pub fn can_short_circuit(lng: f64, lat: f64) -> bool {
        lng > -179.0 && lng < 179.0 && lat > -89.0 && lat < 89.0
    }

    // Coarse-tier candidate indices for (lng, lat). Empty slice = no candidates.
    pub fn coarse_candidates(&self, lng: f64, lat: f64) -> &[u32] {
        self.grid_coarse
            .get(&Self::coarse_key(lng, lat))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // Fine-tier candidate indices for (lng, lat).
    pub fn fine_candidates(&self, lng: f64, lat: f64) -> &[u32] {
        self.grid_fine
            .get(&Self::fine_key(lng, lat))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // Country preindex: fast-path country index for the coarse cell, if known.
    pub fn country_for_cell(&self, lng: f64, lat: f64) -> Option<u32> {
        self.country_preindex
            .get(&Self::coarse_key(lng, lat))
            .copied()
    }

    // Return candidates from `indices` whose bounding box contains (lng, lat).
    // `skip_if` is an optional predicate; returning true causes that candidate
    // to be excluded regardless of bbox.
    pub fn bbox_filtered<'a>(
        &self,
        indices: &'a [u32],
        lng: f64,
        lat: f64,
        skip_if: impl Fn(u32) -> bool,
    ) -> Vec<u32> {
        indices
            .iter()
            .copied()
            .filter(|&idx| {
                if skip_if(idx) {
                    return false;
                }
                let bb = &self.bboxes[idx as usize];
                lng >= bb[0] as f64
                    && lng <= bb[1] as f64
                    && lat >= bb[2] as f64
                    && lat <= bb[3] as f64
            })
            .collect()
    }
}
