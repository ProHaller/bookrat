use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use clap::Parser;

use bookrat::book::Book;
use bookrat::encode::{self, Encoder};
use bookrat::{probe, tui};

/// Bind a folder of MP3s into a single M4B audiobook.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Folder containing the audiobook's audio files. Without it, the TUI
    /// opens on a folder browser.
    dir: Option<PathBuf>,
    /// Output file, or folder to put it in (default: "<Author> - <Title>.m4b"
    /// inside the book folder, next to its parts).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Override the AAC bitrate in kbps.
    #[arg(short, long)]
    bitrate: Option<u32>,
    /// Override the AAC encoder (default: best available).
    #[arg(short, long)]
    encoder: Option<Encoder>,
    /// Bind immediately with detected metadata, without the TUI.
    #[arg(long, requires = "dir")]
    no_tui: bool,
    /// Conversions to run at the same time in the TUI (default: half the
    /// CPU cores, at most 4).
    #[arg(short, long)]
    jobs: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    probe::check_tools()?;

    if args.no_tui {
        return headless(args);
    }
    tui::run(tui::Options {
        dir: args.dir,
        output: args.output,
        bitrate: args.bitrate,
        encoder: args.encoder,
        max_parallel: args.jobs.unwrap_or_else(tui::default_parallelism),
    })
}

fn headless(args: Args) -> Result<()> {
    let dir = args.dir.context("a folder is required with --no-tui")?;
    let mut book = Book::load(&dir, |done, total| {
        eprint!("\rprobing {done}/{total}");
    })?;
    eprintln!();
    if book.chapters.is_empty() {
        bail!(
            "no audio files directly in {} (or in CD/Disc subfolders); point at a single book's folder",
            dir.display()
        );
    }
    if let Some(output) = &args.output {
        book.set_output_path(&std::env::current_dir()?.join(output));
    }
    if let Some(kbps) = args.bitrate {
        book.target.bitrate_kbps = kbps;
    }
    if let Some(e) = args.encoder {
        book.target.encoder = e;
    }

    let out = book.output_path();
    eprintln!("title:    {}", book.meta.title);
    eprintln!("author:   {}", book.meta.author);
    eprintln!("narrator: {}", book.meta.narrator);
    eprintln!("language: {}", book.meta.language);
    eprintln!("chapters: {}", book.chapters.len());
    eprintln!("cover:    {}", book.cover.describe());
    eprintln!("audio:    {}", book.target.describe());
    for w in book.warnings() {
        eprintln!("warning:  {w}");
    }
    eprintln!("output:   {}", out.display());

    let mut shown = -1;
    encode::bind(&book, &out, &AtomicBool::new(false), |p| {
        let tenths = (p * 1000.0) as i32;
        if tenths != shown {
            shown = tenths;
            eprint!("\rencoding {:5.1}%", p * 100.0);
            let _ = std::io::stderr().flush();
        }
    })?;
    eprintln!("\ndone");
    Ok(())
}
