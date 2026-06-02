// Cloudflare Worker: geographic reverse-geocoding via XSCI index + R2 slab.
//
// Cold start: fetches divisions.xs-index.gz from the bound R2 bucket, decompresses
// it, and parses the XSCI compact index into a global OnceLock.
//
// Per request: GET /?lng=<lng>&lat=<lat>[&lang=<lang>]
//   1. Looks up coarse + fine grid candidates from the in-memory index.
//   2. BBox-filters candidates.
//   3. Fetches slab chunks from R2 via Range requests.
//   4. Does point-in-polygon with geometry-rs.
//   5. Returns JSON result.
mod flatbuf;
mod finder;
mod xsci;

use std::sync::OnceLock;

use serde::Serialize;
use worker::*;

use finder::{
    XsFinder, SUBTYPE_COUNTRY, SUBTYPE_COUNTY, SUBTYPE_DEPENDENCY, SUBTYPE_LOCAL_ADMIN,
    SUBTYPE_LOCALITY, SUBTYPE_MACRO_COUNTY, SUBTYPE_MACRO_REGION, SUBTYPE_REGION,
};

static FINDER: OnceLock<XsFinder> = OnceLock::new();

// ---- worker entry point ----

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> Result<HttpResponse> {
    if req.method() != http::Method::GET {
        return make_error(405, "method not allowed");
    }

    let query = req.uri().query().unwrap_or("");
    let (lng, lat, lang) = match parse_params(query) {
        Ok(v) => v,
        Err(msg) => return make_error(400, &msg),
    };

    let finder = match get_or_init_finder(&env).await {
        Ok(f) => f,
        Err(e) => return make_error(500, &format!("index init error: {e}")),
    };

    let bucket = env.bucket("XS_BUCKET")?;
    let slab_key = slab_key(&env);

    match run_query(finder, &bucket, &slab_key, lng, lat, &lang).await {
        Ok(result) => make_json(200, &result),
        Err(e) => make_error(500, &format!("query error: {e}")),
    }
}

// ---- index initialisation ----

async fn get_or_init_finder(env: &Env) -> Result<&'static XsFinder> {
    if let Some(f) = FINDER.get() {
        return Ok(f);
    }
    let finder = load_finder(env).await?;
    // set() fails if another request won the race; that's fine.
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

// Read all bytes from an R2 object's body.
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

// ---- query ----

#[derive(Serialize, Default)]
struct GeoResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    county: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    county_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_admin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_admin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locality_id: Option<String>,
}

async fn run_query(
    finder: &XsFinder,
    bucket: &Bucket,
    slab_key: &str,
    lng: f64,
    lat: f64,
    lang: &str,
) -> Result<GeoResult> {
    let mut r = GeoResult::default();

    // --- country preindex fast path ---
    if let Some(country_idx) = finder.country_for_cell(lng, lat) {
        let chunk = fetch_slab(bucket, slab_key, finder.slab_range(country_idx)).await?;
        r.country = flatbuf::resolve_name(&chunk, lang);
        r.country_id = flatbuf::get_id(&chunk).map(str::to_string);
    }

    // --- coarse tier (Country, Dependency, MacroRegion, Region, MacroCounty) ---
    let coarse = finder.coarse_candidates(lng, lat);
    if coarse.len() == 1 && XsFinder::can_short_circuit(lng, lat) {
        let idx = coarse[0];
        let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
        apply_coarse(
            &mut r,
            finder.subtype(idx),
            flatbuf::resolve_name(&chunk, lang),
            flatbuf::get_id(&chunk).map(str::to_string),
        );
    } else {
        let country_known = r.country.is_some();
        let need = finder.bbox_filtered(coarse, lng, lat, |idx| {
            if country_known {
                let sub = finder.subtype(idx);
                sub == SUBTYPE_COUNTRY || sub == SUBTYPE_DEPENDENCY
            } else {
                false
            }
        });
        for idx in need {
            if r.country.is_some() && r.region.is_some() && r.county.is_some() {
                break;
            }
            let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
            if flatbuf::contains_point(&chunk, lng, lat) {
                apply_coarse(
                    &mut r,
                    finder.subtype(idx),
                    flatbuf::resolve_name(&chunk, lang),
                    flatbuf::get_id(&chunk).map(str::to_string),
                );
            }
        }
    }

    // --- fine tier (County, LocalAdmin, Locality) ---
    let fine = finder.fine_candidates(lng, lat);
    if fine.len() == 1 && XsFinder::can_short_circuit(lng, lat) {
        let idx = fine[0];
        let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
        apply_fine(
            &mut r,
            finder.subtype(idx),
            flatbuf::resolve_name(&chunk, lang),
            flatbuf::get_id(&chunk).map(str::to_string),
        );
    } else {
        let need = finder.bbox_filtered(fine, lng, lat, |_| false);
        for idx in need {
            if r.county.is_some() && r.local_admin.is_some() && r.locality.is_some() {
                break;
            }
            let chunk = fetch_slab(bucket, slab_key, finder.slab_range(idx)).await?;
            if flatbuf::contains_point(&chunk, lng, lat) {
                apply_fine(
                    &mut r,
                    finder.subtype(idx),
                    flatbuf::resolve_name(&chunk, lang),
                    flatbuf::get_id(&chunk).map(str::to_string),
                );
            }
        }
    }

    Ok(r)
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

fn apply_coarse(r: &mut GeoResult, subtype: u8, name: Option<String>, id: Option<String>) {
    match subtype {
        SUBTYPE_COUNTRY | SUBTYPE_DEPENDENCY => {
            if r.country.is_none() {
                r.country = name;
                r.country_id = id;
            }
        }
        SUBTYPE_MACRO_REGION | SUBTYPE_REGION => {
            if r.region.is_none() {
                r.region = name;
                r.region_id = id;
            }
        }
        SUBTYPE_MACRO_COUNTY => {
            if r.county.is_none() {
                r.county = name;
                r.county_id = id;
            }
        }
        _ => {}
    }
}

fn apply_fine(r: &mut GeoResult, subtype: u8, name: Option<String>, id: Option<String>) {
    match subtype {
        SUBTYPE_COUNTY => {
            if r.county.is_none() {
                r.county = name;
                r.county_id = id;
            }
        }
        SUBTYPE_LOCAL_ADMIN => {
            if r.local_admin.is_none() {
                r.local_admin = name;
                r.local_admin_id = id;
            }
        }
        SUBTYPE_LOCALITY => {
            if r.locality.is_none() {
                r.locality = name;
                r.locality_id = id;
            }
        }
        _ => {}
    }
}

// ---- response helpers ----

fn make_json<T: Serialize>(status: u16, value: &T) -> Result<HttpResponse> {
    ResponseBuilder::new()
        .with_status(status)
        .with_header("access-control-allow-origin", "*")?
        .from_json(value)?
        .try_into()
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

fn make_error(status: u16, msg: &str) -> Result<HttpResponse> {
    make_json(status, &ErrorBody { error: msg })
}

// ---- parameter parsing ----

fn parse_params(query: &str) -> std::result::Result<(f64, f64, String), String> {
    let mut lng: Option<f64> = None;
    let mut lat: Option<f64> = None;
    let mut lang = String::new();

    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("");
        let val = it.next().unwrap_or("");
        match key {
            "lng" => lng = val.parse().ok(),
            "lat" => lat = val.parse().ok(),
            "lang" => lang = val.to_string(),
            _ => {}
        }
    }

    let lng = lng.ok_or_else(|| "missing or invalid `lng` parameter".to_string())?;
    let lat = lat.ok_or_else(|| "missing or invalid `lat` parameter".to_string())?;
    Ok((lng, lat, lang))
}
