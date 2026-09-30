//! Thumbnails: decode exactly one keyframe with ffmpeg from an in-memory
//! init segment + fragment.  Cost: one HEVC/H.264 I-frame decode (~50-150 ms
//! of one core for 2688x1520 HEVC in software), done once per event, never
//! per page view.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

/// An ffmpeg child with library thread pools pinned to one thread.
///
/// Ubuntu's ffmpeg links a library that starts an OpenMP pool of one thread
/// per core in every process. On the 48-core production box each snapshot
/// ffmpeg ran ~130 threads and burned ~4.5 s of CPU (mostly thread start-up)
/// for one 640 px JPEG, and a few concurrent ones hit the unit's `TasksMax`
/// (EAGAIN: "ff_frame_thread_encoder_init failed"). With the pool pinned it
/// is 4 threads and ~0.35 s. Codec and filter threads are still chosen per
/// command with `-threads` / `-filter_threads`.
pub fn ffmpeg_command(ffmpeg: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(ffmpeg);
    c.env("OMP_NUM_THREADS", "1");
    c
}

/// Decode the first frame of `frag` (which must start with a keyframe) and
/// return a JPEG scaled to `width` pixels wide.
pub async fn frag_to_jpeg(ffmpeg: &str, init: &[u8], frag: &[u8], width: u32, quality: u8) -> Result<Vec<u8>> {
    let mut child = ffmpeg_command(ffmpeg)
        .args([
            "-nostdin", "-loglevel", "error", "-threads", "2",
            "-f", "mp4", "-i", "pipe:0",
            "-frames:v", "1", "-threads", "1", "-filter_threads", "1",
            "-vf", &format!("scale={width}:-2"),
            "-q:v", &quality.to_string(),
            "-f", "image2", "-vcodec", "mjpeg", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning ffmpeg")?;
    let mut stdin = child.stdin.take().unwrap();
    let mut input = Vec::with_capacity(init.len() + frag.len());
    input.extend_from_slice(init);
    input.extend_from_slice(frag);
    // ffmpeg may close stdin early once it has its frame; ignore write errors after that
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
        let _ = stdin.shutdown().await;
    });
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .context("ffmpeg timeout")??;
    let _ = writer.await;
    if out.stdout.is_empty() {
        anyhow::bail!("ffmpeg produced no image: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Read `len` bytes at `offset` from a file (blocking; call via spawn_blocking
/// or accept the small cost for one fragment).
pub fn read_range(path: &Path, offset: u64, len: u32) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}
