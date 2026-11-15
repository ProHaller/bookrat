//! The interactive app: pick a folder on the home screen, review the book,
//! queue conversions, and keep working while they run in the background.

mod editor;
mod jobs;
mod picker;
mod rat;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

pub use jobs::default_parallelism;

use crate::book::Book;
use crate::encode::{self, Encoder};
use crate::scan::DirSummary;
use editor::{BookScreen, EditorAction};
use jobs::{Jobs, StartJob};
use picker::{Picker, PickerAction};

/// Events from background threads.
pub enum Msg {
    DirSummary {
        generation: u64,
        index: usize,
        summary: DirSummary,
    },
    Probing {
        load_id: u64,
        done: usize,
        total: usize,
    },
    Loaded {
        load_id: u64,
        result: Result<Box<Book>, String>,
    },
    JobProgress {
        id: u64,
        progress: f64,
    },
    JobDone {
        id: u64,
        result: Result<(), String>,
    },
}

/// Settings from the command line.
pub struct Options {
    /// Book to open straight away; the home screen browses from its parent.
    pub dir: Option<PathBuf>,
    /// `-o`: a folder applies to every book opened; a file path only to `dir`.
    pub output: Option<PathBuf>,
    pub bitrate: Option<u32>,
    pub encoder: Option<Encoder>,
    pub max_parallel: usize,
}

enum Screen {
    Home,
    Book(Box<BookScreen>),
}

/// How long the animation holds each frame.
const FRAME_TIME: Duration = Duration::from_millis(150);
/// How long quitting waits for cancelled jobs to stop their ffmpeg processes
/// and remove their partial files.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

struct App {
    screen: Screen,
    picker: Picker,
    jobs: Jobs,
    /// Whether keys go to the conversions panel.
    jobs_focus: bool,
    options: Options,
    tx: Sender<Msg>,
    next_load_id: u64,
    tick: u64,
    /// Latest app-wide message, e.g. a conversion finishing.
    notice: Option<String>,
    confirm_quit: bool,
    quit: bool,
}

pub fn run(options: Options) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(options, tx)?;
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal, &rx);
    ratatui::restore();
    result
}

impl App {
    fn new(options: Options, tx: Sender<Msg>) -> Result<App> {
        App::with_cwd(options, tx, &std::env::current_dir()?)
    }

    /// Builds the app as if started in `cwd`.
    fn with_cwd(options: Options, tx: Sender<Msg>, cwd: &Path) -> Result<App> {
        let book_dir = options.dir.as_deref().map(Path::canonicalize).transpose()?;
        let output = options.output.as_deref().map(|o| cwd.join(o));
        let browse_from = match &book_dir {
            Some(dir) => dir.parent().unwrap_or(dir).to_path_buf(),
            None => cwd.to_path_buf(),
        };
        let mut picker = Picker::new(browse_from.clone(), &tx);
        if let Some(dir) = &book_dir {
            picker.navigate(browse_from, Some(dir), &tx);
        }
        let mut app = App {
            screen: Screen::Home,
            picker,
            jobs: Jobs::new(options.max_parallel),
            jobs_focus: false,
            options: Options {
                dir: book_dir.clone(),
                output,
                ..options
            },
            tx,
            next_load_id: 0,
            tick: 0,
            notice: None,
            confirm_quit: false,
            quit: false,
        };
        if let Some(dir) = book_dir {
            app.open_book(dir);
        }
        Ok(app)
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal, rx: &Receiver<Msg>) -> Result<()> {
        let mut last_frame = Instant::now();
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
            while let Ok(msg) = rx.try_recv() {
                self.on_msg(msg);
            }
            self.start_jobs();
            if last_frame.elapsed() >= FRAME_TIME {
                self.tick += 1;
                last_frame = Instant::now();
            }
        }
        self.shut_down(rx);
        Ok(())
    }

    /// Cancels running jobs and waits for them, so no ffmpeg processes or
    /// partial files outlive the app.
    fn shut_down(&mut self, rx: &Receiver<Msg>) {
        self.jobs.cancel_all();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while self.jobs.running() > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match rx.recv_timeout(left) {
                Ok(msg) => self.on_msg(msg),
                Err(_) => break,
            }
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::DirSummary {
                generation,
                index,
                summary,
            } => {
                self.picker.on_summary(generation, index, summary);
            }
            Msg::Probing {
                load_id,
                done,
                total,
            } => {
                if let Screen::Book(screen) = &mut self.screen
                    && screen.load_id == load_id
                {
                    screen.phase = editor::Phase::Loading { done, total };
                }
            }
            Msg::Loaded { load_id, result } => {
                if let Screen::Book(screen) = &mut self.screen
                    && screen.load_id == load_id
                {
                    let result = result.map(|book| apply_overrides(&self.options, book));
                    screen.loaded(result);
                }
            }
            Msg::JobProgress { id, progress } => self.jobs.on_progress(id, progress),
            Msg::JobDone { id, result } => {
                if let Some(summary) = self.jobs.on_done(id, result) {
                    self.notice = Some(summary);
                }
            }
        }
    }

    fn open_book(&mut self, dir: PathBuf) {
        self.next_load_id += 1;
        let load_id = self.next_load_id;
        let tx = self.tx.clone();
        let load_dir = dir.clone();
        thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = Book::load(&load_dir, move |done, total| {
                let _ = progress_tx.send(Msg::Probing {
                    load_id,
                    done,
                    total,
                });
            })
            .map(Box::new)
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Loaded { load_id, result });
        });
        self.screen = Screen::Book(Box::new(BookScreen::loading(dir, load_id)));
        self.jobs_focus = false;
    }

    fn start_jobs(&mut self) {
        for job in self.jobs.next_to_start() {
            spawn_job(job, self.tx.clone());
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.confirm_quit {
            self.confirm_quit = false;
            self.quit = key.code == KeyCode::Char('y');
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        let typing = matches!(&self.screen, Screen::Book(s) if s.is_typing());
        if !typing {
            self.notice = None;
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                self.cycle_focus();
                return;
            }
            if self.jobs_focus {
                match key.code {
                    KeyCode::Esc => self.jobs_focus = false,
                    _ => self.jobs.on_key(key),
                }
                return;
            }
        }

        match &mut self.screen {
            Screen::Home => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
                _ => {
                    if let PickerAction::Open(dir) = self.picker.on_key(key, &self.tx) {
                        self.open_book(dir);
                    }
                }
            },
            Screen::Book(screen) => match screen.on_key(key) {
                EditorAction::None => {}
                EditorAction::Back => self.screen = Screen::Home,
                EditorAction::Bind(book) => {
                    let title = book.meta.title.clone();
                    screen.notice = Some(match self.jobs.enqueue(*book) {
                        Ok(_) => {
                            format!("Queued \"{title}\". Keep browsing; progress is shown below.")
                        }
                        Err(e) => e,
                    });
                }
            },
        }
    }

    fn cycle_focus(&mut self) {
        if self.jobs_focus {
            self.jobs_focus = false;
            return;
        }
        let wrapped = match &mut self.screen {
            Screen::Home => true,
            Screen::Book(screen) => !screen.focus_next(),
        };
        if wrapped && !self.jobs.is_empty() {
            self.jobs_focus = true;
        }
    }

    fn request_quit(&mut self) {
        if self.jobs.active() > 0 {
            self.confirm_quit = true;
        } else {
            self.quit = true;
        }
    }

    // ---- rendering -------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [header, main, jobs_area, status, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(self.jobs.height()),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());

        self.draw_header(f, header);
        let main_focused = !self.jobs_focus;
        match &mut self.screen {
            Screen::Home => draw_home(f, main, &mut self.picker, self.tick, main_focused),
            Screen::Book(screen) => screen.draw(f, main, main_focused),
        }
        if !self.jobs.is_empty() {
            self.jobs.draw(f, jobs_area, self.jobs_focus);
        }
        self.draw_status(f, status);
        f.render_widget(Line::from(self.help_text()).dark_gray(), help);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let place = match &self.screen {
            Screen::Home => "home".to_string(),
            Screen::Book(screen) => screen.dir.display().to_string(),
        };
        f.render_widget(
            Line::from(vec![
                " bookrat ".bold().black().on_cyan(),
                format!(" {place}").into(),
            ]),
            area,
        );
        if self.jobs.active() > 0 {
            let summary = format!(
                "⟳ {} running · {} queued ",
                self.jobs.running(),
                self.jobs.queued()
            );
            f.render_widget(
                Line::from(summary).green().alignment(Alignment::Right),
                area,
            );
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        if self.confirm_quit {
            let text = format!(
                "{} conversion(s) still active. Quit and cancel them? (y/n)",
                self.jobs.active()
            );
            let p = Paragraph::new(Line::from(text).yellow().bold()).block(Block::bordered());
            f.render_widget(p, area);
            return;
        }
        match &self.screen {
            Screen::Book(screen) => screen.draw_status(f, area, self.notice.as_deref()),
            Screen::Home => {
                let line = match &self.notice {
                    Some(n) => Line::from(n.clone()),
                    None => {
                        Line::from("Pick a folder of audio files. Folders marked ♪ hold audio.")
                            .dark_gray()
                    }
                };
                f.render_widget(
                    Paragraph::new(line)
                        .block(Block::bordered())
                        .wrap(Wrap { trim: true }),
                    area,
                );
            }
        }
    }

    fn help_text(&self) -> &'static str {
        if self.confirm_quit {
            return " y quit and cancel · any other key keeps them running";
        }
        if self.jobs_focus {
            return " ↑↓ select · c cancel · x clear finished · Tab/Esc back";
        }
        match &self.screen {
            Screen::Home => {
                " ↑↓ move · Enter open · → browse in · o open as book · ← up · ~ home · . hidden · Tab conversions · q quit"
            }
            Screen::Book(screen) => screen.help(),
        }
    }
}

/// Command-line overrides: bitrate, encoder and an `-o` folder apply to
/// every book; an `-o` file path only to the book named on the command line.
fn apply_overrides(options: &Options, mut book: Box<Book>) -> Box<Book> {
    if let Some(k) = options.bitrate {
        book.target.bitrate_kbps = k;
    }
    if let Some(e) = options.encoder {
        book.target.encoder = e;
    }
    if let Some(output) = &options.output {
        let is_folder = output.is_dir() || output.as_os_str().to_string_lossy().ends_with('/');
        if is_folder || options.dir.as_deref() == Some(book.dir.as_path()) {
            book.set_output_path(output);
        }
    }
    book
}

fn draw_home(f: &mut Frame, area: Rect, picker: &mut Picker, tick: u64, focused: bool) {
    let mascot_width = rat::WIDTH + 4;
    if area.width < mascot_width + 40 {
        picker.draw(f, area, focused);
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Length(mascot_width), Constraint::Fill(1)]).areas(area);
    let mut lines: Vec<Line> = rat::frame(tick).into_iter().map(Line::from).collect();
    lines.push(Line::from("b o o k r a t").cyan().bold().centered());
    lines.push(Line::from("MP3 folders in,").dark_gray().centered());
    lines.push(Line::from("chaptered M4B out.").dark_gray().centered());
    let block = Block::bordered().border_style(Style::new().dark_gray());
    let inner = block.inner(left);
    f.render_widget(block, left);
    // Vertically centre the mascot in its panel.
    let height = (lines.len() as u16).min(inner.height);
    let top = inner.y + (inner.height - height) / 2;
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            y: top,
            height,
            ..inner
        },
    );
    picker.draw(f, right, focused);
}

/// Runs one bind on its own thread, reporting throttled progress.
fn spawn_job(job: StartJob, tx: Sender<Msg>) {
    thread::spawn(move || {
        let StartJob {
            id,
            book,
            output,
            cancel,
        } = job;
        let progress_tx = tx.clone();
        let mut last_sent = (Instant::now(), 0.0);
        let result = encode::bind(&book, &output, &cancel, |progress| {
            let (at, sent) = last_sent;
            if progress - sent >= 0.002 || at.elapsed() >= Duration::from_millis(250) {
                let _ = progress_tx.send(Msg::JobProgress { id, progress });
                last_sent = (Instant::now(), progress);
            }
        });
        let _ = tx.send(Msg::JobDone {
            id,
            result: result.map_err(|e| format!("{e:#}")),
        });
    });
}

/// A bordered panel, highlighted when focused.
fn panel(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::new().cyan()
    } else {
        Style::new().dark_gray()
    };
    Block::bordered().title(title).border_style(style)
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::new()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::REVERSED)
    }
}

fn hms(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app_in(dir: &Path) -> (App, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let options = Options {
            dir: None,
            output: None,
            bitrate: None,
            encoder: None,
            max_parallel: 2,
        };
        (App::with_cwd(options, tx, dir).unwrap(), rx)
    }

    fn screen(app: &mut App) -> String {
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn loaded_book(app: &mut App) {
        let mut screen = editor::tests::loaded_screen();
        app.next_load_id += 1;
        screen.load_id = app.next_load_id;
        app.screen = Screen::Book(Box::new(screen));
    }

    #[test]
    fn home_shows_mascot_and_picker() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, _rx) = app_in(tmp.path());
        let s = screen(&mut app);
        println!("{s}");
        assert!(s.contains("b o o k r a t") && s.contains("Choose an audiobook folder"));
    }

    #[test]
    fn queuing_a_bind_keeps_the_book_screen_usable() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, _rx) = app_in(tmp.path());
        loaded_book(&mut app);
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Down);
        assert!(matches!(app.screen, Screen::Book(_)) && app.jobs.len() == 1);
    }

    #[test]
    fn leaving_a_book_while_its_job_is_queued_returns_home() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, _rx) = app_in(tmp.path());
        loaded_book(&mut app);
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.screen, Screen::Home));
    }

    #[test]
    fn quitting_with_active_jobs_asks_first() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, _rx) = app_in(tmp.path());
        loaded_book(&mut app);
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.confirm_quit && !app.quit);
    }

    #[test]
    fn tab_reaches_the_jobs_panel_after_the_editor_panels() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, _rx) = app_in(tmp.path());
        loaded_book(&mut app);
        press(&mut app, KeyCode::Char('b'));
        for _ in 0..3 {
            press(&mut app, KeyCode::Tab);
        }
        assert!(app.jobs_focus);
    }

    #[test]
    fn stale_load_results_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, _rx) = app_in(tmp.path());
        app.open_book(tmp.path().to_path_buf());
        let stale = app.next_load_id - 1;
        app.on_msg(Msg::Loaded {
            load_id: stale,
            result: Err("old".into()),
        });
        assert!(
            matches!(&app.screen, Screen::Book(s) if matches!(s.phase, editor::Phase::Loading { .. }))
        );
    }

    #[test]
    #[ignore = "prints the book screen with a running job: cargo test print_book -- --ignored --nocapture"]
    fn print_book_screen_with_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, _rx) = app_in(tmp.path());
        loaded_book(&mut app);
        press(&mut app, KeyCode::Char('b'));
        for job in app.jobs.next_to_start() {
            app.jobs.on_progress(job.id, 0.42);
        }
        println!("{}", screen(&mut app));
    }
}
