//! `taskguard top`: the dashboard.

mod chart;
mod views;

use crate::commands;
use crate::config::{self, Config, SETTINGS};
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
    Config,
    Help,
}

/// The tabs, in order. The Job view is no tab: Enter on a row opens it.
pub const VIEWS: [(View, &str); 8] = [
    (View::Overview, "Overview"),
    (View::Queue, "Queue"),
    (View::Runs, "Runs"),
    (View::Trends, "Trends"),
    (View::Warnings, "Warnings"),
    (View::Namespaces, "Namespaces"),
    (View::Config, "Config"),
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
    ConfirmStart(i32, String),
    /// Needs for a job, typed as "CORES MEMORY", for example "2 4G".
    EditNeeds(i32, String, String),
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
    /// Groups of programs outside taskguard, per column, and their newest reading.
    pub groups: BTreeMap<String, Vec<(f64, f64)>>,
    pub groups_now: Vec<db::GroupReading>,
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
    /// The run of this job that is going on now, if any.
    pub live: Option<LiveRun>,
}

pub struct LiveRun {
    pub entry: crate::queue::Entry,
    pub waiting: bool,
    /// Samples so far: time, cores used, cores wanted, memory, page-ins per second.
    pub samples: Vec<(f64, f64, f64, u64, f64)>,
}

pub struct App {
    pub dir: PathBuf,
    pub cfg: Config,
    pub view: View,
    pub prev_view: View,
    /// The view and row to return to when the Job view closes.
    pub back: (View, usize),
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
    /// Where the tabs and the footer keys sit on screen, for mouse clicks:
    /// (row, first column, last column, what a click does).
    pub hits: Vec<(u16, u16, u16, Click)>,
    pub cwd: PathBuf,
    pub checkout: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Click {
    Tab(View),
    Key(KeyCode),
}

impl App {
    pub fn new(dir: PathBuf, cfg: Config) -> App {
        App {
            dir,
            cfg,
            view: View::Overview,
            prev_view: View::Overview,
            back: (View::Overview, 0),
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
            hits: Vec::new(),
            cwd: PathBuf::new(),
            checkout: PathBuf::new(),
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
                let snap = commands::snapshot(&self.dir, &self.cfg, Some(&db)).ok();
                let live = snap.and_then(|s| {
                    let running = s.running.into_iter().find(|e| &e.key == k).map(|e| (e, false));
                    running.or_else(|| s.waiting.into_iter().find(|w| &w.entry.key == k).map(|w| (w.entry, true)))
                });
                let live = match live {
                    Some((entry, waiting)) => {
                        let mut st = db.conn.prepare(
                            "SELECT ts, cores_used, cores_wanted, mem_kb, pageins_per_s FROM job_samples WHERE run_id = ?1 ORDER BY ts",
                        )?;
                        let samples = st
                            .query_map([entry.run_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u64, r.get(4)?)))?
                            .collect::<std::result::Result<_, _>>()?;
                        Some(LiveRun { entry, waiting, samples })
                    }
                    None => None,
                };
                Some(JobData {
                    live,
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
            groups: dash::group_series(&db, from, to, n, crate::recorder::EVERY)?,
            groups_now: db.latest_groups().unwrap_or_default(),
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
            View::Config => SETTINGS.len(),
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

    /// Open the Job view for a key, and remember where to return to.
    fn open_job(&mut self, key: String) {
        if self.view != View::Job {
            self.back = (self.view, self.sel);
        }
        self.job_key = Some(key);
        self.view = View::Job;
        self.sel = 0;
    }

    fn close_job(&mut self) {
        let (view, sel) = self.back;
        self.view = view;
        self.sel = sel;
    }

    fn step_tab(&mut self, forward: bool) {
        let i = VIEWS.iter().position(|(v, _)| *v == self.view).unwrap_or(0);
        let j = if forward { (i + 1) % VIEWS.len() } else { (i + VIEWS.len() - 1) % VIEWS.len() };
        self.set_view(VIEWS[j].0);
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

    /// Queue view rows: running jobs first, then waiting ones in queue
    /// order. The bool is true for a waiting job.
    pub fn queue_row(&self, i: usize) -> Option<(&crate::queue::Entry, bool)> {
        let s = self.data.snap.as_ref()?;
        if i < s.running.len() { Some((&s.running[i], false)) } else { s.waiting.get(i - s.running.len()).map(|w| (&w.entry, true)) }
    }

    /// The waiting job on the selected Queue row, with its decision.
    pub fn queue_waiting(&self) -> Option<&crate::report::Waiting> {
        let s = self.data.snap.as_ref()?;
        self.sel.checked_sub(s.running.len()).and_then(|i| s.waiting.get(i))
    }

    /// A job from an older taskguard does not read nudges.
    fn nudgeable(&mut self, e: &crate::queue::Entry) -> bool {
        if e.version.is_none() {
            self.message = Some(format!("{} runs an older taskguard (before 0.1.3); it cannot be started or changed from here", e.key));
            return false;
        }
        true
    }

    fn apply_input(&mut self, input: Input) {
        let q = Queue { dir: self.dir.clone() };
        match input {
            Input::ConfirmKill(pid, key) => {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                self.message = Some(format!("sent SIGTERM to {key} (pid {pid})"));
            }
            Input::ConfirmStart(pid, key) => {
                self.message = Some(match q.nudge(pid, |n| n.start = true) {
                    Ok(()) => format!("{key} starts within a second"),
                    Err(e) => format!("could not start {key}: {e}"),
                });
            }
            Input::EditNeeds(pid, key, text) => {
                self.message = Some(match parse_needs(&text) {
                    Ok((cpu, mem)) => match q.nudge(pid, |n| {
                        n.need_cpu = cpu.or(n.need_cpu);
                        n.need_mem_kb = mem.or(n.need_mem_kb);
                    }) {
                        Ok(()) => format!("{key}: needs changed for this run; the next run learns from what it really uses"),
                        Err(e) => format!("could not change {key}: {e}"),
                    },
                    Err(e) => e,
                });
            }
            Input::None | Input::Filter(_) => {}
        }
    }

    /// Change the selected setting by one step, and save it in the user config.
    fn step_setting(&mut self, up: bool) {
        let Some(set) = SETTINGS.get(self.sel) else { return };
        let path = config::user_config_path();
        self.message = Some(match config::step_user_setting(&path, &self.cfg, set, up) {
            Ok(text) => {
                if let Ok(c) = Config::load(&self.cwd, &self.checkout) {
                    self.cfg = c;
                }
                format!("{text}, saved in {}", path.display())
            }
            Err(e) => format!("could not save: {e}"),
        });
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
            Input::ConfirmKill(..) | Input::ConfirmStart(..) => {
                let input = std::mem::replace(&mut self.input, Input::None);
                if let KeyCode::Char('y') = k.code {
                    self.apply_input(input);
                }
                return true;
            }
            Input::EditNeeds(_, _, text) => {
                match k.code {
                    KeyCode::Enter => {
                        let input = std::mem::replace(&mut self.input, Input::None);
                        self.apply_input(input);
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
            Input::None => {}
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return false;
        }
        self.message = None;
        // Cmd+arrows (when the terminal reports Cmd), Option+arrows and
        // Shift+arrows move between tabs. Option+arrows arrive as Alt+b and
        // Alt+f in terminals that send them as words.
        let mods = k.modifiers;
        let tab_mod = mods.intersects(KeyModifiers::SUPER | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::META);
        match k.code {
            KeyCode::Left | KeyCode::Right if tab_mod => {
                self.step_tab(k.code == KeyCode::Right);
                return true;
            }
            KeyCode::Char('b' | 'f') if mods.contains(KeyModifiers::ALT) => {
                self.step_tab(k.code == KeyCode::Char('f'));
                return true;
            }
            _ => {}
        }
        if self.view == View::Job && matches!(k.code, KeyCode::Esc | KeyCode::Backspace) {
            self.close_job();
            return true;
        }
        if self.view == View::Config {
            match k.code {
                KeyCode::Left | KeyCode::Char('-') => return self.then(|a| a.step_setting(false)),
                KeyCode::Right | KeyCode::Char('+' | '=') | KeyCode::Enter => return self.then(|a| a.step_setting(true)),
                _ => {}
            }
        }
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char(c @ '1'..='8') => {
                let i = (c as u8 - b'1') as usize;
                self.set_view(VIEWS[i].0);
            }
            KeyCode::Tab | KeyCode::BackTab => self.step_tab(k.code == KeyCode::Tab),
            KeyCode::Char('j') => match self.job_key.clone() {
                Some(k) => self.open_job(k),
                None => self.message = Some("no job opened yet: select a row and press Enter".into()),
            },
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
                    self.open_job(k);
                }
            }
            KeyCode::Char('k') => {
                if let (View::Queue, Some((e, _))) = (self.view, self.queue_row(self.sel)) {
                    self.input = Input::ConfirmKill(e.pid, e.key.clone());
                } else {
                    self.message = Some("k works on a row in the Queue view".into());
                }
            }
            KeyCode::Char('g') => match (self.view, self.queue_row(self.sel).map(|(e, w)| (e.clone(), w))) {
                (View::Queue, Some((e, true))) => {
                    if self.nudgeable(&e) {
                        self.input = Input::ConfirmStart(e.pid, e.key.clone());
                    }
                }
                (View::Queue, Some((e, false))) => self.message = Some(format!("{} already runs", e.key)),
                _ => self.message = Some("g works on a waiting job in the Queue view".into()),
            },
            KeyCode::Char('e') => match (self.view, self.queue_row(self.sel).map(|(e, _)| e.clone())) {
                (View::Queue, Some(e)) => {
                    if self.nudgeable(&e) {
                        let now = format!("{:.1} {}", e.need_cpu, crate::report::gb(e.need_mem_kb).replace(' ', ""));
                        self.input = Input::EditNeeds(e.pid, e.key.clone(), now);
                    }
                }
                _ => self.message = Some("e works on a job in the Queue view".into()),
            },
            _ => {}
        }
        true
    }

    fn then(&mut self, f: impl FnOnce(&mut App)) -> bool {
        f(self);
        true
    }

    pub fn mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) {
        match kind {
            // The wheel scrolls a list, and zooms the time range on the Overview.
            MouseEventKind::ScrollUp if self.view == View::Overview => self.range = self.range.saturating_sub(1),
            MouseEventKind::ScrollDown if self.view == View::Overview => self.range = (self.range + 1).min(RANGES.len() - 1),
            MouseEventKind::ScrollUp => self.sel = self.sel.saturating_sub(1),
            MouseEventKind::ScrollDown => self.sel = (self.sel + 1).min(self.list_len().saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, _, _, click)) = self.hits.iter().find(|(y, x0, x1, _)| *y == row && col >= *x0 && col <= *x1).copied() {
                    match click {
                        Click::Tab(v) => self.set_view(v),
                        Click::Key(code) => {
                            self.key(KeyEvent::new(code, KeyModifiers::NONE));
                        }
                    }
                    return;
                }
                let (y0, n, first) = self.list_rows;
                if row >= y0 && ((row - y0) as usize) < n {
                    self.sel = first + (row - y0) as usize;
                }
            }
            _ => {}
        }
    }
}

/// "2 4G" sets both needs; "2" only the cores; "4G" or "- 4G" only the memory.
fn parse_needs(text: &str) -> std::result::Result<(Option<f64>, Option<u64>), String> {
    let bad = || format!("could not read {text:?}: type cores and memory, for example \"2 4G\"");
    let parts: Vec<&str> = text.split_whitespace().collect();
    let cores = |p: &str| if p == "-" { Ok(None) } else { p.parse::<f64>().map(Some).map_err(|_| bad()) };
    let mem = |p: &str| if p == "-" { Ok(None) } else { config::parse_size_kb(p).map(Some).map_err(|_| bad()) };
    match parts.as_slice() {
        [one] if one.ends_with(|c: char| c.is_ascii_alphabetic()) => Ok((None, mem(one)?)),
        [one] => Ok((cores(one)?, None)),
        [c, m] => Ok((cores(c)?, mem(m)?)),
        _ => Err(bad()),
    }
    .and_then(|(c, m)| match (c, m) {
        (None, None) => Err(bad()),
        (Some(c), _) if c <= 0.0 => Err("cores must be above 0".into()),
        other => Ok(other),
    })
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
    app.checkout = key::checkout_root(&cwd);
    app.cwd = cwd;
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "--queue") {
        app.view = View::Queue;
    }
    if let Some(v) = arg("--view") {
        match VIEWS.iter().chain([(View::Job, "Job")].iter()).find(|(_, n)| n.eq_ignore_ascii_case(&v)) {
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
    // Terminals that support it then report Cmd (Super) on arrow keys.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
        && crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
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
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    drop(viewer);
    result.map(|_| 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_are_typed_as_cores_and_memory() {
        assert_eq!(parse_needs("2 4G"), Ok((Some(2.0), Some(4 << 20))));
        assert_eq!(parse_needs("1.5"), Ok((Some(1.5), None)));
        assert_eq!(parse_needs("512M"), Ok((None, Some(512 << 10))));
        assert_eq!(parse_needs("- 1G"), Ok((None, Some(1 << 20))));
        assert!(parse_needs("").is_err());
        assert!(parse_needs("0 1G").is_err());
        assert!(parse_needs("two 1G").is_err());
    }
}
