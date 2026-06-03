# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build for production (release WASM)
npx wrangler deploy

# Build dev (faster debug WASM)
npx wrangler dev --env dev

# Manual build steps (used by wrangler internally)
cargo install -q "worker-build@^0.8"
worker-build --release      # production
worker-build                # dev / debug
```

There are no tests in this repository.

## Architecture

This is a Cloudflare Worker written in Rust, compiled to WASM via `worker-build`. Entry point: `src/lib.rs` → `build/index.js` (generated WASM wrapper).

### Data flow for each request

```
GET /?lng=<lng>&lat=<lat>[&format=geojson]
  → parse_params
  → get_or_init_finder (OnceLock — loads index once per isolate lifetime)
      → R2: fetch divisions.xs-index.gz → decompress → xsci::parse → XsFinder
  → collect_matched
      1. Country preindex fast-path (XsFinder::country_for_cell)
      2. Coarse tier (1°×1° grid) — R2 range-fetch + flatbuf::contains_point
      3. Fine tier (0.25°×0.25° grid) — R2 range-fetch + flatbuf::contains_point
  → build_geo_result (JSON) or build_feature_collection (GeoJSON)
```

### Module responsibilities

- **`src/xsci.rs`** — Parses the binary XSCI compact index format (magic `XSCI`, v1). Produces `XsIndex` with division subtypes, bboxes, slab offsets, coarse/fine grid maps, and a country preindex.
- **`src/finder.rs`** — `XsFinder` wraps `XsIndex`. Provides grid lookups, bbox filtering, short-circuit logic (skip polygon test when point is unambiguously inside a single candidate), and slab range lookups.
- **`src/flatbuf.rs`** — Hand-rolled FlatBuffers reader for `CompressedDivision` slab chunks. Decodes delta-varint-encoded rings (zigzag + unsigned varint, scale 1/100000°), runs point-in-polygon via `geometry-rs`, and extracts GeoJSON geometry and metadata fields.
- **`src/lib.rs`** — Worker entry point: HTTP routing, CORS, index init via `OnceLock`, orchestrates the two-tier coarse/fine lookup, serialises output.

### R2 bindings and env vars

| Binding / Var | Default value | Purpose |
|---|---|---|
| `XS_BUCKET` (r2) | `dataset` bucket | Holds the index and slab files |
| `XS_INDEX_KEY` | `xiangshan/divisions.xs-index.gz` | Gzip-compressed XSCI index |
| `XS_SLAB_KEY` | `xiangshan/divisions.xs-poly` | FlatBuffers slab, fetched via HTTP Range |

### CORS

Allowed origins are hard-coded in `src/lib.rs:381`: `https://ringsaturn.github.io` and `http://localhost:9999`. Add new origins there.

### Response formats

- Default: `GeoResult` JSON with fields `country`, `region`, `county`, `local_admin`, `locality`, `elapsed_ms`.
- `?format=geojson`: GeoJSON `FeatureCollection` with polygon geometries decoded from the slab.
