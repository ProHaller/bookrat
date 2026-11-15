//! Inspecting source files with ffprobe/ffmpeg.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    /// Length in seconds as the container reports it. Close, but MP3 headers
    /// can be off slightly; the bind measures exact lengths while decoding.
    pub duration: f64,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// Bits per second, if known.
    pub bit_rate: Option<u32>,
    /// Whether the file carries an embedded picture (ID3 APIC etc.).
    pub has_cover: bool,
    /// Container tags with lowercased keys.
    pub tags: HashMap<String, String>,
    /// Chapter markers embedded in the file (ID3 `CHAP`, MP4 chapters), in
    /// order. Empty for most per-chapter MP3s.
    pub markers: Vec<Marker>,
}

/// A chapter marker within a single source file.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    /// Offset from the start of the file, in seconds.
    pub start: f64,
    pub title: String,
}

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    chapters: Vec<ProbeChapter>,
    format: ProbeFormat,
}

#[derive(Deserialize)]
struct ProbeChapter {
    start_time: String,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: String,
    codec_name: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    bit_rate: Option<String>,
    duration: Option<String>,
    #[serde(default)]
    disposition: HashMap<String, i64>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    bit_rate: Option<String>,
    duration: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

/// Fails with a helpful message if ffmpeg/ffprobe are not on PATH.
pub fn check_tools() -> Result<()> {
    for tool in ["ffmpeg", "ffprobe"] {
        let ok = Command::new(tool)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            bail!("`{tool}` not found on PATH; install ffmpeg (e.g. `brew install ffmpeg`)");
        }
    }
    Ok(())
}

pub fn probe_file(path: &Path) -> Result<SourceFile> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let probe: ProbeOutput =
        serde_json::from_slice(&out.stdout).context("parsing ffprobe output")?;

    let audio = probe
        .streams
        .iter()
        .find(|s| s.codec_type == "audio")
        .ok_or_else(|| anyhow!("no audio stream"))?;
    let sample_rate: u32 = audio
        .sample_rate
        .as_deref()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("unknown sample rate"))?;
    let bit_rate = audio
        .bit_rate
        .as_deref()
        .or(probe.format.bit_rate.as_deref())
        .and_then(|s| s.parse().ok());
    let duration: f64 = audio
        .duration
        .as_deref()
        .or(probe.format.duration.as_deref())
        .and_then(|d| d.parse().ok())
        .filter(|d: &f64| *d > 0.0)
        .ok_or_else(|| anyhow!("unknown duration"))?;
    let has_cover = probe
        .streams
        .iter()
        .any(|s| s.codec_type == "video" && s.disposition.get("attached_pic") == Some(&1));

    Ok(SourceFile {
        path: path.to_path_buf(),
        duration,
        codec: audio.codec_name.clone().unwrap_or_default(),
        sample_rate,
        channels: audio.channels.unwrap_or(2),
        bit_rate,
        has_cover,
        tags: probe
            .format
            .tags
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
            .collect(),
        markers: probe
            .chapters
            .into_iter()
            .filter_map(|c| {
                Some(Marker {
                    start: c.start_time.parse().ok()?,
                    title: c
                        .tags
                        .get("title")
                        .map(|t| t.trim().to_string())
                        .unwrap_or_default(),
                })
            })
            .collect(),
    })
}

/// Probes all files in parallel (ffprobe only, no decoding), preserving
/// order. Each file gets its own result, so one unreadable file doesn't hide
/// the rest. `on_progress` receives the
/// number of files finished so far.
pub fn probe_each(
    paths: &[PathBuf],
    on_progress: impl Fn(usize) + Sync,
) -> Vec<Result<SourceFile>> {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Result<SourceFile>>>> =
        Mutex::new((0..paths.len()).map(|_| None).collect());
    let workers = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(paths.len());

    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(i) else { break };
                    let r = probe_file(path).with_context(|| format!("probing {}", path.display()));
                    results.lock().unwrap_or_else(PoisonError::into_inner)[i] = Some(r);
                    on_progress(done.fetch_add(1, Ordering::Relaxed) + 1);
                }
            });
        }
    });

    results
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .zip(paths)
        .map(|(r, path)| r.unwrap_or_else(|| Err(anyhow!("{} was not probed", path.display()))))
        .collect()
}
