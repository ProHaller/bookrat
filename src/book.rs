//! The book being bound: chapters, metadata and output settings, plus the
//! heuristics that fill them in from the source files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::encode::Encoder;
use crate::lang;
use crate::probe::{self, SourceFile};
use crate::scan;

/// Markers closer together than this (or to the end of the file) are noise.
const MIN_CHAPTER_SECS: f64 = 0.5;

#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub title: String,
    pub author: String,
    pub narrator: String,
    pub year: String,
    pub genre: String,
    pub description: String,
    /// ISO 639-2/T code. Apple players only show chapters whose track
    /// language matches the user's, so this must never be "und".
    pub language: String,
}

/// A chapter: a whole source file, or a slice of one that carries embedded
/// chapter markers.
#[derive(Debug, Clone)]
pub struct Chapter {
    pub title: String,
    pub source: Arc<SourceFile>,
    /// Start within `source`, in seconds.
    pub start: f64,
    /// End within `source`, in seconds; `None` runs to the end of the file.
    pub end: Option<f64>,
}

impl Chapter {
    /// Length in seconds. Estimated from the container when the chapter runs
    /// to the end of its file; the bind measures the exact value.
    pub fn duration(&self) -> f64 {
        (self.end.unwrap_or(self.source.duration) - self.start).max(0.0)
    }
}

#[derive(Debug, Clone)]
pub enum Cover {
    None,
    /// A standalone image file.
    Image(PathBuf),
    /// The attached picture of a source file.
    Embedded(PathBuf),
}

impl Cover {
    pub fn describe(&self) -> String {
        match self {
            Cover::None => "none".into(),
            Cover::Image(p) => file_name(p),
            Cover::Embedded(p) => format!("embedded in {}", file_name(p)),
        }
    }
}

/// Output audio parameters.
#[derive(Debug, Clone)]
pub struct AudioTarget {
    pub encoder: Encoder,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u32,
}

impl AudioTarget {
    pub fn describe(&self) -> String {
        let ch = if self.channels == 1 { "mono" } else { "stereo" };
        format!(
            "{} kbps {ch} {:.1} kHz ({})",
            self.bitrate_kbps,
            self.sample_rate as f64 / 1000.0,
            self.encoder.name()
        )
    }
}

/// An audio file found under the book's folder, in or out of the book.
#[derive(Debug, Clone)]
pub struct BookFile {
    pub source: Arc<SourceFile>,
    /// Path relative to the book folder, for display.
    pub rel: String,
    /// Chapter title used when the file is included without markers.
    pub title: String,
    pub included: bool,
}

#[derive(Debug, Clone)]
pub struct Book {
    pub dir: PathBuf,
    /// Every audio file found under `dir`; the included ones make the book.
    pub files: Vec<BookFile>,
    /// Files outside the default book that could not be probed.
    pub skipped: usize,
    pub chapters: Vec<Chapter>,
    pub meta: Metadata,
    pub cover: Cover,
    pub target: AudioTarget,
    /// Folder the M4B is written to; `None` means the book folder itself,
    /// next to the source parts.
    pub output_dir: Option<PathBuf>,
    /// File name of the M4B; `None` means `<Author> - <Title>.m4b`.
    pub output_name: Option<String>,
}

impl Book {
    /// Scans and probes `dir`, then derives metadata, chapters and settings.
    /// The book starts with the folder's own files (see
    /// [`scan::Candidate::default`]); other files under it are listed but
    /// left out until selected.
    pub fn load(dir: &Path, on_progress: impl Fn(usize, usize) + Sync) -> Result<Book> {
        let dir = dir.canonicalize()?;
        let candidates = scan::candidates(&dir)?;
        let paths: Vec<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
        let total = paths.len();
        on_progress(0, total);
        let results = probe::probe_each(&paths, |done| on_progress(done, total));

        let mut probed = Vec::with_capacity(total);
        let mut skipped = 0;
        for (candidate, result) in candidates.into_iter().zip(results) {
            match result {
                Ok(source) => probed.push((source, candidate.default)),
                // A broken file of this book is an error; one elsewhere isn't.
                Err(e) if candidate.default => return Err(e),
                Err(_) => skipped += 1,
            }
        }
        if probed.is_empty() {
            bail!("no readable audio files in {}", dir.display());
        }
        let mut book = Book::from_candidates(dir, probed);
        book.skipped = skipped;
        Ok(book)
    }

    /// A book of exactly `sources`, all included.
    pub fn from_sources(dir: PathBuf, sources: Vec<SourceFile>) -> Book {
        Book::from_candidates(dir, sources.into_iter().map(|s| (s, true)).collect())
    }

    /// A book over `files`, where the flag says whether a file starts included.
    pub fn from_candidates(dir: PathBuf, files: Vec<(SourceFile, bool)>) -> Book {
        let included: Vec<&SourceFile> = files.iter().filter(|f| f.1).map(|f| &f.0).collect();
        let meta = detect_metadata(&dir, &included);
        let cover = detect_cover(&dir, &included);
        let target = choose_target(&included, Encoder::detect());
        let all: Vec<&SourceFile> = files.iter().map(|f| &f.0).collect();
        let titles = chapter_titles(&dir, &all);

        let files: Vec<BookFile> = files
            .into_iter()
            .zip(titles)
            .map(|((source, included), title)| BookFile {
                rel: relative(&dir, &source.path),
                source: Arc::new(source),
                title,
                included,
            })
            .collect();
        let chapters = files
            .iter()
            .filter(|f| f.included)
            .flat_map(|f| split_at_markers(Arc::clone(&f.source), f.title.clone()))
            .collect();
        Book {
            dir,
            files,
            skipped: 0,
            chapters,
            meta,
            cover,
            target,
            output_dir: None,
            output_name: None,
        }
    }

    /// Adds or removes file `index`. Added chapters go where the file sorts
    /// among the book's other files; chapters of other files keep their
    /// titles and order.
    pub fn set_included(&mut self, index: usize, included: bool) {
        let Some(file) = self.files.get_mut(index) else {
            return;
        };
        if file.included == included {
            return;
        }
        file.included = included;
        let source = Arc::clone(&file.source);
        if included {
            let added = split_at_markers(Arc::clone(&source), file.title.clone());
            let at = self
                .chapters
                .iter()
                .position(|c| self.file_index(&c.source).is_some_and(|i| i > index))
                .unwrap_or(self.chapters.len());
            self.chapters.splice(at..at, added);
        } else {
            self.chapters.retain(|c| !Arc::ptr_eq(&c.source, &source));
        }
        self.retarget();
    }

    /// Adds or removes every file in the same folder as file `index`.
    pub fn set_folder_included(&mut self, index: usize, included: bool) {
        let Some(folder) = self.files.get(index).map(|f| folder_of(&f.rel)) else {
            return;
        };
        let same: Vec<usize> = (0..self.files.len())
            .filter(|&i| folder_of(&self.files[i].rel) == folder)
            .collect();
        for i in same {
            self.set_included(i, included);
        }
    }

    /// Removes chapter `index`; a file left without chapters drops out.
    pub fn remove_chapter(&mut self, index: usize) {
        if index >= self.chapters.len() {
            return;
        }
        let source = self.chapters.remove(index).source;
        if !self
            .chapters
            .iter()
            .any(|c| Arc::ptr_eq(&c.source, &source))
            && let Some(i) = self.file_index(&source)
        {
            self.files[i].included = false;
            self.retarget();
        }
    }

    fn file_index(&self, source: &Arc<SourceFile>) -> Option<usize> {
        self.files
            .iter()
            .position(|f| Arc::ptr_eq(&f.source, source))
    }

    /// Re-derives sample rate and channels from the included files; the
    /// encoder and bitrate stay as chosen.
    fn retarget(&mut self) {
        let included: Vec<&SourceFile> = self
            .files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.source.as_ref())
            .collect();
        if included.is_empty() {
            return;
        }
        let fresh = choose_target(&included, self.target.encoder);
        self.target.sample_rate = fresh.sample_rate;
        self.target.channels = fresh.channels;
    }

    pub fn duration(&self) -> f64 {
        self.chapters.iter().map(Chapter::duration).sum()
    }

    /// The book folder, so the M4B lands next to its source parts.
    pub fn default_output_dir(&self) -> PathBuf {
        self.dir.clone()
    }

    /// `<Author> - <Title>.m4b`
    pub fn default_output_name(&self) -> String {
        let name = match (self.meta.author.trim(), self.meta.title.trim()) {
            ("", "") => file_name(&self.dir),
            ("", t) => t.to_string(),
            (a, "") => a.to_string(),
            (a, t) => format!("{a} - {t}"),
        };
        format!("{}.m4b", sanitize_filename(&name))
    }

    pub fn output_dir(&self) -> PathBuf {
        self.output_dir
            .clone()
            .unwrap_or_else(|| self.default_output_dir())
    }

    pub fn output_name(&self) -> String {
        self.output_name
            .clone()
            .unwrap_or_else(|| self.default_output_name())
    }

    pub fn output_path(&self) -> PathBuf {
        self.output_dir().join(self.output_name())
    }

    /// Sets the output folder from user input. `~` expands to the home
    /// folder; relative paths are taken from the book folder, so `output`
    /// means a subfolder and `..` its parent. Empty restores the default.
    pub fn set_output_dir(&mut self, input: &str) -> Result<(), String> {
        let input = input.trim();
        if input.is_empty() {
            self.output_dir = None;
            return Ok(());
        }
        let dir = self.default_output_dir().join(expand_tilde(input));
        if dir.exists() && !dir.is_dir() {
            return Err(format!("{} is a file, not a folder", dir.display()));
        }
        self.output_dir = Some(dir);
        Ok(())
    }

    /// Sets the output file name from user input, adding `.m4b` if no audio
    /// extension is given. A value containing a path separator sets the folder
    /// too. Empty restores the default name.
    pub fn set_output_name(&mut self, input: &str) -> Result<(), String> {
        let input = input.trim();
        if input.contains(['/', '\\']) {
            let base = self.default_output_dir();
            self.set_output_path(&base.join(expand_tilde(input)));
            return Ok(());
        }
        self.output_name = match input {
            "" => None,
            "." | ".." => return Err(format!("{input:?} is not a file name")),
            name => Some(with_audio_extension(name)),
        };
        Ok(())
    }

    /// Sets the output from a full path: an existing folder, or one written
    /// with a trailing separator, keeps the default file name.
    pub fn set_output_path(&mut self, path: &Path) {
        let ends_with_separator = path.as_os_str().to_string_lossy().ends_with(['/', '\\']);
        if path.is_dir() || ends_with_separator {
            self.output_dir = Some(path.to_path_buf());
            return;
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            self.output_dir = Some(parent.to_path_buf());
        }
        self.output_name = path
            .file_name()
            .map(|n| with_audio_extension(&n.to_string_lossy()));
    }

    pub fn estimated_size_bytes(&self) -> u64 {
        (self.duration() * self.target.bitrate_kbps as f64 * 1000.0 / 8.0) as u64
    }

    /// Human-readable concerns about the sources, empty if none.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        let rates = distinct(self.chapters.iter().map(|c| c.source.sample_rate));
        if rates.len() > 1 {
            w.push(format!(
                "mixed sample rates {rates:?}; resampling to {}",
                self.target.sample_rate
            ));
        }
        let chans = distinct(self.chapters.iter().map(|c| c.source.channels));
        if chans.len() > 1 {
            w.push(format!("mixed channel counts {chans:?}"));
        }
        if self.meta.title.trim().is_empty() {
            w.push("title is empty".into());
        }
        match self.chapters.len() {
            0 => w.push("no files selected: pick some in the Files panel".into()),
            1 => w.push("only one chapter: the source has no chapter markers".into()),
            _ => {}
        }
        if self.skipped > 0 {
            w.push(format!(
                "{} file(s) elsewhere in the folder could not be read",
                self.skipped
            ));
        }
        w
    }
}

/// Picks the most frequent non-empty value of `key` across all sources.
fn majority_tag(sources: &[&SourceFile], keys: &[&str]) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in sources {
        if let Some(v) = keys
            .iter()
            .find_map(|k| s.tags.get(*k).filter(|v| !v.is_empty()))
        {
            *counts.entry(v.as_str()).or_default() += 1;
        }
    }
    // Ties resolve alphabetically so results are deterministic.
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(v, _)| v.to_string())
}

fn detect_metadata(dir: &Path, sources: &[&SourceFile]) -> Metadata {
    let (folder_author, folder_title) = parse_folder_name(&file_name(dir));

    let author = majority_tag(sources, &["album_artist", "artist"])
        .or(folder_author)
        .unwrap_or_default();
    let narrator = majority_tag(sources, &["composer"])
        .filter(|n| *n != author)
        .unwrap_or_default();

    Metadata {
        title: majority_tag(sources, &["album"]).unwrap_or(folder_title),
        author,
        narrator,
        year: majority_tag(sources, &["date", "year"])
            .map(|d| d.chars().take(4).collect())
            .unwrap_or_default(),
        genre: majority_tag(sources, &["genre"]).unwrap_or_default(),
        description: majority_tag(sources, &["comment", "description"]).unwrap_or_default(),
        language: majority_tag(sources, &["language", "lang"])
            .and_then(|l| lang::normalize(&l))
            .or_else(lang::from_environment)
            .unwrap_or_else(|| lang::DEFAULT.to_string()),
    }
}

fn split_at_markers(source: Arc<SourceFile>, file_title: String) -> Vec<Chapter> {
    let mut starts: Vec<(f64, &str)> = Vec::new();
    for m in &source.markers {
        let after_previous = starts
            .last()
            .is_none_or(|&(prev, _)| m.start - prev >= MIN_CHAPTER_SECS);
        if after_previous && m.start < source.duration - MIN_CHAPTER_SECS {
            starts.push((m.start.max(0.0), &m.title));
        }
    }
    if starts.len() < 2 {
        return vec![Chapter {
            title: file_title,
            source,
            start: 0.0,
            end: None,
        }];
    }
    // Anything before the first marker belongs to the first chapter.
    starts[0].0 = 0.0;

    // The last chapter runs to the true end of the file, which only the
    // decoder knows exactly.
    let ends = starts.iter().skip(1).map(|&(s, _)| Some(s)).chain([None]);
    starts
        .iter()
        .zip(ends)
        .enumerate()
        .map(|(i, (&(start, title), end))| Chapter {
            title: if title.is_empty() {
                format!("{file_title} ({})", i + 1)
            } else {
                title.to_string()
            },
            source: Arc::clone(&source),
            start,
            end,
        })
        .collect()
}

/// Splits `Author - Title` folder names; otherwise the whole name is the title.
fn parse_folder_name(name: &str) -> (Option<String>, String) {
    match name.split_once(" - ") {
        Some((a, t)) if !a.trim().is_empty() && !t.trim().is_empty() => {
            (Some(a.trim().to_string()), t.trim().to_string())
        }
        _ => (None, name.trim().to_string()),
    }
}

/// Uses title tags when every file has a distinct one, else file names
/// (prefixed with their subfolder if names repeat, e.g. `CD2 / 01`).
fn chapter_titles(dir: &Path, sources: &[&SourceFile]) -> Vec<String> {
    let tags: Vec<Option<&String>> = sources
        .iter()
        .map(|s| s.tags.get("title").filter(|t| !t.is_empty()))
        .collect();
    let all_present = tags.iter().all(Option::is_some);
    if all_present && distinct(tags.iter().flatten()).len() == tags.len() {
        return tags.into_iter().flatten().cloned().collect();
    }

    let stems: Vec<String> = sources
        .iter()
        .map(|s| {
            s.path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    if distinct(stems.iter()).len() == stems.len() {
        return stems;
    }
    sources
        .iter()
        .map(|s| {
            let rel = s
                .path
                .strip_prefix(dir)
                .unwrap_or(&s.path)
                .with_extension("");
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .collect()
}

fn detect_cover(dir: &Path, sources: &[&SourceFile]) -> Cover {
    if let Some(img) = scan::cover_image(dir) {
        return Cover::Image(img);
    }
    match sources.iter().find(|s| s.has_cover) {
        Some(s) => Cover::Embedded(s.path.clone()),
        None => Cover::None,
    }
}

const BITRATE_STEPS: &[u32] = &[32, 48, 64, 80, 96, 112, 128, 160, 192, 256];

/// Matches the sources: the most common sample rate (capped at 48 kHz), mono
/// if every source is mono, and the next standard AAC bitrate at or above the
/// median source bitrate, capped so speech doesn't get absurdly large files.
fn choose_target(sources: &[&SourceFile], encoder: Encoder) -> AudioTarget {
    let mut rate_counts: HashMap<u32, usize> = HashMap::new();
    for s in sources {
        *rate_counts.entry(s.sample_rate).or_default() += 1;
    }
    let sample_rate = rate_counts
        .into_iter()
        .max_by_key(|&(rate, n)| (n, rate))
        .map_or(44_100, |(rate, _)| rate)
        .min(48_000);

    let channels = if sources.iter().all(|s| s.channels == 1) {
        1
    } else {
        2
    };

    let mut rates: Vec<u32> = sources
        .iter()
        .filter_map(|s| s.bit_rate)
        .map(|b| b / 1000)
        .collect();
    rates.sort_unstable();
    let median = rates.get(rates.len() / 2).copied().unwrap_or(64 * channels);
    let cap = if channels == 1 { 96 } else { 160 };
    let bitrate_kbps = BITRATE_STEPS
        .iter()
        .copied()
        .find(|&step| step >= median)
        .unwrap_or(cap)
        .min(cap);

    AudioTarget {
        encoder,
        bitrate_kbps,
        sample_rate,
        channels,
    }
}

/// Expands a leading `~/` (or a bare `~`) to the home folder.
pub fn expand_tilde(p: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (p, home) {
        ("~", Some(home)) => home,
        (_, Some(home)) if p.starts_with("~/") => home.join(&p[2..]),
        _ => PathBuf::from(p),
    }
}

/// Appends `.m4b` unless the name already ends in an MP4 audio extension.
fn with_audio_extension(name: &str) -> String {
    let lower = name.to_lowercase();
    if [".m4b", ".m4a", ".mp4"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        name.to_string()
    } else {
        format!("{name}.m4b")
    }
}

/// `path` relative to `dir`, for display.
fn relative(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// The folder part of a relative path (`""` for files directly in the book).
fn folder_of(rel: &str) -> &str {
    rel.rsplit_once(['/', '\\'])
        .map_or("", |(folder, _)| folder)
}

fn distinct<T: Ord>(it: impl Iterator<Item = T>) -> Vec<T> {
    let mut v: Vec<T> = it.collect();
    v.sort();
    v.dedup();
    v
}

pub fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Marker;

    fn src(name: &str, rate: u32, ch: u32, kbps: u32, tags: &[(&str, &str)]) -> SourceFile {
        SourceFile {
            path: PathBuf::from(format!("/book/{name}")),
            duration: 10.0,
            codec: "mp3".into(),
            sample_rate: rate,
            channels: ch,
            bit_rate: Some(kbps * 1000),
            has_cover: false,
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            markers: Vec::new(),
        }
    }

    fn with_markers(mut s: SourceFile, duration: f64, markers: &[(f64, &str)]) -> SourceFile {
        s.duration = duration;
        s.markers = markers
            .iter()
            .map(|&(start, title)| Marker {
                start,
                title: title.to_string(),
            })
            .collect();
        s
    }

    fn refs(sources: &[SourceFile]) -> Vec<&SourceFile> {
        sources.iter().collect()
    }

    fn chapters_of(source: SourceFile) -> Vec<Chapter> {
        Book::from_sources(PathBuf::from("/book"), vec![source]).chapters
    }

    /// /book/{01, 02}.mp3 included; /book/Other/{01, 02}.mp3 offered only.
    fn book_with_other_folder() -> Book {
        let files = ["01.mp3", "02.mp3", "Other/01.mp3", "Other/02.mp3"]
            .iter()
            .map(|name| (src(name, 44_100, 2, 128, &[]), !name.starts_with("Other")))
            .collect();
        Book::from_candidates(PathBuf::from("/book"), files)
    }

    fn titles(book: &Book) -> Vec<&str> {
        book.chapters.iter().map(|c| c.title.as_str()).collect()
    }

    fn dune() -> Book {
        let s = src(
            "01.mp3",
            44_100,
            2,
            128,
            &[("album", "Dune"), ("artist", "Frank Herbert")],
        );
        Book::from_sources(PathBuf::from("/books/Dune"), vec![s])
    }

    #[test]
    fn output_defaults_to_the_book_folder() {
        assert_eq!(
            dune().output_path(),
            Path::new("/books/Dune/Frank Herbert - Dune.m4b")
        );
    }

    #[test]
    fn output_dir_accepts_absolute_folders() {
        let mut book = dune();
        book.set_output_dir("/srv/audiobooks/").unwrap();
        assert_eq!(
            book.output_path(),
            Path::new("/srv/audiobooks/Frank Herbert - Dune.m4b")
        );
    }

    #[test]
    fn relative_output_dir_is_inside_the_book_folder() {
        let mut book = dune();
        book.set_output_dir("output").unwrap();
        assert_eq!(book.output_dir(), Path::new("/books/Dune/output"));
    }

    #[test]
    fn output_dir_rejects_a_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut book = dune();
        assert!(book.set_output_dir(&file.path().to_string_lossy()).is_err());
    }

    #[test]
    fn output_name_gets_an_m4b_extension() {
        let mut book = dune();
        book.set_output_name("Dune (Unabridged)").unwrap();
        assert_eq!(book.output_name(), "Dune (Unabridged).m4b");
    }

    #[test]
    fn output_name_with_a_folder_sets_both() {
        let mut book = dune();
        book.set_output_name("/srv/books/dune.m4b").unwrap();
        assert_eq!(book.output_path(), Path::new("/srv/books/dune.m4b"));
    }

    #[test]
    fn output_path_to_an_existing_folder_keeps_the_default_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut book = dune();
        book.set_output_path(dir.path());
        assert_eq!(
            book.output_path(),
            dir.path().join("Frank Herbert - Dune.m4b")
        );
    }

    #[test]
    fn only_default_files_start_as_chapters() {
        assert_eq!(titles(&book_with_other_folder()), ["01", "02"]);
    }

    #[test]
    fn including_a_file_inserts_it_in_file_order() {
        let mut book = book_with_other_folder();
        book.set_included(1, false);
        book.set_included(2, true);
        book.set_included(1, true);
        assert_eq!(titles(&book), ["01", "02", "Other / 01"]);
    }

    #[test]
    fn including_a_file_keeps_renamed_chapters() {
        let mut book = book_with_other_folder();
        book.chapters[0].title = "Prologue".into();
        book.set_included(3, true);
        assert_eq!(titles(&book), ["Prologue", "02", "Other / 02"]);
    }

    #[test]
    fn folder_toggle_includes_every_file_in_that_folder() {
        let mut book = book_with_other_folder();
        book.set_folder_included(2, true);
        assert_eq!(book.chapters.len(), 4);
    }

    #[test]
    fn removing_the_last_chapter_of_a_file_excludes_it() {
        let mut book = book_with_other_folder();
        book.remove_chapter(0);
        assert!(!book.files[0].included);
    }

    #[test]
    fn retargets_channels_when_selection_changes() {
        let files = vec![
            (src("mono.mp3", 22_050, 1, 64, &[]), true),
            (src("Other/stereo.mp3", 22_050, 2, 64, &[]), false),
        ];
        let mut book = Book::from_candidates(PathBuf::from("/book"), files);
        book.set_included(1, true);
        assert_eq!(book.target.channels, 2);
    }

    fn spans(chapters: &[Chapter]) -> Vec<(String, f64, f64)> {
        chapters
            .iter()
            .map(|c| (c.title.clone(), c.start, c.duration()))
            .collect()
    }

    #[test]
    fn single_file_with_markers_splits_into_chapters() {
        let s = with_markers(
            src("book.mp3", 44_100, 1, 64, &[]),
            100.0,
            &[(0.0, "One"), (40.0, "Two"), (70.0, "")],
        );
        assert_eq!(
            spans(&chapters_of(s)),
            [
                ("One".into(), 0.0, 40.0),
                ("Two".into(), 40.0, 30.0),
                ("book (3)".into(), 70.0, 30.0)
            ]
        );
    }

    #[test]
    fn markers_ignore_lead_in_and_trailing_noise() {
        // First marker after 0 absorbs the lead-in; a marker at the very end is dropped.
        let s = with_markers(
            src("b.mp3", 44_100, 1, 64, &[]),
            60.0,
            &[(2.0, "A"), (30.0, "B"), (59.9, "C")],
        );
        assert_eq!(
            spans(&chapters_of(s)),
            [("A".into(), 0.0, 30.0), ("B".into(), 30.0, 30.0)]
        );
    }

    #[test]
    fn file_with_single_marker_stays_one_chapter() {
        let s = with_markers(src("b.mp3", 44_100, 1, 64, &[]), 60.0, &[(0.0, "Only")]);
        assert_eq!(spans(&chapters_of(s)), [("b".into(), 0.0, 60.0)]);
    }

    #[test]
    fn language_comes_from_tags_when_present() {
        let m = detect_metadata(
            Path::new("/x/y"),
            &[&src("1.mp3", 44_100, 2, 128, &[("language", "ger")])],
        );
        assert_eq!(m.language, "deu");
    }

    #[test]
    fn target_never_exceeds_source() {
        let s = vec![
            src("a.mp3", 22_050, 1, 64, &[]),
            src("b.mp3", 22_050, 1, 64, &[]),
        ];
        let t = choose_target(&refs(&s), Encoder::Native);
        assert_eq!((t.bitrate_kbps, t.sample_rate, t.channels), (64, 22_050, 1));
    }

    #[test]
    fn target_rounds_up_and_caps() {
        let s = vec![src("a.mp3", 44_100, 2, 70, &[])];
        assert_eq!(choose_target(&refs(&s), Encoder::Native).bitrate_kbps, 80);
        let s = vec![src("a.mp3", 44_100, 2, 320, &[])];
        assert_eq!(choose_target(&refs(&s), Encoder::Native).bitrate_kbps, 160);
    }

    #[test]
    fn metadata_from_tags_and_folder() {
        let s = vec![
            src(
                "1.mp3",
                44_100,
                2,
                128,
                &[
                    ("album", "Dune"),
                    ("artist", "Frank Herbert"),
                    ("composer", "Scott Brick"),
                ],
            ),
            src(
                "2.mp3",
                44_100,
                2,
                128,
                &[("album", "Dune"), ("artist", "Frank Herbert")],
            ),
        ];
        let m = detect_metadata(Path::new("/x/whatever"), &refs(&s));
        assert_eq!(
            (m.title.as_str(), m.author.as_str(), m.narrator.as_str()),
            ("Dune", "Frank Herbert", "Scott Brick")
        );

        let m = detect_metadata(
            Path::new("/x/Ursula Le Guin - The Dispossessed"),
            &[&src("1.mp3", 44_100, 2, 128, &[])],
        );
        assert_eq!(
            (m.title.as_str(), m.author.as_str()),
            ("The Dispossessed", "Ursula Le Guin")
        );
    }

    #[test]
    fn chapter_titles_fall_back_to_filenames_on_duplicates() {
        let s = vec![
            src("01 Intro.mp3", 44_100, 2, 128, &[("title", "Dune")]),
            src("02 Part.mp3", 44_100, 2, 128, &[("title", "Dune")]),
        ];
        assert_eq!(
            chapter_titles(Path::new("/book"), &refs(&s)),
            ["01 Intro", "02 Part"]
        );
    }

    #[test]
    fn chapter_titles_include_subfolder_when_names_repeat() {
        let s = vec![
            src("CD1/01.mp3", 44_100, 2, 128, &[]),
            src("CD2/01.mp3", 44_100, 2, 128, &[]),
        ];
        assert_eq!(
            chapter_titles(Path::new("/book"), &refs(&s)),
            ["CD1 / 01", "CD2 / 01"]
        );
    }
}
