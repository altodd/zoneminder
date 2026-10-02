//! Evidence export: a ZIP of one MP4 per camera for a time range, with a
//! manifest and checksums, streamed with an exact Content-Length.
mod common;
use common::*;
use sha2::Digest;
use std::path::Path;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000;
fn dts(secs: i64) -> i64 {
    secs * TIMESCALE as i64
}
fn ms(secs: i64) -> i64 {
    secs * 1000
}

struct World {
    fx: Fixture,
    front: i64,
    back: i64,
    mixed: i64,
    empty: i64,
    admin: String,
    viewer: String,
}

/// front: 4 s segments at T0, T0+4, T0+8, then nothing until T0+20 (a gap),
/// T0+20..24; main and sub. back: T0..T0+4. mixed: two codec settings in a
/// row (fragmented-MP4 fallback). empty: nothing recorded.
fn world() -> World {
    let fx = Fixture::new();
    let front = fx.add_camera("Front Door");
    let back = fx.add_camera("Back/Yard");
    let mixed = fx.add_camera("mixed");
    let empty = fx.add_camera("empty");
    let enc = parse_encoded(&encode_fmp4(4, "libx264", 320, 180));
    for s in [0, 4, 8, 20] {
        fx.write_segment(front, "main", &enc, dts(T0 + s), |_| 0);
        fx.write_segment(front, "sub", &enc, dts(T0 + s), |_| 0);
    }
    fx.write_segment(back, "main", &enc, dts(T0), |_| 0);
    let big = parse_encoded(&encode_fmp4(4, "libx264", 640, 360));
    fx.write_segment(mixed, "main", &enc, dts(T0), |_| 0);
    fx.write_segment(mixed, "main", &big, dts(T0 + 4), |_| 0);
    let a = fx.add_admin("admin", "password123");
    let v = fx.add_viewer("guest", "password123", &[front]);
    World { admin: fx.token_for(a), viewer: fx.token_for(v), fx, front, back, mixed, empty }
}

fn probe(path: &Path) -> (String, f64, usize) {
    let out = std::process::Command::new("ffprobe")
        .args(["-hide_banner", "-loglevel", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "format=format_name,duration:stream=nb_read_frames", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    (
        v["format"]["format_name"].as_str().unwrap().to_string(),
        v["format"]["duration"].as_str().unwrap().parse().unwrap(),
        v["streams"][0]["nb_read_frames"].as_str().unwrap().parse().unwrap(),
    )
}

/// Top-level box types of an MP4 file, in order.
fn boxes(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 8 <= data.len() {
        let mut size = u32::from_be_bytes(data[p..p + 4].try_into().unwrap()) as u64;
        out.push(String::from_utf8_lossy(&data[p + 4..p + 8]).to_string());
        if size == 1 {
            size = u64::from_be_bytes(data[p + 8..p + 16].try_into().unwrap());
        }
        if size < 8 {
            break;
        }
        p += size as usize;
    }
    out
}

/// Unpack with Python's zipfile (checks every CRC) into `dir`.
fn unzip(zip: &[u8], dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap();
    let z = dir.join("export.zip");
    std::fs::write(&z, zip).unwrap();
    let out = std::process::Command::new("python3")
        .args(["-c", "import sys,zipfile; z=zipfile.ZipFile(sys.argv[1]); assert z.testzip() is None; z.extractall(sys.argv[2]); print('\\n'.join(z.namelist()))"])
        .arg(&z)
        .arg(dir.join("x"))
        .output()
        .expect("python3");
    assert!(out.status.success(), "zipfile: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn export_zip_has_playable_mp4s_a_manifest_and_checksums() {
    let w = world();
    let r = w.fx.router();
    let q = format!("cameras={},{},{},{}&start={}&end={}", w.front, w.back, w.mixed, w.empty, ms(T0), ms(T0 + 30));
    let pre = get(&r, &format!("/api/export.json?{q}"), &w.admin).await;
    assert_eq!(pre.status, 200, "{}", pre.text());
    let pre = pre.json();
    let files = pre["files"].as_array().unwrap();
    assert_eq!(files.len(), 3, "{pre}");
    assert_eq!(pre["missing"][0]["camera"], "empty");
    let front = files.iter().find(|f| f["camera_id"] == w.front).unwrap();
    assert_eq!(front["format"], "mp4");
    assert_eq!(front["frames"], 160);
    assert_eq!(front["gaps"].as_array().unwrap().len(), 1, "{front}");
    assert_eq!(front["gaps"][0]["seconds"], 8.0);
    let mixed = files.iter().find(|f| f["camera_id"] == w.mixed).unwrap();
    assert_eq!(mixed["format"], "fragmented mp4");
    let total = pre["total_bytes"].as_u64().unwrap();

    let res = get(&r, &format!("/api/export.zip?{q}"), &w.admin).await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.header("content-type").unwrap(), "application/zip");
    assert!(res.header("content-disposition").unwrap().starts_with("attachment; filename=\"zmng-export "), "{:?}", res.header("content-disposition"));
    assert_eq!(res.header("content-length").unwrap().parse::<u64>().unwrap(), res.body.len() as u64);
    assert_eq!(res.body.len() as u64, total, "the preview's size is the archive's size");

    let dir = w.fx.dir.path().join("unzipped");
    let names = unzip(&res.body, &dir);
    assert_eq!(names.len(), 6, "{names:?}");
    assert!(names.iter().any(|n| n.starts_with("Back_Yard ") && n.ends_with(".mp4")), "{names:?}");
    assert_eq!(&names[3..], ["manifest.json", "SHA256SUMS", "README.txt"]);
    let x = dir.join("x");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(x.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["exported_by"], "admin");
    assert_eq!(manifest["missing"][0]["camera"], "empty");
    let sums = std::fs::read_to_string(x.join("SHA256SUMS")).unwrap();
    for f in manifest["files"].as_array().unwrap() {
        let name = f["file"].as_str().unwrap();
        let data = std::fs::read(x.join(name)).unwrap();
        let sha = hex::encode(sha2::Sha256::digest(&data));
        assert_eq!(f["sha256"], sha.as_str(), "{name}");
        assert!(sums.contains(&format!("{sha}  {name}\n")), "{sums}");
        assert_eq!(f["bytes"], data.len());
        let (fmt, dur, frames) = probe(&x.join(name));
        assert!(fmt.contains("mp4"), "{fmt}");
        if f["camera_id"] == w.front {
            // progressive: the index sits before the media; the gap is held, so 24 s of wall time
            assert_eq!(boxes(&data)[..3], ["ftyp", "moov", "mdat"]);
            assert_eq!(frames, 160);
            assert!((dur - 24.0).abs() < 0.3, "front duration {dur}");
            assert_eq!(f["first_frame_utc"], "2027-01-15T08:00:00.000Z");
        }
        if f["camera_id"] == w.mixed {
            assert_eq!(frames, 80);
            assert!(boxes(&data).contains(&"moof".to_string()));
        }
    }
    let manifest_sha = hex::encode(sha2::Sha256::digest(std::fs::read(x.join("manifest.json")).unwrap()));
    assert!(sums.contains(&format!("{manifest_sha}  manifest.json")));
    assert!(std::fs::read_to_string(x.join("README.txt")).unwrap().contains("not included: empty"));
}

#[tokio::test]
async fn export_substream_and_permissions() {
    let w = world();
    let r = w.fx.router();
    // the substream: same frames, smaller files
    let res = get(&r, &format!("/api/export.zip?cameras={}&start={}&end={}&stream=sub", w.front, ms(T0), ms(T0 + 10)), &w.viewer).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let names = unzip(&res.body, &w.fx.dir.path().join("sub"));
    assert!(names[0].ends_with(" sub.mp4"), "{names:?}");
    // a viewer cannot export a camera they cannot see, even mixed with one they can
    let q = format!("cameras={},{}&start={}&end={}", w.front, w.back, ms(T0), ms(T0 + 10));
    assert_eq!(get(&r, &format!("/api/export.zip?{q}"), &w.viewer).await.status, 404);
    assert_eq!(get(&r, &format!("/api/export.json?{q}"), &w.viewer).await.status, 404);
    // bad requests
    for q in [
        format!("cameras=&start={}&end={}", ms(T0), ms(T0 + 10)),
        format!("cameras={}&start={}&end={}", w.front, ms(T0), ms(T0 + 7 * 3600)),
        format!("cameras={}&start={}&end={}", w.front, ms(T0 + 10), ms(T0)),
        format!("cameras={}&start={}&end={}&stream=4k", w.front, ms(T0), ms(T0 + 10)),
        format!("cameras=x&start={}&end={}", ms(T0), ms(T0 + 10)),
    ] {
        assert_eq!(get(&r, &format!("/api/export.json?{q}"), &w.admin).await.status, 400, "{q}");
    }
    // nothing recorded: 404 for the zip, an empty preview
    let none = format!("cameras={}&start={}&end={}", w.empty, ms(T0), ms(T0 + 10));
    assert_eq!(get(&r, &format!("/api/export.zip?{none}"), &w.admin).await.status, 404);
    assert_eq!(get(&r, &format!("/api/export.json?{none}"), &w.admin).await.json()["files"].as_array().unwrap().len(), 0);
    assert_eq!(call(&r, "GET", &format!("/api/export.zip?{q}"), None, None).await.status, 401);
}

/// HEVC (the production cameras' codec) exports as a progressive MP4 that
/// ffmpeg decodes from start to end without an error.
#[tokio::test]
async fn hevc_exports_as_a_clean_progressive_mp4() {
    let fx = Fixture::new();
    let cam = fx.add_camera("hevc");
    let enc = parse_encoded(&encode_fmp4(4, "libx265", 320, 180));
    for s in [0, 4] {
        fx.write_segment(cam, "main", &enc, dts(T0 + s), |_| 0);
    }
    let tok = fx.token_for(fx.add_admin("admin", "password123"));
    let r = fx.router();
    let res = get(&r, &format!("/api/export.zip?cameras={cam}&start={}&end={}", ms(T0), ms(T0 + 8)), &tok).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let dir = fx.dir.path().join("hevc");
    let names = unzip(&res.body, &dir);
    let mp4 = dir.join("x").join(&names[0]);
    assert_eq!(boxes(&std::fs::read(&mp4).unwrap())[..3], ["ftyp", "moov", "mdat"]);
    let (_, dur, frames) = probe(&mp4);
    assert_eq!(frames, 80);
    assert!((dur - 8.0).abs() < 0.3, "{dur}");
    let out = std::process::Command::new("ffmpeg").args(["-hide_banner", "-v", "error", "-i"]).arg(&mp4).args(["-f", "null", "-"]).output().unwrap();
    assert!(out.status.success() && out.stderr.is_empty(), "decode errors: {}", String::from_utf8_lossy(&out.stderr));
    let codec = std::process::Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_tag_string", "-of", "csv=p=0"]).arg(&mp4).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&codec.stdout).trim(), "hvc1");
}
