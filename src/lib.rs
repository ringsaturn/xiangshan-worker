// Cloudflare Worker: geographic reverse-geocoding via XSCI index + R2 slab.
//
// Cold start: fetches divisions.xs-index.gz from the bound R2 bucket, decompresses
// it, and parses the XSCI compact index into a global OnceLock.
//
// Per request: GET /?lng=<lng>&lat=<lat>[&format=geojson]
//   1. Looks up coarse + fine grid candidates from the in-memory index.
//   2. BBox-filters candidates.
//   3. Fetches slab chunks from R2 via Range requests.
//   4. Does point-in-polygon with geometry-rs.
//   5. Returns JSON result (default) or GeoJSON FeatureCollection (format=geojson).
mod finder;
mod flatbuf;
mod xsci;

use std::sync::OnceLock;

use serde::Serialize;
use worker::*;

use finder::{
    XsFinder, SUBTYPE_COUNTRY, SUBTYPE_COUNTY, SUBTYPE_DEPENDENCY, SUBTYPE_LOCALITY,
    SUBTYPE_LOCAL_ADMIN, SUBTYPE_MACRO_COUNTY, SUBTYPE_MACRO_REGION, SUBTYPE_REGION,
};

static FINDER: OnceLock<XsFinder> = OnceLock::new();

const INDEX_HTML: &str = include_str!("web/index.html");

// ---- worker entry point ----

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> Result<HttpResponse> {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok().map(str::to_owned));
    let origin = allowed_origin(origin.as_deref());

    if req.method() == http::Method::OPTIONS {
        return cors_preflight(origin);
    }
    if req.method() != http::Method::GET {
        return make_error(405, "method not allowed", origin);
    }

    let path = req.uri().path();

    if path == "/" || path.is_empty() {
        return make_html(origin);
    }
    if path != "/api" {
        return make_error(404, "not found", origin);
    }

    let query = req.uri().query().unwrap_or("");
    let (lng, lat, geojson) = match parse_params(query) {
        Ok(v) => v,
        Err(msg) => return make_error(400, &msg, origin),
    };

    let finder = match get_or_init_finder(&env).await {
        Ok(f) => f,
        Err(e) => return make_error(500, &format!("index init error: {e}"), origin),
    };

    let bucket = env.bucket("XS_BUCKET")?;
    let slab_key = slab_key(&env);

    let start_ms = js_sys::Date::now();
    let matched = match collect_matched(finder, &bucket, &slab_key, lng, lat).await {
        Ok(m) => m,
        Err(e) => return make_error(500, &format!("query error: {e}"), origin),
    };
    let elapsed_ms = js_sys::Date::now() - start_ms;

    if geojson {
        make_json(200, &build_feature_collection(&matched, elapsed_ms), origin)
    } else {
        let mut result = build_geo_result(&matched);
        result.elapsed_ms = elapsed_ms;
        make_json(200, &result, origin)
    }
}

// ---- index initialisation ----

async fn get_or_init_finder(env: &Env) -> Result<&'static XsFinder> {
    if let Some(f) = FINDER.get() {
        return Ok(f);
    }
    let finder = load_finder(env).await?;
    let _ = FINDER.set(finder);
    FINDER
        .get()
        .ok_or_else(|| Error::RustError("finder initialisation failed".into()))
}

fn index_key(env: &Env) -> String {
    env.var("XS_INDEX_KEY")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "divisions.xs-index.gz".into())
}

fn slab_key(env: &Env) -> String {
    env.var("XS_SLAB_KEY")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "divisions.xs-poly".into())
}

async fn load_finder(env: &Env) -> Result<XsFinder> {
    let bucket = env.bucket("XS_BUCKET")?;
    let key = index_key(env);
    let obj = bucket
        .get(&key)
        .execute()
        .await?
        .ok_or_else(|| Error::RustError(format!("{key} not found in R2")))?;

    let gz = r2_body_bytes(obj)
        .await
        .map_err(|e| Error::RustError(format!("read index: {e}")))?;

    let raw = decompress_gzip(&gz)?;
    let index = xsci::parse(&raw).map_err(Error::RustError)?;

    Ok(XsFinder::new(index))
}

async fn r2_body_bytes(obj: Object) -> Result<Vec<u8>> {
    let body = obj
        .body()
        .ok_or_else(|| Error::RustError("R2 object has no body".into()))?;
    body.bytes().await
}

fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let mut dec = GzDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| Error::RustError(e.to_string()))?;
    Ok(out)
}

// ---- core query: collect matched divisions ----

struct MatchedDiv {
    level: &'static str,
    chunk: Vec<u8>,
}

// Tracks which administrative levels have already been filled.
#[derive(Default)]
struct LevelSet {
    country: bool,
    region: bool,
    county: bool,
    local_admin: bool,
    locality: bool,
}

fn coarse_level_for(sub: u8, has: &mut LevelSet) -> Option<&'static str> {
    match sub {
        SUBTYPE_COUNTRY | SUBTYPE_DEPENDENCY if !has.country => {
            has.country = true;
            Some("country")
        }
        SUBTYPE_MACRO_REGION | SUBTYPE_REGION if !has.region => {
            has.region = true;
            Some("region")
        }
        SUBTYPE_MACRO_COUNTY if !has.county => {
            has.county = true;
            Some("county")
        }
        _ => None,
    }
}

fn fine_level_for(sub: u8, has: &mut LevelSet) -> Option<&'static str> {
    match sub {
        SUBTYPE_COUNTY if !has.county => {
            has.county = true;
            Some("county")
        }
        SUBTYPE_LOCAL_ADMIN if !has.local_admin => {
            has.local_admin = true;
            Some("local_admin")
        }
        SUBTYPE_LOCALITY if !has.locality => {
            has.locality = true;
            Some("locality")
        }
        _ => None,
    }
}

async fn collect_matched(
    finder: &XsFinder,
    bucket: &Bucket,
    slab_key: &str,
    lng: f64,
    lat: f64,
) -> Result<Vec<MatchedDiv>> {
    let mut matched: Vec<MatchedDiv> = Vec::new();
    let mut has = LevelSet::default();

    // --- country preindex fast path ---
    if let Some(country_idx) = finder.country_for_cell(lng, lat) {
        let chunk = fetch_slab(bucket, slab_key, finder.slab_range(country_idx)).await?;
        matched.push(MatchedDiv { level: "country", chunk });
        has.country = true;
    }

    // --- coarse tier (Country, Dependency, MacroRegion, Region, MacroCounty) ---
    let coarse = finder.coarse_candidates(lng, lat);
    if coarse.len() == 1 && XsFinder::can_short_circuit(lng, lat) {
        let idx = coarse[0];
        let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
        if let Some(level) = coarse_level_for(finder.subtype(idx), &mut has) {
            matched.push(MatchedDiv { level, chunk });
        }
    } else {
        let country_known = has.country;
        let need = finder.bbox_filtered(coarse, lng, lat, |idx| {
            if country_known {
                let sub = finder.subtype(idx);
                sub == SUBTYPE_COUNTRY || sub == SUBTYPE_DEPENDENCY
            } else {
                false
            }
        });
        for idx in need {
            if has.country && has.region && has.county {
                break;
            }
            let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
            if flatbuf::contains_point(&chunk, lng, lat) {
                if let Some(level) = coarse_level_for(finder.subtype(idx), &mut has) {
                    matched.push(MatchedDiv { level, chunk });
                }
            }
        }
    }

    // --- fine tier (County, LocalAdmin, Locality) ---
    let fine = finder.fine_candidates(lng, lat);
    if fine.len() == 1 && XsFinder::can_short_circuit(lng, lat) {
        let idx = fine[0];
        let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
        if let Some(level) = fine_level_for(finder.subtype(idx), &mut has) {
            matched.push(MatchedDiv { level, chunk });
        }
    } else {
        let need = finder.bbox_filtered(fine, lng, lat, |_| false);
        for idx in need {
            if has.county && has.local_admin && has.locality {
                break;
            }
            let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
            if flatbuf::contains_point(&chunk, lng, lat) {
                if let Some(level) = fine_level_for(finder.subtype(idx), &mut has) {
                    matched.push(MatchedDiv { level, chunk });
                }
            }
        }
    }

    Ok(matched)
}

async fn fetch_slab(bucket: &Bucket, key: &str, (offset, length): (u64, u64)) -> Result<Vec<u8>> {
    let obj = bucket
        .get(key)
        .range(Range::OffsetWithLength { offset, length })
        .execute()
        .await?
        .ok_or_else(|| Error::RustError(format!("{key} not found in R2")))?;

    r2_body_bytes(obj)
        .await
        .map_err(|e| Error::RustError(format!("read slab: {e}")))
}

// ---- output formats ----

#[derive(Serialize, Clone, Default)]
struct DivisionInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    names: Option<std::collections::HashMap<String, String>>,
}

impl DivisionInfo {
    fn from_chunk(chunk: &[u8]) -> Self {
        DivisionInfo {
            id: flatbuf::get_id(chunk).map(str::to_string),
            name: flatbuf::get_primary_name(chunk),
            names: flatbuf::get_names_map(chunk),
        }
    }
}

#[derive(Serialize, Default)]
struct GeoResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    county: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_admin: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locality: Option<DivisionInfo>,
    elapsed_ms: f64,
}

fn build_geo_result(matched: &[MatchedDiv]) -> GeoResult {
    let mut r = GeoResult::default();
    for div in matched {
        let info = DivisionInfo::from_chunk(&div.chunk);
        match div.level {
            "country"     => { if r.country.is_none()     { r.country     = Some(info); } }
            "region"      => { if r.region.is_none()      { r.region      = Some(info); } }
            "county"      => { if r.county.is_none()      { r.county      = Some(info); } }
            "local_admin" => { if r.local_admin.is_none() { r.local_admin = Some(info); } }
            "locality"    => { if r.locality.is_none()    { r.locality    = Some(info); } }
            _ => {}
        }
    }
    r
}

#[derive(Serialize)]
struct FeatureCollection {
    #[serde(rename = "type")]
    fc_type: &'static str,
    elapsed_ms: f64,
    features: Vec<GeoFeature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    county: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_admin: Option<DivisionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locality: Option<DivisionInfo>,
}

#[derive(Serialize)]
struct GeoFeature {
    #[serde(rename = "type")]
    feature_type: &'static str,
    geometry: serde_json::Value,
    properties: FeatureProps,
}

#[derive(Serialize)]
struct FeatureProps {
    level: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    names: Option<std::collections::HashMap<String, String>>,
}

fn build_feature_collection(matched: &[MatchedDiv], elapsed_ms: f64) -> FeatureCollection {
    let mut fc = FeatureCollection {
        fc_type: "FeatureCollection",
        elapsed_ms,
        features: Vec::new(),
        country: None,
        region: None,
        county: None,
        local_admin: None,
        locality: None,
    };

    for div in matched {
        let info = DivisionInfo::from_chunk(&div.chunk);

        if let Some(geometry) = flatbuf::get_geometry_geojson(&div.chunk) {
            fc.features.push(GeoFeature {
                feature_type: "Feature",
                geometry,
                properties: FeatureProps {
                    level: div.level,
                    id: info.id.clone(),
                    name: info.name.clone(),
                    names: info.names.clone(),
                },
            });
        }

        let slot = match div.level {
            "country"     => &mut fc.country,
            "region"      => &mut fc.region,
            "county"      => &mut fc.county,
            "local_admin" => &mut fc.local_admin,
            "locality"    => &mut fc.locality,
            _             => continue,
        };
        if slot.is_none() {
            *slot = Some(info);
        }
    }

    fc
}

// ---- response helpers ----

const ALLOWED_ORIGINS: &[&str] = &["https://ringsaturn.github.io", "http://localhost:9999"];

fn allowed_origin(origin: Option<&str>) -> Option<&'static str> {
    let o = origin?;
    ALLOWED_ORIGINS.iter().copied().find(|&allowed| allowed == o)
}

fn cors_preflight(origin: Option<&str>) -> Result<HttpResponse> {
    let origin = origin.unwrap_or("");
    ResponseBuilder::new()
        .with_status(204)
        .with_header("access-control-allow-origin", origin)?
        .with_header("access-control-allow-methods", "GET, OPTIONS")?
        .with_header("access-control-allow-headers", "*")?
        .with_header("access-control-max-age", "86400")?
        .with_header("vary", "origin")?
        .empty()
        .try_into()
}

fn make_html(origin: Option<&str>) -> Result<HttpResponse> {
    let origin = origin.unwrap_or("");
    ResponseBuilder::new()
        .with_status(200)
        .with_header("access-control-allow-origin", origin)?
        .with_header("vary", "origin")?
        .from_html(INDEX_HTML)?
        .try_into()
}

fn make_json<T: Serialize>(status: u16, value: &T, origin: Option<&str>) -> Result<HttpResponse> {
    let origin = origin.unwrap_or("");
    ResponseBuilder::new()
        .with_status(status)
        .with_header("access-control-allow-origin", origin)?
        .with_header("access-control-allow-methods", "GET, OPTIONS")?
        .with_header("vary", "origin")?
        .from_json(value)?
        .try_into()
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

fn make_error(status: u16, msg: &str, origin: Option<&str>) -> Result<HttpResponse> {
    make_json(status, &ErrorBody { error: msg }, origin)
}

// ---- parameter parsing ----

fn parse_params(query: &str) -> std::result::Result<(f64, f64, bool), String> {
    let mut lng: Option<f64> = None;
    let mut lat: Option<f64> = None;
    let mut geojson = false;

    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("");
        let val = it.next().unwrap_or("");
        match key {
            "lng" => lng = val.parse().ok(),
            "lat" => lat = val.parse().ok(),
            "format" => geojson = val == "geojson",
            _ => {}
        }
    }

    let lng = lng.ok_or_else(|| "missing or invalid `lng` parameter".to_string())?;
    let lat = lat.ok_or_else(|| "missing or invalid `lat` parameter".to_string())?;
    Ok((lng, lat, geojson))
}
