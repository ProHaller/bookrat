# bookrat

[![CI](https://github.com/ProHaller/bookrat/actions/workflows/ci.yml/badge.svg)](https://github.com/ProHaller/bookrat/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![0RWX: AI made](https://0rwx.com/static/AI-X-dark.png)](https://0rwx.com/)

A terminal UI that binds a folder of MP3 parts into one clean, chaptered
**M4B audiobook**: metadata detected and editable, chapters preserved, audio
quality respected.

<img width="1512" height="913" alt="Screenshot 2026-11-15 at 13 48 42" src="https://github.com/user-attachments/assets/40ead36a-a65c-487f-8e10-2ca88f5d16ce" />


## A vibe-coded project

[![0RWX: AI made](https://0rwx.com/static/AI-X-dark.png)](https://0rwx.com/)

bookrat is **vibed**. On the [0RWX framework](https://0rwx.com/) scale it is
level **X (EXECUTE), "AI made"**: the code was written by an AI coding
assistant ([Claude Code](https://claude.com/claude-code)) from the
maintainer's direction, and the maintainer's role is to curate it: setting
goals, testing against real audiobooks, and deciding what ships. It has not
been reviewed line by line by a human.

What that means in practice:

- Behaviour is pinned down by an automated test suite (unit tests, plus
  integration tests that encode real audio and check that every chapter
  holds the right sound) and by CI on Linux and macOS.
- bookrat never modifies your source files. It writes to a temporary
  `.part` file and only renames it into place when the bind succeeds.
- Still, treat it like any young tool: check the result before you delete
  the MP3s it came from.

## Features

- **Pick a folder, get a book.** Start `bookrat` with no arguments and
  browse to a folder. Folders holding audio are marked ♪ with a file count.
- **One book per folder.** A book is the audio files directly in a folder,
  plus disc subfolders (`CD1`, `Disc 2`, ...). Nested folders are other books
  and are never merged in by accident. A Files panel lets you tick any file
  found under the folder in or out.
- **Chapters that work everywhere**
  - one chapter per file, or one per embedded marker (ID3 `CHAP`) when a
    single file carries them;
  - written both as a QuickTime chapter track (Apple Books, QuickTime, iOS)
    and as a Nero `chpl` list (most other players);
  - placed sample-exactly: chapter marks land within a fraction of a
    millisecond of the audio boundary.
- **Respects audio quality**
  - one continuous AAC encode with gapless joins, and no quantization before
    it;
  - never raises the sample rate or bitrate above the source, and stays mono
    when the source is mono;
  - uses the best encoder your ffmpeg has: `aac_at` (macOS), then
    `libfdk_aac`, then ffmpeg's built-in `aac`.
- **Detected, editable metadata.** Title, author, narrator, year, genre,
  description and language come from the tags, falling back to `Author -
  Title` folder names. The cover comes from `cover.jpg`/`folder.jpg` or the
  MP3s' embedded art. Everything is editable before binding.
- **Background conversions.** Queue as many books as you like and keep
  browsing and editing. Several encode in parallel, with progress, ETA and
  cancel in a Conversions panel.
- **Fast.** Probing a folder takes milliseconds (no decoding up front).
  Binding runs at roughly 200× realtime on a modern laptop.

## Requirements

- **ffmpeg and ffprobe** on your `PATH`. bookrat checks at startup and tells
  you if they're missing.
  - macOS: `brew install ffmpeg`
  - Debian/Ubuntu: `sudo apt install ffmpeg`
- To build from source: Rust **1.89** or newer.

Tested on macOS and Linux. Windows may work but is untested.

## Install

Prebuilt binaries for macOS and Linux are attached to each
[GitHub release](https://github.com/ProHaller/bookrat/releases).

With Cargo:

```sh
cargo install --git https://github.com/ProHaller/bookrat
```

From a clone:

```sh
git clone https://github.com/ProHaller/bookrat
cd bookrat
cargo install --path .
```

## Usage

```sh
bookrat                         # open the folder browser in the current folder
bookrat ~/Audiobooks/Dune       # open a book straight away
bookrat ~/Audiobooks/Dune --no-tui              # bind without the TUI
bookrat ~/Audiobooks/Dune --no-tui -o ~/Books/  # ...into another folder
```

| Option | Meaning |
|---|---|
| `[DIR]` | Book folder to open. Without it, the TUI starts on the folder browser. |
| `-o, --output <PATH>` | Output file, or a folder to put it in. A folder applies to every book opened in the session. |
| `-b, --bitrate <KBPS>` | Override the AAC bitrate. |
| `-e, --encoder <aac_at\|libfdk_aac\|aac>` | Override the encoder. |
| `-j, --jobs <N>` | Conversions to run at once (default: half the CPU cores, at most 4). |
| `--no-tui` | Bind immediately with the detected settings and print progress. |

<img width="1512" height="913" alt="Screenshot 2026-11-15 at 13 49 36" src="https://github.com/user-attachments/assets/5e0e576b-34e1-4761-adac-b8384fe36489" />

### Keys

**Everywhere:** `Tab` moves between panels, including the Conversions panel
once something is queued. `Ctrl+C` quits; if conversions are still running it
asks first, then stops them cleanly.

**Folder browser**

| Key | Action |
|---|---|
| `↑` `↓` / `j` `k` | Move |
| `Enter` | Open a book folder (♪), or browse into any other folder |
| `→` / `l` | Browse into a folder |
| `o` / `Space` | Open the highlighted folder as a book, whatever it contains |
| `←` / `h` / `Backspace` | Up one folder |
| `~` | Home folder |
| `.` | Show or hide hidden folders |
| `q` | Quit |

**Book screen**

| Panel | Key | Action |
|---|---|---|
| Chapters | `Enter` | Rename chapter |
| | `J` / `K` | Move chapter down / up |
| | `d` | Remove chapter |
| Files | `Space` | Include or exclude the file |
| | `a` | Include or exclude its whole folder |
| Metadata | `Enter` | Edit field (`Enter` saves, `Esc` cancels, `Ctrl+U` clears) |
| any | `b` | Queue the bind |
| any | `Esc` / `q` | Back to the browser (queued conversions keep running) |

**Conversions panel:** `c` cancels the selected conversion, `x` clears the
finished ones.

### Where the M4B goes

By default the M4B goes into the **book folder itself**, next to its parts,
named `<Author> - <Title>.m4b`. The editor's **Output dir** and **File name**
fields change that. Default values are shown in italics.

- **Output dir:** relative paths start from the book folder (`output` is a
  subfolder, `..` its parent), and `~` is your home folder. Leave it empty
  to go back to the default.
- **File name:** `.m4b` is added if you leave the extension off. Typing a
  full path sets both fields.

If you bind the same folder again, an `.m4b` already sitting beside the parts
is listed in the Files panel but not ticked, so an earlier result is never
bound in as an extra chapter.

## How it works

1. **Probe.** ffprobe reads each file's format, tags, embedded chapters and
   cover. Nothing is decoded yet, so this is fast.
2. **Decode.** Each part is decoded by its own ffmpeg process to 32-bit
   float PCM at the target sample rate and channel count, and streamed in
   order into a single encoder. Mixed sources (different sample rates,
   mono and stereo) join seamlessly, and MP3 encoder padding is trimmed for
   gapless playback.
3. **Measure.** Chapter lengths are counted from the samples actually fed to
   the encoder, so chapter marks match the audio exactly, whatever the MP3
   headers claim.
4. **Mux.** A stream-copy remux adds the chapters, tags (the media kind is
   set to Audiobook) and cover to the encoded audio.
5. **Fix up.** Two ffmpeg quirks would hide the chapters in Apple players,
   and bookrat corrects both:
   - ffmpeg labels the chapter track's language "undetermined", and Apple
     players only show chapters whose language matches yours. bookrat sets
     it to the book's language (editable; taken from the tags, then your
     locale, then English).
   - A cover embedded in an MP3, muxed directly, makes ffmpeg drop the
     chapter track whenever a chapter is longer than about eight minutes.
     bookrat extracts the cover to a standalone image first.

## Troubleshooting

- **No chapters in Apple Books.** Check the book's **Language** field.
  Apple only shows chapters whose language is one of your device's
  preferred languages.
- **Only one chapter.** The book is a single file with no embedded chapter
  markers. Detecting chapters from silences is on the [roadmap](ROADMAP.md).
- **"no audio files directly in ..."** You pointed `--no-tui` at a library
  rather than a book. Point it at one book's folder, or use the TUI and tick
  files in the Files panel.

## Development

```sh
cargo test                                   # needs ffmpeg for the integration tests
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

The integration tests in `tests/bind.rs` generate audio with ffmpeg (sine
tones of distinct frequencies), bind it, and check the chapters, the chapter
track language, and which tone each chapter actually contains. They skip
themselves when ffmpeg isn't installed.

| Path | What |
|---|---|
| `src/scan.rs` | Finding a book's files, disc folders, covers |
| `src/probe.rs` | ffprobe wrapper |
| `src/book.rs` | Book model: chapters, file selection, metadata detection, output location |
| `src/encode.rs` | The decode → encode → remux pipeline |
| `src/mp4.rs` | Minimal MP4 box walker for the chapter-language fix |
| `src/lang.rs` | Language code normalization |
| `src/tui/` | Home screen, folder browser, book editor, conversion queue |

See [ROADMAP.md](ROADMAP.md) for what's planned.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

The 0RWX badge is by [Namtao Productions](https://0rwx.com/), released into
the public domain (CC0).
