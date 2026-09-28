//! Minimal fragmented-MP4 (fMP4 / ISO BMFF) writer and reader for video-only
//! passthrough recording.
//!
//! Design (see docs/ARCHITECTURE.md):
//! * Every segment file is a self-contained fMP4: `ftyp` + `moov` (the *init
//!   segment*) followed by one `moof`+`mdat` pair per GOP (one *fragment*).
//! * All decode timestamps (`tfdt`) are absolute: 90 kHz ticks since the Unix
//!   epoch.  That makes fragments from different files directly concatenable,
//!   lets HLS `EXT-X-BYTERANGE` playlists point straight into the files, and
//!   means the server never has to rewrite a box to serve a time range.
//! * No B-frames are assumed (dts == pts), which is true for every IP camera
//!   we care about (Hikvision/Dahua/Axis/Reolink/Amcrest/UniFi).  A stream that
//!   violates this is logged and recorded anyway; playback will still work,
//!   just with slightly wrong per-frame timing.
//!
//! The writer produces bytes into a `Vec<u8>` so the caller controls I/O.

use bytes::{BufMut, Bytes, BytesMut};
use std::fmt;

pub const TIMESCALE: u32 = 90_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Codec {
    H264,
    H265,
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Codec::H264 => write!(f, "h264"),
            Codec::H265 => write!(f, "h265"),
        }
    }
}

impl Codec {
    pub fn parse(s: &str) -> Option<Codec> {
        match s {
            "h264" | "avc" | "avc1" => Some(Codec::H264),
            "h265" | "hevc" | "hvc1" | "hev1" => Some(Codec::H265),
            _ => None,
        }
    }
}

/// Everything needed to build an init segment for a track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoParams {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// A complete `VisualSampleEntry` box (`avc1`/`hvc1` ... including the
    /// `avcC`/`hvcC` child), as produced by retina's
    /// `VideoParameters::mp4_sample_entry().build()`.
    pub sample_entry: Bytes,
    /// RFC 6381 codec string, e.g. `avc1.640028` or `hvc1.1.6.L153.B0`.
    pub rfc6381: String,
    /// AAC audio track recorded alongside (opt-in per camera).
    pub audio: Option<AudioParams>,
}

impl VideoParams {
    /// MIME type for MSE, listing the audio codec too when present.
    pub fn mime(&self) -> String {
        match &self.audio {
            Some(a) => format!("video/mp4; codecs=\"{},{}\"", self.rfc6381, a.rfc6381),
            None => format!("video/mp4; codecs=\"{}\"", self.rfc6381),
        }
    }
}

// ---------------------------------------------------------------------------
// Box building helpers
// ---------------------------------------------------------------------------

struct BoxWriter {
    buf: BytesMut,
    stack: Vec<usize>,
}

impl BoxWriter {
    fn new() -> Self {
        BoxWriter { buf: BytesMut::with_capacity(4096), stack: Vec::new() }
    }
    fn open(&mut self, fourcc: &[u8; 4]) {
        self.stack.push(self.buf.len());
        self.buf.put_u32(0);
        self.buf.put_slice(fourcc);
    }
    fn open_full(&mut self, fourcc: &[u8; 4], version: u8, flags: u32) {
        self.open(fourcc);
        self.buf.put_u32(((version as u32) << 24) | (flags & 0x00ff_ffff));
    }
    fn close(&mut self) {
        let start = self.stack.pop().expect("unbalanced box");
        let len = (self.buf.len() - start) as u32;
        self.buf[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }
    fn finish(self) -> Bytes {
        assert!(self.stack.is_empty());
        self.buf.freeze()
    }
}

/// AAC audio track parameters (opt-in audio passthrough).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioParams {
    /// A complete `mp4a` sample entry box (with `esds`).
    pub sample_entry: Bytes,
    /// Track timescale = sample rate (AAC: 1024 samples per frame).
    pub sample_rate: u32,
    pub channels: u16,
    /// e.g. `mp4a.40.2`
    pub rfc6381: String,
}

/// Build an `mp4a` sample entry with an `esds` carrying the given
/// AudioSpecificConfig (2 bytes for AAC-LC).
pub fn aac_sample_entry(sample_rate: u32, channels: u16, asc: &[u8]) -> Bytes {
    let mut w = BoxWriter::new();
    w.open(b"mp4a");
    w.buf.put_slice(&[0u8; 6]); // reserved
    w.buf.put_u16(1); // data_reference_index
    w.buf.put_slice(&[0u8; 8]); // reserved
    w.buf.put_u16(channels);
    w.buf.put_u16(16); // sample size
    w.buf.put_u32(0); // pre_defined + reserved
    w.buf.put_u32(sample_rate << 16);
    // esds: ES_Descriptor(3) > DecoderConfigDescriptor(4) > DecoderSpecificInfo(5), SLConfigDescriptor(6)
    w.open_full(b"esds", 0, 0);
    let dsi_len = asc.len();
    let dcd_len = 13 + 2 + dsi_len;
    let es_len = 3 + 2 + dcd_len + 3;
    w.buf.put_u8(0x03);
    w.buf.put_u8(es_len as u8);
    w.buf.put_u16(1); // ES_ID
    w.buf.put_u8(0); // flags
    w.buf.put_u8(0x04);
    w.buf.put_u8(dcd_len as u8);
    w.buf.put_u8(0x40); // objectTypeIndication: Audio ISO/IEC 14496-3
    w.buf.put_u8(0x15); // streamType audio (5 << 2) | reserved 1
    w.buf.put_slice(&[0, 0, 0]); // bufferSizeDB
    w.buf.put_u32(0); // maxBitrate
    w.buf.put_u32(0); // avgBitrate
    w.buf.put_u8(0x05);
    w.buf.put_u8(dsi_len as u8);
    w.buf.put_slice(asc);
    w.buf.put_u8(0x06);
    w.buf.put_u8(1);
    w.buf.put_u8(0x02);
    w.close();
    w.close();
    w.finish()
}

/// Build the init segment (`ftyp` + `moov`): the video track and, when
/// `p.audio` is set, the AAC track.
pub fn init_segment(p: &VideoParams) -> Bytes {
    init_segment_av(p, p.audio.as_ref())
}

/// Init segment for a video track (id 1) and, optionally, an AAC audio
/// track (id 2).
pub fn init_segment_av(p: &VideoParams, audio: Option<&AudioParams>) -> Bytes {
    let mut w = BoxWriter::new();
    // ftyp
    w.open(b"ftyp");
    w.buf.put_slice(b"iso5");
    w.buf.put_u32(512);
    for b in [b"iso5", b"iso6", b"mp41", b"dash"] {
        w.buf.put_slice(b);
    }
    w.close();

    w.open(b"moov");
    {
        // mvhd
        w.open_full(b"mvhd", 0, 0);
        w.buf.put_u32(0); // creation_time
        w.buf.put_u32(0); // modification_time
        w.buf.put_u32(TIMESCALE);
        w.buf.put_u32(0); // duration (unknown for fragmented)
        w.buf.put_u32(0x0001_0000); // rate 1.0
        w.buf.put_u16(0x0100); // volume
        w.buf.put_u16(0);
        w.buf.put_u64(0);
        for v in UNITY_MATRIX {
            w.buf.put_u32(v);
        }
        w.buf.put_slice(&[0u8; 24]); // pre_defined
        w.buf.put_u32(if audio.is_some() { 3 } else { 2 }); // next_track_id
        w.close();

        // trak
        w.open(b"trak");
        {
            w.open_full(b"tkhd", 0, 0x7); // enabled | in_movie | in_preview
            w.buf.put_u32(0);
            w.buf.put_u32(0);
            w.buf.put_u32(1); // track id
            w.buf.put_u32(0);
            w.buf.put_u32(0); // duration
            w.buf.put_u64(0);
            w.buf.put_u16(0); // layer
            w.buf.put_u16(0); // alternate group
            w.buf.put_u16(0); // volume
            w.buf.put_u16(0);
            for v in UNITY_MATRIX {
                w.buf.put_u32(v);
            }
            w.buf.put_u32(p.width << 16);
            w.buf.put_u32(p.height << 16);
            w.close();

            w.open(b"mdia");
            {
                w.open_full(b"mdhd", 0, 0);
                w.buf.put_u32(0);
                w.buf.put_u32(0);
                w.buf.put_u32(TIMESCALE);
                w.buf.put_u32(0);
                w.buf.put_u16(0x55c4); // 'und'
                w.buf.put_u16(0);
                w.close();

                w.open_full(b"hdlr", 0, 0);
                w.buf.put_u32(0);
                w.buf.put_slice(b"vide");
                w.buf.put_slice(&[0u8; 12]);
                w.buf.put_slice(b"VideoHandler\0");
                w.close();

                w.open(b"minf");
                {
                    w.open_full(b"vmhd", 0, 1);
                    w.buf.put_u64(0);
                    w.close();

                    w.open(b"dinf");
                    w.open_full(b"dref", 0, 0);
                    w.buf.put_u32(1);
                    w.open_full(b"url ", 0, 1);
                    w.close();
                    w.close();
                    w.close();

                    w.open(b"stbl");
                    {
                        w.open_full(b"stsd", 0, 0);
                        w.buf.put_u32(1);
                        w.buf.put_slice(&p.sample_entry);
                        w.close();

                        for b in [b"stts", b"stsc", b"stsz", b"stco"] {
                            w.open_full(b, 0, 0);
                            if b == b"stsz" {
                                w.buf.put_u32(0); // sample_size
                            }
                            w.buf.put_u32(0); // entry_count
                            w.close();
                        }
                    }
                    w.close(); // stbl
                }
                w.close(); // minf
            }
            w.close(); // mdia
        }
        w.close(); // trak

        if let Some(a) = audio {
            w.open(b"trak");
            {
                w.open_full(b"tkhd", 0, 0x7);
                w.buf.put_u32(0);
                w.buf.put_u32(0);
                w.buf.put_u32(2); // track id
                w.buf.put_u32(0);
                w.buf.put_u32(0);
                w.buf.put_u64(0);
                w.buf.put_u16(0);
                w.buf.put_u16(1); // alternate group
                w.buf.put_u16(0x0100); // volume
                w.buf.put_u16(0);
                for v in UNITY_MATRIX {
                    w.buf.put_u32(v);
                }
                w.buf.put_u32(0);
                w.buf.put_u32(0);
                w.close();
                w.open(b"mdia");
                {
                    w.open_full(b"mdhd", 0, 0);
                    w.buf.put_u32(0);
                    w.buf.put_u32(0);
                    w.buf.put_u32(a.sample_rate);
                    w.buf.put_u32(0);
                    w.buf.put_u16(0x55c4);
                    w.buf.put_u16(0);
                    w.close();
                    w.open_full(b"hdlr", 0, 0);
                    w.buf.put_u32(0);
                    w.buf.put_slice(b"soun");
                    w.buf.put_slice(&[0u8; 12]);
                    w.buf.put_slice(b"SoundHandler\0");
                    w.close();
                    w.open(b"minf");
                    {
                        w.open_full(b"smhd", 0, 0);
                        w.buf.put_u32(0);
                        w.close();
                        w.open(b"dinf");
                        w.open_full(b"dref", 0, 0);
                        w.buf.put_u32(1);
                        w.open_full(b"url ", 0, 1);
                        w.close();
                        w.close();
                        w.close();
                        w.open(b"stbl");
                        {
                            w.open_full(b"stsd", 0, 0);
                            w.buf.put_u32(1);
                            w.buf.put_slice(&a.sample_entry);
                            w.close();
                            for b in [b"stts", b"stsc", b"stsz", b"stco"] {
                                w.open_full(b, 0, 0);
                                if b == b"stsz" {
                                    w.buf.put_u32(0);
                                }
                                w.buf.put_u32(0);
                                w.close();
                            }
                        }
                        w.close();
                    }
                    w.close();
                }
                w.close();
            }
            w.close(); // audio trak
        }

        // mvex
        w.open(b"mvex");
        for track in 1..=(if audio.is_some() { 2 } else { 1 }) {
            w.open_full(b"trex", 0, 0);
            w.buf.put_u32(track); // track id
            w.buf.put_u32(1); // default sample description index
            w.buf.put_u32(0);
            w.buf.put_u32(0);
            w.buf.put_u32(0);
            w.close();
        }
        w.close();
    }
    w.close(); // moov
    w.finish()
}

const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// One access unit queued into a fragment.
pub struct Sample {
    /// Absolute decode time, 90 kHz ticks since Unix epoch.
    pub dts: i64,
    pub duration: u32,
    pub is_key: bool,
    /// AVCC/HVCC style: 4-byte big-endian length prefixed NAL units.
    pub data: Bytes,
}

/// Build a `moof` + `mdat` for a list of samples (a GOP).
pub fn fragment(seq: u32, samples: &[Sample]) -> Bytes {
    fragment_av(seq, samples, &[])
}

/// One `moof` with a `traf` per track (video = track 1, audio = track 2,
/// audio sample dts/durations in the audio timescale) and one `mdat`
/// holding the video payloads followed by the audio payloads.
pub fn fragment_av(seq: u32, video: &[Sample], audio: &[Sample]) -> Bytes {
    if audio.is_empty() {
        return fragment_one(seq, video);
    }
    let traf_len = |n: usize| 8 + 16 + 20 + (12 + 4 + 4 + 12 * n);
    let moof_len = 8 + 16 + traf_len(video.len()) + traf_len(audio.len());
    let vbytes: usize = video.iter().map(|s| s.data.len()).sum();
    let abytes: usize = audio.iter().map(|s| s.data.len()).sum();
    let mut w = BoxWriter::new();
    w.buf.reserve(moof_len + 8 + vbytes + abytes);
    w.open(b"moof");
    {
        w.open_full(b"mfhd", 0, 0);
        w.buf.put_u32(seq);
        w.close();
        for (track, samples, data_offset) in [(1u32, video, moof_len + 8), (2u32, audio, moof_len + 8 + vbytes)] {
            w.open(b"traf");
            {
                w.open_full(b"tfhd", 0, 0x0002_0000);
                w.buf.put_u32(track);
                w.close();
                w.open_full(b"tfdt", 1, 0);
                w.buf.put_u64(samples.first().map(|s| s.dts).unwrap_or(0) as u64);
                w.close();
                w.open_full(b"trun", 0, 0x1 | 0x100 | 0x200 | 0x400);
                w.buf.put_u32(samples.len() as u32);
                w.buf.put_u32(data_offset as u32);
                for s in samples {
                    w.buf.put_u32(s.duration);
                    w.buf.put_u32(s.data.len() as u32);
                    w.buf.put_u32(if s.is_key { 0x0200_0000 } else { 0x0101_0000 });
                }
                w.close();
            }
            w.close();
        }
    }
    w.close();
    debug_assert_eq!(w.buf.len(), moof_len);
    w.buf.put_u32((vbytes + abytes + 8) as u32);
    w.buf.put_slice(b"mdat");
    for s in video.iter().chain(audio.iter()) {
        w.buf.put_slice(&s.data);
    }
    w.finish()
}

fn fragment_one(seq: u32, samples: &[Sample]) -> Bytes {
    let base_dts = samples.first().map(|s| s.dts).unwrap_or(0);
    let mdat_payload: usize = samples.iter().map(|s| s.data.len()).sum();
    // Box sizes are fully determined by the sample count, so the trun
    // data_offset can be computed up front (moof size + 8-byte mdat header).
    let trun_len = 12 + 4 + 4 + 12 * samples.len();
    let traf_len = 8 + 16 + 20 + trun_len;
    let moof_len = 8 + 16 + traf_len;

    let mut w = BoxWriter::new();
    w.buf.reserve(moof_len + 8 + mdat_payload);
    w.open(b"moof");
    {
        w.open_full(b"mfhd", 0, 0);
        w.buf.put_u32(seq);
        w.close();

        w.open(b"traf");
        {
            // tfhd: default-base-is-moof (0x20000)
            w.open_full(b"tfhd", 0, 0x0002_0000);
            w.buf.put_u32(1);
            w.close();

            w.open_full(b"tfdt", 1, 0);
            w.buf.put_u64(base_dts as u64);
            w.close();

            // trun: data-offset | sample-duration | sample-size | sample-flags
            w.open_full(b"trun", 0, 0x1 | 0x100 | 0x200 | 0x400);
            w.buf.put_u32(samples.len() as u32);
            w.buf.put_u32((moof_len + 8) as u32);
            for s in samples {
                w.buf.put_u32(s.duration);
                w.buf.put_u32(s.data.len() as u32);
                w.buf.put_u32(if s.is_key { 0x0200_0000 } else { 0x0101_0000 });
            }
            w.close();
        }
        w.close(); // traf
    }
    w.close(); // moof
    debug_assert_eq!(w.buf.len(), moof_len);

    // mdat
    w.buf.put_u32((mdat_payload + 8) as u32);
    w.buf.put_slice(b"mdat");
    for s in samples {
        w.buf.put_slice(&s.data);
    }
    w.finish()
}

// ---------------------------------------------------------------------------
// Reading: scan an fMP4 file and recover the fragment index.
// ---------------------------------------------------------------------------

/// Index entry for one fragment inside a segment file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FragEntry {
    /// Absolute dts of the first sample (90 kHz since epoch).
    pub dts: i64,
    /// Sum of sample durations.
    pub duration: u32,
    /// Byte offset of `moof` in the file.
    pub offset: u64,
    /// Length of `moof` + `mdat`.
    pub len: u32,
    pub samples: u32,
    /// 0..=255 motion score for the fragment (filled in by the detector).
    pub motion: u8,
}

pub const FRAG_ENTRY_BYTES: usize = 8 + 4 + 8 + 4 + 4 + 1 + 3; // 32, padded

/// Serialize the fragment index into a compact blob (32 bytes/entry).
pub fn encode_index(entries: &[FragEntry]) -> Vec<u8> {
    let mut v = Vec::with_capacity(entries.len() * FRAG_ENTRY_BYTES);
    for e in entries {
        v.extend_from_slice(&e.dts.to_le_bytes());
        v.extend_from_slice(&e.duration.to_le_bytes());
        v.extend_from_slice(&e.offset.to_le_bytes());
        v.extend_from_slice(&e.len.to_le_bytes());
        v.extend_from_slice(&e.samples.to_le_bytes());
        v.push(e.motion);
        v.extend_from_slice(&[0u8; 3]);
    }
    v
}

pub fn decode_index(blob: &[u8]) -> Vec<FragEntry> {
    blob.chunks_exact(FRAG_ENTRY_BYTES)
        .map(|c| FragEntry {
            dts: i64::from_le_bytes(c[0..8].try_into().unwrap()),
            duration: u32::from_le_bytes(c[8..12].try_into().unwrap()),
            offset: u64::from_le_bytes(c[12..20].try_into().unwrap()),
            len: u32::from_le_bytes(c[20..24].try_into().unwrap()),
            samples: u32::from_le_bytes(c[24..28].try_into().unwrap()),
            motion: c[28],
        })
        .collect()
}

/// Result of scanning a segment file.
#[derive(Debug)]
pub struct ScannedSegment {
    pub init_len: u32,
    pub frags: Vec<FragEntry>,
}

/// Scan the top-level boxes of an fMP4 written by [`fragment`] and recover the
/// index. Used to re-adopt orphan files after a crash. Tolerates a truncated
/// trailing fragment (it is simply dropped).
pub fn scan_segment(data: &[u8]) -> anyhow::Result<ScannedSegment> {
    let mut pos = 0usize;
    let mut init_len: Option<u32> = None;
    let mut frags = Vec::new();
    let mut pending_moof: Option<(u64, i64, u32, u32)> = None; // offset, dts, duration, samples
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let typ = &data[pos + 4..pos + 8];
        if size < 8 || pos + size > data.len() {
            break; // truncated
        }
        match typ {
            b"ftyp" => {}
            b"moov" => init_len = Some((pos + size) as u32),
            b"moof" => {
                let (dts, dur, n) = parse_moof(&data[pos..pos + size])?;
                pending_moof = Some((pos as u64, dts, dur, n));
            }
            b"mdat" => {
                if let Some((off, dts, dur, n)) = pending_moof.take() {
                    frags.push(FragEntry {
                        dts,
                        duration: dur,
                        offset: off,
                        len: ((pos + size) as u64 - off) as u32,
                        samples: n,
                        motion: 0,
                    });
                }
            }
            _ => {}
        }
        pos += size;
    }
    let init_len = init_len.ok_or_else(|| anyhow::anyhow!("no moov box"))?;
    Ok(ScannedSegment { init_len, frags })
}

/// Scan a segment file without loading `mdat` payloads: reads box headers and
/// `moof` boxes only, seeking over everything else.  Works for our own files
/// and for ZoneMinder's `frag_keyframe+empty_moov` event files.
pub fn scan_segment_file(path: &std::path::Path) -> anyhow::Result<ScannedSegment> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut pos = 0u64;
    let mut init_len: Option<u32> = None;
    let mut frags = Vec::new();
    let mut pending_moof: Option<(u64, i64, u32, u32)> = None;
    let mut hdr = [0u8; 16];
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        if f.read_exact(&mut hdr[..8]).is_err() {
            break;
        }
        let mut size = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
        let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let mut hdr_len = 8u64;
        if size == 1 {
            f.read_exact(&mut hdr[8..16])?;
            size = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
            hdr_len = 16;
        } else if size == 0 {
            size = len - pos; // box extends to EOF
        }
        if size < hdr_len || pos + size > len {
            break; // truncated trailing box
        }
        match &typ {
            b"moov" => {
                init_len = Some((pos + size) as u32);
            }
            b"moof" => {
                if size > 16 << 20 {
                    anyhow::bail!("moof too large ({size} bytes)");
                }
                let mut buf = vec![0u8; size as usize];
                f.seek(SeekFrom::Start(pos))?;
                f.read_exact(&mut buf)?;
                let (dts, dur, n) = parse_moof(&buf)?;
                pending_moof = Some((pos, dts, dur, n));
            }
            b"mdat" => {
                if let Some((off, dts, dur, n)) = pending_moof.take() {
                    frags.push(FragEntry { dts, duration: dur, offset: off, len: (pos + size - off) as u32, samples: n, motion: 0 });
                }
            }
            _ => {}
        }
        pos += size;
    }
    let init_len = init_len.ok_or_else(|| anyhow::anyhow!("no moov box"))?;
    Ok(ScannedSegment { init_len, frags })
}

/// Add `offset` to the `tfdt` baseMediaDecodeTime of every `traf` in a
/// `moof`+`mdat` fragment buffer (in place).  Used to serve imported
/// ZoneMinder files, whose timestamps start near zero, on our absolute
/// timeline.  Returns false if no tfdt was found.
pub fn shift_tfdt(frag: &mut [u8], offset: i64) -> bool {
    if offset == 0 {
        return true;
    }
    if frag.len() < 8 || &frag[4..8] != b"moof" {
        return false;
    }
    let moof_len = u32::from_be_bytes(frag[0..4].try_into().unwrap()) as usize;
    if moof_len > frag.len() {
        return false;
    }
    let mut found = false;
    let mut pos = 8;
    while pos + 8 <= moof_len {
        let size = u32::from_be_bytes(frag[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > moof_len {
            break;
        }
        if &frag[pos + 4..pos + 8] == b"traf" {
            let mut p = pos + 8;
            let end = pos + size;
            while p + 8 <= end {
                let s = u32::from_be_bytes(frag[p..p + 4].try_into().unwrap()) as usize;
                if s < 8 || p + s > end {
                    break;
                }
                if &frag[p + 4..p + 8] == b"tfdt" {
                    let version = frag[p + 8];
                    if version == 1 && s >= 20 {
                        let old = u64::from_be_bytes(frag[p + 12..p + 20].try_into().unwrap()) as i64;
                        frag[p + 12..p + 20].copy_from_slice(&((old + offset) as u64).to_be_bytes());
                        found = true;
                    } else if version == 0 && s >= 16 {
                        // upgrade in place is impossible (box would grow); ZoneMinder/ffmpeg
                        // write version 1 for fragmented output, so this is rare
                        let old = u32::from_be_bytes(frag[p + 12..p + 16].try_into().unwrap()) as i64;
                        let new = old + offset;
                        if new < 0 || new > u32::MAX as i64 {
                            return false;
                        }
                        frag[p + 12..p + 16].copy_from_slice(&(new as u32).to_be_bytes());
                        found = true;
                    }
                }
                p += s;
            }
        }
        pos += size;
    }
    found
}

/// Make a `moof`+`mdat` fragment self-contained and absolute-timed:
/// * `tfhd` with `base-data-offset-present` (what ffmpeg/ZoneMinder write:
///   an absolute file offset) is converted to `default-base-is-moof`, which
///   is required for MSE appends and for concatenating fragments from
///   different files; the box shrinks by 8 bytes and `trun.data_offset` is
///   adjusted;
/// * `tfdt` is shifted by `dts_offset`.
/// Our own files already satisfy both, so this is a cheap pass-through for
/// them.  Returns the rewritten fragment.
pub fn rebase_fragment(frag: &[u8], dts_offset: i64) -> Vec<u8> {
    let mut out = frag.to_vec();
    if out.len() < 8 || &out[4..8] != b"moof" {
        return out;
    }
    let moof_len = u32::from_be_bytes(out[0..4].try_into().unwrap()) as usize;
    if moof_len > out.len() {
        return out;
    }
    // locate traf boxes; process each: (traf_pos, traf_len)
    let mut pos = 8;
    let mut removed_total = 0usize;
    while pos + 8 <= moof_len - removed_total {
        let size = u32::from_be_bytes(out[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > moof_len - removed_total {
            break;
        }
        if &out[pos + 4..pos + 8] == b"traf" {
            let mut removed_here = 0usize;
            let mut p = pos + 8;
            let mut end = pos + size;
            let mut trun_positions: Vec<usize> = Vec::new();
            while p + 8 <= end {
                let s = u32::from_be_bytes(out[p..p + 4].try_into().unwrap()) as usize;
                if s < 8 || p + s > end {
                    break;
                }
                match &out[p + 4..p + 8] {
                    b"tfhd" => {
                        let flags = u32::from_be_bytes(out[p + 8..p + 12].try_into().unwrap()) & 0xff_ffff;
                        if flags & 0x1 != 0 && s >= 24 {
                            // drop the 8-byte base_data_offset (right after track_ID at p+12..16)
                            out.drain(p + 16..p + 24);
                            let new_flags = (flags & !0x1) | 0x2_0000;
                            out[p + 8..p + 12].copy_from_slice(&new_flags.to_be_bytes());
                            out[p..p + 4].copy_from_slice(&((s - 8) as u32).to_be_bytes());
                            removed_here += 8;
                            end -= 8;
                            p += s - 8;
                            continue;
                        } else if flags & 0x2_0000 == 0 {
                            // neither base offset nor default-base-is-moof: set the flag (base = moof)
                            out[p + 8..p + 12].copy_from_slice(&(flags | 0x2_0000).to_be_bytes());
                        }
                    }
                    b"tfdt" => {
                        if dts_offset != 0 {
                            let version = out[p + 8];
                            if version == 1 && s >= 20 {
                                let old = u64::from_be_bytes(out[p + 12..p + 20].try_into().unwrap()) as i64;
                                out[p + 12..p + 20].copy_from_slice(&((old + dts_offset) as u64).to_be_bytes());
                            } else if version == 0 && s >= 16 {
                                let old = u32::from_be_bytes(out[p + 12..p + 16].try_into().unwrap()) as i64;
                                let new = (old + dts_offset).clamp(0, u32::MAX as i64);
                                out[p + 12..p + 16].copy_from_slice(&(new as u32).to_be_bytes());
                            }
                        }
                    }
                    b"trun" => trun_positions.push(p),
                    _ => {}
                }
                p += s;
            }
            if removed_here > 0 {
                // fix traf size and every trun data_offset (relative to moof start)
                out[pos..pos + 4].copy_from_slice(&((size - removed_here) as u32).to_be_bytes());
                for tp in trun_positions {
                    let flags = u32::from_be_bytes(out[tp + 8..tp + 12].try_into().unwrap()) & 0xff_ffff;
                    if flags & 0x1 != 0 {
                        let off = i32::from_be_bytes(out[tp + 16..tp + 20].try_into().unwrap());
                        out[tp + 16..tp + 20].copy_from_slice(&(off - removed_here as i32).to_be_bytes());
                    }
                }
                removed_total += removed_here;
                pos += size - removed_here;
                continue;
            }
        }
        pos += size;
    }
    if removed_total > 0 {
        out[0..4].copy_from_slice(&((moof_len - removed_total) as u32).to_be_bytes());
    }
    out
}

/// Rewrite an `hev1` sample entry tag to `hvc1` (parameter sets are in
/// `hvcC` either way for ffmpeg-muxed files; Safari/HLS prefer `hvc1`).
pub fn prefer_hvc1(entry: &mut [u8]) {
    if entry.len() >= 8 && &entry[4..8] == b"hev1" {
        entry[4..8].copy_from_slice(b"hvc1");
    }
}

fn parse_moof(moof: &[u8]) -> anyhow::Result<(i64, u32, u32)> {
    // moof > traf > (tfhd, tfdt, trun); every read is bounds-checked so a
    // corrupt file cannot panic the daemon (reindex runs at boot).
    let rd32 = |b: &[u8], at: usize| -> anyhow::Result<u32> {
        b.get(at..at + 4).map(|x| u32::from_be_bytes(x.try_into().unwrap())).ok_or_else(|| anyhow::anyhow!("truncated box"))
    };
    let rd64 = |b: &[u8], at: usize| -> anyhow::Result<u64> {
        b.get(at..at + 8).map(|x| u64::from_be_bytes(x.try_into().unwrap())).ok_or_else(|| anyhow::anyhow!("truncated box"))
    };
    let mut dts = 0i64;
    let mut duration = 0u32;
    let mut samples = 0u32;
    let mut pos = 8;
    while pos + 8 <= moof.len() {
        let size = rd32(moof, pos)? as usize;
        let typ = &moof[pos + 4..pos + 8];
        if size < 8 || pos + size > moof.len() {
            anyhow::bail!("bad moof child");
        }
        if typ == b"traf" {
            let traf = &moof[pos..pos + size];
            let mut p = 8;
            let mut default_dur = 0u32;
            let mut skip = false; // an audio traf (track != 1) does not count
            while p + 8 <= traf.len() && !skip {
                let s = rd32(traf, p)? as usize;
                let t = &traf[p + 4..p + 8];
                if s < 8 || p + s > traf.len() {
                    anyhow::bail!("bad traf child");
                }
                match t {
                    b"tfhd" => {
                        let flags = rd32(traf, p + 8)? & 0xff_ffff;
                        if rd32(traf, p + 12)? != 1 {
                            skip = true;
                            continue;
                        }
                        let mut q = p + 16;
                        if flags & 0x1 != 0 { q += 8; }
                        if flags & 0x2 != 0 { q += 4; }
                        if flags & 0x8 != 0 { default_dur = rd32(traf, q)?; }
                    }
                    b"tfdt" => {
                        let version = *traf.get(p + 8).ok_or_else(|| anyhow::anyhow!("truncated tfdt"))?;
                        dts = if version == 1 { rd64(traf, p + 12)? as i64 } else { rd32(traf, p + 12)? as i64 };
                    }
                    b"trun" => {
                        let flags = rd32(traf, p + 8)? & 0xff_ffff;
                        let count = rd32(traf, p + 12)?;
                        samples += count;
                        let mut q = p + 16;
                        if flags & 0x1 != 0 {
                            q += 4;
                        }
                        if flags & 0x4 != 0 {
                            q += 4;
                        }
                        if flags & 0x100 == 0 {
                            duration = duration.saturating_add(default_dur.saturating_mul(count));
                        }
                        for _ in 0..count {
                            if flags & 0x100 != 0 {
                                duration = duration.saturating_add(rd32(traf, q)?);
                                q += 4;
                            }
                            if flags & 0x200 != 0 {
                                q += 4;
                            }
                            if flags & 0x400 != 0 {
                                q += 4;
                            }
                            if flags & 0x800 != 0 {
                                q += 4;
                            }
                        }
                    }
                    _ => {}
                }
                p += s;
            }
        }
        pos += size;
    }
    Ok((dts, duration, samples))
}

/// One sample as described by a fragment's `trun` (size, duration, keyframe
/// flag) plus its payload slice inside the fragment buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleInfo {
    pub duration: u32,
    pub size: u32,
    pub is_key: bool,
    /// Byte offset of the sample payload inside the fragment buffer.
    pub offset: usize,
}

/// Parse the per-sample table of a single-`traf` `moof`+`mdat` fragment
/// (ours or ffmpeg's `default_base_moof` files). Handles `tfhd` defaults and
/// every `trun` optional field. Bounds-checked: a corrupt fragment yields an
/// error, never a panic.
pub fn parse_fragment_samples(frag: &[u8]) -> anyhow::Result<Vec<SampleInfo>> {
    let rd32 = |b: &[u8], at: usize| -> anyhow::Result<u32> {
        b.get(at..at + 4).map(|x| u32::from_be_bytes(x.try_into().unwrap())).ok_or_else(|| anyhow::anyhow!("truncated box"))
    };
    if frag.len() < 8 || &frag[4..8] != b"moof" {
        anyhow::bail!("not a moof");
    }
    let moof_len = rd32(frag, 0)? as usize;
    if moof_len > frag.len() {
        anyhow::bail!("truncated moof");
    }
    let moof = &frag[..moof_len];
    let mut out = Vec::new();
    let mut pos = 8;
    while pos + 8 <= moof.len() {
        let size = rd32(moof, pos)? as usize;
        if size < 8 || pos + size > moof.len() {
            anyhow::bail!("bad moof child");
        }
        if &moof[pos + 4..pos + 8] == b"traf" {
            let traf = &moof[pos..pos + size];
            let (mut def_dur, mut def_size, mut def_flags) = (0u32, 0u32, 0u32);
            let mut base_is_moof = false;
            let mut p = 8;
            while p + 8 <= traf.len() {
                let s = rd32(traf, p)? as usize;
                if s < 8 || p + s > traf.len() {
                    anyhow::bail!("bad traf child");
                }
                match &traf[p + 4..p + 8] {
                    b"tfhd" => {
                        let flags = rd32(traf, p + 8)? & 0xff_ffff;
                        if rd32(traf, p + 12)? != 1 {
                            break; // audio traf: not part of the video sample table
                        }
                        base_is_moof = flags & 0x2_0000 != 0;
                        let mut q = p + 16;
                        if flags & 0x1 != 0 { q += 8; }
                        if flags & 0x2 != 0 { q += 4; }
                        if flags & 0x8 != 0 { def_dur = rd32(traf, q)?; q += 4; }
                        if flags & 0x10 != 0 { def_size = rd32(traf, q)?; q += 4; }
                        if flags & 0x20 != 0 { def_flags = rd32(traf, q)?; }
                    }
                    b"trun" => {
                        let flags = rd32(traf, p + 8)? & 0xff_ffff;
                        let count = rd32(traf, p + 12)? as usize;
                        let mut q = p + 16;
                        let mut data_offset = 0i64;
                        if flags & 0x1 != 0 {
                            data_offset = rd32(traf, q)? as i32 as i64;
                            q += 4;
                        }
                        let mut first_flags = None;
                        if flags & 0x4 != 0 {
                            first_flags = Some(rd32(traf, q)?);
                            q += 4;
                        }
                        if !base_is_moof {
                            anyhow::bail!("fragment does not use default-base-is-moof");
                        }
                        let mut off = data_offset;
                        for i in 0..count {
                            let dur = if flags & 0x100 != 0 { let v = rd32(traf, q)?; q += 4; v } else { def_dur };
                            let sz = if flags & 0x200 != 0 { let v = rd32(traf, q)?; q += 4; v } else { def_size };
                            let fl = if flags & 0x400 != 0 { let v = rd32(traf, q)?; q += 4; v } else if i == 0 { first_flags.unwrap_or(def_flags) } else { def_flags };
                            if flags & 0x800 != 0 { q += 4; }
                            // sample_is_non_sync_sample is bit 16; depends_on==2 (bits 24-25) also marks an I-frame
                            let is_key = fl & 0x0001_0000 == 0;
                            if off < 0 || off as usize + sz as usize > frag.len() {
                                anyhow::bail!("sample outside fragment");
                            }
                            out.push(SampleInfo { duration: dur, size: sz, is_key, offset: off as usize });
                            off += sz as i64;
                        }
                    }
                    _ => {}
                }
                p += s;
            }
        }
        pos += size;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> VideoParams {
        VideoParams {
            codec: Codec::H264,
            width: 640,
            height: 360,
            // a fake but structurally valid 'avc1' sample entry box (header + 78 bytes + avcC)
            sample_entry: {
                let mut v = Vec::new();
                let avcc: &[u8] = &[1, 0x64, 0, 0x1e, 0xff, 0xe1, 0, 0, 0x01, 0, 0, 0];
                let total = 8 + 78 + 8 + avcc.len();
                v.extend_from_slice(&(total as u32).to_be_bytes());
                v.extend_from_slice(b"avc1");
                v.extend_from_slice(&[0u8; 78]);
                v.extend_from_slice(&((8 + avcc.len()) as u32).to_be_bytes());
                v.extend_from_slice(b"avcC");
                v.extend_from_slice(avcc);
                Bytes::from(v)
            },
            rfc6381: "avc1.64001e".into(),
            audio: None,
        }
    }

    #[test]
    fn init_segment_is_well_formed() {
        let init = init_segment(&params());
        assert_eq!(&init[4..8], b"ftyp");
        let ftyp_len = u32::from_be_bytes(init[0..4].try_into().unwrap()) as usize;
        assert_eq!(&init[ftyp_len + 4..ftyp_len + 8], b"moov");
        let moov_len = u32::from_be_bytes(init[ftyp_len..ftyp_len + 4].try_into().unwrap()) as usize;
        assert_eq!(ftyp_len + moov_len, init.len());
    }

    #[test]
    fn rebase_converts_base_data_offset() {
        // build a fragment the ffmpeg way: tfhd flags 0x39 with base_data_offset
        let s = Sample { dts: 1000, duration: 3000, is_key: true, data: Bytes::from(vec![7u8; 10]) };
        let ours = fragment(1, &[s]);
        // hand-craft: moof{mfhd, traf{tfhd(v0, flags 0x39: track, base_data_offset, def_dur, def_size, def_flags), tfdt, trun}} mdat
        let mut w = BoxWriter::new();
        w.open(b"moof");
        w.open_full(b"mfhd", 0, 0); w.buf.put_u32(1); w.close();
        w.open(b"traf");
        w.open_full(b"tfhd", 0, 0x39); w.buf.put_u32(1); w.buf.put_u64(944); w.buf.put_u32(3000); w.buf.put_u32(10); w.buf.put_u32(0x0101_0000); w.close();
        w.open_full(b"tfdt", 1, 0); w.buf.put_u64(0); w.close();
        w.open_full(b"trun", 0, 0x1 | 0x4); w.buf.put_u32(1); let dpos = w.buf.len(); w.buf.put_u32(0); w.buf.put_u32(0x0200_0000); w.close();
        w.close();
        w.close();
        let moof_len = w.buf.len();
        w.buf[dpos..dpos + 4].copy_from_slice(&((moof_len + 8) as u32).to_be_bytes());
        w.buf.put_u32(18); w.buf.put_slice(b"mdat"); w.buf.put_slice(&[7u8; 10]);
        let ffm = w.finish();
        let rebased = rebase_fragment(&ffm, 5_000_000);
        assert_eq!(rebased.len(), ffm.len() - 8);
        let new_moof_len = u32::from_be_bytes(rebased[0..4].try_into().unwrap()) as usize;
        assert_eq!(new_moof_len, moof_len - 8);
        assert_eq!(&rebased[new_moof_len + 4..new_moof_len + 8], b"mdat");
        let (dts, _, n) = parse_moof(&rebased[..new_moof_len]).unwrap();
        assert_eq!(dts, 5_000_000);
        assert_eq!(n, 1);
        // trun data_offset must now point at the mdat payload
        let scanned = scan_segment(&[init_segment(&params()).as_ref(), &rebased].concat()).unwrap();
        assert_eq!(scanned.frags.len(), 1);
        // our own fragments pass through unchanged
        assert_eq!(rebase_fragment(&ours, 0), ours.to_vec());
    }

    #[test]
    fn shift_tfdt_moves_base_time() {
        let s = |dts: i64| Sample { dts, duration: 3000, is_key: true, data: Bytes::from(vec![1u8; 10]) };
        let mut f = fragment(1, &[s(1000)]).to_vec();
        assert!(shift_tfdt(&mut f, 5_000_000));
        let (dts, _, _) = parse_moof(&f[..u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize]).unwrap();
        assert_eq!(dts, 5_001_000);
    }

    #[test]
    fn fragment_roundtrip_via_scan() {
        let init = init_segment(&params());
        let s = |dts: i64, key: bool, n: usize| Sample {
            dts,
            duration: 3000,
            is_key: key,
            data: Bytes::from(vec![0xAB; n]),
        };
        let f1 = fragment(1, &[s(90_000, true, 100), s(93_000, false, 20), s(96_000, false, 30)]);
        let f2 = fragment(2, &[s(99_000, true, 100), s(102_000, false, 20)]);
        let mut file = Vec::new();
        file.extend_from_slice(&init);
        file.extend_from_slice(&f1);
        file.extend_from_slice(&f2);
        // add a truncated fragment
        file.extend_from_slice(&f1[..40]);
        let scanned = scan_segment(&file).unwrap();
        assert_eq!(scanned.init_len as usize, init.len());
        assert_eq!(scanned.frags.len(), 2);
        assert_eq!(scanned.frags[0].dts, 90_000);
        assert_eq!(scanned.frags[0].duration, 9000);
        assert_eq!(scanned.frags[0].samples, 3);
        assert_eq!(scanned.frags[0].offset as usize, init.len());
        assert_eq!(scanned.frags[0].len as usize, f1.len());
        assert_eq!(scanned.frags[1].dts, 99_000);
        assert_eq!(scanned.frags[1].offset as usize, init.len() + f1.len());
        // trun data_offset must point at first byte after the mdat header
        let moof_len = u32::from_be_bytes(f1[0..4].try_into().unwrap()) as usize;
        assert_eq!(&f1[moof_len + 4..moof_len + 8], b"mdat");
        let blob = encode_index(&scanned.frags);
        assert_eq!(decode_index(&blob), scanned.frags);
    }

    #[test]
    fn av_fragment_indexes_only_the_video_track() {
        let v = |dts: i64| Sample { dts, duration: 9000, is_key: true, data: Bytes::from(vec![1u8; 20]) };
        let a = |dts: i64| Sample { dts, duration: 1024, is_key: true, data: Bytes::from(vec![2u8; 8]) };
        let f = fragment_av(3, &[v(90_000), v(99_000)], &[a(48_000), a(49_024), a(50_048)]);
        let (dts, dur, n) = parse_moof(&f[..u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize]).unwrap();
        assert_eq!((dts, dur, n), (90_000, 18_000, 2));
        let samples = parse_fragment_samples(&f).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(&f[samples[0].offset..samples[0].offset + 20], &[1u8; 20]);
        // the audio payload follows the video payload inside the mdat
        assert_eq!(&f[samples[1].offset + 20..samples[1].offset + 28], &[2u8; 8]);
        let audio = AudioParams { sample_entry: aac_sample_entry(48_000, 1, &[0x11, 0x88]), sample_rate: 48_000, channels: 1, rfc6381: "mp4a.40.2".into() };
        let init = init_segment_av(&params(), Some(&audio));
        let (fourcc, _, _, _) = extract_sample_entry(&init).unwrap();
        assert_eq!(fourcc, "avc1", "video stays the first track");
        assert_eq!(VideoParams { audio: Some(audio.clone()), ..params() }.mime(), "video/mp4; codecs=\"avc1.64001e,mp4a.40.2\"");
        assert_eq!(extract_audio_entry(&init).map(|(e, r, c)| (e[4..8].to_vec(), r, c)), Some((b"mp4a".to_vec(), 48_000, 1)));
        assert!(extract_audio_entry(&init_segment(&params())).is_none());
        let scanned = scan_segment(&[init.as_ref(), &f].concat()).unwrap();
        assert_eq!(scanned.frags.len(), 1);
        assert_eq!(scanned.frags[0].samples, 2);
        assert_eq!(scanned.frags[0].duration, 18_000);
    }

    #[test]
    fn fragment_samples_roundtrip() {
        let s = |dts: i64, key: bool, n: usize| Sample { dts, duration: 3000, is_key: key, data: Bytes::from(vec![0xCD; n]) };
        let f = fragment(7, &[s(0, true, 50), s(3000, false, 10), s(6000, false, 20)]);
        let samples = parse_fragment_samples(&f).unwrap();
        assert_eq!(samples.len(), 3);
        assert!(samples[0].is_key && !samples[1].is_key && !samples[2].is_key);
        assert_eq!(samples.iter().map(|x| x.size).collect::<Vec<_>>(), vec![50, 10, 20]);
        assert_eq!(samples[0].duration, 3000);
        // offsets are contiguous inside the mdat payload
        assert_eq!(samples[1].offset, samples[0].offset + 50);
        assert_eq!(&f[samples[2].offset..samples[2].offset + 20], &[0xCD; 20]);
        assert!(parse_fragment_samples(&f[..40]).is_err());
    }
}

// ---------------------------------------------------------------------------
// Sample entry recovery (for re-adopting files)
// ---------------------------------------------------------------------------

fn find_box<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let mut cur = data;
    for (i, want) in path.iter().enumerate() {
        let mut pos = 0usize;
        let mut found = None;
        while pos + 8 <= cur.len() {
            let size = u32::from_be_bytes(cur[pos..pos + 4].try_into().ok()?) as usize;
            if size < 8 || pos + size > cur.len() {
                return None;
            }
            if &cur[pos + 4..pos + 8] == *want {
                found = Some(&cur[pos..pos + size]);
                break;
            }
            pos += size;
        }
        let b = found?;
        // full boxes (stsd) have a 4-byte version/flags + entry count before children
        cur = if i + 1 < path.len() {
            if *want == b"stsd" { &b[16..] } else { &b[8..] }
        } else {
            b
        };
    }
    Some(cur)
}

/// From an init segment, return (fourcc, whole sample entry box, width, height).
pub fn extract_sample_entry(init: &[u8]) -> Option<(String, Vec<u8>, u32, u32)> {
    let stsd = find_box(init, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"])?;
    // stsd: 8 header + 4 version/flags + 4 entry_count, then the entry box
    let entry = stsd.get(16..)?;
    let size = u32::from_be_bytes(entry.get(0..4)?.try_into().ok()?) as usize;
    if size < 86 || size > entry.len() {
        return None;
    }
    let fourcc = String::from_utf8_lossy(&entry[4..8]).to_string();
    let w = u16::from_be_bytes(entry[32..34].try_into().ok()?) as u32;
    let h = u16::from_be_bytes(entry[34..36].try_into().ok()?) as u32;
    Some((fourcc, entry[..size].to_vec(), w, h))
}

/// From an init segment with an audio track (the `trak` whose handler is
/// `soun`): (whole `mp4a` sample entry box, sample rate, channels).
pub fn extract_audio_entry(init: &[u8]) -> Option<(Vec<u8>, u32, u16)> {
    let moov = find_box(init, &[b"moov"])?;
    let mut pos = 8;
    while pos + 8 <= moov.len() {
        let size = u32::from_be_bytes(moov[pos..pos + 4].try_into().ok()?) as usize;
        if size < 8 || pos + size > moov.len() {
            return None;
        }
        if &moov[pos + 4..pos + 8] == b"trak" {
            let trak = &moov[pos..pos + size];
            let hdlr = find_box(trak, &[b"trak", b"mdia", b"hdlr"]);
            if hdlr.map(|h| h.len() >= 20 && &h[16..20] == b"soun").unwrap_or(false) {
                let stsd = find_box(trak, &[b"trak", b"mdia", b"minf", b"stbl", b"stsd"])?;
                let entry = stsd.get(16..)?;
                let esize = u32::from_be_bytes(entry.get(0..4)?.try_into().ok()?) as usize;
                if esize < 36 || esize > entry.len() {
                    return None;
                }
                let channels = u16::from_be_bytes(entry[24..26].try_into().ok()?);
                let rate = u32::from_be_bytes(entry[32..36].try_into().ok()?) >> 16;
                return Some((entry[..esize].to_vec(), rate, channels));
            }
        }
        pos += size;
    }
    None
}

/// Derive the RFC 6381 codec string from a sample entry (avcC / hvcC).
pub fn rfc6381_from_sample_entry(codec: Codec, entry: &[u8]) -> Option<String> {
    let body = entry.get(86..)?; // children after the 78-byte VisualSampleEntry fields
    let mut pos = 0;
    while pos + 8 <= body.len() {
        let size = u32::from_be_bytes(body[pos..pos + 4].try_into().ok()?) as usize;
        let typ = &body[pos + 4..pos + 8];
        if size < 8 || pos + size > body.len() {
            return None;
        }
        let payload = &body[pos + 8..pos + size];
        match (codec, typ) {
            (Codec::H264, b"avcC") if payload.len() >= 4 => {
                return Some(format!("avc1.{:02X}{:02X}{:02X}", payload[1], payload[2], payload[3]));
            }
            (Codec::H265, b"hvcC") if payload.len() >= 13 => {
                let profile_space = payload[1] >> 6;
                let tier = (payload[1] >> 5) & 1;
                let profile_idc = payload[1] & 0x1f;
                let compat = u32::from_be_bytes(payload[2..6].try_into().ok()?);
                let compat_rev = compat.reverse_bits();
                let level = payload[12];
                let space = match profile_space { 1 => "A", 2 => "B", 3 => "C", _ => "" };
                let constraint = payload[6..12].iter().rev().skip_while(|b| **b == 0).collect::<Vec<_>>();
                let mut s = format!("hvc1.{space}{profile_idc}.{compat_rev:X}.{}{level}", if tier == 1 { "H" } else { "L" });
                for b in constraint.iter().rev() {
                    s.push_str(&format!(".{b:X}"));
                }
                return Some(s);
            }
            _ => {}
        }
        pos += size;
    }
    None
}

#[cfg(test)]
mod tests_extract {
    use super::*;

    #[test]
    fn hevc_rfc6381() {
        // hvcC: configurationVersion=1, profile_space=0 tier=0 idc=1, compat=0x60000000, constraints 90 00 00 00 00 00, level=153
        let mut entry = vec![0u8; 86];
        entry[4..8].copy_from_slice(b"hvc1");
        let hvcc = [1u8, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 153, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        entry.extend_from_slice(&((8 + hvcc.len()) as u32).to_be_bytes());
        entry.extend_from_slice(b"hvcC");
        entry.extend_from_slice(&hvcc);
        let total = entry.len() as u32;
        entry[0..4].copy_from_slice(&total.to_be_bytes());
        assert_eq!(rfc6381_from_sample_entry(Codec::H265, &entry).unwrap(), "hvc1.1.6.L153.90");
    }
}
