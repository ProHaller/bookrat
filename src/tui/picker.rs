//! The home-screen folder browser for picking an audiobook folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table, TableState};

use super::{Msg, highlight, panel};
use crate::scan::{self, DirSummary, natural_cmp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// The current folder itself, offered when it holds audio files.
    This,
    Parent,
    Dir,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: EntryKind,
    pub name: String,
    pub path: PathBuf,
    /// `None` until the background count arrives.
    pub summary: Option<DirSummary>,
}

pub enum PickerAction {
    None,
    /// Open this folder as a book.
    Open(PathBuf),
}

pub struct Picker {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub state: TableState,
    pub show_hidden: bool,
    /// Error from the last listing, shown instead of entries.
    pub error: Option<String>,
    /// Tags background counts so results for an old listing are dropped.
    generation: u64,
}

impl Picker {
    pub fn new(dir: PathBuf, tx: &Sender<Msg>) -> Picker {
        let mut picker = Picker {
            dir: PathBuf::new(),
            entries: Vec::new(),
            state: TableState::default(),
            show_hidden: false,
            error: None,
            generation: 0,
        };
        picker.navigate(dir, None, tx);
        picker
    }

    /// Lists `dir`, selecting the entry for `select` if given.
    pub fn navigate(&mut self, dir: PathBuf, select: Option<&Path>, tx: &Sender<Msg>) {
        self.generation += 1;
        self.dir = dir;
        self.error = None;
        self.entries = match self.list() {
            Ok(entries) => entries,
            Err(e) => {
                self.error = Some(format!("cannot read {}: {e}", self.dir.display()));
                self.parent_entry().into_iter().collect()
            }
        };
        let index = select
            .and_then(|p| self.entries.iter().position(|e| e.path == p))
            .unwrap_or(0);
        self.state.select(Some(index));
        self.count_in_background(tx);
    }

    fn parent_entry(&self) -> Option<Entry> {
        self.dir.parent().map(|parent| Entry {
            kind: EntryKind::Parent,
            name: "..".into(),
            path: parent.to_path_buf(),
            summary: None,
        })
    }

    fn list(&self) -> std::io::Result<Vec<Entry>> {
        let mut dirs: Vec<Entry> = fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter_map(|path| {
                let name = path.file_name()?.to_string_lossy().into_owned();
                (self.show_hidden || !name.starts_with('.')).then_some(Entry {
                    kind: EntryKind::Dir,
                    name,
                    path,
                    summary: None,
                })
            })
            .collect();
        dirs.sort_by(|a, b| natural_cmp(&a.name, &b.name));

        let mut entries = Vec::with_capacity(dirs.len() + 2);
        // One directory read, so it's fine to do inline.
        let here = scan::summarize(&self.dir).unwrap_or_default();
        if here.audio_files > 0 {
            entries.push(Entry {
                kind: EntryKind::This,
                name: ".".into(),
                path: self.dir.clone(),
                summary: Some(here),
            });
        }
        entries.extend(self.parent_entry());
        entries.extend(dirs);
        Ok(entries)
    }

    /// Summarizing means reading every subfolder, which can be slow on
    /// network drives, so it happens off the UI thread.
    fn count_in_background(&self, tx: &Sender<Msg>) {
        let jobs: Vec<(usize, PathBuf)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == EntryKind::Dir)
            .map(|(i, e)| (i, e.path.clone()))
            .collect();
        let (tx, generation) = (tx.clone(), self.generation);
        thread::spawn(move || {
            for (index, path) in jobs {
                let summary = scan::summarize(&path).unwrap_or_default();
                if tx
                    .send(Msg::DirSummary {
                        generation,
                        index,
                        summary,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    pub fn on_summary(&mut self, generation: u64, index: usize, summary: DirSummary) {
        if generation == self.generation
            && let Some(entry) = self.entries.get_mut(index)
        {
            entry.summary = Some(summary);
        }
    }

    fn selected(&self) -> Option<&Entry> {
        self.state.selected().and_then(|i| self.entries.get(i))
    }

    fn go_up(&mut self, tx: &Sender<Msg>) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            let from = self.dir.clone();
            self.navigate(parent, Some(&from), tx);
        }
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.entries.len().saturating_sub(1);
        let i = self.state.selected().unwrap_or(0);
        self.state
            .select(Some(i.saturating_add_signed(delta).min(last)));
    }

    pub fn on_key(&mut self, key: KeyEvent, tx: &Sender<Msg>) -> PickerAction {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') => self.state.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.go_up(tx),
            KeyCode::Char('~') => {
                if let Some(home) = std::env::var_os("HOME") {
                    self.navigate(PathBuf::from(home), None, tx);
                }
            }
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                let selected = self.selected().map(|e| e.path.clone());
                self.navigate(self.dir.clone(), selected.as_deref(), tx);
            }
            // Explicitly open as a book, e.g. one split into CD1/, CD2/.
            KeyCode::Char('o') | KeyCode::Char(' ') => {
                if let Some(e) = self.selected().filter(|e| e.kind != EntryKind::Parent) {
                    return PickerAction::Open(e.path.clone());
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let Some(entry) = self.selected().cloned() else {
                    return PickerAction::None;
                };
                let is_book = entry.summary.is_some_and(|s| s.is_book());
                match entry.kind {
                    EntryKind::This => return PickerAction::Open(entry.path),
                    EntryKind::Parent => self.go_up(tx),
                    // Enter on a book folder opens it; arrows browse.
                    EntryKind::Dir if is_book && key.code == KeyCode::Enter => {
                        return PickerAction::Open(entry.path);
                    }
                    EntryKind::Dir => self.navigate(entry.path, None, tx),
                }
            }
            _ => {}
        }
        PickerAction::None
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let rows: Vec<Row> = self
            .entries
            .iter()
            .map(|e| {
                let (name, style) = match e.kind {
                    EntryKind::This => {
                        ("./  (this folder)".to_string(), Style::new().green().bold())
                    }
                    EntryKind::Parent => ("../".to_string(), Style::new().dark_gray()),
                    EntryKind::Dir if e.summary.is_some_and(|s| s.is_book()) => {
                        (format!("{}/", e.name), Style::new().green())
                    }
                    EntryKind::Dir => (format!("{}/", e.name), Style::new()),
                };
                Row::new(vec![Cell::from(name).style(style), Cell::from(describe(e))])
            })
            .collect();

        let title = format!(" Choose an audiobook folder · {} ", self.dir.display());
        let mut table = Table::new(rows, [Constraint::Fill(1), Constraint::Length(20)])
            .block(panel(title, focused))
            .row_highlight_style(highlight(focused));
        if let Some(err) = &self.error {
            table = table.footer(Row::new([Line::from(err.clone()).red()]));
        }
        f.render_stateful_widget(table, area, &mut self.state);
    }
}

fn describe(e: &Entry) -> Line<'static> {
    if e.kind == EntryKind::Parent {
        return Line::default();
    }
    match e.summary {
        None => Line::from("…").dark_gray(),
        Some(s) if s.audio_files > 0 => Line::from(vec![
            Span::from("♪ ").green(),
            Span::from(format!("{} audio files", s.audio_files)),
        ]),
        Some(s) if s.is_book() => Line::from(vec![
            Span::from("♪ ").green(),
            Span::from(format!("{} discs", s.disc_subdirs)),
        ]),
        Some(s) if s.subdirs > 0 => Line::from(format!("{} folders", s.subdirs)).dark_gray(),
        Some(_) => Line::from("empty").dark_gray(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use std::sync::mpsc;

    /// library/{Dune (2 mp3s), Series/Book 1 (1 mp3), Notes (txt only)}
    fn library() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path();
        for (dir, files) in [
            ("Dune", &["01.mp3", "02.mp3"][..]),
            ("Series/Book 1", &["a.mp3"][..]),
            ("Notes", &["readme.txt"][..]),
        ] {
            fs::create_dir_all(lib.join(dir)).unwrap();
            for f in files {
                fs::write(lib.join(dir).join(f), b"").unwrap();
            }
        }
        tmp
    }

    /// A picker on `dir` with all background counts applied.
    fn picker_at(dir: &Path) -> Picker {
        let (tx, rx) = mpsc::channel();
        let mut p = Picker::new(dir.to_path_buf(), &tx);
        drop(tx);
        settle(&mut p, &rx);
        p
    }

    fn settle(p: &mut Picker, rx: &mpsc::Receiver<Msg>) {
        while let Ok(Msg::DirSummary {
            generation,
            index,
            summary,
        }) = rx.recv_timeout(std::time::Duration::from_secs(2))
        {
            p.on_summary(generation, index, summary);
        }
    }

    fn press(p: &mut Picker, code: KeyCode) -> PickerAction {
        let (tx, rx) = mpsc::channel();
        let action = p.on_key(KeyEvent::new(code, KeyModifiers::NONE), &tx);
        drop(tx);
        settle(p, &rx);
        action
    }

    fn select(p: &mut Picker, name: &str) {
        let i = p.entries.iter().position(|e| e.name == name).unwrap();
        p.state.select(Some(i));
    }

    fn names(p: &Picker) -> Vec<&str> {
        p.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn lists_parent_then_folders_in_natural_order() {
        let tmp = library();
        assert_eq!(
            names(&picker_at(tmp.path())),
            ["..", "Dune", "Notes", "Series"]
        );
    }

    #[test]
    fn counts_audio_files_per_folder() {
        let tmp = library();
        let p = picker_at(tmp.path());
        let dune = p.entries.iter().find(|e| e.name == "Dune").unwrap();
        assert_eq!(dune.summary.map(|s| s.audio_files), Some(2));
    }

    #[test]
    fn enter_on_folder_with_audio_opens_it_as_a_book() {
        let tmp = library();
        let mut p = picker_at(tmp.path());
        select(&mut p, "Dune");
        let action = press(&mut p, KeyCode::Enter);
        assert!(matches!(action, PickerAction::Open(path) if path.ends_with("Dune")));
    }

    #[test]
    fn enter_on_folder_without_audio_browses_into_it() {
        let tmp = library();
        let mut p = picker_at(tmp.path());
        select(&mut p, "Series");
        press(&mut p, KeyCode::Enter);
        assert_eq!(names(&p), ["..", "Book 1"]);
    }

    #[test]
    fn folder_with_audio_offers_itself_first() {
        let tmp = library();
        let p = picker_at(&tmp.path().join("Dune"));
        assert_eq!(p.entries[0].kind, EntryKind::This);
    }

    #[test]
    fn enter_on_folder_of_discs_opens_it_as_a_book() {
        let tmp = library();
        for disc in ["Multi/CD1", "Multi/CD2"] {
            fs::create_dir_all(tmp.path().join(disc)).unwrap();
            fs::write(tmp.path().join(disc).join("01.mp3"), b"").unwrap();
        }
        let mut p = picker_at(tmp.path());
        select(&mut p, "Multi");
        assert!(matches!(
            press(&mut p, KeyCode::Enter),
            PickerAction::Open(_)
        ));
    }

    #[test]
    fn going_up_reselects_the_folder_we_came_from() {
        let tmp = library();
        let mut p = picker_at(&tmp.path().join("Series"));
        press(&mut p, KeyCode::Backspace);
        assert_eq!(p.selected().map(|e| e.name.as_str()), Some("Series"));
    }
}
