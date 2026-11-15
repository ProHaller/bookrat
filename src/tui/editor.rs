//! The book screen: review and edit detected chapters and metadata, then
//! queue the bind.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Gauge, Paragraph, Row, Table, TableState, Wrap};

use super::{highlight, hms, panel};
use crate::book::Book;
use crate::lang;

pub enum Phase {
    Loading { done: usize, total: usize },
    LoadFailed(String),
    Ready,
    ConfirmOverwrite,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Chapters,
    Files,
    Fields,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Author,
    Narrator,
    Year,
    Genre,
    Description,
    Language,
    OutputDir,
    OutputName,
    Bitrate,
}

const FIELDS: [Field; 10] = [
    Field::Title,
    Field::Author,
    Field::Narrator,
    Field::Year,
    Field::Genre,
    Field::Description,
    Field::Language,
    Field::OutputDir,
    Field::OutputName,
    Field::Bitrate,
];

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Title => "Title",
            Field::Author => "Author",
            Field::Narrator => "Narrator",
            Field::Year => "Year",
            Field::Genre => "Genre",
            Field::Description => "Description",
            Field::Language => "Language",
            Field::OutputDir => "Output dir",
            Field::OutputName => "File name",
            Field::Bitrate => "Bitrate kbps",
        }
    }

    fn get(self, book: &Book) -> String {
        let m = &book.meta;
        match self {
            Field::Title => m.title.clone(),
            Field::Author => m.author.clone(),
            Field::Narrator => m.narrator.clone(),
            Field::Year => m.year.clone(),
            Field::Genre => m.genre.clone(),
            Field::Description => m.description.clone(),
            Field::Language => m.language.clone(),
            Field::OutputDir => book.output_dir().display().to_string(),
            Field::OutputName => book.output_name(),
            Field::Bitrate => book.target.bitrate_kbps.to_string(),
        }
    }

    fn set(self, book: &mut Book, value: String) -> Result<(), String> {
        let m = &mut book.meta;
        match self {
            Field::Title => m.title = value,
            Field::Author => m.author = value,
            Field::Narrator => m.narrator = value,
            Field::Year => m.year = value,
            Field::Genre => m.genre = value,
            Field::Description => m.description = value,
            Field::Language => match lang::normalize(&value) {
                Some(code) => m.language = code,
                None => {
                    return Err(
                        "language must be an ISO 639 code or name, e.g. eng, de, French".into(),
                    );
                }
            },
            Field::OutputDir => book.set_output_dir(&value)?,
            Field::OutputName => book.set_output_name(&value)?,
            Field::Bitrate => match value.trim().parse::<u32>() {
                Ok(k) if (16..=320).contains(&k) => book.target.bitrate_kbps = k,
                _ => return Err("bitrate must be a number between 16 and 320".into()),
            },
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum EditTarget {
    Field(Field),
    Chapter(usize),
}

/// A single-line text input.
struct Input {
    target: EditTarget,
    buf: Vec<char>,
    cursor: usize,
}

pub enum EditorAction {
    None,
    Back,
    /// Queue a bind of this snapshot of the book.
    Bind(Box<Book>),
}

pub struct BookScreen {
    pub dir: PathBuf,
    /// Matches load results to the load that started them.
    pub load_id: u64,
    pub book: Option<Book>,
    pub phase: Phase,
    pub focus: Focus,
    chapters: TableState,
    files: TableState,
    field: usize,
    input: Option<Input>,
    /// Transient message for the status bar (validation errors, queued, ...).
    pub notice: Option<String>,
}

impl BookScreen {
    pub fn loading(dir: PathBuf, load_id: u64) -> BookScreen {
        BookScreen {
            dir,
            load_id,
            book: None,
            phase: Phase::Loading { done: 0, total: 0 },
            focus: Focus::Chapters,
            chapters: TableState::default().with_selected(0),
            files: TableState::default().with_selected(0),
            field: 0,
            input: None,
            notice: None,
        }
    }

    pub fn loaded(&mut self, result: Result<Box<Book>, String>) {
        match result {
            Ok(book) => {
                self.book = Some(*book);
                self.phase = Phase::Ready;
            }
            Err(e) => self.phase = Phase::LoadFailed(e),
        }
    }

    /// Whether keystrokes are going into a text input.
    pub fn is_typing(&self) -> bool {
        self.input.is_some()
    }

    /// Moves focus to the next panel. Returns `false` when focus would wrap
    /// around, so the caller can hand it to the jobs panel instead.
    pub fn focus_next(&mut self) -> bool {
        let (next, wrapped) = match self.focus {
            Focus::Chapters => (Focus::Files, false),
            Focus::Files => (Focus::Fields, false),
            Focus::Fields => (Focus::Chapters, true),
        };
        self.focus = next;
        !wrapped
    }

    pub fn on_key(&mut self, key: KeyEvent) -> EditorAction {
        if self.input.is_some() {
            self.on_input_key(key);
            return EditorAction::None;
        }
        match self.phase {
            Phase::Loading { .. } | Phase::LoadFailed(_) => {
                return match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => EditorAction::Back,
                    _ => EditorAction::None,
                };
            }
            Phase::ConfirmOverwrite => {
                self.phase = Phase::Ready;
                return match key.code {
                    KeyCode::Char('y') => self.bind_now(),
                    _ => EditorAction::None,
                };
            }
            Phase::Ready => {}
        }

        self.notice = None;
        match (self.focus, key.code) {
            (_, KeyCode::Esc | KeyCode::Char('q')) => return EditorAction::Back,
            (_, KeyCode::Down | KeyCode::Char('j')) => self.move_cursor(1),
            (_, KeyCode::Up | KeyCode::Char('k')) => self.move_cursor(-1),
            (_, KeyCode::Char('b')) => return self.request_bind(),
            (Focus::Files, KeyCode::Char(' ') | KeyCode::Enter) => self.toggle_file(false),
            (Focus::Files, KeyCode::Char('a')) => self.toggle_file(true),
            (_, KeyCode::Enter | KeyCode::Char('e')) => self.begin_edit(),
            (Focus::Chapters, KeyCode::Char('J')) => self.move_chapter(1),
            (Focus::Chapters, KeyCode::Char('K')) => self.move_chapter(-1),
            (Focus::Chapters, KeyCode::Char('d') | KeyCode::Delete) => {
                if let (Some(book), Some(i)) = (&mut self.book, self.chapters.selected()) {
                    book.remove_chapter(i);
                }
                self.clamp_chapter_selection();
            }
            _ => {}
        }
        EditorAction::None
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        let Some(ed) = self.input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => self.commit_edit(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ed.buf.clear();
                ed.cursor = 0;
            }
            KeyCode::Char(c) => {
                ed.buf.insert(ed.cursor, c);
                ed.cursor += 1;
            }
            KeyCode::Backspace if ed.cursor > 0 => {
                ed.cursor -= 1;
                ed.buf.remove(ed.cursor);
            }
            KeyCode::Delete if ed.cursor < ed.buf.len() => {
                ed.buf.remove(ed.cursor);
            }
            KeyCode::Left => ed.cursor = ed.cursor.saturating_sub(1),
            KeyCode::Right => ed.cursor = (ed.cursor + 1).min(ed.buf.len()),
            KeyCode::Home => ed.cursor = 0,
            KeyCode::End => ed.cursor = ed.buf.len(),
            _ => {}
        }
    }

    /// Flips the highlighted file in or out of the book; with `folder`, does
    /// the same to every file in its folder.
    fn toggle_file(&mut self, folder: bool) {
        let (Some(book), Some(i)) = (&mut self.book, self.files.selected()) else {
            return;
        };
        let Some(include) = book.files.get(i).map(|f| !f.included) else {
            return;
        };
        if folder {
            book.set_folder_included(i, include);
        } else {
            book.set_included(i, include);
        }
        self.clamp_chapter_selection();
    }

    fn clamp_chapter_selection(&mut self) {
        let len = self.book.as_ref().map_or(0, |b| b.chapters.len());
        let i = self.chapters.selected().unwrap_or(0);
        self.chapters.select(Some(i.min(len.saturating_sub(1))));
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Focus::Files => {
                let len = self.book.as_ref().map_or(0, |b| b.files.len());
                let i = self.files.selected().unwrap_or(0);
                self.files.select(Some(
                    i.saturating_add_signed(delta).min(len.saturating_sub(1)),
                ));
            }
            Focus::Chapters => {
                let len = self.book.as_ref().map_or(0, |b| b.chapters.len());
                if len > 0 {
                    let i = self.chapters.selected().unwrap_or(0);
                    self.chapters
                        .select(Some(i.saturating_add_signed(delta).min(len - 1)));
                }
            }
            Focus::Fields => {
                self.field = self
                    .field
                    .saturating_add_signed(delta)
                    .min(FIELDS.len() - 1);
            }
        }
    }

    fn move_chapter(&mut self, delta: isize) {
        let (Some(book), Some(i)) = (&mut self.book, self.chapters.selected()) else {
            return;
        };
        let j = i.saturating_add_signed(delta);
        if j < book.chapters.len() && j != i {
            book.chapters.swap(i, j);
            self.chapters.select(Some(j));
        }
    }

    fn begin_edit(&mut self) {
        let Some(book) = &self.book else { return };
        let (target, value) = match self.focus {
            Focus::Files => return,
            Focus::Fields => {
                let f = FIELDS[self.field];
                (EditTarget::Field(f), f.get(book))
            }
            Focus::Chapters => {
                let Some(ch) = self
                    .chapters
                    .selected()
                    .and_then(|i| book.chapters.get(i).map(|c| (i, c)))
                else {
                    return;
                };
                (EditTarget::Chapter(ch.0), ch.1.title.clone())
            }
        };
        let buf: Vec<char> = value.chars().collect();
        self.input = Some(Input {
            target,
            cursor: buf.len(),
            buf,
        });
    }

    fn commit_edit(&mut self) {
        let (Some(ed), Some(book)) = (self.input.take(), &mut self.book) else {
            return;
        };
        let value: String = ed.buf.iter().collect();
        match ed.target {
            EditTarget::Chapter(i) => {
                if let Some(ch) = book.chapters.get_mut(i) {
                    ch.title = value;
                }
            }
            EditTarget::Field(f) => {
                if let Err(e) = f.set(book, value) {
                    self.notice = Some(e);
                }
            }
        }
    }

    fn request_bind(&mut self) -> EditorAction {
        let Some(book) = &self.book else {
            return EditorAction::None;
        };
        if book.chapters.is_empty() {
            self.notice = Some("Nothing to bind: select files in the Files panel (Space).".into());
            return EditorAction::None;
        }
        if book.output_path().exists() {
            self.phase = Phase::ConfirmOverwrite;
            EditorAction::None
        } else {
            self.bind_now()
        }
    }

    fn bind_now(&mut self) -> EditorAction {
        match &self.book {
            Some(book) => EditorAction::Bind(Box::new(book.clone())),
            None => EditorAction::None,
        }
    }

    pub fn help(&self) -> &'static str {
        if self.input.is_some() {
            return " Enter save · Esc cancel · ←/→ move · Ctrl+U clear";
        }
        match self.phase {
            Phase::Loading { .. } | Phase::LoadFailed(_) => " Esc back",
            Phase::ConfirmOverwrite => " y overwrite · any other key cancels",
            Phase::Ready if self.focus == Focus::Chapters => {
                " Tab next panel · ↑↓ move · Enter rename · J/K reorder · d remove · b bind · Esc back"
            }
            Phase::Ready if self.focus == Focus::Files => {
                " Tab next panel · ↑↓ move · Space include/exclude file · a whole folder · b bind · Esc back"
            }
            Phase::Ready => " Tab next panel · ↑↓ move · Enter edit · b bind · Esc back",
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let Some(book) = &self.book else {
            let text = match &self.phase {
                Phase::LoadFailed(_) => "Could not load this folder.",
                _ => "Reading audio files…",
            };
            let p = Paragraph::new(text)
                .block(Block::bordered().title(format!(" {} ", self.dir.display())));
            f.render_widget(p, area);
            return;
        };
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(area);
        let files_height = (book.files.len().min(8) as u16 + 2).min(left.height / 2);
        let [left, files_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(files_height)]).areas(left);
        let [form, details] = Layout::vertical([
            Constraint::Length(FIELDS.len() as u16 + 2),
            Constraint::Min(3),
        ])
        .areas(right);
        let active = |focus| focused && self.focus == focus && self.input.is_none();
        draw_chapters(f, left, book, &mut self.chapters, active(Focus::Chapters));
        draw_files(f, files_area, book, &mut self.files, active(Focus::Files));
        draw_fields(f, form, book, self.field, active(Focus::Fields));
        draw_details(f, details, book);
    }

    /// Draws the status bar. Falls back to `fallback` (e.g. a job finishing)
    /// when the screen itself has nothing to say.
    pub fn draw_status(&self, f: &mut Frame, area: Rect, fallback: Option<&str>) {
        if let Some(ed) = &self.input {
            let label = match ed.target {
                EditTarget::Field(field) => field.label().to_string(),
                EditTarget::Chapter(i) => format!("Chapter {}", i + 1),
            };
            let block = Block::bordered()
                .title(format!(" Edit {label} "))
                .border_style(Style::new().yellow());
            let inner = block.inner(area);
            // Scroll horizontally so the cursor stays visible.
            let width = inner.width.saturating_sub(1) as usize;
            let skip = ed.cursor.saturating_sub(width);
            let text: String = ed.buf.iter().skip(skip).collect();
            f.render_widget(Paragraph::new(text).block(block), area);
            f.set_cursor_position((inner.x + (ed.cursor - skip) as u16, inner.y));
            return;
        }

        let block = Block::bordered();
        if let Phase::Loading { done, total } = self.phase {
            let ratio = if total == 0 {
                0.0
            } else {
                done as f64 / total as f64
            };
            let gauge = Gauge::default()
                .block(block.title(" Analyzing "))
                .gauge_style(Style::new().cyan())
                .ratio(ratio)
                .label(format!("probing {done}/{total} files"));
            f.render_widget(gauge, area);
            return;
        }
        let line = match (&self.phase, &self.notice, fallback) {
            (Phase::LoadFailed(e), _, _) => Line::from(e.clone()).red(),
            (Phase::ConfirmOverwrite, _, _) => Line::from("Output file exists. Overwrite? (y/n)")
                .yellow()
                .bold(),
            (_, Some(n), _) => Line::from(n.clone()).yellow(),
            (_, None, Some(msg)) => Line::from(msg.to_string()),
            _ => Line::from("Review metadata and chapters, then press b to bind."),
        };
        f.render_widget(
            Paragraph::new(line).block(block).wrap(Wrap { trim: true }),
            area,
        );
    }
}

fn draw_chapters(f: &mut Frame, area: Rect, book: &Book, state: &mut TableState, focused: bool) {
    let mut start = 0.0;
    let rows: Vec<Row> = book
        .chapters
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let row = Row::new(vec![
                Cell::from(format!("{:>3}", i + 1)),
                Cell::from(ch.title.clone()),
                Cell::from(hms(start)),
                Cell::from(hms(ch.duration())),
            ]);
            start += ch.duration();
            row
        })
        .collect();
    let title = format!(
        " Chapters ({}) · {} ",
        book.chapters.len(),
        hms(book.duration())
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(8),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(["  #", "Title", "Start", "Length"]).bold())
    .block(panel(title, focused))
    .row_highlight_style(highlight(focused));
    f.render_stateful_widget(table, area, state);
}

fn draw_files(f: &mut Frame, area: Rect, book: &Book, state: &mut TableState, focused: bool) {
    let rows = book.files.iter().map(|file| {
        let (mark, style) = if file.included {
            ("[x]", Style::new())
        } else {
            ("[ ]", Style::new().dark_gray())
        };
        Row::new(vec![
            Cell::from(mark),
            Cell::from(file.rel.clone()),
            Cell::from(hms(file.source.duration)),
        ])
        .style(style)
    });
    let included = book.files.iter().filter(|f| f.included).count();
    let title = format!(" Files · {included} of {} in this book ", book.files.len());
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(8),
        ],
    )
    .block(panel(title, focused))
    .row_highlight_style(highlight(focused));
    f.render_stateful_widget(table, area, state);
}

fn draw_fields(f: &mut Frame, area: Rect, book: &Book, selected: usize, focused: bool) {
    let rows = FIELDS.iter().map(|&field| {
        let value = field.get(book);
        let is_default = match field {
            Field::OutputDir => book.output_dir.is_none(),
            Field::OutputName => book.output_name.is_none(),
            _ => false,
        };
        // Defaults follow the metadata and book location; italics say so.
        let value = if is_default {
            Span::from(value).italic()
        } else {
            Span::from(value)
        };
        Row::new(vec![
            Cell::from(field.label()).dark_gray(),
            Cell::from(value),
        ])
    });
    let mut state = TableState::default().with_selected(selected);
    let table = Table::new(rows, [Constraint::Length(12), Constraint::Fill(1)])
        .block(panel(" Metadata ".into(), focused))
        .row_highlight_style(highlight(focused));
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_details(f: &mut Frame, area: Rect, book: &Book) {
    let Some(first) = book.chapters.first().map(|c| &c.source) else {
        return;
    };
    let src_kbps = first
        .bit_rate
        .map_or("?".into(), |b| (b / 1000).to_string());
    let label = |s: &'static str| Span::from(format!("{s:<9}")).dark_gray();
    let mut lines = vec![
        Line::from(vec![
            label("Source"),
            format!(
                "{} {src_kbps} kbps, {} ch, {:.1} kHz",
                first.codec,
                first.channels,
                f64::from(first.sample_rate) / 1000.0
            )
            .into(),
        ]),
        Line::from(vec![label("Output"), book.target.describe().into()]),
        Line::from(vec![
            label("Size"),
            format!("≈ {}", human_size(book.estimated_size_bytes())).into(),
        ]),
        Line::from(vec![label("Cover"), book.cover.describe().into()]),
    ];
    for w in book.warnings() {
        lines.push(Line::from(format!("⚠ {w}")).yellow());
    }
    let p = Paragraph::new(lines)
        .block(
            Block::bordered()
                .title(" Details ")
                .border_style(Style::new().dark_gray()),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::probe::SourceFile;
    use std::path::Path;

    fn source(name: &str, duration: f64) -> SourceFile {
        SourceFile {
            path: PathBuf::from(format!("/books/Dune/{name}.mp3")),
            duration,
            codec: "mp3".into(),
            sample_rate: 44_100,
            channels: 2,
            bit_rate: Some(128_000),
            has_cover: false,
            tags: [("album", "Dune"), ("artist", "Frank Herbert")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into(),
            markers: Vec::new(),
        }
    }

    pub(crate) fn loaded_screen() -> BookScreen {
        let mut screen = BookScreen::loading(PathBuf::from("/books/Dune"), 1);
        let book = Book::from_sources(
            PathBuf::from("/books/Dune"),
            vec![
                source("01 Intro", 65.0),
                source("02 Desert", 3725.0),
                source("03 Spice", 90.0),
            ],
        );
        screen.loaded(Ok(Box::new(book)));
        screen
    }

    fn press(s: &mut BookScreen, code: KeyCode) -> EditorAction {
        s.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(s: &mut BookScreen, c: char) {
        s.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn type_str(s: &mut BookScreen, text: &str) {
        text.chars().for_each(|c| {
            press(s, KeyCode::Char(c));
        });
    }

    fn titles(s: &BookScreen) -> Vec<String> {
        s.book.as_ref().map_or_else(Vec::new, |b| {
            b.chapters.iter().map(|c| c.title.clone()).collect()
        })
    }

    #[test]
    fn rename_reorder_and_remove_chapters() {
        let mut s = loaded_screen();
        press(&mut s, KeyCode::Enter);
        ctrl(&mut s, 'u');
        type_str(&mut s, "Prologue");
        press(&mut s, KeyCode::Enter);
        press(&mut s, KeyCode::Char('J'));
        press(&mut s, KeyCode::Up);
        press(&mut s, KeyCode::Char('d'));
        assert_eq!(titles(&s), ["Prologue", "03 Spice"]);
    }

    #[test]
    fn edits_metadata_fields() {
        let mut s = loaded_screen();
        s.focus = Focus::Fields;
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Enter);
        type_str(&mut s, "Scott Brick");
        press(&mut s, KeyCode::Enter);
        assert_eq!(
            s.book.as_ref().map(|b| b.meta.narrator.as_str()),
            Some("Scott Brick")
        );
    }

    #[test]
    fn rejects_invalid_bitrate_with_a_notice() {
        let mut s = loaded_screen();
        s.focus = Focus::Fields;
        for _ in 0..FIELDS.len() {
            press(&mut s, KeyCode::Down);
        }
        press(&mut s, KeyCode::Enter);
        ctrl(&mut s, 'u');
        type_str(&mut s, "abc");
        press(&mut s, KeyCode::Enter);
        assert!(s.notice.is_some());
    }

    #[test]
    fn typing_q_into_a_field_does_not_leave_the_screen() {
        let mut s = loaded_screen();
        press(&mut s, KeyCode::Enter);
        assert!(matches!(
            press(&mut s, KeyCode::Char('q')),
            EditorAction::None
        ));
    }

    #[test]
    fn bind_hands_out_a_snapshot_of_the_book() {
        let mut s = loaded_screen();
        assert!(matches!(
            press(&mut s, KeyCode::Char('b')),
            EditorAction::Bind(_)
        ));
    }

    fn edit_field(s: &mut BookScreen, field: Field, value: &str) {
        s.focus = Focus::Fields;
        s.field = FIELDS.iter().position(|&f| f == field).unwrap();
        press(s, KeyCode::Enter);
        ctrl(s, 'u');
        type_str(s, value);
        press(s, KeyCode::Enter);
    }

    #[test]
    fn typed_output_dir_is_where_the_book_is_written() {
        let mut s = loaded_screen();
        edit_field(&mut s, Field::OutputDir, "/srv/audiobooks");
        let out = s.book.as_ref().map(Book::output_path);
        assert_eq!(
            out.as_deref(),
            Some(Path::new("/srv/audiobooks/Frank Herbert - Dune.m4b"))
        );
    }

    #[test]
    fn clearing_output_dir_restores_the_default() {
        let mut s = loaded_screen();
        edit_field(&mut s, Field::OutputDir, "/srv/audiobooks");
        edit_field(&mut s, Field::OutputDir, "");
        let dir = s.book.as_ref().map(Book::output_dir);
        assert_eq!(dir.as_deref(), Some(Path::new("/books/Dune")));
    }

    #[test]
    fn focus_wraps_after_fields() {
        let mut s = loaded_screen();
        assert_eq!(
            (s.focus_next(), s.focus_next(), s.focus_next()),
            (true, true, false)
        );
    }

    #[test]
    fn space_in_files_panel_excludes_a_file() {
        let mut s = loaded_screen();
        s.focus = Focus::Files;
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Char(' '));
        assert_eq!(titles(&s), ["01 Intro", "03 Spice"]);
    }

    #[test]
    fn binding_with_no_files_selected_is_refused() {
        let mut s = loaded_screen();
        s.focus = Focus::Files;
        for _ in 0..3 {
            press(&mut s, KeyCode::Char(' '));
            press(&mut s, KeyCode::Down);
        }
        let action = press(&mut s, KeyCode::Char('b'));
        assert!(matches!(action, EditorAction::None) && s.notice.is_some());
    }
}
