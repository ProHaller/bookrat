//! End-to-end binds of generated audio. Every fixture chapter is a sine tone
//! of a distinct frequency, so chapter placement can be checked against the
//! actual audio. Requires ffmpeg/ffprobe on PATH; skipped otherwise.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use bookrat::{book::Book, encode, mp4};
use serde_json::Value;

const RATE: u32 = 44_100;

fn have_ffmpeg() -> bool {
    let ok = bookrat::probe::check_tools().is_ok();
    if !ok {
        eprintln!("skipping: ffmpeg/ffprobe not installed");
    }
    ok
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn sine_mp3(path: &Path, freq: u32, secs: f64, title: &str) {
    let src = format!("sine=frequency={freq}:duration={secs}:sample_rate={RATE}");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &src,
        "-c:a",
        "libmp3lame",
        "-b:a",
        "96k",
        "-metadata",
        &format!("title={title}"),
        "-metadata",
        "album=Fixture Book",
        "-metadata",
        "artist=Fixture Author",
        path.to_str().unwrap(),
    ]);
}

/// One MP3 holding tones of `freqs` back to back, with an ID3 chapter each.
fn chaptered_mp3(dir: &Path, freqs: &[u32], secs: f64) -> PathBuf {
    let mut inputs = Vec::new();
    let mut meta = String::from(";FFMETADATA1\nalbum=Single File\n");
    for (i, f) in freqs.iter().enumerate() {
        inputs.push("-f".to_string());
        inputs.push("lavfi".into());
        inputs.push("-i".into());
        inputs.push(format!(
            "sine=frequency={f}:duration={secs}:sample_rate={RATE}"
        ));
        let (start, end) = (
            (i as f64 * secs * 1000.0) as u64,
            ((i + 1) as f64 * secs * 1000.0) as u64,
        );
        meta += &format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle=Part {}\n",
            i + 1
        );
    }
    let meta_path = dir.join("chapters.txt");
    std::fs::write(&meta_path, meta).unwrap();
    let concat = format!(
        "{}concat=n={}:v=0:a=1",
        (0..freqs.len())
            .map(|i| format!("[{i}]"))
            .collect::<String>(),
        freqs.len()
    );
    let book_dir = dir.join("book");
    std::fs::create_dir_all(&book_dir).unwrap();
    let out = book_dir.join("book.mp3");
    let mut args: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let meta_idx = freqs.len().to_string();
    let map_meta = meta_idx.clone();
    args.extend([
        "-i",
        meta_path.to_str().unwrap(),
        "-filter_complex",
        &concat,
        "-map_metadata",
        &map_meta,
        "-map_chapters",
        &meta_idx,
        "-c:a",
        "libmp3lame",
        "-b:a",
        "96k",
        "-id3v2_version",
        "3",
        out.to_str().unwrap(),
    ]);
    ffmpeg(&args);
    book_dir
}

/// (start seconds, title) of every chapter, as ffprobe reads them.
fn chapters(path: &Path) -> Vec<(f64, String)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_chapters"])
        .arg(path)
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    json["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["start_time"].as_str().unwrap().parse().unwrap(),
                c["tags"]["title"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn titles(path: &Path) -> Vec<String> {
    chapters(path).into_iter().map(|(_, t)| t).collect()
}

fn decode_mono(path: &Path) -> Vec<f32> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a",
            "-ac",
            "1",
            "-ar",
            &RATE.to_string(),
            "-f",
            "f32le",
            "-",
        ])
        .output()
        .unwrap();
    out.stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect()
}

/// Energy at `freq` in a window of `pcm` centred on `at` seconds (Goertzel).
fn energy(pcm: &[f32], at: f64, freq: f64) -> f64 {
    let n = (0.05 * f64::from(RATE)) as usize;
    let start = (at * f64::from(RATE)) as usize - n / 2;
    let k = 2.0 * (2.0 * std::f64::consts::PI * freq / f64::from(RATE)).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in &pcm[start..start + n] {
        let s = f64::from(x) + k * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    s1 * s1 + s2 * s2 - k * s1 * s2
}

/// The candidate frequency that dominates the middle of each chapter.
fn tone_per_chapter(path: &Path, candidates: &[u32]) -> Vec<u32> {
    let pcm = decode_mono(path);
    let marks = chapters(path);
    let total = pcm.len() as f64 / f64::from(RATE);
    let ends = marks.iter().skip(1).map(|&(s, _)| s).chain([total]);
    marks
        .iter()
        .zip(ends)
        .map(|(&(start, _), end)| {
            let mid = (start + end) / 2.0;
            *candidates
                .iter()
                .max_by(|&&a, &&b| {
                    energy(&pcm, mid, a.into()).total_cmp(&energy(&pcm, mid, b.into()))
                })
                .unwrap()
        })
        .collect()
}

fn run_cli(book_dir: &Path, out: &Path, lang_env: &str) {
    let status = Command::new(env!("CARGO_BIN_EXE_bookrat"))
        .arg(book_dir)
        .args(["--no-tui", "-o"])
        .arg(out)
        .env("LANG", lang_env)
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn per_file_book(tmp: &Path) -> PathBuf {
    let dir = tmp.join("Fixture Author - Fixture Book");
    std::fs::create_dir_all(&dir).unwrap();
    sine_mp3(&dir.join("01.mp3"), 300, 2.0, "Opening");
    sine_mp3(&dir.join("02.mp3"), 700, 2.5, "Middle");
    sine_mp3(&dir.join("10.mp3"), 1500, 2.0, "Ending");
    dir
}

#[test]
fn per_file_book_gets_one_chapter_per_file_in_natural_order() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.m4b");
    run_cli(&per_file_book(tmp.path()), &out, "en_US.UTF-8");
    assert_eq!(titles(&out), ["Opening", "Middle", "Ending"]);
}

#[test]
fn per_file_chapters_contain_their_own_audio() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.m4b");
    run_cli(&per_file_book(tmp.path()), &out, "en_US.UTF-8");
    assert_eq!(tone_per_chapter(&out, &[300, 700, 1500]), [300, 700, 1500]);
}

/// Apple players hide chapters whose track language is "und".
#[test]
fn chapter_track_language_follows_locale() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.m4b");
    run_cli(&per_file_book(tmp.path()), &out, "de_DE.UTF-8");
    assert_eq!(mp4::chapter_track_languages(&out).unwrap(), ["deu"]);
}

#[test]
fn single_file_with_embedded_chapters_keeps_them() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.m4b");
    run_cli(
        &chaptered_mp3(tmp.path(), &[300, 700, 1500], 2.0),
        &out,
        "en_US.UTF-8",
    );
    let starts: Vec<f64> = chapters(&out)
        .iter()
        .map(|&(s, _)| (s * 1000.0).round() / 1000.0)
        .collect();
    assert_eq!(starts, [0.0, 2.0, 4.0]);
}

#[test]
fn reordered_slices_of_one_file_carry_the_right_audio() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = chaptered_mp3(tmp.path(), &[300, 700, 1500], 2.0);
    let mut book = Book::load(&dir, |_, _| {}).unwrap();
    book.chapters.reverse();
    let out = tmp.path().join("reversed.m4b");
    encode::bind(&book, &out, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(tone_per_chapter(&out, &[300, 700, 1500]), [1500, 700, 300]);
}

/// Regression: MP3-embedded cover art used to push ffmpeg's chapter track
/// timescale so high that chapters over ~8 minutes overflowed and the
/// QuickTime chapter track (the one Apple players read) was dropped.
#[test]
fn long_chapters_with_embedded_cover_keep_the_chapter_track() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Long Book");
    std::fs::create_dir_all(&dir).unwrap();
    let cover = tmp.path().join("cover.jpg");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=red:s=64x64",
        "-frames:v",
        "1",
        cover.to_str().unwrap(),
    ]);
    for (i, freq) in [(1, 300), (2, 700)] {
        let src = format!("sine=frequency={freq}:duration=540:sample_rate=22050");
        let out = dir.join(format!("{i:02}.mp3"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &src,
            "-i",
            cover.to_str().unwrap(),
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "32k",
            "-ac",
            "1",
            "-c:v",
            "copy",
            "-disposition:v",
            "attached_pic",
            "-id3v2_version",
            "3",
            out.to_str().unwrap(),
        ]);
    }
    let out = tmp.path().join("out.m4b");
    run_cli(&dir, &out, "en_US.UTF-8");
    assert_eq!(mp4::chapter_track_languages(&out).unwrap(), ["eng"]);
}

/// Regression: opening a folder used to pull in every audio file below it,
/// merging nested books into one.
#[test]
fn nested_book_folders_are_not_merged_in() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = per_file_book(tmp.path());
    let nested = dir.join("Another Book");
    std::fs::create_dir_all(&nested).unwrap();
    sine_mp3(&nested.join("01.mp3"), 2500, 1.0, "Intruder");
    let out = tmp.path().join("out.m4b");
    run_cli(&dir, &out, "en_US.UTF-8");
    assert_eq!(titles(&out), ["Opening", "Middle", "Ending"]);
}

#[test]
fn disc_subfolders_are_bound_as_one_book() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Two Disc Book");
    for (disc, freq) in [("CD1", 300), ("CD2", 700)] {
        std::fs::create_dir_all(dir.join(disc)).unwrap();
        sine_mp3(&dir.join(disc).join("01.mp3"), freq, 1.0, disc);
    }
    let out = tmp.path().join("out.m4b");
    run_cli(&dir, &out, "en_US.UTF-8");
    assert_eq!(titles(&out), ["CD1", "CD2"]);
}

#[test]
fn library_folder_is_refused_headless() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    per_file_book(tmp.path());
    let status = Command::new(env!("CARGO_BIN_EXE_bookrat"))
        .arg(tmp.path())
        .args(["--no-tui", "-o"])
        .arg(tmp.path().join("out.m4b"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stderr).contains("no audio files directly in"));
}

fn run_cli_args(book_dir: &Path, extra: &[&std::ffi::OsStr]) {
    let out = Command::new(env!("CARGO_BIN_EXE_bookrat"))
        .arg(book_dir)
        .arg("--no-tui")
        .args(extra)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn output_defaults_to_the_book_folder_and_is_not_rebound_as_a_part() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = per_file_book(tmp.path());
    let expected = dir.join("Fixture Author - Fixture Book.m4b");
    run_cli_args(&dir, &[]);
    let first = titles(&expected);
    // Binding again must not pick up the first result as a fourth chapter.
    std::fs::rename(&expected, dir.join("Earlier Result.m4b")).unwrap();
    run_cli_args(&dir, &[]);
    assert_eq!((first.len(), titles(&expected).len()), (3, 3));
}

#[test]
fn output_flag_with_a_folder_keeps_the_default_name() {
    if !have_ffmpeg() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = per_file_book(tmp.path());
    let target = tmp.path().join("Library");
    std::fs::create_dir_all(&target).unwrap();
    run_cli_args(&dir, &["-o".as_ref(), target.as_os_str()]);
    assert!(target.join("Fixture Author - Fixture Book.m4b").exists());
}
