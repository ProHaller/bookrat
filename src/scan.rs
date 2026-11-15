//! Discovering audio files and cover images in a book folder.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const AUDIO_EXTS: &[&str] = &["mp3", "m4a", "m4b", "aac", "flac", "ogg", "opus", "wav"];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png"];
const COVER_STEMS: &[&str] = &["cover", "folder", "front"];

/// Most files listed for one opened folder, so opening a whole library root
/// stays responsive.
pub const MAX_CANDIDATES: usize = 2000;
/// How deep below the opened folder to look for other files to offer.
const MAX_DEPTH: usize = 4;

/// An audio file found under an opened folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    /// Whether it belongs to the folder's own book: it sits directly in the
    /// folder or in a disc subfolder (`CD1/`, `Disc 2/`). Files elsewhere in
    /// the tree are other books and only offered for manual selection.
    pub default: bool,
}

/// Audio files under `dir`, in natural order of their relative paths (so
/// `CD2/Track 10` sorts after `CD2/Track 9`). Stops at [`MAX_CANDIDATES`].
pub fn candidates(dir: &Path) -> Result<Vec<Candidate>> {
    let mut files = Vec::new();
    walk(dir, 0, &mut files)?;
    let mut out: Vec<(String, Candidate)> = files
        .into_iter()
        .map(|path| {
            let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
            let default = rel.parent().is_none_or(|p| {
                p.components()
                    .all(|c| is_disc_dir(&c.as_os_str().to_string_lossy()))
            });
            (
                rel.to_string_lossy().into_owned(),
                Candidate { path, default },
            )
        })
        .collect();
    if out.is_empty() {
        bail!("no audio files found in {}", dir.display());
    }
    // Bound books are written next to their parts by default, so an M4B
    // beside other audio is most likely an earlier result, not a part.
    let has_parts = out.iter().any(|(_, c)| c.default && !is_m4b(&c.path));
    if has_parts {
        for (_, c) in &mut out {
            c.default &= !is_m4b(&c.path);
        }
    }
    out.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    Ok(out.into_iter().map(|(_, c)| c).collect())
}

fn is_m4b(path: &Path) -> bool {
    has_ext(path, &["m4b"])
}

/// Whether a folder name marks one disc of a multi-disc book, like `CD1`,
/// `cd 02`, `Disc 3 - Epilogue` or `Disk_4`.
pub fn is_disc_dir(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    ["cd", "disc", "disk"].iter().any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|rest| {
            rest.trim_start_matches([' ', '_', '-', '#', '.'])
                .starts_with(|c: char| c.is_ascii_digit())
        })
    })
}

/// Finds a cover image in `dir` itself: a well-known name first
/// (`cover.jpg`, `folder.png`, ...), otherwise any image if it's the only one.
pub fn cover_image(dir: &Path) -> Option<PathBuf> {
    let mut images: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && has_ext(p, IMAGE_EXTS))
        .collect();
    images.sort();
    let named = images.iter().find(|p| {
        p.file_stem()
            .map(|s| COVER_STEMS.contains(&s.to_string_lossy().to_lowercase().as_str()))
            .unwrap_or(false)
    });
    match named {
        Some(p) => Some(p.clone()),
        None if images.len() == 1 => images.pop(),
        None => None,
    }
}

/// What a folder holds, one level deep: enough to tell a book from a library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirSummary {
    pub audio_files: usize,
    pub subdirs: usize,
    /// Subfolders that look like discs of one book (`CD1`, `Disc 2`).
    pub disc_subdirs: usize,
}

impl DirSummary {
    /// Whether the folder looks like a single book: audio of its own, or
    /// nothing but disc subfolders.
    pub fn is_book(&self) -> bool {
        self.audio_files > 0 || (self.subdirs > 0 && self.disc_subdirs == self.subdirs)
    }
}

/// Counts audio files and visible subfolders directly inside `dir`.
pub fn summarize(dir: &Path) -> std::io::Result<DirSummary> {
    let mut summary = DirSummary::default();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            summary.subdirs += 1;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_disc_dir(&name) {
                summary.disc_subdirs += 1;
            }
        } else if has_ext(&path, AUDIO_EXTS) {
            summary.audio_files += 1;
        }
    }
    Ok(summary)
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let mut subdirs = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            subdirs.push(path);
        } else if has_ext(&path, AUDIO_EXTS) && out.len() < MAX_CANDIDATES {
            out.push(path);
        }
    }
    if depth < MAX_DEPTH {
        for sub in subdirs {
            // Unreadable folders elsewhere in the tree shouldn't block the book.
            if out.len() >= MAX_CANDIDATES || walk(&sub, depth + 1, out).is_err() {
                continue;
            }
        }
    }
    Ok(())
}

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .map(|e| exts.contains(&e.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Case-insensitive comparison that orders embedded numbers by value.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = take_number(&mut a);
                let nb = take_number(&mut b);
                // Compare by magnitude (length after trimming zeros), then digits.
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn take_number(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
        s.push(c);
        it.next();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for f in files {
            let path = tmp.path().join(f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"").unwrap();
        }
        tmp
    }

    fn defaults(tmp: &tempfile::TempDir) -> Vec<String> {
        candidates(tmp.path())
            .unwrap()
            .into_iter()
            .filter(|c| c.default)
            .map(|c| {
                c.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn nested_books_are_not_part_of_the_default_book() {
        let tmp = tree(&["01.mp3", "02.mp3", "Other Book/01.mp3"]);
        assert_eq!(defaults(&tmp), ["01.mp3", "02.mp3"]);
    }

    #[test]
    fn disc_folders_belong_to_the_default_book() {
        let tmp = tree(&["CD1/01.mp3", "CD2/01.mp3", "Extras/bonus.mp3"]);
        assert_eq!(defaults(&tmp), ["CD1/01.mp3", "CD2/01.mp3"]);
    }

    #[test]
    fn library_folder_offers_files_without_defaults() {
        let tmp = tree(&["Book A/01.mp3", "Book B/01.mp3"]);
        assert_eq!(
            (candidates(tmp.path()).unwrap().len(), defaults(&tmp).len()),
            (2, 0)
        );
    }

    #[test]
    fn earlier_m4b_output_beside_parts_is_not_a_default_part() {
        let tmp = tree(&["01.mp3", "02.mp3", "Author - Title.m4b"]);
        assert_eq!(defaults(&tmp), ["01.mp3", "02.mp3"]);
    }

    #[test]
    fn m4b_parts_alone_are_default_parts() {
        let tmp = tree(&["Part 1.m4b", "Part 2.m4b"]);
        assert_eq!(defaults(&tmp), ["Part 1.m4b", "Part 2.m4b"]);
    }

    #[test]
    fn recognizes_disc_folder_names() {
        let names = [
            "CD1",
            "cd 02",
            "Disc 3 - Epilogue",
            "Disk_4",
            "CD",
            "Discography",
            "Part 1",
        ];
        let hits: Vec<bool> = names.iter().map(|n| is_disc_dir(n)).collect();
        assert_eq!(hits, [true, true, true, true, false, false, false]);
    }

    #[test]
    fn natural_order() {
        let mut v = vec![
            "Track 10.mp3",
            "track 2.mp3",
            "Track 1.mp3",
            "CD2/01.mp3",
            "CD1/10.mp3",
        ];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            [
                "CD1/10.mp3",
                "CD2/01.mp3",
                "Track 1.mp3",
                "track 2.mp3",
                "Track 10.mp3"
            ]
        );
    }

    #[test]
    fn leading_zeros_equal_value() {
        assert_eq!(natural_cmp("01", "1"), Ordering::Equal);
        assert_eq!(natural_cmp("002", "10"), Ordering::Less);
    }
}
