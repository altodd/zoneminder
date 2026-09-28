//! Preview strips for scrubbing (phase 3).
//!
//! The detector already decodes every camera's substream at ~5 fps. Every
//! `preview_secs` one of those frames is shrunk to a 160-pixel-wide colour
//! tile and appended to an hourly file, so hovering over the timeline can show
//! a picture for any instant with one small file read and no ffmpeg, even on
//! a slow link (a tile is ~3 KB). An hour of tiles is ~2 MB per camera.
//!
//! Layout: `<thumb_dir>/<camera>/preview/<YYYYMMDDHH>.pvs` (UTC hour).
//! File format: 5-byte magic `ZMPV\x01`, then records of
//! `dts:i64le | len:u32le | jpeg bytes`. Append-only; a truncated trailing
//! record (crash mid-write) is ignored by the scanner.
//!
//! `sprite()` composes an hour into one JPEG sheet (30 columns) plus a JSON
//! manifest, the way YouTube storyboards work, for filmstrip UIs.

use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::mp4::TIMESCALE;

pub const MAGIC: &[u8; 5] = b"ZMPV\x01";
pub const TILE_W: u32 = 160;
pub const SPRITE_COLS: u32 = 30;
const JPEG_QUALITY: u8 = 70;

/// Tile height for a source of `w`x`h` (keeps the aspect, even numbers).
pub fn tile_dims(w: u32, h: u32) -> (u32, u32) {
    let th = ((TILE_W as u64 * h as u64 / w.max(1) as u64) as u32).max(2);
    (TILE_W, th & !1)
}

/// Box-filter a planar YUV 4:2:0 frame (`w`x`h`, as ffmpeg's `yuv420p`
/// rawvideo) down to an RGB8 tile of `tw`x`th`.
pub fn yuv420_to_rgb_tile(yuv: &[u8], w: usize, h: usize, tw: u32, th: u32) -> Vec<u8> {
    let (tw, th) = (tw as usize, th as usize);
    let mut out = vec![0u8; tw * th * 3];
    if yuv.len() < w * h * 3 / 2 || w == 0 || h == 0 {
        return out;
    }
    let (y_plane, rest) = yuv.split_at(w * h);
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let (u_plane, v_plane) = rest.split_at(cw * ch);
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let (mut sy, mut su, mut sv, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    sy += y_plane[y * w + x] as u32;
                    let ci = (y / 2) * cw + x / 2;
                    su += u_plane.get(ci).copied().unwrap_or(128) as u32;
                    sv += v_plane.get(ci).copied().unwrap_or(128) as u32;
                    n += 1;
                }
            }
            let (yy, uu, vv) = (sy as f32 / n as f32, su as f32 / n as f32 - 128.0, sv as f32 / n as f32 - 128.0);
            // BT.601 full range (ffmpeg's yuvj420p for IP cameras is full range; limited-range
            // sources come out slightly low-contrast, which does not matter for a 160 px preview)
            let r = yy + 1.402 * vv;
            let g = yy - 0.344 * uu - 0.714 * vv;
            let b = yy + 1.772 * uu;
            let o = (ty * tw + tx) * 3;
            out[o] = r.clamp(0.0, 255.0) as u8;
            out[o + 1] = g.clamp(0.0, 255.0) as u8;
            out[o + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }
    out
}

pub fn encode_jpeg(rgb: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    enc.encode(rgb, w, h, image::ExtendedColorType::Rgb8)?;
    Ok(buf)
}

/// UTC hour key of a dts, e.g. `2027011510`.
pub fn hour_key(dts: i64) -> String {
    let secs = dts / TIMESCALE as i64;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap_or_default().format("%Y%m%d%H").to_string()
}

/// Start dts of an hour key (None if malformed).
pub fn hour_start(key: &str) -> Option<i64> {
    let t = chrono::NaiveDateTime::parse_from_str(&format!("{key}0000"), "%Y%m%d%H%M%S").ok()?;
    Some(t.and_utc().timestamp() * TIMESCALE as i64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileRef {
    pub dts: i64,
    pub offset: u64,
    pub len: u32,
}

/// Read the record table of a `.pvs` file (payloads are skipped).
pub fn scan(path: &Path) -> Result<Vec<TileRef>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut magic = [0u8; 5];
    if f.read_exact(&mut magic).is_err() || &magic != MAGIC {
        anyhow::bail!("not a preview file");
    }
    let mut pos = 5u64;
    let mut out = Vec::new();
    let mut hdr = [0u8; 12];
    while pos + 12 <= len {
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut hdr)?;
        let dts = i64::from_le_bytes(hdr[0..8].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        if n == 0 || pos + 12 + n as u64 > len {
            break; // truncated tail
        }
        out.push(TileRef { dts, offset: pos + 12, len: n });
        pos += 12 + n as u64;
    }
    Ok(out)
}

fn read_tile(path: &Path, t: TileRef) -> Result<Vec<u8>> {
    crate::thumbs::read_range(path, t.offset, t.len)
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(thumb_dir: &Path) -> Store {
        Store { root: thumb_dir.to_path_buf() }
    }
    pub fn dir(&self, cam: i64) -> PathBuf {
        self.root.join(cam.to_string()).join("preview")
    }
    pub fn file(&self, cam: i64, hour: &str) -> PathBuf {
        self.dir(cam).join(format!("{hour}.pvs"))
    }

    /// Append one tile. Creates the hour file with its magic on first use.
    pub fn append(&self, cam: i64, dts: i64, jpeg: &[u8]) -> Result<()> {
        let path = self.file(cam, &hour_key(dts));
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        if f.metadata()?.len() == 0 {
            f.write_all(MAGIC)?;
        }
        let mut rec = Vec::with_capacity(12 + jpeg.len());
        rec.extend_from_slice(&dts.to_le_bytes());
        rec.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        rec.extend_from_slice(jpeg);
        f.write_all(&rec)?;
        Ok(())
    }

    /// The tile closest to `dts` within `max_delta` ticks, looking at the
    /// hour file of `dts` and, near the boundary, its neighbours.
    pub fn nearest(&self, cam: i64, dts: i64, max_delta: i64) -> Result<Option<Vec<u8>>> {
        let mut best: Option<(i64, PathBuf, TileRef)> = None;
        for d in [0i64, -1, 1] {
            let key = hour_key(dts + d * 3600 * TIMESCALE as i64);
            let path = self.file(cam, &key);
            if !path.is_file() {
                continue;
            }
            for t in scan(&path)? {
                let delta = (t.dts - dts).abs();
                if delta <= max_delta && best.as_ref().map(|b| delta < b.0).unwrap_or(true) {
                    best = Some((delta, path.clone(), t));
                }
            }
            if d == 0 && best.as_ref().map(|b| b.0 < max_delta / 2).unwrap_or(false) {
                break; // a close hit in the right hour; no need to open the neighbours
            }
        }
        match best {
            Some((_, p, t)) => Ok(Some(read_tile(&p, t)?)),
            None => Ok(None),
        }
    }

    /// Hour keys with previews for a camera, oldest first.
    pub fn hours(&self, cam: i64) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.dir(cam))
            .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".pvs").map(|s| s.to_string())).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Compose an hour into a sprite sheet (`SPRITE_COLS` per row) and a
    /// manifest `{cols, tile_w, tile_h, times: [ms, ...]}`. Closed hours are
    /// cached next to the source as `<hour>.jpg` / `<hour>.json`.
    pub fn sprite(&self, cam: i64, hour: &str) -> Result<Option<(Vec<u8>, serde_json::Value)>> {
        let src = self.file(cam, hour);
        if !src.is_file() {
            return Ok(None);
        }
        let jpg = src.with_extension("jpg");
        let json = src.with_extension("json");
        let closed = hour_start(hour).map(|s| crate::db::now_dts() > s + 3600 * TIMESCALE as i64 + 60 * TIMESCALE as i64).unwrap_or(false);
        if closed && jpg.is_file() && json.is_file() {
            let m: serde_json::Value = serde_json::from_slice(&std::fs::read(&json)?)?;
            return Ok(Some((std::fs::read(&jpg)?, m)));
        }
        let refs = scan(&src)?;
        if refs.is_empty() {
            return Ok(None);
        }
        let mut tiles = Vec::with_capacity(refs.len());
        let mut times = Vec::with_capacity(refs.len());
        let (mut tw, mut th) = (TILE_W, 0u32);
        for r in &refs {
            let img = image::load_from_memory(&read_tile(&src, *r)?).context("decoding tile")?.to_rgb8();
            if th == 0 {
                tw = img.width();
                th = img.height();
            }
            tiles.push(img);
            times.push(crate::video::dts_to_ms(r.dts));
        }
        let rows = (tiles.len() as u32).div_ceil(SPRITE_COLS);
        let mut sheet = image::RgbImage::new(tw * SPRITE_COLS.min(tiles.len() as u32), th * rows);
        for (i, t) in tiles.iter().enumerate() {
            let (cx, cy) = ((i as u32 % SPRITE_COLS) * tw, (i as u32 / SPRITE_COLS) * th);
            if t.width() == tw && t.height() == th {
                image::imageops::replace(&mut sheet, t, cx as i64, cy as i64);
            }
        }
        let bytes = encode_jpeg(sheet.as_raw(), sheet.width(), sheet.height(), JPEG_QUALITY)?;
        let manifest = serde_json::json!({"cols": SPRITE_COLS, "tile_w": tw, "tile_h": th, "times": times});
        if closed {
            let _ = std::fs::write(&jpg, &bytes);
            let _ = std::fs::write(&json, manifest.to_string());
        }
        Ok(Some((bytes, manifest)))
    }

    /// Delete preview files of hours that ended before `before_dts`.
    pub fn prune(&self, cam: i64, before_dts: i64) -> usize {
        let mut n = 0;
        let Ok(rd) = std::fs::read_dir(self.dir(cam)) else { return 0 };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(key) = name.split('.').next() else { continue };
            if let Some(start) = hour_start(key) {
                if start + 3600 * TIMESCALE as i64 <= before_dts && std::fs::remove_file(e.path()).is_ok() {
                    n += 1;
                }
            }
        }
        n
    }
}

/// Per-detector state: turns one decoded frame every `interval` into a tile.
pub struct PreviewWriter {
    store: Store,
    cam: i64,
    interval: i64,
    last: i64,
    dims: (u32, u32),
}

impl PreviewWriter {
    pub fn new(thumb_dir: &Path, cam: i64, interval_secs: u32, src_w: u32, src_h: u32) -> PreviewWriter {
        PreviewWriter { store: Store::new(thumb_dir), cam, interval: interval_secs as i64 * TIMESCALE as i64, last: 0, dims: tile_dims(src_w, src_h) }
    }
    /// Feed a yuv420p frame; returns true when a tile was written.
    pub fn maybe(&mut self, dts: i64, yuv: &[u8], w: usize, h: usize) -> Result<bool> {
        if self.interval <= 0 || dts - self.last < self.interval {
            return Ok(false);
        }
        self.last = dts;
        let (tw, th) = self.dims;
        let rgb = yuv420_to_rgb_tile(yuv, w, h, tw, th);
        let jpeg = encode_jpeg(&rgb, tw, th, JPEG_QUALITY)?;
        self.store.append(self.cam, dts, &jpeg)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_yuv(w: usize, h: usize, y: u8, u: u8, v: u8) -> Vec<u8> {
        let mut f = vec![y; w * h];
        f.extend(vec![u; w.div_ceil(2) * h.div_ceil(2)]);
        f.extend(vec![v; w.div_ceil(2) * h.div_ceil(2)]);
        f
    }

    #[test]
    fn tile_dims_keep_aspect() {
        assert_eq!(tile_dims(320, 180), (160, 90));
        assert_eq!(tile_dims(640, 480), (160, 120));
        assert_eq!(tile_dims(320, 181), (160, 90)); // rounded to even
    }

    #[test]
    fn yuv_conversion_produces_expected_colours() {
        // pure red in full-range BT.601: Y=76 U=85 V=255
        let f = synthetic_yuv(320, 180, 76, 85, 255);
        let rgb = yuv420_to_rgb_tile(&f, 320, 180, 160, 90);
        assert_eq!(rgb.len(), 160 * 90 * 3);
        let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
        assert!(r > 240 && g < 20 && b < 20, "{r},{g},{b}");
        // grey stays grey and a short buffer yields black, never a panic
        let f = synthetic_yuv(320, 180, 128, 128, 128);
        let rgb = yuv420_to_rgb_tile(&f, 320, 180, 160, 90);
        assert!((rgb[0] as i32 - 128).abs() < 2 && rgb[0] == rgb[1] && rgb[1] == rgb[2]);
        assert!(yuv420_to_rgb_tile(&f[..100], 320, 180, 160, 90).iter().all(|b| *b == 0));
    }

    #[test]
    fn store_roundtrip_nearest_and_truncation() {
        let d = tempfile::tempdir().unwrap();
        let st = Store::new(d.path());
        let base = 1_800_000_000 * TIMESCALE as i64; // 2027-01-15 08:00:00 UTC exactly
        let jpeg = encode_jpeg(&vec![200u8; 160 * 90 * 3], 160, 90, 70).unwrap();
        for i in 0..5 {
            st.append(7, base + i * 5 * TIMESCALE as i64, &jpeg).unwrap();
        }
        // one tile in the previous hour, 2 s before the boundary
        st.append(7, base - 2 * TIMESCALE as i64, b"\xff\xd8prev").unwrap();
        let refs = scan(&st.file(7, &hour_key(base))).unwrap();
        assert_eq!(refs.len(), 5);
        assert_eq!(refs[1].dts, base + 5 * TIMESCALE as i64);
        assert_eq!(st.nearest(7, base + 12 * TIMESCALE as i64, 3 * TIMESCALE as i64).unwrap().unwrap(), jpeg);
        assert!(st.nearest(7, base + 100 * TIMESCALE as i64, 3 * TIMESCALE as i64).unwrap().is_none());
        // just after the hour boundary the previous hour's tile is closer
        assert_eq!(st.nearest(7, base - TIMESCALE as i64, 3 * TIMESCALE as i64).unwrap().unwrap(), b"\xff\xd8prev");
        assert_eq!(st.hours(7), vec![hour_key(base - TIMESCALE as i64), hour_key(base)]);
        // a truncated trailing record is ignored
        let p = st.file(7, &hour_key(base));
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.extend_from_slice(&(base + 99).to_le_bytes());
        bytes.extend_from_slice(&5000u32.to_le_bytes());
        bytes.extend_from_slice(b"short");
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(scan(&p).unwrap().len(), 5);
        // prune removes hours that ended before the cutoff
        assert_eq!(st.prune(7, base), 1);
        assert_eq!(st.hours(7), vec![hour_key(base)]);
        assert_eq!(st.prune(7, base + 3600 * TIMESCALE as i64), 1);
        assert!(st.hours(7).is_empty());
        assert!(scan(&d.path().join("nope")).is_err());
    }

    #[test]
    fn sprite_composes_tiles_and_manifest() {
        let d = tempfile::tempdir().unwrap();
        let st = Store::new(d.path());
        let base = 1_700_000_000 * TIMESCALE as i64; // 2023: a long-closed hour, so the sprite is cached
        for i in 0..35u8 {
            let jpeg = encode_jpeg(&vec![i * 7; 160 * 90 * 3], 160, 90, 70).unwrap();
            st.append(3, base + i as i64 * 5 * TIMESCALE as i64, &jpeg).unwrap();
        }
        let (jpg, manifest) = st.sprite(3, &hour_key(base)).unwrap().unwrap();
        let img = image::load_from_memory(&jpg).unwrap();
        assert_eq!(img.width(), 160 * 30);
        assert_eq!(img.height(), 90 * 2);
        assert_eq!(manifest["cols"], 30);
        assert_eq!(manifest["times"].as_array().unwrap().len(), 35);
        assert_eq!(manifest["times"][1], crate::video::dts_to_ms(base + 5 * TIMESCALE as i64));
        // the hour is long past: cached files exist and a second call serves them
        assert!(st.file(3, &hour_key(base)).with_extension("jpg").is_file());
        let (jpg2, _) = st.sprite(3, &hour_key(base)).unwrap().unwrap();
        assert_eq!(jpg, jpg2);
        assert!(st.sprite(3, "2020010100").unwrap().is_none());
    }

    #[test]
    fn writer_respects_interval() {
        let d = tempfile::tempdir().unwrap();
        let mut w = PreviewWriter::new(d.path(), 1, 5, 320, 180);
        let f = synthetic_yuv(320, 180, 100, 128, 128);
        let t0 = 1_800_000_000 * TIMESCALE as i64;
        assert!(w.maybe(t0, &f, 320, 180).unwrap());
        assert!(!w.maybe(t0 + 2 * TIMESCALE as i64, &f, 320, 180).unwrap());
        assert!(w.maybe(t0 + 5 * TIMESCALE as i64, &f, 320, 180).unwrap());
        assert_eq!(scan(&Store::new(d.path()).file(1, &hour_key(t0))).unwrap().len(), 2);
        let mut off = PreviewWriter::new(d.path(), 2, 0, 320, 180);
        assert!(!off.maybe(t0, &f, 320, 180).unwrap());
    }
}
