//! Audio passthrough: an AAC track in our fMP4 alongside the video decodes
//! with ffprobe, indexes as video-only fragments, and serves as a range.
mod common;
use common::*;
use bytes::Bytes;
use zmng::mp4::{self, TIMESCALE};

/// AAC-LC frames (raw, ADTS headers stripped) plus the AudioSpecificConfig.
pub fn aac_frames(secs: u32) -> (Vec<Bytes>, [u8; 2], u32) {
    let tmp = tempfile::NamedTempFile::with_suffix(".aac").unwrap();
    let st = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", &secs.to_string(), "-c:a", "aac", "-b:a", "64k", "-f", "adts"])
        .arg(tmp.path())
        .status()
        .unwrap();
    assert!(st.success());
    let d = std::fs::read(tmp.path()).unwrap();
    let mut frames = Vec::new();
    let mut i = 0;
    while i + 7 <= d.len() {
        let hdr = &d[i..i + 7];
        assert_eq!(hdr[0], 0xFF);
        let protection_absent = hdr[1] & 1 == 1;
        let len = (((hdr[3] & 3) as usize) << 11) | ((hdr[4] as usize) << 3) | ((hdr[5] as usize) >> 5);
        let hl = if protection_absent { 7 } else { 9 };
        frames.push(Bytes::copy_from_slice(&d[i + hl..i + len]));
        i += len;
    }
    // AAC-LC (object type 2), sample-rate index 3 (48 kHz), 1 channel
    (frames, [0x11, 0x88], 48_000)
}

#[tokio::test]
async fn av_segment_decodes_and_serves() {
    let fx = Fixture::new();
    let cam = fx.add_camera("talker");
    let enc = parse_encoded(&encode_fmp4(3, "libx264", 320, 180));
    let (frames, asc, rate) = aac_frames(3);
    let audio = mp4::AudioParams { sample_entry: mp4::aac_sample_entry(rate, 1, &asc), sample_rate: rate, channels: 1, rfc6381: "mp4a.40.2".into() };
    let mut vp = enc.params.clone();
    vp.audio = Some(audio.clone());
    let base = 1_800_000_000i64 * TIMESCALE as i64;
    // write the file the way the recorder does: one fragment per GOP, audio frames of that second riding along
    let init = mp4::init_segment(&vp);
    let se = fx.db.intern_sample_entry(vp.codec, &vp.rfc6381, vp.width, vp.height, &vp.sample_entry, vp.audio.as_ref()).unwrap();
    let mut file = init.to_vec();
    let mut frags = Vec::new();
    let mut vdts = base;
    let mut seq = 0;
    let per_gop = 10usize; // 10 fps, GOP 10
    let frames_per_sec = rate as usize / 1024; // ~46 AAC frames per second
    for (g, chunk) in enc.samples.chunks(per_gop).enumerate() {
        let gop: Vec<mp4::Sample> = chunk.iter().map(|(dur, key, data)| { let s = mp4::Sample { dts: vdts, duration: *dur, is_key: *key, data: data.clone() }; vdts += *dur as i64; s }).collect();
        // the audio frames whose time falls inside this GOP's second, like the recorder buckets them
        let base_a = (base as i128 * rate as i128 / TIMESCALE as i128) as i64;
        let asamples: Vec<mp4::Sample> = frames.iter().enumerate()
            .filter(|(i, _)| (i * 1024) / rate as usize == g)
            .map(|(i, f)| mp4::Sample { dts: base_a + (i * 1024) as i64, duration: 1024, is_key: true, data: f.clone() })
            .collect();
        seq += 1;
        let frag = mp4::fragment_av(seq, &gop, &asamples);
        let dur: u32 = gop.iter().map(|s| s.duration).sum();
        frags.push(mp4::FragEntry { dts: gop[0].dts, duration: dur, offset: file.len() as u64, len: frag.len() as u32, samples: gop.len() as u32, motion: 0 });
        file.extend_from_slice(&frag);
    }
    let rel = format!("{cam}/20270115/080000.mp4");
    let abs = fx.storage_path().join(&rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(&abs, &file).unwrap();
    fx.db.insert_segment_ext(cam, fx.storage_id, se, &rel, file.len() as i64, init.len() as u32, &frags, 0, None, "main").unwrap();
    // the file itself: two streams, both decodable
    let probe = ffprobe_json(&abs);
    let streams = probe["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 2, "{probe}");
    assert_eq!(streams[0]["codec_type"], "video");
    assert_eq!(streams[1]["codec_type"], "audio");
    assert_eq!(streams[1]["codec_name"], "aac");
    assert_eq!(streams[1]["sample_rate"], "48000");
    assert_eq!(count_frames(&file), 30);
    assert!(count_audio_frames(&file) >= 3 * frames_per_sec - 2);
    // the scanner indexes video fragments only, matching what we wrote
    let scanned = mp4::scan_segment_file(&abs).unwrap();
    assert_eq!(scanned.frags.len(), frags.len());
    assert_eq!(scanned.frags[1].samples, 10);
    assert_eq!(scanned.frags[1].duration, 90_000);
    // adoption recovers the audio entry from the file's moov
    let (entry, r, ch) = mp4::extract_audio_entry(&init).expect("audio entry");
    assert_eq!((r, ch), (48_000, 1));
    assert_eq!(&entry[4..8], b"mp4a");
    // a served range regenerates an init with both tracks and the right mime
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    let res = get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}", base / 90 + 1000, base / 90 + 3000), &tok).await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.header("content-type").unwrap(), "video/mp4; codecs=\"avc1.64001e,mp4a.40.2\"".replace("64001e", &vp.rfc6381[5..]));
    assert_eq!(count_frames(&res.body), 20);
    assert!(count_audio_frames(&res.body) >= 2 * frames_per_sec - 2);
    // a re-adopted file keeps its audio track in the regenerated init
    let seg = fx.db.segments_in_range(cam, base, base + 1).unwrap()[0].clone();
    fx.db.delete_segment(seg.id).unwrap();
    assert_eq!(zmng::retention::reindex(&fx.db).unwrap(), 1);
    let res = get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}", base / 90, base / 90 + 1000), &tok).await;
    assert!(res.header("content-type").unwrap().contains("mp4a.40.2"));
    assert!(count_audio_frames(&res.body) >= frames_per_sec - 2);
    // an event download is re-stamped to start at t=0: the audio track's
    // tfdt is shifted in ITS timescale, so the tracks stay in sync
    // (ffprobe normalizes each stream's start, so the tfdt values are read directly)
    let ev = fx.db.insert_event(cam, base + TIMESCALE as i64, "motion").unwrap();
    fx.db.close_event(ev, base + 3 * TIMESCALE as i64, None, "{}").unwrap();
    let dl = call(&r, "GET", &format!("/zm/index.php?view=view_video&eid={ev}&token={tok}"), None, None).await;
    assert_eq!(dl.status, 200, "{}", dl.text());
    let tfdts = tfdt_pairs(&dl.body);
    assert_eq!(tfdts.len(), 2, "{tfdts:?}");
    assert_eq!(tfdts[0].0, 0, "video starts at t=0");
    for (v, a) in &tfdts {
        let (vs, as_) = (*v as f64 / TIMESCALE as f64, *a as f64 / rate as f64);
        assert!((vs - as_).abs() < 0.05, "audio tfdt {as_:.3}s drifted from video {vs:.3}s");
    }
    let out = fx.storage_path().join("event.mp4");
    std::fs::write(&out, &dl.body).unwrap();
    assert_eq!(ffprobe_json(&out)["streams"].as_array().unwrap().len(), 2);
    assert!(count_audio_frames(&dl.body) >= 2 * frames_per_sec - 2);
}

/// (video tfdt, audio tfdt) of every moof in a fragmented MP4, in order.
fn tfdt_pairs(mp4: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut cur: Vec<u64> = Vec::new();
    let mut i = 0;
    while i + 8 <= mp4.len() {
        let size = u32::from_be_bytes(mp4[i..i + 4].try_into().unwrap()) as usize;
        let kind = &mp4[i + 4..i + 8];
        if kind == b"moof" || kind == b"traf" {
            if kind == b"moof" && cur.len() == 2 { out.push((cur[0], cur[1])); cur.clear(); }
            i += 8; // descend
            continue;
        }
        if kind == b"tfdt" && mp4[i + 8] == 1 {
            cur.push(u64::from_be_bytes(mp4[i + 12..i + 20].try_into().unwrap()));
        }
        i += size.max(8);
    }
    if cur.len() == 2 { out.push((cur[0], cur[1])); }
    out
}

fn count_audio_frames(mp4: &[u8]) -> usize {
    use std::io::Write;
    let mut child = std::process::Command::new("ffprobe")
        .args(["-hide_banner", "-loglevel", "error", "-f", "mp4", "-i", "pipe:0", "-select_streams", "a:0", "-count_frames", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(mp4).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}
