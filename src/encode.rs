//! Binding a [`Book`] into a single M4B with ffmpeg.
//!
//! Each source is decoded by its own ffmpeg process straight to the target
//! sample rate and channel count as 32-bit float PCM, and the samples are
//! streamed in order into one encoder process. This keeps the timeline
//! sample-exact no matter how mixed the sources are (no filter-graph resets on
//! format changes, no decoder-delay timestamp offsets), gives gapless joins,
//! and avoids any quantization before the single AAC encode.
//!
//! Chapter lengths are measured from the samples actually fed, so probing
//! never has to decode anything. The encoder writes audio only; a final
//! stream-copy remux adds the exact chapters, tags and cover.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};

use crate::book::{AudioTarget, Book, Chapter, Cover};
use crate::mp4;
use crate::probe::SourceFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Encoder {
    /// Apple AudioToolbox (macOS), best available quality there.
    #[value(name = "aac_at")]
    AudioToolbox,
    /// Fraunhofer FDK, only in custom ffmpeg builds.
    #[value(name = "libfdk_aac")]
    Fdk,
    /// ffmpeg's built-in AAC encoder.
    #[value(name = "aac")]
    Native,
}

impl Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Encoder::AudioToolbox => "aac_at",
            Encoder::Fdk => "libfdk_aac",
            Encoder::Native => "aac",
        }
    }

    /// Best AAC encoder the installed ffmpeg offers.
    pub fn detect() -> Encoder {
        let list = Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let has = |name: &str| {
            list.lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
        };
        [Encoder::AudioToolbox, Encoder::Fdk]
            .into_iter()
            .find(|e| has(e.name()))
            .unwrap_or(Encoder::Native)
    }
}

/// Encodes `book` to `output`. Writes to `<output>.part` and renames on
/// success, so a failed or cancelled run never leaves a broken file behind.
/// `on_progress` receives the fraction done (0.0..=1.0).
pub fn bind(
    book: &Book,
    output: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(f64),
) -> Result<()> {
    if book.chapters.is_empty() {
        bail!("nothing to bind");
    }
    let out_dir = output.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // The intermediate lives next to the output: it is as large as the result,
    // and the temp dir may be on a smaller filesystem.
    let audio = tempfile::Builder::new()
        .prefix(".bookrat-")
        .suffix(".m4a")
        .tempfile_in(out_dir)
        .context("creating intermediate file")?;
    let meta = tempfile::Builder::new()
        .prefix(".bookrat-")
        .suffix(".txt")
        .tempfile_in(out_dir)
        .context("creating metadata file")?;
    let part = PathBuf::from(format!("{}.part", output.display()));

    let result = (|| {
        let durations = encode_audio(book, audio.path(), cancel, &mut on_progress)?;
        fs::write(meta.path(), ffmetadata(book, &durations))?;
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let cover = prepare_cover(book, out_dir)?;
        remux(book, audio.path(), meta.path(), cover.as_ref(), &part)?;
        label_chapter_track(&part, &book.meta.language)
    })();

    match result {
        Ok(()) => {
            fs::rename(&part, output)
                .with_context(|| format!("moving output to {}", output.display()))?;
            on_progress(1.0);
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Share of the progress bar given to encoding; the remux takes the rest.
const ENCODE_SHARE: f64 = 0.97;

/// Encodes all chapters into an audio-only file at `out`. Returns the exact
/// duration of each chapter in seconds.
fn encode_audio(
    book: &Book,
    out: &Path,
    cancel: &AtomicBool,
    on_progress: &mut impl FnMut(f64),
) -> Result<Vec<f64>> {
    let t = &book.target;
    let bytes_per_frame = u64::from(t.channels) * 4;
    let total_bytes = book.duration() * f64::from(t.sample_rate) * bytes_per_frame as f64;

    let mut encoder = encoder_command(book, out)
        .spawn()
        .context("starting ffmpeg encoder")?;
    let encoder_err = drain(encoder.stderr.take());
    let Some(mut sink) = encoder.stdin.take() else {
        kill(&mut encoder);
        bail!("ffmpeg encoder stdin not captured");
    };

    let fed = feed_all(book, &mut sink, cancel, |bytes| {
        on_progress((bytes as f64 / total_bytes).clamp(0.0, 1.0) * ENCODE_SHARE);
    });
    // Closing stdin tells the encoder the stream is complete.
    drop(sink);

    let frames = match fed {
        Ok(frames) => frames,
        Err(e) => {
            kill(&mut encoder);
            let err = encoder_err.join().unwrap_or_default();
            // A broken pipe means the encoder died first; its stderr says why.
            let broken_pipe = e.downcast_ref::<io::Error>().map(io::Error::kind)
                == Some(io::ErrorKind::BrokenPipe);
            if broken_pipe && !err.is_empty() {
                bail!("ffmpeg encoder failed: {}", tail_lines(&err));
            }
            return Err(e);
        }
    };

    let status = encoder.wait()?;
    let err = encoder_err.join().unwrap_or_default();
    if !status.success() {
        bail!("ffmpeg encoder failed ({status}): {}", tail_lines(&err));
    }
    let rate = f64::from(t.sample_rate);
    Ok(frames.into_iter().map(|n| n as f64 / rate).collect())
}

/// The cover as the remux should read it.
enum CoverInput {
    Image(PathBuf),
    /// A picture extracted from a source file; deleted when dropped.
    Extracted(tempfile::NamedTempFile),
}

/// Pictures embedded in MP3s carry the MP3's very fine timebase. Muxed
/// directly, that drives ffmpeg's chapter track timescale so high that any
/// chapter longer than about eight minutes overflows, and the QuickTime
/// chapter track is silently dropped: Apple players then show no chapters.
/// Copying the picture out to a standalone image first avoids it.
fn prepare_cover(book: &Book, dir: &Path) -> Result<Option<CoverInput>> {
    match &book.cover {
        Cover::None => Ok(None),
        Cover::Image(path) => Ok(Some(CoverInput::Image(path.clone()))),
        Cover::Embedded(source) => {
            let file = tempfile::Builder::new()
                .prefix(".bookrat-")
                .suffix(".cover")
                .tempfile_in(dir)
                .context("creating cover file")?;
            let output = Command::new("ffmpeg")
                .args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"])
                .arg(source)
                .args(["-map", "0:v:0", "-c", "copy", "-f", "image2pipe"])
                .arg(file.path())
                .output()
                .context("running ffmpeg cover extraction")?;
            if !output.status.success() {
                bail!(
                    "extracting cover from {} failed: {}",
                    source.display(),
                    tail_lines(&String::from_utf8_lossy(&output.stderr))
                );
            }
            Ok(Some(CoverInput::Extracted(file)))
        }
    }
}

/// Copies the encoded audio into the final container with chapters, tags and
/// cover. No re-encode, so it costs about as much as copying the file.
fn remux(
    book: &Book,
    audio: &Path,
    meta: &Path,
    cover: Option<&CoverInput>,
    out: &Path,
) -> Result<()> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"])
        .arg(audio)
        .args(["-f", "ffmetadata", "-i"])
        .arg(meta);
    match cover {
        Some(CoverInput::Image(path)) => {
            cmd.arg("-i").arg(path);
        }
        // Raw picture bytes; the pipe demuxer detects JPEG vs PNG by content.
        Some(CoverInput::Extracted(file)) => {
            cmd.args(["-f", "image2pipe", "-i"]).arg(file.path());
        }
        None => {}
    }
    cmd.args(["-map", "0:a"]);
    if cover.is_some() {
        cmd.args(["-map", "2:v:0", "-disposition:v:0", "attached_pic"]);
    }
    cmd.args(["-c", "copy", "-map_metadata", "1", "-map_chapters", "1"])
        .arg("-metadata:s:a:0")
        .arg(format!("language={}", book.meta.language))
        .args(["-movflags", "+faststart", "-f", "mp4"])
        .arg(out);
    let output = cmd.output().context("running ffmpeg remux")?;
    if !output.status.success() {
        bail!(
            "ffmpeg remux failed ({}): {}",
            output.status,
            tail_lines(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(())
}

/// ffmpeg leaves the chapter track's language undetermined, which hides the
/// chapters in Apple players; see [`mp4`].
fn label_chapter_track(path: &Path, language: &str) -> Result<()> {
    let patched = mp4::set_chapter_track_language(path, language)
        .context("setting chapter track language")?;
    if patched == 0 {
        bail!("ffmpeg wrote no chapter track");
    }
    Ok(())
}

/// A contiguous run of audio to decode in one go: a whole file, or a slice of
/// one when chapters come from embedded markers.
struct Segment<'a> {
    source: &'a SourceFile,
    start: f64,
    /// `None` runs to the end of the file.
    end: Option<f64>,
    /// The chapters this segment covers, in order.
    chapters: Vec<&'a Chapter>,
}

/// Merges adjacent chapters that continue the same file, so a single-file
/// book with fifty markers is still decoded once, front to back.
fn segments(chapters: &[Chapter]) -> Vec<Segment<'_>> {
    let mut out: Vec<Segment> = Vec::new();
    for ch in chapters {
        if let Some(last) = out.last_mut()
            && std::ptr::eq(last.source, ch.source.as_ref())
            && last.end == Some(ch.start)
        {
            last.end = ch.end;
            last.chapters.push(ch);
            continue;
        }
        out.push(Segment {
            source: &ch.source,
            start: ch.start,
            end: ch.end,
            chapters: vec![ch],
        });
    }
    out
}

/// Decodes every chapter in order into `sink`. `on_bytes` receives the
/// running total of PCM bytes written. Returns each chapter's length in
/// sample frames at the target rate.
fn feed_all(
    book: &Book,
    sink: &mut ChildStdin,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> Result<Vec<u64>> {
    let t = &book.target;
    let bytes_per_frame = u64::from(t.channels) * 4;
    let mut buf = vec![0u8; 1 << 16];
    let mut written = 0u64;
    let mut frames = Vec::with_capacity(book.chapters.len());

    for segment in segments(&book.chapters) {
        let path = &segment.source.path;
        let mut decoder = decoder_command(&segment, t)
            .spawn()
            .context("starting ffmpeg decoder")?;
        let decoder_err = drain(decoder.stderr.take());
        let Some(mut pcm) = decoder.stdout.take() else {
            kill(&mut decoder);
            bail!("ffmpeg decoder stdout not captured");
        };

        let mut segment_bytes = 0u64;
        let copied = (|| -> Result<()> {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    bail!("cancelled");
                }
                let n = pcm.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                sink.write_all(&buf[..n])?;
                segment_bytes += n as u64;
                on_bytes(written + segment_bytes);
            }
        })();

        if copied.is_err() {
            kill(&mut decoder);
            return copied.map(|()| Vec::new());
        }
        let status = decoder.wait()?;
        if !status.success() {
            let err = decoder_err.join().unwrap_or_default();
            bail!("decoding {} failed: {}", path.display(), tail_lines(&err));
        }
        written += segment_bytes;
        frames.extend(split_frames(
            &segment,
            segment_bytes / bytes_per_frame,
            t.sample_rate,
        ));
    }
    Ok(frames)
}

/// Splits a decoded segment's frame count among its chapters: inner
/// boundaries sit at the planned marker times, the last chapter gets the rest.
fn split_frames(segment: &Segment, total: u64, rate: u32) -> Vec<u64> {
    let rate = f64::from(rate);
    let mut bounds: Vec<u64> = segment
        .chapters
        .iter()
        .skip(1)
        .map(|ch| (((ch.start - segment.start) * rate).round() as u64).min(total))
        .collect();
    bounds.insert(0, 0);
    bounds.push(total);
    bounds
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect()
}

fn decoder_command(segment: &Segment, t: &AudioTarget) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(&segment.source.path);
    if segment.start > 0.0 || segment.end.is_some() {
        // Trim by sample count at the source rate, after rebasing timestamps
        // on the sample count: MP3 decoder delay offsets the raw timestamps.
        let rate = f64::from(segment.source.sample_rate);
        let mut filter = format!(
            "asetpts=N/SR/TB,atrim=start_sample={}",
            (segment.start * rate).round() as u64
        );
        if let Some(end) = segment.end {
            let _ = write!(filter, ":end_sample={}", (end * rate).round() as u64);
        }
        cmd.args(["-af", &filter]);
    }
    cmd.args([
        "-map",
        "0:a:0",
        "-ar",
        &t.sample_rate.to_string(),
        "-ac",
        &t.channels.to_string(),
    ])
    .args(["-c:a", "pcm_f32le", "-f", "f32le", "-"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    cmd
}

fn encoder_command(book: &Book, out: &Path) -> Command {
    let t = &book.target;
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-y", "-v", "error", "-nostats"])
        .args([
            "-f",
            "f32le",
            "-ar",
            &t.sample_rate.to_string(),
            "-ac",
            &t.channels.to_string(),
        ])
        .args(["-i", "pipe:0", "-map", "0:a"])
        .args(["-c:a", t.encoder.name()])
        .args(["-b:a", &format!("{}k", t.bitrate_kbps)])
        .args(["-f", "mp4"])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Collects a child's stderr on a thread so it can never fill up and block.
fn drain(stderr: Option<impl Read + Send + 'static>) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut s);
        }
        s
    })
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn tail_lines(s: &str) -> String {
    let lines: Vec<&str> = s.trim().lines().collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

/// Escapes special characters per the FFMETADATA format.
fn escape(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Tags plus one `[CHAPTER]` per entry of `durations` (seconds).
fn ffmetadata(book: &Book, durations: &[f64]) -> String {
    let m = &book.meta;
    let mut s = String::from(";FFMETADATA1\n");
    let mut tag = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            let _ = writeln!(s, "{k}={}", escape(v.trim()));
        }
    };
    tag("title", &m.title);
    tag("album", &m.title);
    tag("artist", &m.author);
    tag("album_artist", &m.author);
    tag("composer", &m.narrator);
    tag("date", &m.year);
    tag("genre", &m.genre);
    tag("description", &m.description);
    tag("comment", &m.description);
    // iTunes media kind 2 = Audiobook
    tag("media_type", "2");

    let mut start_ms = 0u64;
    let mut elapsed = 0.0;
    for (ch, duration) in book.chapters.iter().zip(durations) {
        elapsed += duration;
        let end_ms = (elapsed * 1000.0).round() as u64;
        let _ = write!(
            s,
            "\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={start_ms}\nEND={end_ms}\ntitle={}\n",
            escape(&ch.title)
        );
        start_ms = end_ms;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_metadata() {
        assert_eq!(escape("a=b;c#d\\e\nf"), "a\\=b\\;c\\#d\\\\e\\\nf");
    }
}
