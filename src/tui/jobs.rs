//! The conversion queue: binds run in the background, a bounded number at a
//! time, while the user keeps browsing and editing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table, TableState};

use super::{highlight, hms, panel};
use crate::book::Book;

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Queued,
    Running { progress: f64, started: Instant },
    Done { took: Duration },
    Failed(String),
    Cancelled,
}

impl JobStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, JobStatus::Queued | JobStatus::Running { .. })
    }
}

pub struct Job {
    pub id: u64,
    pub title: String,
    pub output: PathBuf,
    pub status: JobStatus,
    cancel: Arc<AtomicBool>,
    /// Snapshot of the book as it was when queued; taken when the job starts.
    book: Option<Box<Book>>,
}

/// Everything a worker thread needs to run one bind.
pub struct StartJob {
    pub id: u64,
    pub book: Box<Book>,
    pub output: PathBuf,
    pub cancel: Arc<AtomicBool>,
}

pub struct Jobs {
    list: Vec<Job>,
    pub max_parallel: usize,
    next_id: u64,
    pub state: TableState,
}

/// Default parallelism: each bind keeps about two cores busy (decoder +
/// encoder), and beyond four the disk becomes the bottleneck.
pub fn default_parallelism() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4))
}

impl Jobs {
    pub fn new(max_parallel: usize) -> Jobs {
        Jobs {
            list: Vec::new(),
            max_parallel: max_parallel.max(1),
            next_id: 1,
            state: TableState::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.list.len()
    }

    #[cfg(test)]
    pub fn jobs(&self) -> &[Job] {
        &self.list
    }

    pub fn running(&self) -> usize {
        self.count(|s| matches!(s, JobStatus::Running { .. }))
    }

    pub fn queued(&self) -> usize {
        self.count(|s| matches!(s, JobStatus::Queued))
    }

    pub fn finished(&self) -> usize {
        self.count(|s| matches!(s, JobStatus::Done { .. }))
    }

    pub fn active(&self) -> usize {
        self.count(JobStatus::is_active)
    }

    fn count(&self, pred: impl Fn(&JobStatus) -> bool) -> usize {
        self.list.iter().filter(|j| pred(&j.status)).count()
    }

    /// Queues `book`. Refuses if another active job writes the same file.
    pub fn enqueue(&mut self, book: Book) -> Result<u64, String> {
        let output = book.output_path();
        if self
            .list
            .iter()
            .any(|j| j.status.is_active() && j.output == output)
        {
            return Err(format!("{} is already being converted", output.display()));
        }
        let id = self.next_id;
        self.next_id += 1;
        let title = if book.meta.title.trim().is_empty() {
            crate::book::file_name(&book.dir)
        } else {
            book.meta.title.clone()
        };
        self.list.push(Job {
            id,
            title,
            output,
            status: JobStatus::Queued,
            cancel: Arc::new(AtomicBool::new(false)),
            book: Some(Box::new(book)),
        });
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        Ok(id)
    }

    /// Marks as many queued jobs running as the parallelism limit allows and
    /// hands them out to be started, oldest first.
    pub fn next_to_start(&mut self) -> Vec<StartJob> {
        let free = self.max_parallel.saturating_sub(self.running());
        let mut out = Vec::new();
        for job in self
            .list
            .iter_mut()
            .filter(|j| j.status == JobStatus::Queued)
            .take(free)
        {
            let Some(book) = job.book.take() else {
                continue;
            };
            job.status = JobStatus::Running {
                progress: 0.0,
                started: Instant::now(),
            };
            out.push(StartJob {
                id: job.id,
                book,
                output: job.output.clone(),
                cancel: Arc::clone(&job.cancel),
            });
        }
        out
    }

    pub fn on_progress(&mut self, id: u64, p: f64) {
        if let Some(JobStatus::Running { progress, .. }) = self.find(id).map(|j| &mut j.status) {
            *progress = p;
        }
    }

    /// Records a job's outcome and returns a one-line summary for the status bar.
    pub fn on_done(&mut self, id: u64, result: Result<(), String>) -> Option<String> {
        let job = self.find(id)?;
        let took = match job.status {
            JobStatus::Running { started, .. } => started.elapsed(),
            _ => Duration::ZERO,
        };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        job.status = match result {
            Ok(()) => JobStatus::Done { took },
            Err(_) if cancelled => JobStatus::Cancelled,
            Err(e) => JobStatus::Failed(e),
        };
        Some(match &job.status {
            JobStatus::Done { took } => {
                format!("✓ Finished {} in {}", job.title, hms(took.as_secs_f64()))
            }
            JobStatus::Cancelled => format!("Cancelled {}", job.title),
            JobStatus::Failed(e) => format!("✗ {} failed: {e}", job.title),
            _ => return None,
        })
    }

    fn find(&mut self, id: u64) -> Option<&mut Job> {
        self.list.iter_mut().find(|j| j.id == id)
    }

    pub fn cancel_selected(&mut self) {
        let Some(job) = self.state.selected().and_then(|i| self.list.get_mut(i)) else {
            return;
        };
        match job.status {
            // Never started: nothing to stop.
            JobStatus::Queued => {
                job.book = None;
                job.status = JobStatus::Cancelled;
            }
            JobStatus::Running { .. } => job.cancel.store(true, Ordering::Relaxed),
            _ => {}
        }
    }

    pub fn cancel_all(&self) {
        for job in &self.list {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn clear_finished(&mut self) {
        self.list.retain(|j| j.status.is_active());
        let last = self.list.len().checked_sub(1);
        self.state
            .select(self.state.selected().zip(last).map(|(i, last)| i.min(last)));
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let last = self.list.len().saturating_sub(1);
        let i = self.state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.state.select(Some((i + 1).min(last))),
            KeyCode::Up | KeyCode::Char('k') => self.state.select(Some(i.saturating_sub(1))),
            KeyCode::Char('c') | KeyCode::Delete => self.cancel_selected(),
            KeyCode::Char('x') => self.clear_finished(),
            _ => {}
        }
    }

    /// Rows the panel wants, including its border.
    pub fn height(&self) -> u16 {
        if self.list.is_empty() {
            0
        } else {
            self.list.len().min(5) as u16 + 2
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let rows = self.list.iter().map(|job| {
            let (status, style) = status_cell(&job.status);
            Row::new(vec![
                Cell::from(job.title.clone()),
                Cell::from(status).style(style),
                Cell::from(crate::book::file_name(&job.output)).dark_gray(),
            ])
        });
        let title = format!(
            " Conversions · {} running · {} queued · {} done · up to {} at once ",
            self.running(),
            self.queued(),
            self.finished(),
            self.max_parallel
        );
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(35),
                Constraint::Length(34),
                Constraint::Fill(1),
            ],
        )
        .block(panel(title, focused))
        .row_highlight_style(highlight(focused));
        f.render_stateful_widget(table, area, &mut self.state);
    }
}

fn status_cell(status: &JobStatus) -> (Line<'static>, Style) {
    match status {
        JobStatus::Queued => (Line::from("queued"), Style::new().dark_gray()),
        JobStatus::Running { progress, started } => {
            let elapsed = started.elapsed().as_secs_f64();
            let eta = if *progress > 0.01 {
                hms(elapsed / progress - elapsed)
            } else {
                "--:--".into()
            };
            (
                Line::from(vec![
                    Span::from(bar(*progress, 12)).green(),
                    Span::from(format!(" {:>3.0}% ETA {eta}", progress * 100.0)),
                ]),
                Style::new(),
            )
        }
        JobStatus::Done { took } => (
            Line::from(format!("✓ done in {}", hms(took.as_secs_f64()))),
            Style::new().green(),
        ),
        JobStatus::Failed(e) => (Line::from(format!("✗ {e}")), Style::new().red()),
        JobStatus::Cancelled => (Line::from("cancelled"), Style::new().yellow()),
    }
}

/// A text progress bar `width` cells wide, with eighth-cell resolution.
fn bar(progress: f64, width: usize) -> String {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (progress.clamp(0.0, 1.0) * (width * 8) as f64) as usize;
    let mut s = "█".repeat(eighths / 8);
    if !eighths.is_multiple_of(8) && eighths / 8 < width {
        s.push(PARTIAL[eighths % 8]);
    }
    let pad = width.saturating_sub(s.chars().count());
    s + &"·".repeat(pad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::SourceFile;

    fn book(title: &str) -> Book {
        let source = SourceFile {
            path: PathBuf::from(format!("/books/{title}/01.mp3")),
            duration: 60.0,
            codec: "mp3".into(),
            sample_rate: 44_100,
            channels: 2,
            bit_rate: Some(128_000),
            has_cover: false,
            tags: [("album".to_string(), title.to_string())].into(),
            markers: Vec::new(),
        };
        Book::from_sources(PathBuf::from(format!("/books/{title}")), vec![source])
    }

    fn statuses(jobs: &Jobs) -> Vec<&'static str> {
        jobs.jobs()
            .iter()
            .map(|j| match j.status {
                JobStatus::Queued => "queued",
                JobStatus::Running { .. } => "running",
                JobStatus::Done { .. } => "done",
                JobStatus::Failed(_) => "failed",
                JobStatus::Cancelled => "cancelled",
            })
            .collect()
    }

    #[test]
    fn starts_no_more_than_max_parallel() {
        let mut jobs = Jobs::new(2);
        for t in ["A", "B", "C"] {
            jobs.enqueue(book(t)).unwrap();
        }
        assert_eq!(jobs.next_to_start().len(), 2);
    }

    #[test]
    fn finishing_a_job_frees_a_slot_for_the_next() {
        let mut jobs = Jobs::new(1);
        let first = jobs.enqueue(book("A")).unwrap();
        jobs.enqueue(book("B")).unwrap();
        jobs.next_to_start();
        jobs.on_done(first, Ok(()));
        let started: Vec<u64> = jobs.next_to_start().iter().map(|s| s.id).collect();
        assert_eq!(started, [first + 1]);
    }

    #[test]
    fn rejects_a_second_active_job_for_the_same_output() {
        let mut jobs = Jobs::new(2);
        jobs.enqueue(book("A")).unwrap();
        assert!(jobs.enqueue(book("A")).is_err());
    }

    #[test]
    fn cancelling_a_queued_job_never_starts_it() {
        let mut jobs = Jobs::new(1);
        jobs.enqueue(book("A")).unwrap();
        jobs.cancel_selected();
        assert!(jobs.next_to_start().is_empty());
    }

    #[test]
    fn error_after_cancel_is_reported_as_cancelled() {
        let mut jobs = Jobs::new(1);
        let id = jobs.enqueue(book("A")).unwrap();
        jobs.next_to_start();
        jobs.cancel_selected();
        jobs.on_done(id, Err("cancelled".into()));
        assert_eq!(statuses(&jobs), ["cancelled"]);
    }

    #[test]
    fn clear_finished_keeps_active_jobs() {
        let mut jobs = Jobs::new(1);
        let a = jobs.enqueue(book("A")).unwrap();
        jobs.enqueue(book("B")).unwrap();
        jobs.next_to_start();
        jobs.on_done(a, Ok(()));
        jobs.clear_finished();
        assert_eq!(statuses(&jobs), ["queued"]);
    }

    #[test]
    fn bar_fills_proportionally() {
        assert_eq!(bar(0.5, 4), "██··");
    }
}
