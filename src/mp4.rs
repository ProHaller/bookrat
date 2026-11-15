//! Minimal MP4 box walking for the one fix-up ffmpeg can't do itself.
//!
//! ffmpeg writes the hidden QuickTime chapter text track with language code 0,
//! which AVFoundation reads as "und". Apple players (Books, QuickTime, Finder)
//! look chapters up by the user's preferred languages, never match "und", and
//! so show no chapters at all. We patch the language field of each chapter
//! track's `mdhd` box in place; it is fixed-size, so nothing else moves.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

/// Sets the language of every chapter track in `path` to `lang` (ISO 639-2/T,
/// e.g. `eng`). Returns how many chapter tracks were patched.
pub fn set_chapter_track_language(path: &Path, lang: &str) -> Result<usize> {
    let code = pack_language(lang)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let (moov_pos, moov) = read_moov(&mut file)?;
    let tracks = chapter_tracks(&moov)?;
    for t in &tracks {
        file.seek(SeekFrom::Start(moov_pos + t.lang_pos as u64))?;
        file.write_all(&code.to_be_bytes())?;
    }
    file.sync_all()?;
    Ok(tracks.len())
}

/// Languages of the chapter tracks in `path`, in track order.
pub fn chapter_track_languages(path: &Path) -> Result<Vec<String>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let (_, moov) = read_moov(&mut file)?;
    chapter_tracks(&moov)?
        .iter()
        .map(|t| Ok(unpack_language(be_u16(&moov, t.lang_pos)?)))
        .collect()
}

struct ChapterTrack {
    /// Offset of the `mdhd` language field within the `moov` buffer.
    lang_pos: usize,
}

struct Mp4Box {
    kind: [u8; 4],
    /// Start of the payload (after the size/type header).
    body: usize,
    end: usize,
}

/// Finds the top-level `moov` box and reads it whole. It is small compared to
/// the audio, so this avoids touching `mdat` at all.
fn read_moov(file: &mut File) -> Result<(u64, Vec<u8>)> {
    let len = file.metadata()?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8])?;
        let size = match u32::from_be_bytes([header[0], header[1], header[2], header[3]]) {
            0 => len - pos,
            1 => {
                file.read_exact(&mut header[8..])?;
                u64::from_be_bytes(header[8..].try_into()?)
            }
            n => u64::from(n),
        };
        if size < 8 {
            bail!("corrupt MP4: box of size {size} at offset {pos}");
        }
        if &header[4..8] == b"moov" {
            let mut buf = vec![0u8; usize::try_from(size)?];
            file.seek(SeekFrom::Start(pos))?;
            file.read_exact(&mut buf)?;
            return Ok((pos, buf));
        }
        pos += size;
    }
    bail!("no moov box found")
}

/// Child boxes within `buf[start..end]`.
fn children(buf: &[u8], start: usize, end: usize) -> Result<Vec<Mp4Box>> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        let (size, header) = match be_u32(buf, pos)? {
            0 => (end - pos, 8),
            1 => (usize::try_from(be_u64(buf, pos + 8)?)?, 16),
            n => (usize::try_from(n)?, 8),
        };
        if size < header || pos + size > end {
            bail!("corrupt MP4: box of size {size} at offset {pos}");
        }
        out.push(Mp4Box {
            kind: buf[pos + 4..pos + 8].try_into()?,
            body: pos + header,
            end: pos + size,
        });
        pos += size;
    }
    Ok(out)
}

fn child<'a>(boxes: &'a [Mp4Box], kind: &[u8; 4]) -> Option<&'a Mp4Box> {
    boxes.iter().find(|b| &b.kind == kind)
}

/// Tracks referenced by another track's `tref/chap`, i.e. chapter tracks.
fn chapter_tracks(moov: &[u8]) -> Result<Vec<ChapterTrack>> {
    let top = children(moov, 8, moov.len())?;
    let mut tracks = Vec::new(); // (track id, mdhd language offset)
    let mut chapter_ids = Vec::new();

    for trak in top.iter().filter(|b| &b.kind == b"trak") {
        let parts = children(moov, trak.body, trak.end)?;
        let tkhd = child(&parts, b"tkhd").ok_or_else(|| anyhow!("trak without tkhd"))?;
        // tkhd: version(1) flags(3) creation/modification (4+4 or 8+8) track_id(4)
        let id_pos = tkhd.body + if moov[tkhd.body] == 1 { 20 } else { 12 };
        let id = be_u32(moov, id_pos)?;

        if let Some(tref) = child(&parts, b"tref") {
            for chap in children(moov, tref.body, tref.end)?
                .iter()
                .filter(|b| &b.kind == b"chap")
            {
                for pos in (chap.body..chap.end).step_by(4) {
                    chapter_ids.push(be_u32(moov, pos)?);
                }
            }
        }

        let mdia = child(&parts, b"mdia").ok_or_else(|| anyhow!("trak without mdia"))?;
        let mdia_parts = children(moov, mdia.body, mdia.end)?;
        let mdhd = child(&mdia_parts, b"mdhd").ok_or_else(|| anyhow!("mdia without mdhd"))?;
        // mdhd: version(1) flags(3) creation/modification/timescale/duration
        // (4+4+4+4 or 8+8+4+8) language(2)
        let lang_pos = mdhd.body + if moov[mdhd.body] == 1 { 32 } else { 20 };
        tracks.push((id, lang_pos));
    }

    Ok(tracks
        .into_iter()
        .filter(|(id, _)| chapter_ids.contains(id))
        .map(|(_, lang_pos)| ChapterTrack { lang_pos })
        .collect())
}

/// ISO 639-2/T code packed as three 5-bit letters, as `mdhd` stores it.
fn pack_language(lang: &str) -> Result<u16> {
    let bytes = lang.as_bytes();
    if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_lowercase) {
        bail!("language must be a three-letter ISO 639-2 code, got {lang:?}");
    }
    Ok(bytes
        .iter()
        .fold(0u16, |acc, &c| (acc << 5) | u16::from(c - 0x60)))
}

fn unpack_language(code: u16) -> String {
    [10, 5, 0]
        .iter()
        .map(|shift| char::from((((code >> shift) & 0x1f) as u8) + 0x60))
        .collect()
}

fn be_u16(buf: &[u8], pos: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(read_n(buf, pos)?))
}

fn be_u32(buf: &[u8], pos: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(read_n(buf, pos)?))
}

fn be_u64(buf: &[u8], pos: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(read_n(buf, pos)?))
}

fn read_n<const N: usize>(buf: &[u8], pos: usize) -> Result<[u8; N]> {
    buf.get(pos..pos + N)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| anyhow!("corrupt MP4: truncated box at offset {pos}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_language_round_trips() {
        let code = pack_language("deu").unwrap();
        assert_eq!(unpack_language(code), "deu");
    }

    #[test]
    fn pack_language_matches_mdhd_encoding_of_eng() {
        // 0x15C7 is the well-known packed value for "eng".
        assert_eq!(pack_language("eng").unwrap(), 0x15C7);
    }

    #[test]
    fn pack_language_rejects_invalid_codes() {
        assert!(pack_language("en").is_err());
    }
}
