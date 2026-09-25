//! `taskguard top`: the dashboard.

mod chart;
mod views;

use crate::commands;
use crate::config::{self, Config};
use crate::dash::{self, Bucket, Marker, NsRow, RunRow, TrendRow, Warning};
use crate::db::{self, Db};
use crate::key;
use crate::queue::Queue;
use crate::report::Snapshot;
use crate::runner;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const RANGES: [(&str, f64); 6] = [("5m", 300.0), ("15m", 900.0), ("1h", 3600.0), ("6h", 21600.0), ("24h", 86400.0), ("7d", 604800.0)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Overview,
    Queue,
    Runs,
    Job,
    Trends,
    Warnings,
    Namespaces,
    Help,
}

pub const VIEWS: [(View, &str); 8] = [
    (View::Overview, "Overview"),
    (View::Queue, "Queue"),
    (View::Runs, "Runs"),
    (View::Job, "Job"),
    (View::Trends, "Trends"),
    (View::Warnings, "Warnings"),
    (View::Namespaces, "Namespaces"),
    (View::Help, "Help"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charts {
    Both,
    Cpu,
    Mem,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    None,
    Filter(String),
    ConfirmKill(i32, String),
}

/// Everything a frame draws, loaded once per refresh.
#[derive(Default)]
pub struct Data {
    pub now: f64,
    pub from: f64,
    pub to: f64,
    pub snap: Option<Snapshot>,
    pub machine: Vec<Bucket>,
    pub ncpu: usize,
    pub mem_total_kb: f64,
    pub ns: BTreeMap<String, Vec<(f64, f64)>>,
    pub waiting: Vec<(usize, String)>,
    pub markers: Vec<(f64, Marker)>,
    pub runs: Vec<RunRow>,
    pub trends: Vec<TrendRow>,
    pub warnings: Vec<Warning>,
    pub namespaces: Vec<NsRow>,
    pub ns_list: Vec<String>,
    pub recorder: bool,
    pub cursor_runs: Vec<(String, String)>,
    pub job: Option<JobData>,
    pub run_spans: Vec<(f64, String, String)>,
}

pub struct JobData {
    pub key: String,
    pub learned: db::Learned,
    pub runs: Vec<RunRow>,
    pub adjustments: Vec<dash::Adjustment>,
    pub min_cpu: Option<f64>,
    pub min_mem_kb: Option<u64>,
}

pub struct App {
    pub dir: PathBuf,
    pub cfg: Config,
    pub view: View,
    pub prev_view: View,
    pub range: usize,
    pub ns_filter: Option<String>,
    pub text_filter: String,
    pub charts: Charts,
    pub show_other: bool,
    pub cursor: Option<usize>,
    pub paused: bool,
    pub sort: usize,
    pub reverse: bool,
    pub sel: usize,
    pub job_key: Option<String>,
    pub input: Input,
    pub columns: usize,
    pub data: Data,
    pub message: Option<String>,
    /// Rows of the current list view on screen, for mouse clicks:
    /// (first y, rows shown, index of the first row shown).
    pub list_rows: (u16, usize, usize),
}

impl App {
    pub fn new(dir: PathBuf, cfg: Config) -> App {
        App {
            dir,
            cfg,
            view: View::Overview,
            prev_view: View::Overview,
            range: 2,
            ns_filter: None,
            text_filter: String::new(),
            charts: Charts::Both,
            show_other: true,
            cursor: None,
            paused: false,
            sort: 0,
            reverse: false,
            sel: 0,
            job_key: None,
            input: Input::None,
            columns: 100,
            data: Data::default(),
            message: None,
            list_rows: (0, 0, 0),
        }
    }

    pub fn load(&mut self) -> Result<()> {
        let db = Db::open_dir(&self.dir)?;
        let now = db::now();
        let span = RANGES[self.range].1;
        let n = self.columns.max(10);
        // Column edges sit on whole multiples of the column width, not on
        // "now". Otherwise every edge moves a little each second, samples
        // change columns, and bars that are already drawn keep changing. Now a
        // finished column never changes: only the newest one grows, and the
        // chart moves one column left when a new one starts.
        let w = span / n as f64;
        let to = (now / w).ceil() * w;
        let from = to - span;
        let (machine, ncpu, mem_total) = dash::machine_series(&db, from, to, n)?;
        let mut ns = dash::ns_series(&db, from, to, n, self.cfg.sample_every)?;
        let ns_list: Vec<String> = {
            let mut s = db.conn.prepare("SELECT DISTINCT ns FROM runs WHERE imported = 0 ORDER BY ns")?;
            s.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        if let Some(f) = &self.ns_filter {
            ns.retain(|k, _| k == f);
        }
        let cursor_runs = match self.cursor {
            Some(c) => dash::runs_at(&db, from + (c as f64 + 0.5) * span / n as f64)?,
            None => Vec::new(),
        };
        let mut runs = dash::runs(&db, self.ns_filter.as_deref(), &self.text_filter, 500)?;
        sort_runs(&mut runs, self.sort, self.reverse);
        let trends: Vec<TrendRow> = dash::trends(&db, 10)?
            .into_iter()
            .filter(|t| self.ns_filter.as_ref().is_none_or(|f| &t.ns == f) && t.key.contains(&self.text_filter))
            .collect();
        let job = match &self.job_key {
            Some(k) => {
                let runs = dash::key_runs(&db, k, 30)?;
                let (min_cpu, min_mem_kb) = db
                    .conn
                    .query_row("SELECT min_cpu, min_mem_kb FROM runs WHERE key = ?1 ORDER BY id DESC LIMIT 1", [k], |r| {
                        Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<i64>>(1)?.map(|v| v as u64)))
                    })
                    .unwrap_or((None, None));
                Some(JobData {
                    key: k.clone(),
                    learned: db.learned(k, self.cfg.hist_keep, self.cfg.boost_runs)?,
                    runs,
                    adjustments: dash::adjustments(&db, Some(k), 0.0)?,
                    min_cpu,
                    min_mem_kb,
                })
            }
            None => None,
        };
        let run_spans = match (self.view, runs.get(self.sel)) {
            (View::Runs, Some(r)) => dash::wait_spans(&db, r.id)?,
            _ => Vec::new(),
        };
        let mut namespaces = dash::namespaces(&db, from, self.cfg.sample_every)?;
        sort_ns(&mut namespaces, self.sort, self.reverse);
        self.data = Data {
            now,
            from,
            to,
            snap: commands::snapshot(&self.dir, &self.cfg, Some(&db)).ok(),
            machine,
            ncpu,
            mem_total_kb: mem_total,
            ns,
            waiting: dash::waiting_series(&db, from, to, n)?,
            markers: dash::markers(&db, from, to)?,
            runs,
            trends,
            warnings: dash::warnings(&db, now - 7.0 * 86400.0, self.cfg.cpu_max, self.cfg.mem_max)?,
            namespaces,
            ns_list,
            recorder: recorder_running(&self.dir),
            cursor_runs,
            job,
            run_spans,
        };
        Ok(())
    }

    fn list_len(&self) -> usize {
        match self.view {
            View::Queue => self.data.snap.as_ref().map(|s| s.waiting.len() + s.running.len()).unwrap_or(0),
            View::Runs => self.data.runs.len(),
            View::Trends => self.data.trends.len(),
            View::Warnings => self.data.warnings.len(),
            View::Namespaces => self.data.namespaces.len(),
            View::Job => self.data.job.as_ref().map(|j| j.runs.len()).unwrap_or(0),
            _ => 0,
        }
    }

    fn set_view(&mut self, v: View) {
        if v != self.view {
            self.prev_view = self.view;
            self.view = v;
            self.sel = 0;
            self.sort = 0;
            self.reverse = false;
        }
    }

    fn selected_key(&self) -> Option<String> {
        match self.view {
            View::Runs => self.data.runs.get(self.sel).map(|r| r.key.clone()),
            View::Trends => self.data.trends.get(self.sel).map(|t| t.key.clone()),
            View::Warnings => self.data.warnings.get(self.sel).map(|w| w.key.clone()),
            View::Queue => self.queue_row(self.sel).map(|(e, _)| e.key.clone()),
            View::Namespaces => {
                let ns = self.data.namespaces.get(self.sel)?;
                ns.top_keys.first().map(|k| k.0.clone())
            }
            _ => None,
        }
    }

    /// Queue view rows: waiting jobs first (they are what needs explaining),
    /// then running ones. The bool is true for a waiting job.
    pub fn queue_row(&self, i: usize) -> Option<(&crate::queue::Entry, bool)> {
        let s = self.data.snap.as_ref()?;
        if i < s.waiting.len() { Some((&s.waiting[i].entry, true)) } else { s.running.get(i - s.waiting.len()).map(|e| (e, false)) }
    }

    /// Returns false to quit.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        if k.kind != KeyEventKind::Press {
            return true;
        }
        match &mut self.input {
            Input::Filter(text) => {
                match k.code {
                    KeyCode::Enter => {
                        self.text_filter = text.clone();
                        self.input = Input::None;
                        self.sel = 0;
                    }
                    KeyCode::Esc => self.input = Input::None,
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) => text.push(c),
                    _ => {}
                }
                return true;
            }
            Input::ConfirmKill(pid, key) => {
                if let KeyCode::Char('y') = k.code {
                    unsafe { libc::kill(*pid, libc::SIGTERM) };
                    self.message = Some(format!("sent SIGTERM to {key} (pid {pid})"));
                }
                self.input = Input::None;
                return true;
            }
            Input::None => {}
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return false;
        }
        self.message = None;
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char(c @ '1'..='8') => {
                let i = (c as u8 - b'1') as usize;
                if VIEWS[i].0 == View::Job && self.job_key.is_none() {
                    self.message = Some("pick a job first: select a row in Runs, Trends or Warnings and press Enter".into());
                } else {
                    self.set_view(VIEWS[i].0);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let i = VIEWS.iter().position(|(v, _)| *v == self.view).unwrap_or(0);
                let step = if k.code == KeyCode::Tab { 1 } else { VIEWS.len() - 1 };
                let mut j = (i + step) % VIEWS.len();
                if VIEWS[j].0 == View::Job && self.job_key.is_none() {
                    j = (j + step) % VIEWS.len();
                }
                self.set_view(VIEWS[j].0);
            }
            KeyCode::Char('t') => self.range = (self.range + 1) % RANGES.len(),
            KeyCode::Char('T') => self.range = (self.range + RANGES.len() - 1) % RANGES.len(),
            KeyCode::Char('n') => {
                let list = &self.data.ns_list;
                self.ns_filter = match &self.ns_filter {
                    None => list.first().cloned(),
                    Some(cur) => list.iter().position(|x| x == cur).and_then(|i| list.get(i + 1)).cloned(),
                };
                self.sel = 0;
            }
            KeyCode::Char('/') => self.input = Input::Filter(self.text_filter.clone()),
            KeyCode::Esc => {
                if self.ns_filter.is_some() || !self.text_filter.is_empty() || self.cursor.is_some() {
                    self.ns_filter = None;
                    self.text_filter.clear();
                    self.cursor = None;
                } else {
                    let back = self.prev_view;
                    self.set_view(back);
                }
            }
            KeyCode::Char('c') => self.charts = Charts::Cpu,
            KeyCode::Char('m') => self.charts = Charts::Mem,
            KeyCode::Char('b') => self.charts = Charts::Both,
            KeyCode::Char('o') => self.show_other = !self.show_other,
            KeyCode::Char('s') => self.sort += 1,
            KeyCode::Char('r') => self.reverse = !self.reverse,
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Left => {
                let c = self.cursor.unwrap_or(self.columns);
                self.cursor = Some(c.saturating_sub(1));
            }
            KeyCode::Right => {
                if let Some(c) = self.cursor {
                    self.cursor = if c + 1 >= self.columns { None } else { Some(c + 1) };
                }
            }
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                if self.sel + 1 < self.list_len() {
                    self.sel += 1;
                }
            }
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = self.list_len().saturating_sub(1),
            KeyCode::Enter => {
                if let Some(k) = self.selected_key() {
                    self.job_key = Some(k);
                    self.set_view(View::Job);
                }
            }
            KeyCode::Char('k') => {
                if let (View::Queue, Some((e, _))) = (self.view, self.queue_row(self.sel)) {
                    self.input = Input::ConfirmKill(e.pid, e.key.clone());
                } else {
                    self.message = Some("k works on a row in the Queue view".into());
                }
            }
            _ => {}
        }
        true
    }

    pub fn mouse(&mut self, kind: MouseEventKind, _col: u16, row: u16) {
        match kind {
            MouseEventKind::ScrollUp => self.range = self.range.saturating_sub(1),
            MouseEventKind::ScrollDown => self.range = (self.range + 1).min(RANGES.len() - 1),
            MouseEventKind::Down(MouseButton::Left) => {
                let (y0, n, first) = self.list_rows;
                if row >= y0 && ((row - y0) as usize) < n {
                    self.sel = first + (row - y0) as usize;
                }
            }
            _ => {}
        }
    }
}

fn sort_runs(v: &mut [RunRow], col: usize, rev: bool) {
    match col % 5 {
        1 => v.sort_by(|a, b| b.waited_s.total_cmp(&a.waited_s)),
        2 => v.sort_by(|a, b| b.dur_s.total_cmp(&a.dur_s)),
        3 => v.sort_by(|a, b| b.cores_wanted.unwrap_or(0.0).total_cmp(&a.cores_wanted.unwrap_or(0.0))),
        4 => v.sort_by_key(|r| std::cmp::Reverse(r.peak_mem_kb)),
        _ => {}
    }
    if rev {
        v.reverse();
    }
}

pub const RUN_SORTS: [&str; 5] = ["newest", "waited", "duration", "cores", "memory"];
pub const NS_SORTS: [&str; 4] = ["CPU-hours", "GB-hours", "runs", "wait"];

fn sort_ns(v: &mut [NsRow], col: usize, rev: bool) {
    match col % 4 {
        1 => v.sort_by(|a, b| b.gb_hours.total_cmp(&a.gb_hours)),
        2 => v.sort_by_key(|r| std::cmp::Reverse(r.runs)),
        3 => v.sort_by(|a, b| b.wait_s.total_cmp(&a.wait_s)),
        _ => v.sort_by(|a, b| b.cpu_hours.total_cmp(&a.cpu_hours)),
    }
    if rev {
        v.reverse();
    }
}

fn recorder_running(dir: &std::path::Path) -> bool {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("recorder.lock"))
        .map(|f| f.try_lock().is_err())
        .unwrap_or(false)
}

/// Marks this process as a viewer, so the recorder keeps running while the
/// dashboard is open. Removed on drop.
struct Viewer(PathBuf);

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run(args: &[String]) -> Result<i32> {
    let dir = config::state_dir();
    let q = Queue::open(&dir)?;
    let cwd = std::env::current_dir()?;
    let cfg = Config::load(&cwd, &key::checkout_root(&cwd))?;
    let viewer = Viewer(dir.join("viewers").join(std::process::id().to_string()));
    std::fs::write(&viewer.0, "")?;
    runner::ensure_recorder(&q.dir);

    let mut app = App::new(dir, cfg);
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "--queue") {
        app.view = View::Queue;
    }
    if let Some(v) = arg("--view") {
        match VIEWS.iter().find(|(_, n)| n.eq_ignore_ascii_case(&v)) {
            Some((view, _)) => app.view = *view,
            None => {
                anyhow::bail!("unknown view {v:?}; one of: {}", VIEWS.iter().map(|(_, n)| n.to_lowercase()).collect::<Vec<_>>().join(", "))
            }
        }
    }
    if let Some(r) = arg("--range")
        && let Some(i) = RANGES.iter().position(|(n, _)| *n == r)
    {
        app.range = i;
    }
    app.job_key = arg("--job");
    if app.view == View::Job && app.job_key.is_none() {
        anyhow::bail!("--view job needs --job KEY");
    }
    // One frame as plain text, for scripts, agents and screenshots.
    if args.iter().any(|a| a == "--print") {
        let (w, h) = crossterm::terminal::size().unwrap_or((140, 40));
        let w = arg("--width").and_then(|v| v.parse().ok()).unwrap_or(w.max(100));
        let h = arg("--height").and_then(|v| v.parse().ok()).unwrap_or(h.max(30));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))?;
        // The first draw sizes the chart; the second draws it with data for that size.
        for _ in 0..2 {
            app.load()?;
            term.draw(|f| views::draw(f, &mut app))?;
        }
        let buf = term.backend().buffer().clone();
        for y in 0..h {
            let line: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
        return Ok(0);
    }
    let mut term = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let result = (|| -> Result<()> {
        let mut last = Instant::now() - Duration::from_secs(10);
        loop {
            if !app.paused && last.elapsed() >= Duration::from_secs(1) {
                if let Err(e) = app.load() {
                    app.message = Some(format!("could not read data: {e}"));
                }
                last = Instant::now();
            }
            term.draw(|f| views::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(k) => {
                        if !app.key(k) {
                            return Ok(());
                        }
                        last = Instant::now() - Duration::from_secs(10);
                    }
                    Event::Mouse(m) => {
                        app.mouse(m.kind, m.column, m.row);
                        last = Instant::now() - Duration::from_secs(10);
                    }
                    _ => {}
                }
            }
        }
    })();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    drop(viewer);
    result.map(|_| 0)
}
