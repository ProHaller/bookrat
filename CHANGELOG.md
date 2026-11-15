# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0]

First public release.

- TUI with an animated home screen and folder browser; opens without arguments.
- One book per folder (own files plus `CD1`/`Disc 2` subfolders), with a Files
  panel to include or exclude any file found below the folder.
- Chapters per file or from embedded markers, written as a QuickTime chapter
  track and a Nero `chpl` list, placed sample-exactly.
- Chapter-track language set from tags or locale, so Apple players show the
  chapters.
- Embedded MP3 covers extracted before muxing, so long chapters keep their
  chapter track.
- Gapless single-pass AAC encode that never exceeds the source quality.
- Background conversion queue with parallel jobs, progress, ETA and cancel.
- Output defaults to the book folder; editable output folder and file name.
- Headless mode (`--no-tui`).
