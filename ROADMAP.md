# bookrat — Roadmap

A Ratatui TUI that binds a folder of MP3s into a single, clean M4B audiobook:
chapters preserved, metadata auto-detected and editable, audio quality respected.

## Guiding principles

- **Never make the audio worse than it has to be.** No upsampling, no bitrate
  inflation, one single encode pass (no MP3 → intermediate → AAC chains).
- **Chapters are first-class.** Every chapter boundary is derived from exact
  decoded sample counts, not from (often wrong) MP3 header durations.
- **Detect, then let the user decide.** Auto-detection proposes; the TUI is
  where the user reviews and fixes before anything is written.
- **Non-destructive.** Source files are never modified. Output is written to a
  temp file and atomically renamed on success.

## Key technical decisions

| Concern | Choice | Rationale |
|---|---|---|
| TUI | `ratatui` + `crossterm` | Target framework; crossterm works on macOS/Linux/Windows |
| Probing & tags | `ffprobe` (JSON output) | Codec, sample rate, channels, bitrate, tags, embedded cover. `lofty` later for ID3 `CHAP` frames |
| Exact durations | counted from the PCM fed to the encoder during the bind | Probing is ffprobe-only (~38× faster than decoding); the encoder writes audio-only, then a stream-copy remux adds the exact chapters |
| Decode | one `ffmpeg` per file → f32 PCM at target rate/channels, piped through bookrat | Sample-exact timeline even with mixed sources (concat demuxer leaked decoder delay and glitched on format changes) |
| Encode | single `ffmpeg` reading PCM from stdin | One AAC encode, gapless joins, no intermediate quantization |
| AAC encoder | `aac_at` (macOS) → `libfdk_aac` → native `aac` | Picked by availability probe, overridable |
| M4B tags & chapters | FFMETADATA file fed to the encoder | ffmpeg writes iTunes atoms, `stik`=Audiobook, cover, QuickTime chapter track + Nero `chpl`. bookrat then patches the chapter track's `mdhd` language (ffmpeg leaves it `und`, which makes Apple players hide all chapters). Covers embedded in MP3s are extracted first: muxed directly they push the chapter track timescale so high that chapters over ~8 min overflow and the chapter track is dropped |
| Async/work | background worker thread(s) + channels | Keep UI responsive during probe/encode/network |
| Online metadata | `reqwest` + `serde` | Audible (via Audnexus), Open Library, Google Books |
| Config | `serde` + `toml`, `directories` | User defaults (output dir, encoder, naming template) |
| Errors/logging | `anyhow`/`thiserror`, `tracing` to file | Logs must not corrupt the TUI |

`ffmpeg`/`ffprobe` are an external runtime dependency; bookrat checks for them
at startup and explains how to install them if missing.

---

## Milestone 0 — Foundations

- [x] Crate layout: `scan`, `probe`, `book` (model + detection), `encode`, `tui`
- [x] Event loop: input events + worker messages over a channel
- [x] Terminal setup/teardown with panic hook that restores the terminal
- [ ] `tracing` to a log file; `--verbose` flag
- [x] Detect `ffmpeg`/`ffprobe` and the best available AAC encoder
- [ ] CI: `cargo fmt`, `clippy -D warnings`, tests on macOS + Linux

**Exit:** app launches, shows an empty shell, exits cleanly, reports ffmpeg status.

## Milestone 1 — Headless core (CLI path, no TUI yet)

Build the pipeline first so the TUI is just a front-end to it.

- [x] **Scan:** given a folder, collect audio files; natural sort
      (`Chapter 2` before `Chapter 10`), recursive into `CD1/`, `CD2/`
- [ ] Sort by track/disc tag when present
- [x] **Probe:** `ffprobe` each file in parallel → duration (from decoded
      stream), sample rate, channels, bitrate, codec
- [x] **Chapters (basic):** one chapter per file, title from ID3 title → filename
      → `subfolder / filename` when names repeat
- [x] **Encode:** PCM pipeline → single AAC stream in `.m4b`
      (`-movflags +faststart`), progress from bytes fed, `.part` + atomic rename
- [x] **Tag:** write title/author/narrator/album/year/genre/description/cover
      and chapter list with `mp4ameta`
- [x] `bookrat <dir> --no-tui [-o out.m4b] [-b kbps] [-e encoder]`
- [x] Integration tests (`tests/bind.rs`) with generated sine-tone fixtures:
      chapter order/titles, audio-in-the-right-chapter (Goertzel), chapter
      track language, embedded chapters, reordered slices
- [x] Chapter track language set from tags → locale → `eng`; editable in the TUI
- [ ] CI check with AVFoundation on macOS (`loadChapterMetadataGroups`), the
      same API Books/QuickTime use

**Exit:** a folder of MP3s becomes a playable M4B with correct chapters in
Apple Books, VLC, and a phone player (e.g. Smart AudioBook Player / BookPlayer).

## Milestone 2 — Audio quality

- [ ] **Quality analysis:** (partial: mixed rate/channel warnings) summarize sources (min/max/median bitrate, sample
      rates, channel layouts, VBR/CBR) and flag mismatches
- [x] **Target selection:**
  - sample rate = most common source rate (never higher than max source)
  - channels = mono if all sources are mono (or near-identical L/R)
  - bitrate = matched to source with sane caps (e.g. ~64 kbps mono /
    ~96–128 kbps stereo for spoken word), never above source-equivalent
- [ ] Presets: *Transparent*, *Balanced* (default), *Small*, plus custom
- [ ] **Stream copy mode** when sources are already AAC (M4A/M4B input)
- [x] Gapless joins: single continuous encode, so no per-file AAC priming gaps
- [ ] Optional loudness normalization (`loudnorm`, two-pass) — off by default
- [x] Estimated output size shown before encoding

**Exit:** output is measurably no worse than source at a reasonable size;
no clicks/gaps at file boundaries.

## Milestone 3 — TUI v1

> Status: the TUI starts on a home screen (animated mascot + folder browser
> with audio counts) when run without a folder. The book screen offers
> chapter rename/reorder/remove, metadata, details/warnings. Binds are queued
> as background jobs, several in parallel (`--jobs`), with a conversions panel
> for progress/ETA/cancel, so browsing and editing continue while encoding.

Screens (tabbed or wizard-style, `Tab`/number keys to switch):

1. **Source** — folder picker / path input, file list with duration and
   format, reorder (`J`/`K`), include/exclude, warnings column
2. **Metadata** — form with editable fields, cover preview, source of each
   value shown (ID3, folder name, online, manual)
3. **Chapters** — table of index/title/start/duration; rename inline, merge,
   split, delete, bulk rename (`Chapter {n}` templates)
4. **Output** — quality preset, encoder, output path/filename template, size
   estimate
5. **Bind** — progress bar, ETA, speed, live log tail, cancel (`Ctrl+C` stops
   ffmpeg cleanly and removes partial output)

Cross-cutting:

- [ ] Text input widget (e.g. `tui-input`) with cursor, paste, undo
- [ ] Keybinding help overlay (`?`), consistent vim + arrow-key navigation
- [ ] Validation panel: blocks binding on errors, warns on oddities
- [ ] Responsive layout for small terminals (≥ 80×24)
- [ ] Cover art preview via `ratatui-image` (Kitty/iTerm2/Sixel, halfblock fallback)

**Exit:** a non-technical user can bind a book end-to-end without the CLI.

## Milestone 4 — Smart metadata detection

Local heuristics, merged with confidence scores:

- [ ] ID3 tags: majority vote across files for album/artist/year/genre
- [ ] Composer / `TPE2` / comments → narrator heuristics
- [ ] Folder structure: `Author/Series/## - Title`, `Author - Title (Year)`,
      `Title [Narrator]` etc. via a configurable pattern list
- [ ] Series name + index detection (`Book 3`, `#3`, `Vol. 3`)
- [ ] Cover: embedded APIC → `cover.jpg`/`folder.jpg` in directory → online
- [ ] Strip noise from titles (`[MP3]`, `(Unabridged)`, bitrates, release-group tags)

Online lookup (opt-in, cached on disk):

- [ ] Search by title/author → candidate list in the TUI; user picks one
- [ ] Providers: Audnexus/Audible (ASIN, narrator, series, chapters),
      Open Library, Google Books
- [ ] Field-by-field merge: user picks which fields to accept from a match
- [ ] Rate limiting, timeouts, offline-first behaviour

**Exit:** for a typical well-named folder, ≥ 80% of fields are correct
without manual edits.

## Milestone 5 — Advanced chapters

- [x] Read embedded chapters from sources (ID3 `CHAP`, M4B chapters) and
      offset them into the combined timeline; contiguous slices decode once
- [ ] Import chapters from Audnexus (by ASIN) with drift correction
- [ ] **Silence detection** (`silencedetect`) to propose boundaries when one
      long file contains many chapters; tunable threshold/min duration
- [ ] Snap proposed boundaries to nearest silence
- [ ] Import/export chapters as CUE, simple text (`00:00:00 Title`), or JSON
- [ ] Audio preview: play a few seconds around a boundary (`ffplay` or `rodio`)

**Exit:** chapters are right for single-file, per-chapter-file, and
multi-part-per-chapter books.

## Milestone 6 — Batch & workflow

- [x] One book per folder by default (own files + `CD1/`/`Disc 2/` subfolders);
      nested folders are other books. Files panel to include/exclude any file
      found below the opened folder
- [ ] Library mode: scan a parent folder, detect multiple books, queue them
- [x] Job queue with per-book status, parallel conversions, cancel
- [ ] Resume a failed/cancelled job
- [ ] Output naming/organizing templates (`{author}/{series}/{title}.m4b`)
- [ ] Save/load project files (`.bookrat.toml`) so edits survive restarts
- [ ] Optional: delete/move sources after successful, verified bind
- [ ] Post-bind verification: re-probe output, compare duration and chapter count

## Milestone 7 — Polish & release

- [ ] Theming (light/dark, respects `NO_COLOR`)
- [ ] Mouse support for lists and tables
- [ ] Config screen for defaults
- [ ] Man page / `--help`, README with screenshots/asciinema
- [ ] Release binaries via `cargo-dist`; Homebrew tap; `cargo install`
- [ ] 1.0: stable CLI flags and project-file format

---

## Open questions

- **Bundle ffmpeg or require it?** Requiring it keeps the binary small and
  licensing simple (fdk-aac is not redistributable); revisit if install
  friction is high.
- **Chapter atom format:** both QuickTime track and Nero `chpl` are written;
  verified in AVFoundation. Still to check on Android players and VLC.
- **Multilingual chapter lookup:** Apple matches the chapter track language
  against the user's preferred languages only. A German book on an
  English-only device shows no chapters — add a second chapter track per
  extra language?
- **Large books (> 4 GB / > 24 h):** verify `co64` handling and player limits.
- **Encoder parity:** `aac_at` is macOS-only; quantify quality gap vs native
  `aac` on Linux at spoken-word bitrates to pick sensible defaults per platform.

## Out of scope (for now)

- DRM removal of any kind
- Editing audio content (trimming, noise reduction)
- Formats other than M4B as output
