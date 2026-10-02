//! Evidence export: one ZIP for a time range and a set of cameras, with
//!
//! * one MP4 per camera, cut from the recording without re-encoding. Our own
//!   video-only fragments become a *progressive* MP4 (index at the front,
//!   plays in any player); recordings with audio, several codec settings in
//!   the range, or imported ZoneMinder files are exported as fragmented MP4
//!   (VLC, browsers, ffmpeg). Gaps in the recording are kept as gaps: the
//!   frame before a gap is held, so playback time is wall-clock time.
//! * `manifest.json`: cameras, the range and each file's first/last frame in
//!   local time and UTC, codec, size, frame count, gaps, SHA-256, who
//!   exported it and when;
//! * `SHA256SUMS` (`sha256sum -c SHA256SUMS`) and a short `README.txt`.
//!
//! The ZIP is written as a stream (entries "stored": video does not
//! compress) with data descriptors, and ZIP64 records where an entry or an
//! offset needs them. A dry run of the same writer gives the exact length,
//! so the browser can show progress for large exports.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::db::{Camera, Db};
use crate::mp4::{self, FragEntry, TIMESCALE};

/// Longest range one export may cover (the same limit as `video.mp4`).
pub const MAX_RANGE_DTS: i64 = 6 * 3600 * TIMESCALE as i64;
/// Most cameras in one export.
pub const MAX_CAMERAS: usize = 32;
/// Exports written at the same time; more get 429 until one finishes.
pub const MAX_RUNNING: usize = 2;
/// A hole in the recording longer than this is listed as a gap.
const GAP_DTS: i64 = TIMESCALE as i64;

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3, as ZIP uses it), slice-by-8
// ---------------------------------------------------------------------------

fn crc_tables() -> &'static [[u32; 256]; 8] {
    static T: std::sync::OnceLock<[[u32; 256]; 8]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for (i, e) in t[0].iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        for i in 0..256 {
            for k in 1..8 {
                t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xff) as usize];
            }
        }
        t
    })
}

#[derive(Clone)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Crc32(!0)
    }
}

impl Crc32 {
    pub fn update(&mut self, mut data: &[u8]) {
        let t = crc_tables();
        let mut c = self.0;
        while data.len() >= 8 {
            let lo = c ^ u32::from_le_bytes(data[0..4].try_into().unwrap());
            let hi = u32::from_le_bytes(data[4..8].try_into().unwrap());
            c = t[7][(lo & 0xff) as usize]
                ^ t[6][((lo >> 8) & 0xff) as usize]
                ^ t[5][((lo >> 16) & 0xff) as usize]
                ^ t[4][(lo >> 24) as usize]
                ^ t[3][(hi & 0xff) as usize]
                ^ t[2][((hi >> 8) & 0xff) as usize]
                ^ t[1][((hi >> 16) & 0xff) as usize]
                ^ t[0][(hi >> 24) as usize];
            data = &data[8..];
        }
        for &b in data {
            c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }
    pub fn value(&self) -> u32 {
        !self.0
    }
}

// ---------------------------------------------------------------------------
// streaming ZIP writer (stored entries, data descriptors, ZIP64 when needed)
// ---------------------------------------------------------------------------

/// A finished entry: what the central directory and the manifest need.
#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub crc: u32,
    pub sha256: String,
    zip64: bool,
}

struct Current {
    name: String,
    offset: u64,
    zip64: bool,
    size: u64,
    crc: Crc32,
    sha: Sha256,
}

/// ZIP archive written front to back to any `Write`. In a dry run nothing
/// is hashed and media bytes are only counted ([`Zip::skip`]), so the
/// archive's exact length is known before anything is read from disk.
pub struct Zip<W: Write> {
    w: W,
    pos: u64,
    dos: (u16, u16),
    done: Vec<ZipEntry>,
    cur: Option<Current>,
    dry: bool,
    /// entries at least this large get ZIP64 local records (tests lower it)
    pub zip64_threshold: u64,
}

const U32_MAX: u64 = 0xFFFF_FFFF;

impl<W: Write> Zip<W> {
    /// `when` (local time) is stamped on every entry.
    pub fn new(w: W, when: chrono::NaiveDateTime, dry: bool) -> Self {
        use chrono::{Datelike, Timelike};
        let date = (((when.year() - 1980).clamp(0, 127) as u16) << 9) | ((when.month() as u16) << 5) | when.day() as u16;
        let time = ((when.hour() as u16) << 11) | ((when.minute() as u16) << 5) | (when.second() as u16 / 2);
        Zip { w, pos: 0, dos: (time, date), done: Vec::new(), cur: None, dry, zip64_threshold: 0xF000_0000 }
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    /// Start an entry of (at most about) `size_hint` bytes; `None` = unknown.
    pub fn start(&mut self, name: &str, size_hint: Option<u64>) -> io::Result<()> {
        assert!(self.cur.is_none(), "entry still open");
        let zip64 = size_hint.is_none_or(|s| s >= self.zip64_threshold);
        let mut h = Vec::with_capacity(30 + name.len() + 20);
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&(if zip64 { 45u16 } else { 20u16 }).to_le_bytes());
        h.extend_from_slice(&0x0808u16.to_le_bytes()); // data descriptor follows | UTF-8 names
        h.extend_from_slice(&0u16.to_le_bytes()); // stored
        h.extend_from_slice(&self.dos.0.to_le_bytes());
        h.extend_from_slice(&self.dos.1.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes()); // crc: in the descriptor
        let sz: u32 = if zip64 { u32::MAX } else { 0 };
        h.extend_from_slice(&sz.to_le_bytes());
        h.extend_from_slice(&sz.to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(if zip64 { 20u16 } else { 0u16 }).to_le_bytes());
        h.extend_from_slice(name.as_bytes());
        if zip64 {
            h.extend_from_slice(&0x0001u16.to_le_bytes());
            h.extend_from_slice(&16u16.to_le_bytes());
            h.extend_from_slice(&0u64.to_le_bytes());
            h.extend_from_slice(&0u64.to_le_bytes());
        }
        let offset = self.pos;
        self.put(&h)?;
        self.cur = Some(Current { name: name.to_string(), offset, zip64, size: 0, crc: Crc32::default(), sha: Sha256::new() });
        Ok(())
    }

    /// Entry data.
    pub fn data(&mut self, b: &[u8]) -> io::Result<()> {
        let dry = self.dry;
        let c = self.cur.as_mut().expect("no entry open");
        if !dry {
            c.crc.update(b);
            c.sha.update(b);
        }
        c.size += b.len() as u64;
        self.put(b)
    }

    /// Dry run: account for `n` entry bytes without having them.
    pub fn skip(&mut self, n: u64) {
        assert!(self.dry, "skip is for dry runs");
        self.cur.as_mut().expect("no entry open").size += n;
        self.pos += n;
    }

    /// Close the entry (data descriptor).
    pub fn end(&mut self) -> io::Result<ZipEntry> {
        let c = self.cur.take().expect("no entry open");
        let crc = if self.dry { 0 } else { c.crc.value() };
        let sha256 = if self.dry { "0".repeat(64) } else { hex::encode(c.sha.finalize()) };
        let mut d = Vec::with_capacity(24);
        d.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        d.extend_from_slice(&crc.to_le_bytes());
        if c.zip64 {
            d.extend_from_slice(&c.size.to_le_bytes());
            d.extend_from_slice(&c.size.to_le_bytes());
        } else {
            if c.size > U32_MAX {
                return Err(io::Error::other(format!("{} grew past 4 GB without ZIP64", c.name)));
            }
            d.extend_from_slice(&(c.size as u32).to_le_bytes());
            d.extend_from_slice(&(c.size as u32).to_le_bytes());
        }
        self.put(&d)?;
        let e = ZipEntry { name: c.name, offset: c.offset, size: c.size, crc, sha256, zip64: c.zip64 };
        self.done.push(e.clone());
        Ok(e)
    }

    /// Central directory and end records. Returns the writer and the
    /// archive's total length.
    pub fn finish(mut self) -> io::Result<(W, u64)> {
        assert!(self.cur.is_none(), "entry still open");
        let cd_start = self.pos;
        let entries = std::mem::take(&mut self.done);
        for e in &entries {
            let big_size = e.size >= U32_MAX;
            let big_off = e.offset >= U32_MAX;
            let mut extra = Vec::new();
            if big_size || big_off {
                let mut body = Vec::new();
                if big_size {
                    body.extend_from_slice(&e.size.to_le_bytes());
                    body.extend_from_slice(&e.size.to_le_bytes());
                }
                if big_off {
                    body.extend_from_slice(&e.offset.to_le_bytes());
                }
                extra.extend_from_slice(&0x0001u16.to_le_bytes());
                extra.extend_from_slice(&(body.len() as u16).to_le_bytes());
                extra.extend_from_slice(&body);
            }
            let need = if e.zip64 || big_size || big_off { 45u16 } else { 20u16 };
            let mut h = Vec::with_capacity(46 + e.name.len() + extra.len());
            h.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            h.extend_from_slice(&need.to_le_bytes()); // made by
            h.extend_from_slice(&need.to_le_bytes()); // needed
            h.extend_from_slice(&0x0808u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&self.dos.0.to_le_bytes());
            h.extend_from_slice(&self.dos.1.to_le_bytes());
            h.extend_from_slice(&e.crc.to_le_bytes());
            let sz = if big_size { u32::MAX } else { e.size as u32 };
            h.extend_from_slice(&sz.to_le_bytes());
            h.extend_from_slice(&sz.to_le_bytes());
            h.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // comment
            h.extend_from_slice(&0u16.to_le_bytes()); // disk
            h.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            h.extend_from_slice(&0u32.to_le_bytes()); // external attributes
            h.extend_from_slice(&(if big_off { u32::MAX } else { e.offset as u32 }).to_le_bytes());
            h.extend_from_slice(e.name.as_bytes());
            h.extend_from_slice(&extra);
            self.put(&h)?;
        }
        let cd_len = self.pos - cd_start;
        let n = entries.len() as u64;
        if n >= 0xFFFF || cd_start >= U32_MAX || cd_len >= U32_MAX {
            let eocd64_at = self.pos;
            let mut r = Vec::with_capacity(56 + 20);
            r.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
            r.extend_from_slice(&44u64.to_le_bytes());
            r.extend_from_slice(&45u16.to_le_bytes());
            r.extend_from_slice(&45u16.to_le_bytes());
            r.extend_from_slice(&0u32.to_le_bytes());
            r.extend_from_slice(&0u32.to_le_bytes());
            r.extend_from_slice(&n.to_le_bytes());
            r.extend_from_slice(&n.to_le_bytes());
            r.extend_from_slice(&cd_len.to_le_bytes());
            r.extend_from_slice(&cd_start.to_le_bytes());
            r.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
            r.extend_from_slice(&0u32.to_le_bytes());
            r.extend_from_slice(&eocd64_at.to_le_bytes());
            r.extend_from_slice(&1u32.to_le_bytes());
            self.put(&r)?;
        }
        let mut r = Vec::with_capacity(22);
        r.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        let n16 = n.min(0xFFFF) as u16;
        r.extend_from_slice(&n16.to_le_bytes());
        r.extend_from_slice(&n16.to_le_bytes());
        r.extend_from_slice(&(cd_len.min(U32_MAX) as u32).to_le_bytes());
        r.extend_from_slice(&(cd_start.min(U32_MAX) as u32).to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        self.put(&r)?;
        self.w.flush()?;
        Ok((self.w, self.pos))
    }
}

// ---------------------------------------------------------------------------
// planning
// ---------------------------------------------------------------------------

/// A byte range of a segment file copied into the export as it is.
#[derive(Clone, Debug)]
struct Run {
    path: PathBuf,
    offset: u64,
    len: u64,
}

enum Body {
    /// progressive MP4: header, then sample data runs
    Progressive { head: bytes::Bytes, runs: Vec<Run> },
    /// fragmented MP4 through the range planner (timeline shifted to 0)
    Fragmented(crate::video::RangePlan),
}

pub struct FilePlan {
    pub name: String,
    pub camera_id: i64,
    pub camera: String,
    pub stream: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// "mp4" (progressive) or "fragmented mp4"
    pub format: &'static str,
    /// exact size in bytes when known in advance
    pub size: Option<u64>,
    pub frames: u64,
    pub first_dts: i64,
    pub end_dts: i64,
    pub gaps: Vec<(i64, i64)>,
    body: Body,
}

pub struct Missing {
    pub camera_id: i64,
    pub camera: String,
    pub reason: String,
}

pub struct ExportPlan {
    pub start_dts: i64,
    pub end_dts: i64,
    pub stream: String,
    pub files: Vec<FilePlan>,
    pub missing: Vec<Missing>,
}

impl ExportPlan {
    /// Every file's size is known: the archive length can be computed.
    pub fn exact(&self) -> bool {
        self.files.iter().all(|f| f.size.is_some())
    }
}

/// Characters some file systems refuse in names.
fn safe_name(s: &str) -> String {
    let t: String = s.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).collect();
    let t = t.trim().trim_matches('.').to_string();
    if t.is_empty() { "camera".into() } else { t }
}

pub fn local(dts: i64) -> chrono::DateTime<chrono::Local> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(dts / 90).unwrap_or_default().with_timezone(&chrono::Local)
}
fn utc(dts: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(dts / 90).unwrap_or_default()
}

/// Holes in a run of fragments longer than [`GAP_DTS`], as (start, end).
fn gaps_of(frags: &[(PathBuf, FragEntry)]) -> Vec<(i64, i64)> {
    frags.windows(2).filter_map(|w| {
        let end = w[0].1.dts + w[0].1.duration as i64;
        (w[1].1.dts - end > GAP_DTS).then_some((end, w[1].1.dts))
    }).collect()
}

/// Read a fragment's `moof` and return its samples.
fn read_moof(file: &mut std::fs::File, frag: &FragEntry) -> Result<Vec<mp4::SampleInfo>> {
    let mut head = [0u8; 8];
    file.seek(SeekFrom::Start(frag.offset))?;
    file.read_exact(&mut head)?;
    let len = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
    if &head[4..8] != b"moof" || len < 8 || len > frag.len as usize {
        anyhow::bail!("fragment at {} is not a moof", frag.offset);
    }
    let mut moof = vec![0u8; len];
    moof[..8].copy_from_slice(&head);
    file.read_exact(&mut moof[8..])?;
    mp4::parse_moof_samples(&moof, frag.len as usize)
}

/// Progressive MP4 for a run of our own (video-only, one sample entry)
/// fragments: the sample table from each `moof`, a gap held on the frame
/// before it, one chunk per fragment.
fn progressive(params: &mp4::VideoParams, frags: &[(PathBuf, FragEntry)]) -> Result<(bytes::Bytes, Vec<Run>, u64)> {
    let mut t = mp4::SampleTable::default();
    let mut runs: Vec<Run> = Vec::new();
    let mut open: Option<(PathBuf, std::fs::File)> = None;
    let mut payload = 0u64;
    for (i, (path, f)) in frags.iter().enumerate() {
        if open.as_ref().is_none_or(|(p, _)| p != path) {
            open = Some((path.clone(), std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?));
        }
        let samples = read_moof(&mut open.as_mut().unwrap().1, f)?;
        if samples.is_empty() {
            continue;
        }
        t.chunks.push((samples.len() as u32, payload));
        let first_sample = t.durations.len();
        for s in &samples {
            t.durations.push(s.duration);
            t.sizes.push(s.size);
            t.keys.push(s.is_key);
            let at = f.offset + s.offset as u64;
            match runs.last_mut() {
                Some(r) if r.path == *path && r.offset + r.len == at => r.len += s.size as u64,
                _ => runs.push(Run { path: path.clone(), offset: at, len: s.size as u64 }),
            }
            payload += s.size as u64;
        }
        // hold the last frame until the next fragment: playback time stays wall-clock time
        if let Some((_, next)) = frags.get(i + 1) {
            let played: i64 = t.durations[first_sample..].iter().map(|&d| d as i64).sum();
            let hole = next.dts - (f.dts + played);
            if hole > 0 {
                let last = t.durations.last_mut().unwrap();
                *last = last.saturating_add(hole.min(u32::MAX as i64) as u32);
            }
        }
    }
    if t.durations.is_empty() {
        anyhow::bail!("no samples");
    }
    let created = frags[0].1.dts / TIMESCALE as i64;
    let head = mp4::progressive_head(params, &t, created);
    let size = head.len() as u64 + payload;
    Ok((head, runs, size))
}

/// Consecutive fragments of one camera recorded with the same codec
/// settings; a change of settings inside the range starts a new one, and
/// each becomes its own file.
struct Stretch {
    entry: i64,
    frags: Vec<(PathBuf, FragEntry)>,
    dts_offsets: Vec<i64>,
    legacy: bool,
}

/// Plan an export of `cams` (`stream` "main" or "sub") over [start, end).
/// Reads only segment indexes and fragment headers. One file per camera, or
/// one per stretch of unchanged codec settings ("part 1 of 2", ...).
pub fn plan(db: &Db, cams: &[Camera], stream: &str, start: i64, end: i64) -> Result<ExportPlan> {
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for cam in cams {
        let mut stretches: Vec<Stretch> = Vec::new();
        for seg in &db.segments_in_range_stream(cam.id, stream, start, end)? {
            let Some(st) = db.storage(seg.storage_id)? else { continue };
            let path = PathBuf::from(&st.path).join(&seg.path);
            let frags: Vec<FragEntry> = crate::video::overlapping(seg, start, end).cloned().collect();
            if frags.is_empty() {
                continue;
            }
            if stretches.last().is_none_or(|s| s.entry != seg.sample_entry_id) {
                stretches.push(Stretch { entry: seg.sample_entry_id, frags: Vec::new(), dts_offsets: Vec::new(), legacy: false });
            }
            let s = stretches.last_mut().unwrap();
            s.legacy |= seg.dts_offset != 0;
            for f in frags {
                s.frags.push((path.clone(), f));
                s.dts_offsets.push(seg.dts_offset);
            }
        }
        if stretches.is_empty() {
            missing.push(Missing { camera_id: cam.id, camera: cam.name.clone(), reason: "no recording in this range".into() });
            continue;
        }
        let parts = stretches.len();
        for (i, s) in stretches.into_iter().enumerate() {
            let Some(se) = db.sample_entry(s.entry)? else {
                let (a, b) = (s.frags[0].1.dts, s.frags.last().map(|(_, f)| f.dts + f.duration as i64).unwrap_or_default());
                let reason = format!("codec settings missing from the index for {} to {}", iso_local(a), iso_local(b));
                missing.push(Missing { camera_id: cam.id, camera: cam.name.clone(), reason });
                continue;
            };
            files.push(file_plan(cam, stream, &se.video_params(), s, (parts > 1).then_some((i + 1, parts))));
        }
    }
    unique_names(&mut files);
    Ok(ExportPlan { start_dts: start, end_dts: end, stream: stream.to_string(), files, missing })
}

fn file_plan(cam: &Camera, stream: &str, params: &mp4::VideoParams, s: Stretch, part: Option<(usize, usize)>) -> FilePlan {
    let first_dts = s.frags[0].1.dts;
    let end_dts = s.frags.last().map(|(_, f)| f.dts + f.duration as i64).unwrap_or(first_dts);
    let frames: u64 = s.frags.iter().map(|(_, f)| f.samples as u64).sum();
    let gaps = gaps_of(&s.frags);
    let (a, b) = (local(first_dts), local(end_dts));
    let name = format!(
        "{} {}-{}{}{}.mp4",
        safe_name(&cam.name),
        a.format("%Y-%m-%d %H%M%S"),
        b.format("%H%M%S"),
        if stream == "sub" { " sub" } else { "" },
        part.map(|(i, n)| format!(" part {i} of {n}")).unwrap_or_default()
    );
    let simple = !s.legacy && params.audio.is_none();
    let (format, size, body) = match simple.then(|| progressive(params, &s.frags)) {
        Some(Ok((head, runs, size))) => ("mp4", Some(size), Body::Progressive { head, runs }),
        other => {
            if let Some(Err(e)) = other {
                tracing::warn!(camera = cam.id, "export: progressive MP4 not possible ({e:#}); exporting fragmented MP4");
            }
            let p = fragmented(params, &s);
            ("fragmented mp4", p.exact_len.then_some(p.bytes), Body::Fragmented(p))
        }
    };
    FilePlan {
        name, camera_id: cam.id, camera: cam.name.clone(), stream: stream.to_string(), codec: params.rfc6381.clone(),
        width: params.width, height: params.height, format, size, frames, first_dts, end_dts, gaps, body,
    }
}

/// Fragmented MP4 of one stretch (legacy fragments, or audio): its one init
/// segment and the fragments as recorded, the timeline shifted to start at 0.
fn fragmented(params: &mp4::VideoParams, s: &Stretch) -> crate::video::RangePlan {
    use crate::video::RangeItem;
    let init = mp4::init_segment(params);
    let audio_rate = params.audio.as_ref().map(|a| a.sample_rate);
    let mut bytes = init.len() as u64;
    let mut items = vec![RangeItem::Init(init)];
    for ((path, f), &dts_offset) in s.frags.iter().zip(&s.dts_offsets) {
        bytes += f.len as u64;
        items.push(RangeItem::Frag { path: path.clone(), offset: f.offset, len: f.len, dts: f.dts, duration: f.duration, dts_offset, audio_rate });
    }
    let first = s.frags[0].1.dts;
    let last = s.frags.last().map(|(_, f)| f.dts + f.duration as i64);
    crate::video::RangePlan { items, bytes, exact_len: !s.legacy, tfdt_shift: -first, first_dts: Some(first), last_end_dts: last, mime: Some(params.mime()) }
}

/// Make file names unique within the archive, ignoring case (Windows and
/// macOS unpack "Back/Yard" and "back_yard" to the same file): a later
/// duplicate gets " (camera N)", then a counter.
fn unique_names(files: &mut [FilePlan]) {
    let mut seen = std::collections::HashSet::new();
    for f in files.iter_mut() {
        if seen.insert(f.name.to_lowercase()) {
            continue;
        }
        let stem = f.name.strip_suffix(".mp4").unwrap_or(&f.name).to_string();
        let mut name = format!("{stem} (camera {}).mp4", f.camera_id);
        let mut k = 2;
        while !seen.insert(name.to_lowercase()) {
            name = format!("{stem} (camera {} {k}).mp4", f.camera_id);
            k += 1;
        }
        f.name = name;
    }
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

/// Who exports, from which server, when (fixed before the dry run so the
/// dry run and the real one produce the same bytes).
pub struct ExportInfo {
    pub user: String,
    pub server: String,
    pub timezone: String,
    pub at: chrono::DateTime<chrono::Local>,
}

fn iso_local(dts: i64) -> String {
    local(dts).to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}
fn iso_utc(dts: i64) -> String {
    utc(dts).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Gaps as JSON, the same in the API preview and the manifest.
pub fn gaps_json(gaps: &[(i64, i64)]) -> Vec<Value> {
    gaps.iter().map(|(a, b)| json!({"start": iso_local(*a), "end": iso_local(*b), "start_ms": a / 90, "end_ms": b / 90, "seconds": ((b - a) as f64 / TIMESCALE as f64 * 10.0).round() / 10.0})).collect()
}

fn copy_run(z: &mut Zip<impl Write>, file: &mut std::fs::File, run: &Run, buf: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(run.offset))?;
    let mut left = run.len;
    while left > 0 {
        let n = (left as usize).min(buf.len());
        file.read_exact(&mut buf[..n]).with_context(|| format!("reading {}", run.path.display()))?;
        z.data(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

/// Write the archive for `plan` to `w` (a dry run only counts). Returns the
/// archive length.
pub fn write(plan: &ExportPlan, info: &ExportInfo, w: impl Write, dry: bool) -> Result<u64> {
    let mut z = Zip::new(w, info.at.naive_local(), dry);
    let mut listed = Vec::new();
    let mut buf = vec![0u8; if dry { 0 } else { 1 << 20 }];
    for f in &plan.files {
        z.start(&f.name, f.size)?;
        match &f.body {
            Body::Progressive { head, runs } => {
                z.data(head)?;
                let mut open: Option<(PathBuf, std::fs::File)> = None;
                for r in runs {
                    if dry {
                        z.skip(r.len);
                        continue;
                    }
                    if open.as_ref().is_none_or(|(p, _)| *p != r.path) {
                        open = Some((r.path.clone(), std::fs::File::open(&r.path).with_context(|| format!("opening {}", r.path.display()))?));
                    }
                    copy_run(&mut z, &mut open.as_mut().unwrap().1, r, &mut buf)?;
                }
            }
            Body::Fragmented(p) => {
                let mut open: Option<(PathBuf, std::fs::File)> = None;
                for item in &p.items {
                    match item {
                        crate::video::RangeItem::Init(b) => z.data(b)?,
                        crate::video::RangeItem::Frag { path, offset, len, dts_offset, audio_rate, .. } => {
                            if dry {
                                z.skip(*len as u64);
                                continue;
                            }
                            if open.as_ref().is_none_or(|(op, _)| op != path) {
                                open = Some((path.clone(), std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?));
                            }
                            let file = &mut open.as_mut().unwrap().1;
                            let mut frag = vec![0u8; *len as usize];
                            file.seek(SeekFrom::Start(*offset))?;
                            file.read_exact(&mut frag)?;
                            z.data(&mp4::rebase_fragment_av(&frag, dts_offset + p.tfdt_shift, *audio_rate))?;
                        }
                    }
                }
            }
        }
        let e = z.end()?;
        listed.push(json!({
            "file": f.name, "camera_id": f.camera_id, "camera": f.camera, "stream": f.stream, "format": f.format,
            "codec": f.codec, "width": f.width, "height": f.height, "frames": f.frames,
            "first_frame": iso_local(f.first_dts), "first_frame_utc": iso_utc(f.first_dts),
            "end": iso_local(f.end_dts), "end_utc": iso_utc(f.end_dts),
            "gaps": gaps_json(&f.gaps), "bytes": e.size, "sha256": e.sha256,
        }));
    }
    let manifest = json!({
        "export": "zmng evidence export",
        "exported_at": info.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "exported_by": info.user,
        "server": info.server,
        "timezone": info.timezone,
        "range": {"start": iso_local(plan.start_dts), "end": iso_local(plan.end_dts), "start_utc": iso_utc(plan.start_dts), "end_utc": iso_utc(plan.end_dts), "stream": plan.stream},
        "files": listed,
        "missing": plan.missing.iter().map(|m| json!({"camera_id": m.camera_id, "camera": m.camera, "reason": m.reason})).collect::<Vec<_>>(),
        "notes": [
            "Video is copied from the recording as the camera encoded it (no re-encoding).",
            "Times are the server clock; first_frame can be up to one keyframe interval before the range start.",
            "A gap is a time without recording; the frame before it is held so playback time matches wall-clock time."
        ],
    });
    let manifest = serde_json::to_vec_pretty(&manifest)?;
    z.start("manifest.json", Some(manifest.len() as u64))?;
    z.data(&manifest)?;
    let m = z.end()?;
    let mut sums = String::new();
    for (f, e) in plan.files.iter().zip(listed.iter()) {
        sums.push_str(&format!("{}  {}\n", e["sha256"].as_str().unwrap_or_default(), f.name));
    }
    sums.push_str(&format!("{}  manifest.json\n", m.sha256));
    z.start("SHA256SUMS", Some(sums.len() as u64))?;
    z.data(sums.as_bytes())?;
    z.end()?;
    let mut readme = format!(
        "Video export from {}\r\nRange: {} to {} ({})\r\nExported by {} at {}\r\n\r\n",
        info.server, iso_local(plan.start_dts), iso_local(plan.end_dts), info.timezone, info.user,
        info.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    );
    for f in &plan.files {
        let gaps = if f.gaps.is_empty() { String::new() } else { format!(", {} gap(s) without recording", f.gaps.len()) };
        readme.push_str(&format!("  {}  ({} {}x{}{})\r\n", f.name, f.camera, f.width, f.height, gaps));
    }
    for m in &plan.missing {
        readme.push_str(&format!("  not included: {} ({})\r\n", m.camera, m.reason));
    }
    readme.push_str(
        "\r\nThe .mp4 files play in VLC, Windows Media Player, QuickTime and web browsers.\r\n\
         manifest.json lists each file's cameras, times (local and UTC), gaps and SHA-256.\r\n\
         To check the files are unchanged: sha256sum -c SHA256SUMS (Linux/macOS),\r\n\
         or Get-FileHash -Algorithm SHA256 <file> in Windows PowerShell.\r\n",
    );
    z.start("README.txt", Some(readme.len() as u64))?;
    z.data(readme.as_bytes())?;
    z.end()?;
    Ok(z.finish()?.1)
}

/// Length of the archive `write` will produce (dry run), or None when a
/// file's size is only known once it is written.
pub fn length(plan: &ExportPlan, info: &ExportInfo) -> Result<Option<u64>> {
    if !plan.exact() {
        return Ok(None);
    }
    Ok(Some(write(plan, info, Counter(0), true)?))
}

struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0 += b.len() as u64;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A `Write` that hands ~512 KB chunks to an async response body.
pub struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, io::Error>>,
    buf: Vec<u8>,
}

impl ChannelWriter {
    pub fn new(tx: tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, io::Error>>) -> Self {
        ChannelWriter { tx, buf: Vec::with_capacity(512 << 10) }
    }
    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = bytes::Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(512 << 10)));
        self.tx.blocking_send(Ok(chunk)).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "download cancelled"))
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        if self.buf.len() >= 512 << 10 {
            self.send()?;
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        let mut c = Crc32::default();
        c.update(b"123456789");
        assert_eq!(c.value(), 0xCBF4_3926);
        // split updates and the slice-by-8 path agree with byte-at-a-time
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut a = Crc32::default();
        a.update(&data);
        let mut b = Crc32::default();
        for chunk in data.chunks(3) {
            b.update(chunk);
        }
        assert_eq!(a.value(), b.value());
    }

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("Front/Door: east?"), "Front_Door_ east_");
        assert_eq!(safe_name(" .. "), "camera");
        assert_eq!(safe_name("Church Hall"), "Church Hall");
    }

    /// A small archive and one with ZIP64 records forced on: both must read
    /// back with every CRC right (checked by Python's zipfile, when present).
    #[test]
    fn zip_reads_back_with_and_without_zip64() {
        for threshold in [u64::MAX, 0] {
            let mut z = Zip::new(Vec::new(), chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap().and_hms_opt(14, 30, 0).unwrap(), false);
            z.zip64_threshold = threshold;
            z.start("a.txt", Some(5)).unwrap();
            z.data(b"hello").unwrap();
            let a = z.end().unwrap();
            assert_eq!(a.sha256, hex::encode(Sha256::digest(b"hello")));
            z.start("Kamera ü.mp4", None).unwrap();
            z.data(&vec![7u8; 100_000]).unwrap();
            z.end().unwrap();
            let (bytes, total) = z.finish().unwrap();
            assert_eq!(total, bytes.len() as u64);
            // dry run produces the same length
            let mut d = Zip::new(Counter(0), chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap().and_hms_opt(14, 30, 0).unwrap(), true);
            d.zip64_threshold = threshold;
            d.start("a.txt", Some(5)).unwrap();
            d.skip(5);
            d.end().unwrap();
            d.start("Kamera ü.mp4", None).unwrap();
            d.skip(100_000);
            d.end().unwrap();
            assert_eq!(d.finish().unwrap().1, bytes.len() as u64);
            let dir = std::env::temp_dir().join(format!("zmng-zip-test-{}-{threshold}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("t.zip");
            std::fs::write(&p, &bytes).unwrap();
            let py = std::process::Command::new("python3")
                .args(["-c", "import sys,zipfile; z=zipfile.ZipFile(sys.argv[1]); assert z.testzip() is None; print(','.join(f'{i.filename}:{i.file_size}' for i in z.infolist()))"])
                .arg(&p)
                .output();
            if let Ok(out) = py {
                assert!(out.status.success(), "python zipfile: {}", String::from_utf8_lossy(&out.stderr));
                assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "a.txt:5,Kamera ü.mp4:100000");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
