//! The machine-wide queue: one file per waiting job and one per running job,
//! under a state directory, guarded by one file lock.
//!
//! The owner pid is part of every file name, so a job that dies (Ctrl-C,
//! SIGKILL) is reaped by the next process that looks, and never wedges the
//! queue. The kernel releases the lock itself when its holder dies, which
//! replaces tsc-queue's mkdir lock and its stale-age guess.

use crate::machine::MachineSample;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

/// One waiting or running job, as a JSON file in `wait/` or `run/`.
///
/// Every taskguard version on the machine reads these files, and DALP pins a
/// version per worktree, so old and new versions run side by side. The rules:
/// - Never remove or rename a field, and never change its JSON type. v0.1.0 to
///   v0.1.2 need every field below except `estimate_from`; an entry they cannot
///   read is a job they do not see, and they start work on top of it.
/// - A new field is optional: `#[serde(default)]` covers it, so this version
///   still reads the entries of older ones.
/// - A change that cannot follow these rules needs a new state directory.
///
/// `tests::entry_format_is_stable` holds the v0.1.2 shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Entry {
    pub ticket: u64,
    pub pid: i32,
    pub run_id: i64,
    pub key: String,
    pub ns: String,
    pub label: Option<String>,
    /// The pool name as shown, and the pool identity used for counting slots
    /// (it includes the checkout for per-checkout pools).
    pub pool: Option<String>,
    pub pool_key: Option<String>,
    pub pool_slots: Option<u32>,
    pub checkout: String,
    pub queued_at: f64,
    pub started_at: Option<f64>,
    pub need_cpu: f64,
    pub need_mem_kb: u64,
    /// False while the job has no history: its needs are still unknown.
    pub known: bool,
    /// A minimum raised the need above what was learned.
    pub raised_by_min: bool,
    /// Started with --now: it skipped the queue.
    pub now: bool,
    pub live_cpu: f64,
    pub live_mem_kb: u64,
    /// When a newer job first started while this one waited.
    pub bypassed_since: Option<f64>,
    /// The main blocker, in words, for the status screen, and since when it holds.
    pub blocker: Option<String>,
    pub blocker_since: Option<f64>,
    /// Learned median duration, for estimated start times.
    pub est_dur_s: Option<f64>,
    /// For a job with no history: where its estimated need comes from.
    pub estimate_from: Option<String>,
    /// The taskguard version that owns the entry. Older versions leave it
    /// out, and they do not read nudges either.
    pub version: Option<String>,
    /// Set when someone changed the needs by hand, in words.
    pub by_hand: Option<String>,
    /// When the job's memory last rose more than 10% above its peak so far.
    /// A first run that has stopped growing shows what it takes.
    pub mem_grew_at: Option<f64>,
    /// The needs the job started with. While it runs, its needs follow its
    /// peak; these keep what it was admitted with, to show the overage.
    pub start_need_cpu: Option<f64>,
    pub start_need_mem_kb: Option<u64>,
    /// Higher goes first. 0 by default; older versions leave it out.
    pub priority: i32,
    /// The owner pauses its command itself under memory pressure (auto_pause).
    /// Older versions leave it out, and their jobs are never picked to pause.
    pub pausable: bool,
    /// The command's pid, so a paused command can be resumed when its owner
    /// dies.
    pub child_pid: i32,
    /// Set while the command is paused (SIGSTOP), with when it was paused.
    pub paused_since: Option<f64>,
    /// How often, and how long in total, this run was paused so far.
    pub pauses: u32,
    pub paused_s: f64,
    /// Paused by hand (taskguard pause, or p in the dashboard). Only a resume
    /// by hand ends it; auto_pause leaves it alone.
    pub paused_by_hand: bool,
    /// The run of a task runner this job belongs to ("turbo:4242"), and when
    /// that run queued its first job. A job of an older run that can start
    /// goes first, so one run finishes before the next one takes the room.
    /// Older versions leave both out, and their jobs never step aside.
    pub pipeline: Option<String>,
    pub pipeline_since: Option<f64>,
    /// A long-lived job (a dev stack) left its start-up phase then: from
    /// that moment its needs follow what it uses. Older versions leave it out.
    pub steady_since: Option<f64>,
    /// Another job saw this one able to start for `STALL_S` while it still
    /// waited (`stall/<ticket>.<pid>`). It holds no job back any more. Read
    /// from the queue, never written into the entry.
    #[serde(skip)]
    pub stalled: bool,
}

impl Entry {
    /// Paused by auto_pause, not by hand: it resumes before anything new starts.
    pub fn auto_paused(&self) -> bool {
        self.paused_since.is_some() && !self.paused_by_hand
    }

    /// A long-lived job past its start-up. It runs until someone stops it,
    /// so the queue treats it as load on the machine, as it does programs
    /// outside taskguard: no job waits for it to end.
    pub fn steady(&self) -> bool {
        self.steady_since.is_some()
    }

    /// Seconds a running job has left by its learned duration, at least one.
    pub fn remaining(&self, now: f64) -> Option<f64> {
        self.est_dur_s.map(|d| (d - (now - self.started_at.unwrap_or(now))).max(1.0))
    }
}

impl Entry {
    /// When this job's place in line was taken: when its task-runner run
    /// queued its first job, or for a job outside any run, when it queued.
    fn line_since(&self) -> f64 {
        match (&self.pipeline, self.pipeline_since) {
            (Some(_), Some(since)) => since,
            _ => self.queued_at,
        }
    }

    /// Is `self` ahead of `other` in line? The one order every rule that holds
    /// a job back for another follows: a higher priority, then the older run
    /// (a job outside any run counts from when it queued), then the older
    /// ticket. Every hold points from a job to one behind it in this fixed
    /// order, so jobs can never wait for each other in a circle.
    pub fn ahead_of(&self, other: &Entry) -> bool {
        self.line_order(other).is_lt()
    }

    /// The order behind `ahead_of`, for sorting.
    ///
    /// The same ticket and pid is the same job, whatever its other fields say:
    /// a job compares its own entry, read back from the wait file, with the
    /// copy it holds, and a queue time that came back one step off must never
    /// put a job in line before itself.
    pub fn line_order(&self, other: &Entry) -> std::cmp::Ordering {
        if self.ticket == other.ticket && self.pid == other.pid {
            return std::cmp::Ordering::Equal;
        }
        self.priority
            .cmp(&other.priority)
            .reverse()
            .then_with(|| self.line_since().total_cmp(&other.line_since()))
            .then_with(|| self.ticket.cmp(&other.ticket))
            .then_with(|| self.pid.cmp(&other.pid))
    }
}

/// A first run has settled when its memory has not grown for this long...
pub const SETTLE_S: f64 = 20.0;
/// ...or when it has run this long: a long job must not hold back every new
/// one, and by then its needs follow its peak.
pub const SETTLE_MAX_S: f64 = 120.0;

impl Entry {
    /// Still on a guess: no history, no minimum, no needs set by hand.
    pub fn guessed(&self) -> bool {
        !self.known && !self.raised_by_min && self.by_hand.is_none()
    }

    /// How long until this running first run counts as settled; None once it has.
    pub fn settling_for(&self, now: f64) -> Option<f64> {
        let started = self.started_at?;
        if !self.guessed() {
            return None;
        }
        let grew = self.mem_grew_at.unwrap_or(started);
        let left = (SETTLE_S - (now - grew)).min(SETTLE_MAX_S - (now - started));
        (left > 0.0).then_some(left)
    }
}

/// A long-lived job, such as a dev stack: it runs until someone stops it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongLived {
    /// Seconds after its start that it holds its start-up peak.
    pub startup: f64,
    /// The highest memory its past runs took after their start-up: what it
    /// still grows to once it settles.
    pub steady_mem_kb: Option<u64>,
}

/// Room kept above a steady job's measured CPU use.
const STEADY_CPU_MARGIN: f64 = 0.25;

impl Entry {
    /// Move a running job's needs after one reading of it: `mem_kb` and the
    /// cores it used in the recent window.
    ///
    /// Past its needs, a job's needs follow its peak, with room to grow: what
    /// it promises to take must keep up with what it takes. A long-lived job
    /// does so through its start-up only. After that its needs start again
    /// from what it uses: a dev stack that took every core while it started
    /// and seeded, and idles at a few percent, must not keep every core for
    /// hours. Its memory need is what it holds plus a quarter, or what its
    /// past runs grew to after their start-up when that is more, and follows
    /// its peak from there: running out of memory kills processes. Its CPU
    /// need is measured use plus a margin, up or down, because runnable wait
    /// can reflect priority and contention rather than useful work.
    pub fn follow(&mut self, mem_kb: u64, wanted: f64, used_recent: f64, ncpu: f64, now: f64, long: Option<LongLived>) {
        if let Some(l) = long
            && self.steady_since.is_none()
            && now - self.started_at.unwrap_or(now) >= l.startup
        {
            self.steady_since = Some(now);
        }
        if self.steady_since.is_some() {
            // Once steady, historical peaks are startup-only. Follow current
            // use in both dimensions so idle stacks release their booking.
            self.need_mem_kb = mem_kb + mem_kb / 4;
            self.need_cpu = (used_recent * (1.0 + STEADY_CPU_MARGIN)).min(ncpu);
        } else if mem_kb > self.need_mem_kb {
            self.need_mem_kb = mem_kb + mem_kb / 4;
        } else if wanted > self.need_cpu {
            self.need_cpu = wanted.min(ncpu);
        }
    }
}

/// A request from the dashboard to one job: start now, or use other needs.
/// The job's own process reads it (`nudge/<pid>`) and acts on it, because
/// only that process may move its entry or decide that it starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Nudge {
    pub start: bool,
    /// Pause (true) or resume (false) a running job by hand.
    pub pause: Option<bool>,
    pub need_cpu: Option<f64>,
    pub need_mem_kb: Option<u64>,
}

pub struct Queue {
    pub dir: PathBuf,
}

pub struct Guard {
    _file: File,
}

impl Queue {
    pub fn open(dir: &Path) -> Result<Queue> {
        for d in ["wait", "run", "viewers", "nudge", "pipeline", "stall"] {
            fs::create_dir_all(dir.join(d)).with_context(|| format!("creating {}", dir.join(d).display()))?;
        }
        Ok(Queue { dir: dir.to_path_buf() })
    }

    pub fn lock(&self) -> Result<Guard> {
        let f = OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join("lock"))?;
        f.lock()?;
        Ok(Guard { _file: f })
    }

    pub fn next_ticket(&self) -> Result<u64> {
        let p = self.dir.join("seq");
        let n: u64 = fs::read_to_string(&p).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0) + 1;
        fs::write(&p, n.to_string())?;
        Ok(n)
    }

    pub fn wait_path(&self, e: &Entry) -> PathBuf {
        self.dir.join("wait").join(format!("{}.{}", e.ticket, e.pid))
    }

    pub fn run_path(&self, pid: i32) -> PathBuf {
        self.dir.join("run").join(pid.to_string())
    }

    /// When the task-runner run `id` ("turbo:4242", owned by `pid`) queued
    /// its first job. The first job of the run writes it; the file outlives
    /// that job, so the run keeps its age between waves of jobs.
    pub fn pipeline_since(&self, id: &str, pid: i32, now: f64) -> f64 {
        let path = self.dir.join("pipeline").join(format!("{}.{pid}", id.split(':').next().unwrap_or("run")));
        if let Some(t) = fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()) {
            return t;
        }
        let _ = fs::write(&path, now.to_string());
        now
    }

    fn stall_path(&self, e: &Entry) -> PathBuf {
        self.dir.join("stall").join(format!("{}.{}", e.ticket, e.pid))
    }

    /// Mark a waiting job as stalled: from now on it holds no job back. The
    /// mark stays until the job leaves the queue.
    pub fn mark_stalled(&self, e: &Entry, note: &Stall) -> Result<()> {
        let path = self.stall_path(e);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, serde_json::to_string(note)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn nudge_path(&self, pid: i32) -> PathBuf {
        self.dir.join("nudge").join(pid.to_string())
    }

    /// Add to the nudge for a job; an earlier one that was not read yet stays.
    pub fn nudge(&self, pid: i32, change: impl FnOnce(&mut Nudge)) -> Result<()> {
        let path = self.nudge_path(pid);
        let mut n: Nudge = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        change(&mut n);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, serde_json::to_string(&n)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Read and remove the nudge for a job, if there is one.
    pub fn take_nudge(&self, pid: i32) -> Option<Nudge> {
        let path = self.nudge_path(pid);
        let text = fs::read_to_string(&path).ok()?;
        let _ = fs::remove_file(&path);
        serde_json::from_str(&text).ok()
    }

    /// Write through a temp file and rename, so a reader never sees half a file.
    pub fn write(&self, path: &Path, e: &Entry) -> Result<()> {
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, serde_json::to_string(e)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn read_dir(&self, sub: &str) -> Vec<(PathBuf, Entry)> {
        let Ok(rd) = fs::read_dir(self.dir.join(sub)) else {
            return Vec::new();
        };
        let mut v: Vec<(PathBuf, Entry)> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            // Skip half-written files. Only the file name is checked: the state
            // directory itself may sit under a path that contains ".tmp".
            .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().contains(".tmp")))
            .filter_map(|p| {
                let e: Entry = serde_json::from_str(&fs::read_to_string(&p).ok()?).ok()?;
                Some((p, e))
            })
            .collect();
        v.sort_by_key(|(_, e)| e.ticket);
        v
    }

    pub fn waiting(&self) -> Vec<Entry> {
        let v: Vec<Entry> = self.read_dir("wait").into_iter().map(|(_, e)| e).collect();
        self.with_stalled(v)
    }

    fn with_stalled(&self, mut v: Vec<Entry>) -> Vec<Entry> {
        for e in &mut v {
            e.stalled = self.stall_path(e).exists();
        }
        v
    }

    /// The names in `run/` and `wait/`, and whether a nudge waits for `pid`.
    /// Listing a directory opens no file, so a waiting job can afford this
    /// often; it changes whenever a job joins or leaves the queue.
    pub fn shape(&self, pid: i32) -> Vec<String> {
        let mut v: Vec<String> = ["run", "wait"]
            .iter()
            .flat_map(|sub| fs::read_dir(self.dir.join(sub)).into_iter().flatten())
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| !n.contains(".tmp"))
            .collect();
        v.sort();
        if self.nudge_path(pid).exists() {
            v.push("nudge".into());
        }
        v
    }

    /// `reap`, then the running and the waiting jobs, from one read of each
    /// file.
    pub fn reap_and_read(&self) -> (Vec<i64>, Vec<Entry>, Vec<Entry>) {
        let (gone, [wait, run]) = self.reap_kept();
        (gone, run, self.with_stalled(wait))
    }

    pub fn running(&self) -> Vec<Entry> {
        self.read_dir("run").into_iter().map(|(_, e)| e).collect()
    }

    /// Drop entries whose owner process is gone. Returns the run ids that
    /// were abandoned, so the caller can close them in the database.
    pub fn reap(&self) -> Vec<i64> {
        self.reap_kept().0
    }

    /// `reap`, plus the entries of `wait/` and `run/` that stay.
    fn reap_kept(&self) -> (Vec<i64>, [Vec<Entry>; 2]) {
        let mut gone = Vec::new();
        let mut kept: [Vec<Entry>; 2] = Default::default();
        for (i, sub) in ["wait", "run"].into_iter().enumerate() {
            let mut read: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            for (p, e) in self.read_dir(sub) {
                read.insert(p.clone());
                if alive(e.pid) {
                    kept[i].push(e);
                } else {
                    // An owner that died while its command was paused cannot
                    // resume it any more: do it here.
                    if e.paused_since.is_some() {
                        signal_tree(e.child_pid, libc::SIGCONT);
                    }
                    let _ = fs::remove_file(&p);
                    gone.push(e.run_id);
                }
            }
            // An entry this version cannot read (a newer version broke the
            // format rules on `Entry`) still names its owner in the file name:
            // `wait/<ticket>.<pid>` and `run/<pid>`. Drop it once that is gone.
            let Ok(rd) = fs::read_dir(self.dir.join(sub)) else { continue };
            for p in rd.filter_map(|e| e.ok()).map(|e| e.path()) {
                let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
                let owner = name.rsplit('.').next().and_then(|pid| pid.parse::<i32>().ok());
                if read.contains(&p)
                    || name.contains(".tmp")
                    || fs::read_to_string(&p).is_ok_and(|t| serde_json::from_str::<Entry>(&t).is_ok())
                {
                    continue;
                }
                if owner.is_some_and(|pid| !alive(pid)) {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        // stall/<ticket>.<pid>: the mark goes when the job leaves the queue.
        if let Ok(rd) = fs::read_dir(self.dir.join("stall")) {
            for f in rd.filter_map(|e| e.ok()) {
                if !self.dir.join("wait").join(f.file_name()).exists() {
                    let _ = fs::remove_file(f.path());
                }
            }
        }
        for sub in ["viewers", "nudge", "pipeline"] {
            let Ok(rd) = fs::read_dir(self.dir.join(sub)) else { continue };
            for f in rd.filter_map(|e| e.ok()) {
                // viewers/<pid>, nudge/<pid>, pipeline/<runner>.<pid>
                let name = f.file_name().to_string_lossy().into_owned();
                let field = if sub == "pipeline" { name.rsplit('.').next() } else { name.split('.').next() };
                let pid: i32 = field.and_then(|p| p.parse().ok()).unwrap_or(0);
                if !alive(pid) {
                    let _ = fs::remove_file(f.path());
                }
            }
        }
        (gone, kept)
    }

    pub fn viewers(&self) -> usize {
        fs::read_dir(self.dir.join("viewers")).map(|rd| rd.count()).unwrap_or(0)
    }

    /// When recent jobs with no history started, newest last.
    pub fn unknown_starts(&self) -> Vec<f64> {
        fs::read_to_string(self.dir.join("unknown_starts"))
            .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Record a start and forget the ones older than `window` seconds.
    pub fn add_unknown_start(&self, t: f64, window: f64) {
        let mut v: Vec<f64> = self.unknown_starts().into_iter().filter(|s| t - s < window).collect();
        v.push(t);
        let text: String = v.iter().map(|s| format!("{s}\n")).collect();
        let _ = fs::write(self.dir.join("unknown_starts"), text);
    }

    /// When a job last paused or resumed for memory.
    pub fn last_pause(&self) -> f64 {
        fs::read_to_string(self.dir.join("last_pause")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0)
    }

    pub fn set_last_pause(&self, t: f64) {
        let _ = fs::write(self.dir.join("last_pause"), t.to_string());
    }

    /// When the queue was last busy, for the recorder's idle exit.
    pub fn touch_activity(&self) {
        let _ = fs::write(self.dir.join("last_activity"), crate::db::now().to_string());
    }

    pub fn last_activity(&self) -> f64 {
        fs::read_to_string(self.dir.join("last_activity")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0)
    }
}

pub fn alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0) checks existence without sending anything. EPERM means the
    // process exists but belongs to someone else.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// ---------------------------------------------------------------- decide ----

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub cpu_max_pct: f64,
    pub mem_max_pct: f64,
    pub learn_stagger: f64,
    pub max_bypass: f64,
    /// Until an older job has waited this long, its reservation holds only
    /// while it could start; newer jobs that fit may start while it cannot.
    /// At or below `max_bypass` (0 by default) a reservation always holds.
    pub max_backfill: f64,
    /// Jobs that usually end sooner than this skip the CPU check.
    pub cpu_min_duration: f64,
    /// Memory pressure (0-100) at which nothing new starts.
    pub pressure_max: f64,
    /// A job short of memory by at most this share of RAM (0-100) starts.
    pub noise_mem_pct: f64,
    /// A job short of CPU by at most this many cores starts.
    pub noise_cpu: f64,
    /// A job that only programs outside taskguard keep out starts, while
    /// memory would stay under `outside_mem_max_pct` of RAM.
    pub outside_admit: bool,
    pub outside_mem_max_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Blocker {
    /// Nothing runs, but an older job is first in line.
    Older {
        key: String,
    },
    /// An older job has been passed for too long; it goes first now.
    Reserved {
        key: String,
        waited_s: f64,
    },
    /// A waiting job with a higher priority goes first.
    Priority {
        key: String,
        priority: i32,
    },
    /// A job of an older task-runner run can start: that run goes first.
    Pipeline {
        key: String,
        pipeline: String,
    },
    Slots {
        pool: String,
        busy: u32,
        max: u32,
        holders: Vec<String>,
    },
    LearnStagger {
        wait_s: f64,
    },
    /// A first run still grows: another first run waits until it has shown
    /// what it takes.
    Settling {
        key: String,
        wait_s: f64,
    },
    /// The kernel reports memory pressure: nothing new starts.
    Pressure {
        level: f64,
        limit: f64,
    },
    /// A running job is paused for memory: it resumes before anything new starts.
    Paused {
        key: String,
    },
    Memory {
        would_pct: f64,
        limit_pct: f64,
        short_kb: u64,
        used_kb: u64,
        reserve_kb: u64,
        need_kb: u64,
        total_kb: u64,
    },
    Cpu {
        would: f64,
        limit: f64,
        short: f64,
        busy: f64,
        reserve: f64,
        need: f64,
    },
}

impl Blocker {
    pub fn name(&self) -> &'static str {
        match self {
            Blocker::Older { .. } => "order",
            Blocker::Reserved { .. } => "reserved",
            Blocker::Priority { .. } => "priority",
            Blocker::Pipeline { .. } => "pipeline",
            Blocker::Slots { .. } => "slots",
            Blocker::LearnStagger { .. } | Blocker::Settling { .. } => "learning",
            Blocker::Memory { .. } => "memory",
            Blocker::Pressure { .. } => "pressure",
            Blocker::Paused { .. } => "paused",
            Blocker::Cpu { .. } => "cpu",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Admit {
        reason: String,
        /// Set when the job starts before the limits say it fits: what it
        /// lacks, and the rule that lets it start anyway.
        #[serde(skip_serializing_if = "Option::is_none")]
        early: Option<String>,
    },
    Wait {
        blockers: Vec<Blocker>,
    },
}

/// Memory that running jobs have not claimed yet: a job sitting at 2 GB whose
/// history says it peaks at 20 GB still has 18 GB to take, and that has to be
/// reserved now. Without this, a second job is admitted into space the first
/// one is about to eat. The same holds for CPU.
/// A job paused by hand reserves no CPU: it uses none until someone resumes
/// it, and the room it leaves is what the pause was for. Its memory stays
/// reserved, since it keeps what it holds and grows again when it resumes.
pub fn reserve(running: &[Entry]) -> (f64, u64) {
    running.iter().fold((0.0, 0), |(c, m), e| {
        let cpu = if e.paused_by_hand { 0.0 } else { (e.need_cpu - e.live_cpu).max(0.0) };
        (c + cpu, m + e.need_mem_kb.saturating_sub(e.live_mem_kb))
    })
}

/// Does `a` belong to an older task-runner run than `b`? Only jobs that are
/// both in a run, in different runs, are ordered this way.
pub fn runs_before(a: &Entry, b: &Entry) -> bool {
    matches!((&a.pipeline, a.pipeline_since, &b.pipeline, b.pipeline_since), (Some(pa), Some(sa), Some(pb), Some(sb)) if pa != pb && sa < sb)
}

/// The cores running jobs need in all. A job paused by hand needs none.
pub fn promised_cpu(running: &[Entry]) -> f64 {
    running.iter().filter(|e| !e.paused_by_hand).map(|e| e.need_cpu).sum()
}

/// How far running jobs are above their needs right now: the part of the
/// load the estimates did not see coming.
pub fn over(running: &[Entry]) -> (f64, u64) {
    running.iter().fold((0.0, 0), |(c, m), e| {
        let (cpu, mem) = (e.start_need_cpu.unwrap_or(e.need_cpu), e.start_need_mem_kb.unwrap_or(e.need_mem_kb));
        (c + (e.live_cpu - cpu).max(0.0), m + e.live_mem_kb.saturating_sub(mem))
    })
}

/// May `me` start now? A pure function of the machine reading and the queue,
/// so the status screen and the waiting job always agree.
pub fn decide(
    m: &MachineSample,
    lim: &Limits,
    running: &[Entry],
    waiting: &[Entry],
    me: &Entry,
    now: f64,
    unknown_starts: &[f64],
) -> Decision {
    // Jobs ahead in line, first in line first.
    // A stalled job holds nobody back: it does not start whatever we wait.
    let mut older: Vec<&Entry> = waiting.iter().filter(|w| !w.stalled && w.ahead_of(me)).collect();
    older.sort_by(|a, b| a.line_order(b));

    // The kernel's own alarm comes first and holds back every job, even when
    // taskguard runs nothing: then the pressure comes from other programs, and
    // one more job only makes it worse. This cannot deadlock the queue: jobs
    // start again as soon as the pressure eases, and --now still skips it.
    if let Some(p) = m.mem_pressure.filter(|p| *p >= lim.pressure_max) {
        return Decision::Wait { blockers: vec![Blocker::Pressure { level: p, limit: lim.pressure_max }] };
    }

    // A job paused for memory gets its room back before anything new starts.
    if let Some(p) = running.iter().find(|e| e.auto_paused()) {
        return Decision::Wait { blockers: vec![Blocker::Paused { key: p.key.clone() }] };
    }

    // Always make progress: with nothing running, the oldest job starts,
    // whatever the readings say. This is what keeps the queue from deadlocking
    // on a machine that is busy with work outside taskguard. A long-lived job
    // past its start-up counts as such work: it may run for hours, and a job
    // it keeps out would otherwise hold up the whole queue until it is stopped.
    if running.iter().all(Entry::steady) {
        return match older.first() {
            None if running.is_empty() => Decision::Admit { reason: "nothing else is running".into(), early: None },
            None => Decision::Admit { reason: "nothing else is running but long-lived jobs past their start-up".into(), early: None },
            Some(o) => Decision::Wait { blockers: vec![Blocker::Older { key: o.key.clone() }] },
        };
    }

    let room = Room::new(m, lim, running);

    let mut blockers = Vec::new();
    // With backfill on, a job ahead that goes first holds its turn only while
    // it could start itself: a job that waits for memory cannot use the room
    // it would hold, so newer jobs that fit may start meanwhile. Past
    // `max_backfill` only jobs that should end before it could start anyway
    // still do. They take no room it could use, and a stream of them cannot
    // keep it out: it starts once the running jobs have made room for it, or
    // when none runs, which they do not put off.
    let holds = |w: &Entry| {
        let (theirs, _) = room.left(m, lim, running, w, now, unknown_starts);
        theirs.is_empty()
            || now - w.queued_at > lim.max_backfill && !me.est_dur_s.zip(soonest_start(&theirs, running, now)).is_some_and(|(d, t)| d <= t)
    };
    // An older job that newer ones have passed for `max_bypass` goes first.
    // Only a job ahead in line holds `me` back, so a job of an older run is
    // never held back for one of a newer run, or for a job that queued after
    // that run began.
    if let Some(r) = older
        .iter()
        .find(|w| w.bypassed_since.is_some() && now - w.queued_at > lim.max_bypass && (lim.max_backfill <= lim.max_bypass || holds(w)))
    {
        blockers.push(Blocker::Reserved { key: r.key.clone(), waited_s: now - r.queued_at });
    }
    // A waiting job with a higher priority goes first, unless its own pool is
    // full: then it cannot start anyway, and holding others back gains nothing.
    // With backfill on, it holds them back as a reservation does.
    let pool_full = |w: &Entry| match (&w.pool_key, w.pool_slots) {
        (Some(pk), Some(max)) => running.iter().filter(|e| e.pool_key.as_ref() == Some(pk)).count() as u32 >= max,
        _ => false,
    };
    if let Some(h) = older.iter().find(|w| w.priority > me.priority && !pool_full(w) && (lim.max_backfill <= 0.0 || holds(w))) {
        blockers.push(Blocker::Priority { key: h.key.clone(), priority: h.priority });
    }
    // A job of an older task-runner run that could start now goes first, so
    // one run finishes before the next one takes the room: twenty runs that
    // each move a little finish later than twenty runs in turn. A job of the
    // older run that cannot start holds nothing back, so no room is wasted.
    if let Some(o) = older.iter().find(|w| runs_before(w, me) && room.left(m, lim, running, w, now, unknown_starts).0.is_empty()) {
        blockers.push(Blocker::Pipeline { key: o.key.clone(), pipeline: o.pipeline.clone().unwrap_or_default() });
    }
    let (mine, early) = room.left(m, lim, running, me, now, unknown_starts);
    blockers.extend(mine);
    if blockers.is_empty() {
        let cpu = if short(lim, me) {
            format!("CPU {:.1}+{:.1} promised of {:.1} cores (short job)", room.promised_cpu, me.need_cpu, room.cpu_limit)
        } else {
            format!("CPU {:.1}+{:.1}+{:.1} of {:.1} cores", m.cpu_busy, room.res_cpu, me.need_cpu, room.cpu_limit)
        };
        let mem_would = room.mem_would(me);
        let reason = format!(
            "{}: {cpu}, memory {:.0}% of {:.0}%",
            if early.is_some() { "nearly fits" } else { "fits" },
            pct(mem_would, m.mem_total_kb as f64),
            lim.mem_max_pct
        );
        Decision::Admit { reason, early }
    } else {
        Decision::Wait { blockers }
    }
}

/// In how many seconds a job that `blockers` keep out could start at the
/// soonest, by the learned durations of the running jobs: once they have
/// freed what it lacks, or else once none of them runs but long-lived jobs
/// past their start-up, as then the first in line starts whatever the readings
/// say. None when that hangs on a job whose duration is unknown.
fn soonest_start(blockers: &[Blocker], running: &[Entry], now: f64) -> Option<f64> {
    let drained = running.iter().filter(|e| !e.steady()).try_fold(0.0, |t: f64, e| Some(t.max(e.remaining(now)?)));
    blockers.iter().try_fold(0.0, |t: f64, b| Some(t.max(crate::report::start_eta(b, running, now).or(drained)?)))
}

// ----------------------------------------------------------------- stall ----

/// A job ahead in line that may start does so within one poll. One that may
/// start by these rules, yet still waits this long, has an owner that decides
/// by other rules or not at all, and holds the queue up for nothing.
pub const STALL_S: f64 = 10.0;

/// The first job ahead of `me` in line that these rules would start now.
pub fn startable_ahead<'a>(
    m: &MachineSample,
    lim: &Limits,
    running: &[Entry],
    waiting: &'a [Entry],
    me: &Entry,
    now: f64,
    unknown_starts: &[f64],
) -> Option<&'a Entry> {
    let mut ahead: Vec<&Entry> = waiting.iter().filter(|w| !w.stalled && w.ahead_of(me)).collect();
    ahead.sort_by(|a, b| a.line_order(b));
    ahead.into_iter().find(|w| matches!(decide(m, lim, running, waiting, w, now, unknown_starts), Decision::Admit { .. }))
}

/// Watches, for one waiting job, the job ahead that could start but does not.
///
/// Its owner may keep the line in another order: taskguard 0.4.0 and older
/// order it by ticket, 0.4.1 and later by the age of a job's run. With
/// nothing running, an old and a new owner can each wait for the other as
/// the older job, and nothing starts until one of them ends. Or its owner
/// does not decide at all (stopped with Ctrl-Z). Either way, waiting for it
/// gains nothing.
#[derive(Debug, Default)]
pub struct StallWatch {
    /// Ticket, pid, and since when it could start.
    suspect: Option<(u64, i32, f64)>,
}

impl StallWatch {
    /// The job to mark as stalled: while `me` is held back for jobs ahead in
    /// line, the first of them that could start has not, for `STALL_S`.
    #[allow(clippy::too_many_arguments)]
    pub fn step<'a>(
        &mut self,
        m: &MachineSample,
        lim: &Limits,
        running: &[Entry],
        waiting: &'a [Entry],
        me: &Entry,
        d: &Decision,
        now: f64,
        unknown_starts: &[f64],
    ) -> Option<&'a Entry> {
        let held = match d {
            Decision::Wait { blockers } => blockers.iter().any(|b| {
                matches!(b, Blocker::Older { .. } | Blocker::Reserved { .. } | Blocker::Priority { .. } | Blocker::Pipeline { .. })
            }),
            Decision::Admit { .. } => false,
        };
        let Some(w) = held.then(|| startable_ahead(m, lim, running, waiting, me, now, unknown_starts)).flatten() else {
            self.suspect = None;
            return None;
        };
        match self.suspect {
            Some((ticket, pid, since)) if ticket == w.ticket && pid == w.pid => {
                if now - since < STALL_S {
                    return None;
                }
                self.suspect = None;
                Some(w)
            }
            _ => {
                self.suspect = Some((w.ticket, w.pid, now));
                None
            }
        }
    }
}

/// What the job that marked another as stalled saw, in `stall/<ticket>.<pid>`:
/// the trace a stall leaves behind.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Stall {
    pub at: f64,
    /// The waiting job that saw it.
    pub by_pid: i32,
    pub by_key: String,
    /// The stalled job, and what its own owner said it waits for.
    pub key: String,
    pub pid: i32,
    pub version: Option<String>,
    pub waited_s: f64,
    pub blocker: Option<String>,
    pub blocker_since: Option<f64>,
}

// ----------------------------------------------------------------- pause ----

/// When running jobs are paused for memory (auto_pause in the config).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PauseRules {
    pub on: bool,
    /// Pause a job when memory in use reaches this share of RAM...
    pub pause_at: f64,
    /// ...and resume one when it is back under this share.
    pub resume_at: f64,
}

/// Seconds between two pauses or resumes, so the readings can show what the
/// last one did before the next one.
pub const PAUSE_GAP: f64 = 10.0;
/// A paused job stays paused this long at least. Each new pause of the same
/// run doubles it, up to `PAUSE_MAX`, so a job that fills the machine again at
/// once does not flip between the two states.
pub const PAUSE_MIN: f64 = 10.0;
pub const PAUSE_MAX: f64 = 300.0;

/// What the job of `me` (a pid) must do now: Some(true) pause, Some(false)
/// resume. Every job asks for itself; the rules pick the same job for every
/// asker, so only that one acts. `last` is when any job last paused or resumed.
///
/// - Memory at or above `pause_at`, or the kernel at critical pressure: pause
///   the running job with the lowest priority, newest first. At least one job
///   keeps running, --now jobs are never paused, and only one job pauses per
///   `PAUSE_GAP`. Nor is a long-lived job past its start-up: a stopped dev
///   stack stops answering everything that talks to it, and keeps its memory.
/// - Memory under `resume_at` and no pressure warning: the paused job with the
///   highest priority, paused first, resumes after its minimum pause.
/// - A paused job always resumes when no other job runs, and when auto_pause
///   is off: nothing is left paused for good.
/// - A job paused by hand is left alone: only a resume by hand ends it, and it
///   does not count as running.
pub fn pause_step(m: &MachineSample, r: &PauseRules, running: &[Entry], now: f64, last: f64, me: i32) -> Option<bool> {
    let mine = running.iter().find(|e| e.pid == me).filter(|e| !e.paused_by_hand)?;
    let active: Vec<&Entry> = running.iter().filter(|e| e.paused_since.is_none()).collect();
    let pressure = m.mem_pressure.unwrap_or(0.0);
    if let Some(since) = mine.paused_since {
        // The paused job that resumes first: the highest priority, then the one paused first.
        let first = running
            .iter()
            .filter(|e| !e.paused_by_hand)
            .filter_map(|e| e.paused_since.map(|s| (e, s)))
            .min_by(|(a, sa), (b, sb)| b.priority.cmp(&a.priority).then(sa.total_cmp(sb)))
            .map(|(e, _)| e.pid);
        if !r.on {
            return Some(false);
        }
        if first != Some(me) {
            return None;
        }
        if active.is_empty() {
            return Some(false);
        }
        let min = (PAUSE_MIN * 2f64.powi(mine.pauses.saturating_sub(1).min(8) as i32)).min(PAUSE_MAX);
        let room = m.mem_pct() < r.resume_at && pressure < 50.0;
        return (room && now - since >= min && now - last >= PAUSE_GAP).then_some(false);
    }
    let high = m.mem_pct() >= r.pause_at || pressure >= 100.0;
    if !r.on || !high || active.len() < 2 || now - last < PAUSE_GAP {
        return None;
    }
    let victim = active
        .iter()
        .filter(|e| e.pausable && !e.now && !e.steady() && e.child_pid > 0)
        .min_by(|a, b| a.priority.cmp(&b.priority).then(b.started_at.unwrap_or(0.0).total_cmp(&a.started_at.unwrap_or(0.0))))?;
    (victim.pid == me).then_some(true)
}

/// Send `sig` to a command and every process below it. Twice for SIGSTOP:
/// a process that was forking during the first pass is caught by the second.
pub fn signal_tree(root: i32, sig: i32) {
    if root <= 0 {
        return;
    }
    for _ in 0..if sig == libc::SIGSTOP { 2 } else { 1 } {
        let procs = crate::sys::list_procs();
        for pid in crate::sys::descendants(root, &crate::sys::children_map(&procs)) {
            unsafe { libc::kill(pid, sig) };
        }
    }
}

/// The room the running jobs leave, read once per decision.
struct Room {
    res_cpu: f64,
    res_mem: u64,
    cpu_limit: f64,
    mem_limit: f64,
    mem_used: u64,
    /// The cores running jobs need in all, whatever they use right now.
    promised_cpu: f64,
    /// What the running jobs hold now.
    ours_mem: u64,
}

impl Room {
    fn new(m: &MachineSample, lim: &Limits, running: &[Entry]) -> Room {
        let (res_cpu, res_mem) = reserve(running);
        let ours_mem = running.iter().map(|e| e.live_mem_kb).sum();
        Room {
            res_cpu,
            res_mem,
            promised_cpu: promised_cpu(running),
            cpu_limit: m.ncpu as f64 * lim.cpu_max_pct / 100.0,
            mem_limit: m.mem_total_kb as f64 * lim.mem_max_pct / 100.0,
            mem_used: m.mem_for_admission(ours_mem),
            ours_mem,
        }
    }

    fn mem_would(&self, job: &Entry) -> f64 {
        (self.mem_used + self.res_mem + job.need_mem_kb) as f64
    }

    /// What keeps `job` from starting, after the leeway: a job that falls
    /// short only by a little, or only because of programs outside taskguard,
    /// starts anyway. Then the second value says what it lacks and why it
    /// starts, for the job's warning line.
    fn left(
        &self,
        m: &MachineSample,
        lim: &Limits,
        running: &[Entry],
        job: &Entry,
        now: f64,
        unknown_starts: &[f64],
    ) -> (Vec<Blocker>, Option<String>) {
        let blockers = self.blockers(m, lim, running, job, now, unknown_starts);
        match self.early(m, lim, job, &blockers) {
            Some(why) => (Vec::new(), Some(why)),
            None => (blockers, None),
        }
    }

    /// Why a job that `blockers` keep out may start anyway, when each of them
    /// is a shortfall of memory or CPU that is either small (`noise_mem`,
    /// `noise_cpu`) or only there because of programs outside taskguard
    /// (`outside_admit`). Readings move by a few percent from one second to
    /// the next, and the room taskguard's own jobs need is what it can count
    /// on: holding a job back for a browser tab gains little. Memory stays a
    /// hard rule above `outside_mem_max_pct`, as there the machine swaps.
    /// A shortfall is measured with every job started before counted in, so
    /// a run of such starts cannot pile up: the next one falls short by more.
    fn early(&self, m: &MachineSample, lim: &Limits, job: &Entry, blockers: &[Blocker]) -> Option<String> {
        if blockers.is_empty() || !blockers.iter().all(|b| matches!(b, Blocker::Memory { .. } | Blocker::Cpu { .. })) {
            return None;
        }
        let total = m.mem_total_kb as f64;
        let gb = |kb: f64| kb / (1024.0 * 1024.0);
        let mut why = Vec::new();
        for b in blockers {
            match *b {
                Blocker::Memory { short_kb, .. } => {
                    let would = self.mem_would(job);
                    if would > total * lim.outside_mem_max_pct / 100.0 {
                        return None;
                    }
                    let outside = self.mem_used.saturating_sub(self.ours_mem);
                    let ours = (self.ours_mem + self.res_mem + job.need_mem_kb) as f64;
                    if short_kb as f64 <= total * lim.noise_mem_pct / 100.0 {
                        why.push(format!(
                            "memory short by {:.1} GB, within noise_mem {:.0}% of RAM",
                            gb(short_kb as f64),
                            lim.noise_mem_pct
                        ));
                    } else if lim.outside_admit && ours <= self.mem_limit {
                        why.push(format!(
                            "memory short by {:.1} GB, but only because programs outside taskguard hold {:.1} GB (outside_admit; memory would reach {:.0}%, under {:.0}%)",
                            gb(short_kb as f64),
                            gb(outside as f64),
                            pct(would, total),
                            lim.outside_mem_max_pct
                        ));
                    } else {
                        return None;
                    }
                }
                Blocker::Cpu { short, busy, .. } => {
                    if short <= lim.noise_cpu {
                        why.push(format!("CPU short by {short:.1} cores, within noise_cpu {:.1}", lim.noise_cpu));
                    } else if lim.outside_admit && busy > 0.0 && self.promised_cpu + job.need_cpu <= self.cpu_limit + 1e-9 {
                        why.push(format!(
                            "CPU short by {short:.1} cores, but only because programs outside taskguard keep cores busy (outside_admit)"
                        ));
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        Some(why.join("; "))
    }

    /// What keeps `job` itself from starting now, apart from the jobs ahead
    /// of it in line.
    fn blockers(&self, m: &MachineSample, lim: &Limits, running: &[Entry], job: &Entry, now: f64, unknown_starts: &[f64]) -> Vec<Blocker> {
        let mut blockers = Vec::new();
        if let (Some(pk), Some(max)) = (&job.pool_key, job.pool_slots) {
            let holders: Vec<String> = running.iter().filter(|e| e.pool_key.as_ref() == Some(pk)).map(|e| e.key.clone()).collect();
            if holders.len() as u32 >= max {
                blockers.push(Blocker::Slots { pool: job.pool.clone().unwrap_or_default(), busy: holders.len() as u32, max, holders });
            }
        }
        // Jobs with no history start in small groups. Their needs are guesses
        // until they have run a while, and the readings need time to show what
        // they really take. A group is as large as the free room allows: one
        // job per free core, and as many as fit in the free memory at this
        // job's guess. Two rules hold it to that size:
        // - At most a group of first runs may still be growing at once; one
        //   that has stopped growing (or has ended) makes room for the next.
        // - At most a group of first runs starts per `learn_stagger` seconds.
        // A first run guessed to end within `cpu_min_duration` (a lint that
        // similar jobs finish in a second) is over before it could grow, so it
        // neither waits nor counts. Memory is still checked for it.
        if !job.known && !job.raised_by_min && !short(lim, job) {
            let free_cpu = self.cpu_limit - m.cpu_busy - self.res_cpu;
            let free_mem_kb = self.mem_limit - (self.mem_used + self.res_mem) as f64;
            let per_job_kb = job.need_mem_kb.max(512 * 1024) as f64;
            let group = free_cpu.min(free_mem_kb / per_job_kb).floor().max(1.0) as usize;
            if job.guessed() {
                let growing: Vec<(&Entry, f64)> =
                    running.iter().filter(|r| !short(lim, r)).filter_map(|r| r.settling_for(now).map(|w| (r, w))).collect();
                if growing.len() >= group
                    && let Some((r, wait_s)) = growing.iter().min_by(|a, b| a.1.total_cmp(&b.1))
                {
                    blockers.push(Blocker::Settling { key: r.key.clone(), wait_s: *wait_s });
                }
            }
            let recent: Vec<f64> = unknown_starts.iter().copied().filter(|t| now - t < lim.learn_stagger).collect();
            if !recent.is_empty() && recent.len() >= group {
                let newest = recent.iter().copied().fold(f64::MIN, f64::max);
                blockers.push(Blocker::LearnStagger { wait_s: (lim.learn_stagger - (now - newest)).max(0.0) });
            }
        }
        let mem_would = self.mem_would(job);
        if mem_would > self.mem_limit {
            blockers.push(Blocker::Memory {
                would_pct: pct(mem_would, m.mem_total_kb as f64),
                limit_pct: lim.mem_max_pct,
                short_kb: (mem_would - self.mem_limit) as u64,
                used_kb: self.mem_used,
                reserve_kb: self.res_mem,
                need_kb: job.need_mem_kb,
                total_kb: m.mem_total_kb,
            });
        }
        // A short job is over before the CPU reading shows it, so the reading
        // does not hold it back. What taskguard has promised its own jobs
        // does: without that, a task runner with a high concurrency starts
        // every short job at once and fills every core.
        if short(lim, job) {
            let promised = self.promised_cpu + job.need_cpu;
            if promised > self.cpu_limit + 1e-9 {
                blockers.push(Blocker::Cpu {
                    would: promised,
                    limit: self.cpu_limit,
                    short: promised - self.cpu_limit,
                    busy: 0.0,
                    reserve: self.promised_cpu,
                    need: job.need_cpu,
                });
            }
            return blockers;
        }
        let cpu_would = m.cpu_busy + self.res_cpu + job.need_cpu;
        if cpu_would > self.cpu_limit + 1e-9 {
            blockers.push(Blocker::Cpu {
                would: cpu_would,
                limit: self.cpu_limit,
                short: cpu_would - self.cpu_limit,
                busy: m.cpu_busy,
                reserve: self.res_cpu,
                need: job.need_cpu,
            });
        }
        blockers
    }
}

/// A job that usually ends within a few seconds is over before the CPU
/// reading could react to it, so holding it back only makes it late. Too
/// little CPU only slows a job down; memory stays a hard rule for all.
pub fn short(lim: &Limits, job: &Entry) -> bool {
    job.est_dur_s.is_some_and(|d| d < lim.cpu_min_duration)
}

fn pct(a: f64, b: f64) -> f64 {
    if b <= 0.0 { 0.0 } else { a * 100.0 / b }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024;
    const LIM: Limits = Limits {
        cpu_max_pct: 100.0,
        mem_max_pct: 85.0,
        learn_stagger: 2.0,
        max_bypass: 120.0,
        max_backfill: 0.0,
        cpu_min_duration: 5.0,
        pressure_max: 20.0,
        noise_mem_pct: 0.0,
        noise_cpu: 0.0,
        outside_admit: false,
        outside_mem_max_pct: 95.0,
    };

    /// A running entry exactly as v0.1.2 writes it.
    const ENTRY_V0_1_2: &str = r#"{"ticket":7,"pid":4242,"run_id":31,"key":"packages/api:tsc","ns":"shop","label":"typecheck","pool":null,"pool_key":null,"pool_slots":null,"checkout":"/w/shop","queued_at":1000.5,"started_at":1002.0,"need_cpu":3.0,"need_mem_kb":1468006,"known":true,"raised_by_min":false,"now":false,"live_cpu":1.2,"live_mem_kb":900000,"bypassed_since":null,"blocker":null,"blocker_since":null,"est_dur_s":20.0,"estimate_from":null}"#;

    #[test]
    fn entry_format_is_stable() {
        // This version reads what v0.1.2 wrote.
        let old: Entry = serde_json::from_str(ENTRY_V0_1_2).unwrap();
        assert_eq!((old.pid, old.need_mem_kb, old.started_at), (4242, 1468006, Some(1002.0)));

        // v0.1.2 reads what this version writes: every field it needs is still
        // there, with the same JSON type. Fields may only be added.
        let old: serde_json::Map<String, serde_json::Value> = serde_json::from_str(ENTRY_V0_1_2).unwrap();
        let new = serde_json::to_value(Entry { started_at: Some(1.0), ..Default::default() }).unwrap();
        assert!(new["priority"].is_i64(), "the priority is a whole number");
        let kind = |v: &serde_json::Value| match v {
            serde_json::Value::Number(n) if n.is_f64() => "float",
            serde_json::Value::Number(_) => "integer",
            serde_json::Value::Bool(_) => "bool",
            serde_json::Value::String(_) => "string",
            _ => "null or other",
        };
        for (field, value) in &old {
            let Some(now) = new.get(field) else { panic!("field {field} is gone; older versions need it") };
            if !value.is_null() && !now.is_null() {
                // Whole-number floats print as 1.0, so compare floats loosely.
                let (a, b) = (kind(value), kind(now));
                assert!(a == b || (a != "bool" && a != "string" && b != "bool" && b != "string"), "field {field} changed type: {a} -> {b}");
            }
        }
    }

    #[test]
    fn entries_from_other_versions_are_read() {
        // A newer version added a field; an older one left some out.
        let newer = ENTRY_V0_1_2.replace(r#""ticket":7"#, r#""ticket":7,"priority":3"#);
        assert_eq!(serde_json::from_str::<Entry>(&newer).unwrap().ticket, 7);
        let older: Entry = serde_json::from_str(r#"{"ticket":3,"pid":10,"run_id":1,"key":"k"}"#).unwrap();
        assert_eq!((older.ticket, older.need_cpu), (3, 0.0));
    }

    #[test]
    fn unreadable_entries_of_dead_owners_are_dropped() {
        let dir = std::env::temp_dir().join(format!("tg-reap-{}", std::process::id()));
        let q = Queue::open(&dir).unwrap();
        let dead = i32::MAX - 1;
        let live = std::process::id() as i32;
        fs::write(dir.join("wait").join(format!("9.{dead}")), "{not json").unwrap();
        fs::write(dir.join("run").join(dead.to_string()), r#"{"pid":"not a number"}"#).unwrap();
        fs::write(dir.join("run").join(live.to_string()), "{not json").unwrap();
        q.reap();
        assert!(!dir.join("wait").join(format!("9.{dead}")).exists());
        assert!(!dir.join("run").join(dead.to_string()).exists());
        assert!(dir.join("run").join(live.to_string()).exists(), "a live owner keeps its entry");
        fs::remove_dir_all(&dir).ok();
    }

    /// Queue times as a real clock gives them: seconds since 1970 with
    /// sub-microsecond fractions, where float parsing is least forgiving.
    fn clock_times() -> impl Iterator<Item = f64> {
        (0..200_000u64).map(|i| 1_791_125_928.0 + i as f64 * 0.000_123_456_7 + (i % 97) as f64 * 1e-7)
    }

    #[test]
    fn an_entry_read_back_is_never_ahead_of_itself() {
        // A waiter compares its own entry, as another process wrote or read it,
        // with the copy it holds. A queue time that comes back from JSON one
        // step off must not put the job in line before itself.
        for (i, t) in clock_times().enumerate() {
            let me = Entry { ticket: i as u64, pid: 4242, key: "k".into(), queued_at: t, ..Default::default() };
            let back: Entry = serde_json::from_str(&serde_json::to_string(&me).unwrap()).unwrap();
            assert_eq!(back.queued_at.to_bits(), t.to_bits(), "queued_at {t:?} read back as {:?}", back.queued_at);
            assert!(!back.ahead_of(&me) && !me.ahead_of(&back), "queued_at {t:?} put the job ahead of itself");
        }
    }

    #[test]
    fn a_waiter_is_never_held_back_by_its_own_entry() {
        // Nothing runs and only this job waits: it starts, whatever its queue
        // time looks like after a trip through the wait file.
        for (i, t) in clock_times().step_by(97).enumerate() {
            let me = Entry { ticket: i as u64, pid: 4242, key: "k".into(), known: true, queued_at: t, ..Default::default() };
            let on_disk: Entry = serde_json::from_str(&serde_json::to_string(&me).unwrap()).unwrap();
            let d = decide(&machine(0.0, 1), &LIM, &[], &[on_disk], &me, t + 1.0, &[]);
            assert!(matches!(d, Decision::Admit { .. }), "queued_at {t:?}: {:?}", blockers(d));
        }
    }

    fn job(ticket: u64, key: &str, cpu: f64, mem_gb: u64) -> Entry {
        Entry { ticket, key: key.into(), need_cpu: cpu, need_mem_kb: mem_gb * GB, known: true, queued_at: 1000.0, ..Default::default() }
    }

    fn machine(busy: f64, used_gb: u64) -> MachineSample {
        MachineSample::fixed(busy, 12, used_gb * GB, 32 * GB)
    }

    fn blockers(d: Decision) -> Vec<&'static str> {
        match d {
            Decision::Admit { .. } => vec![],
            Decision::Wait { blockers } => blockers.iter().map(|b| b.name()).collect(),
        }
    }

    #[test]
    fn nothing_running_always_admits_the_oldest() {
        let big = job(1, "big", 64.0, 500);
        assert!(blockers(decide(&machine(12.0, 31), &LIM, &[], std::slice::from_ref(&big), &big, 1000.0, &[])).is_empty());
        let young = job(2, "young", 1.0, 1);
        assert_eq!(blockers(decide(&machine(0.0, 1), &LIM, &[], &[big.clone(), young.clone()], &young, 1000.0, &[])), vec!["order"]);
    }

    #[test]
    fn memory_and_cpu_include_the_reserve() {
        let mut running = job(1, "api", 4.0, 18);
        running.live_cpu = 1.0;
        running.live_mem_kb = 5 * GB;
        let me = job(2, "worker", 2.0, 4);
        // 10 GB used + 13 GB still to be taken by api + 4 GB = 27 GB = 84% -> fits.
        assert!(blockers(decide(&machine(2.0, 10), &LIM, &[running.clone()], std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty());
        // 12 GB used -> 29 GB = 90% -> memory blocks.
        assert_eq!(
            blockers(decide(&machine(2.0, 12), &LIM, &[running.clone()], std::slice::from_ref(&me), &me, 1000.0, &[])),
            vec!["memory"]
        );
        // 8 busy + 3 still to come + 2 = 13 > 12 cores -> CPU blocks too.
        assert_eq!(
            blockers(decide(&machine(8.0, 12), &LIM, &[running.clone()], std::slice::from_ref(&me), &me, 1000.0, &[])),
            vec!["memory", "cpu"]
        );
    }

    fn early(d: Decision) -> Option<String> {
        match d {
            Decision::Admit { early, .. } => early,
            Decision::Wait { blockers } => panic!("waits: {:?}", blockers.iter().map(|b| b.name()).collect::<Vec<_>>()),
        }
    }

    #[test]
    fn a_job_that_lacks_a_little_or_only_room_other_programs_hold_starts_anyway() {
        let lim = Limits { noise_mem_pct: 2.0, noise_cpu: 0.5, outside_admit: true, outside_mem_max_pct: 92.0, ..LIM };
        let mut running = job(1, "api", 4.0, 18);
        running.live_cpu = 1.0;
        running.live_mem_kb = 5 * GB;
        let run = std::slice::from_ref(&running);
        let me = job(2, "worker", 2.0, 4);
        let me_too = std::slice::from_ref(&me);
        let mem = |kb: u64| MachineSample::fixed(2.0, 12, kb, 32 * GB);
        // 27.5 GB of the 27.2 GB limit: 0.3 GB short, within 2% of 32 GB.
        let why = early(decide(&mem(10 * GB + GB / 2), &lim, run, me_too, &me, 1000.0, &[])).unwrap();
        assert!(why.contains("memory short by 0.3 GB, within noise_mem 2%"), "{why}");
        // 29 GB: 1.8 GB short, but taskguard's jobs need only 22 GB of it; the
        // other 7 GB belong to other programs.
        let why = early(decide(&mem(12 * GB), &lim, run, me_too, &me, 1000.0, &[])).unwrap();
        assert!(why.contains("only because programs outside taskguard hold 7.0 GB"), "{why}");
        // 30 GB is past pause_at's 92%: memory stays a hard rule there.
        assert_eq!(blockers(decide(&mem(13 * GB), &lim, run, me_too, &me, 1000.0, &[])), vec!["memory"]);
        // Without the leeway the job waits, as before.
        assert_eq!(blockers(decide(&mem(10 * GB + GB / 2), &LIM, run, me_too, &me, 1000.0, &[])), vec!["memory"]);
        // When taskguard's own jobs fill the room, outside programs are no excuse.
        let mut fat = job(1, "api", 4.0, 22);
        fat.live_mem_kb = 20 * GB;
        let big = job(2, "worker", 2.0, 6);
        assert_eq!(
            blockers(decide(&mem(21 * GB), &lim, std::slice::from_ref(&fat), std::slice::from_ref(&big), &big, 1000.0, &[])),
            vec!["memory"]
        );
        // CPU: 7.4 busy + 3 still to come + 2 = 12.4 of 12 cores, 0.4 short.
        let cpu = |busy: f64| MachineSample::fixed(busy, 12, 4 * GB, 32 * GB);
        assert!(early(decide(&cpu(7.4), &lim, run, me_too, &me, 1000.0, &[])).unwrap().contains("within noise_cpu"));
        // 1 core short, but taskguard has promised only 6 of the 12.
        assert!(early(decide(&cpu(8.0), &lim, run, me_too, &me, 1000.0, &[])).unwrap().contains("keep cores busy"));
        // A job that fits starts as before, with no warning.
        assert_eq!(early(decide(&cpu(1.0), &lim, run, me_too, &me, 1000.0, &[])), None);
    }

    #[test]
    fn a_small_job_passes_a_big_one() {
        let running = job(1, "r", 1.0, 1);
        let big = job(2, "big", 2.0, 30);
        let small = job(3, "small", 1.0, 1);
        let waiting = [big.clone(), small.clone()];
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, std::slice::from_ref(&running), &waiting, &big, 1000.0, &[])), vec!["memory"]);
        assert!(blockers(decide(&machine(1.0, 8), &LIM, &[running], &waiting, &small, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_higher_priority_goes_first() {
        let running = job(1, "r", 1.0, 1);
        let r = std::slice::from_ref(&running);
        // The urgent job is newer and waits for memory; the small old one fits.
        let old = job(2, "old", 1.0, 1);
        let mut urgent = job(3, "urgent", 1.0, 30);
        urgent.priority = 1;
        let waiting = [old.clone(), urgent.clone()];
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, r, &waiting, &old, 1000.0, &[])), vec!["priority"]);
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, r, &waiting, &urgent, 1000.0, &[])), vec!["memory"]);
        // With max_backfill, an urgent job that cannot fit lets jobs that fit
        // start meanwhile, until it has waited max_backfill seconds.
        let backfill = Limits { max_backfill: 60.0, ..LIM };
        let mut waited = urgent.clone();
        waited.queued_at = 990.0;
        let w = [old.clone(), waited.clone()];
        assert!(blockers(decide(&machine(1.0, 8), &backfill, r, &w, &old, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&machine(1.0, 8), &backfill, r, &w, &old, 1051.0, &[])), vec!["priority"]);
        // Its pool is full: it cannot start anyway, so it holds nobody back.
        urgent.pool_key = Some("e2e".into());
        urgent.pool_slots = Some(1);
        let mut in_pool = running.clone();
        in_pool.pool_key = Some("e2e".into());
        let waiting = [old.clone(), urgent.clone()];
        assert!(blockers(decide(&machine(1.0, 8), &LIM, &[in_pool], &waiting, &old, 1000.0, &[])).is_empty());
        // With nothing running, the highest priority starts, not the oldest.
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, &[], &waiting, &old, 1000.0, &[])), vec!["order"]);
        assert!(blockers(decide(&machine(1.0, 8), &LIM, &[], &waiting, &urgent, 1000.0, &[])).is_empty());
    }

    fn running(pid: i32, started: f64, priority: i32) -> Entry {
        Entry {
            pid,
            child_pid: pid + 1000,
            started_at: Some(started),
            priority,
            pausable: true,
            ..job(pid as u64, &format!("j{pid}"), 1.0, 2)
        }
    }

    #[test]
    fn auto_pause_picks_the_newest_lowest_priority_job_and_keeps_one_running() {
        let rules = PauseRules { on: true, pause_at: 90.0, resume_at: 80.0 };
        let full = machine(1.0, 30); // 94%
        let (old, new, urgent) = (running(1, 100.0, 0), running(2, 200.0, 0), running(3, 300.0, 1));
        let jobs = [old.clone(), new.clone(), urgent.clone()];
        let step = |m: &MachineSample, jobs: &[Entry], pid, now, last| pause_step(m, &rules, jobs, now, last, pid);
        // The newest job with the lowest priority pauses; the others stay.
        assert_eq!(step(&full, &jobs, 2, 1000.0, 0.0), Some(true));
        assert_eq!(step(&full, &jobs, 1, 1000.0, 0.0), None);
        assert_eq!(step(&full, &jobs, 3, 1000.0, 0.0), None);
        // Room enough, or off: nothing pauses.
        assert_eq!(step(&machine(1.0, 20), &jobs, 2, 1000.0, 0.0), None);
        assert_eq!(pause_step(&full, &PauseRules { on: false, ..rules }, &jobs, 1000.0, 0.0, 2), None);
        // Only one pause per gap.
        assert_eq!(step(&full, &jobs, 2, 1000.0, 995.0), None);
        // Old versions and --now jobs are never picked; the last running job is never paused.
        let mut now_job = new.clone();
        now_job.now = true;
        let mut old_version = old.clone();
        old_version.pausable = false;
        assert_eq!(step(&full, &[old_version, now_job], 2, 1000.0, 0.0), None);
        assert_eq!(step(&full, std::slice::from_ref(&new), 2, 1000.0, 0.0), None);
        // Nor is a dev stack past its start-up: the job before it pauses instead.
        let stack = Entry { steady_since: Some(250.0), ..new.clone() };
        assert_eq!(step(&full, &[old.clone(), stack.clone()], 2, 1000.0, 0.0), None);
        assert_eq!(step(&full, &[old, stack], 1, 1000.0, 0.0), Some(true));
    }

    #[test]
    fn auto_pause_resumes_under_resume_at_after_the_minimum_pause() {
        let rules = PauseRules { on: true, pause_at: 90.0, resume_at: 80.0 };
        let mut paused = running(2, 200.0, 0);
        paused.paused_since = Some(1000.0);
        paused.pauses = 1;
        let jobs = [running(1, 100.0, 0), paused.clone()];
        let step = |m: &MachineSample, jobs: &[Entry], now| pause_step(m, &rules, jobs, now, 1000.0, 2);
        assert_eq!(step(&machine(1.0, 28), &jobs, 1020.0), None, "85% is not under resume_at");
        assert_eq!(step(&machine(1.0, 20), &jobs, 1005.0), None, "the minimum pause is not over");
        assert_eq!(step(&machine(1.0, 20), &jobs, 1010.0), Some(false));
        let mut warned = machine(1.0, 20);
        warned.mem_pressure = Some(50.0);
        assert_eq!(step(&warned, &jobs, 1020.0), None, "not while the kernel warns");
        // A job paused before waits longer each time.
        let mut again = paused.clone();
        again.pauses = 3;
        assert_eq!(step(&machine(1.0, 20), &[jobs[0].clone(), again.clone()], 1030.0), None);
        assert_eq!(step(&machine(1.0, 20), &[jobs[0].clone(), again], 1040.0), Some(false));
        // Nothing else runs: it resumes whatever memory says. Off: it resumes too.
        assert_eq!(step(&machine(1.0, 31), std::slice::from_ref(&paused), 1001.0), Some(false));
        let off = PauseRules { on: false, ..rules };
        assert_eq!(pause_step(&machine(1.0, 31), &off, &jobs, 1001.0, 1000.0, 2), Some(false));
        // While a job is paused, nothing new starts.
        let me = job(9, "new", 0.1, 0);
        assert_eq!(blockers(decide(&machine(1.0, 1), &LIM, &jobs, std::slice::from_ref(&me), &me, 1001.0, &[])), vec!["paused"]);
    }

    #[test]
    fn a_job_paused_by_hand_is_left_alone_and_gives_up_its_cpu() {
        let rules = PauseRules { on: true, pause_at: 90.0, resume_at: 80.0 };
        let mut held = running(2, 200.0, 0);
        held.paused_since = Some(1000.0);
        held.paused_by_hand = true;
        held.need_cpu = 4.0;
        let other = running(1, 100.0, 0);
        let jobs = [other.clone(), held.clone()];
        // Memory is free and auto_pause may resume: it still stays paused.
        assert_eq!(pause_step(&machine(1.0, 5), &rules, &jobs, 2000.0, 0.0, 2), None);
        assert_eq!(pause_step(&machine(1.0, 5), &PauseRules { on: false, ..rules }, &jobs, 2000.0, 0.0, 2), None);
        // It does not count as running: the last running job is not paused.
        assert_eq!(pause_step(&machine(1.0, 31), &rules, &jobs, 2000.0, 0.0, 1), None);
        // New jobs may start, and use its CPU.
        assert_eq!(reserve(std::slice::from_ref(&held)).0, 0.0);
        let me = job(9, "new", 1.0, 1);
        assert!(blockers(decide(&machine(1.0, 5), &LIM, &jobs, std::slice::from_ref(&me), &me, 2000.0, &[])).is_empty());
    }

    #[test]
    fn a_job_passed_for_too_long_is_reserved() {
        let running = job(1, "r", 1.0, 1);
        let mut big = job(2, "big", 2.0, 30);
        big.bypassed_since = Some(1010.0);
        let small = job(3, "small", 1.0, 1);
        let waiting = [big.clone(), small.clone()];
        // Not yet past max_bypass.
        assert!(blockers(decide(&machine(1.0, 8), &LIM, std::slice::from_ref(&running), &waiting, &small, 1100.0, &[])).is_empty());
        // Past it: nobody newer may start.
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, &[running], &waiting, &small, 1200.0, &[])), vec!["reserved"]);
    }

    /// A 20 GB compile has run since 1000 and holds its memory. At 1000 a
    /// 12 GB job arrived that does not fit next to it, and newer jobs have
    /// passed it since 1010.
    fn blocked_head() -> (Entry, Entry) {
        let mut running = job(1, "compile", 1.0, 20);
        running.started_at = Some(1000.0);
        running.live_mem_kb = 20 * GB;
        let mut head = job(2, "typecheck", 2.0, 12);
        head.bypassed_since = Some(1010.0);
        (running, head)
    }

    #[test]
    fn a_reserved_job_that_does_not_fit_lets_newer_jobs_that_fit_start() {
        let (running, head) = blocked_head();
        let small = job(3, "install", 0.2, 1);
        let waiting = [head.clone(), small.clone()];
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        // 20 GB held + 12 GB needed is more than 27.2 GB (85% of 32).
        let m = machine(2.0, 21);
        let r = std::slice::from_ref(&running);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &head, 1300.0, &[])), vec!["memory"]);
        // The small job fits and starts: the head could not use the room.
        assert!(blockers(decide(&m, &backfill, r, &waiting, &small, 1300.0, &[])).is_empty());
        // Without backfill, the head keeps its reservation, as before.
        assert_eq!(blockers(decide(&m, &LIM, r, &waiting, &small, 1300.0, &[])), vec!["reserved"]);

        // The compile shrinks to 5 GB: now the head fits, so it goes first.
        let mut shrunk = running.clone();
        shrunk.need_mem_kb = 5 * GB;
        shrunk.live_mem_kb = 5 * GB;
        let m = machine(2.0, 6);
        let r = std::slice::from_ref(&shrunk);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &small, 1300.0, &[])), vec!["reserved"]);
        assert!(blockers(decide(&m, &backfill, r, &waiting, &head, 1300.0, &[])).is_empty());
    }

    #[test]
    fn past_max_backfill_the_reservation_holds_even_when_the_job_does_not_fit() {
        let (running, head) = blocked_head();
        let small = job(3, "install", 0.2, 1);
        let waiting = [head.clone(), small.clone()];
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        let m = machine(2.0, 21);
        let r = std::slice::from_ref(&running);
        assert!(blockers(decide(&m, &backfill, r, &waiting, &small, 2799.0, &[])).is_empty());
        // Waiting longer than 1800 s: a newer job that may not end before the
        // head could start stops (this one has no learned duration), so memory
        // drains until the head fits.
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &small, 2801.0, &[])), vec!["reserved"]);
        // A window no longer than max_bypass is no window: the old rule.
        let none = Limits { max_backfill: 120.0, ..LIM };
        assert_eq!(blockers(decide(&m, &none, r, &waiting, &small, 1300.0, &[])), vec!["reserved"]);
    }

    /// The stall of 2026-10-04 on a 32-core Linux box: two long jobs ran,
    /// and programs outside taskguard (dev stacks, browsers, agents) kept the
    /// CPU busy. The head of the line needs 7.4 cores and did not fit. Past
    /// `max_backfill` (300 s) its reservation held back every job behind it,
    /// small ones that fit included, until the long jobs ended half an hour
    /// later.
    fn outside_load() -> (MachineSample, Limits, Vec<Entry>, Entry) {
        let lim = Limits { max_backfill: 300.0, ..LIM };
        let running: Vec<Entry> = ["e2e-a", "e2e-b"]
            .iter()
            .map(|k| Entry { started_at: Some(0.0), est_dur_s: Some(2400.0), live_cpu: 2.0, live_mem_kb: 2 * GB, ..job(1, k, 2.0, 2) })
            .collect();
        let mut head = job(2, "vocab/abi:check-byte-stability", 7.4, 2);
        head.queued_at = 300.0;
        head.bypassed_since = Some(310.0);
        // 25 cores busy outside taskguard, 4 in its jobs: the head would use
        // 36.4 of 32 cores, and would not fit even with nothing of taskguard
        // running.
        (MachineSample::fixed(29.0, 32, 40 * GB, 128 * GB), lim, running, head)
    }

    #[test]
    fn a_reserved_job_kept_out_by_other_programs_lets_jobs_that_end_in_time_start() {
        let (m, lim, running, head) = outside_load();
        let lint = Entry { est_dur_s: Some(40.0), ..job(3, "web/dashboard:oxlint", 1.0, 1) };
        let long = Entry { est_dur_s: Some(900.0), ..job(4, "e2e/dapi:test", 1.0, 1) };
        let waiting = [head.clone(), lint.clone(), long.clone()];
        // 26 minutes in line, long past max_backfill.
        let now = 1860.0;
        assert_eq!(blockers(decide(&m, &lim, &running, &waiting, &head, now, &[])), vec!["cpu"]);
        // The long jobs end in about 540 s, and nothing taskguard runs ends
        // sooner that could make room: the head starts then at the soonest,
        // once nothing runs. A lint ends long before that, so it starts.
        assert!(blockers(decide(&m, &lim, &running, &waiting, &lint, now, &[])).is_empty());
        // A job that would still run then waits, so the head is not kept out longer.
        assert_eq!(blockers(decide(&m, &lim, &running, &waiting, &long, now, &[])), vec!["reserved"]);
        // So does a job whose duration is unknown.
        let first = job(5, "new:test", 1.0, 1);
        assert_eq!(blockers(decide(&m, &lim, &running, &[head.clone(), first.clone()], &first, now, &[])), vec!["reserved"]);
        // The outside load eases: the head fits, and the lint waits for it.
        let calm = MachineSample { cpu_busy: 20.0, ..m.clone() };
        assert!(blockers(decide(&calm, &lim, &running, &waiting, &head, now, &[])).is_empty());
        assert_eq!(blockers(decide(&calm, &lim, &running, &waiting, &lint, now, &[])), vec!["reserved"]);
    }

    #[test]
    fn a_reserved_job_short_of_memory_lets_jobs_that_end_in_time_start() {
        let (_, lim, running, mut head) = outside_load();
        let lim = Limits { mem_max_pct: 70.0, ..lim };
        // Other programs hold 84 GB, the long jobs 4: an 18 GB head would
        // reach 83% (limit 70%).
        let m = MachineSample::fixed(8.0, 32, 88 * GB, 128 * GB);
        head.need_cpu = 2.0;
        head.need_mem_kb = 18 * GB;
        let lint = Entry { est_dur_s: Some(40.0), ..job(3, "onchain/evm:solhint", 1.0, 1) };
        let long = Entry { est_dur_s: Some(900.0), ..job(4, "e2e/dapi:test", 1.0, 1) };
        let waiting = [head.clone(), lint.clone(), long.clone()];
        let now = 1860.0;
        assert_eq!(blockers(decide(&m, &lim, &running, &waiting, &head, now, &[])), vec!["memory"]);
        assert!(blockers(decide(&m, &lim, &running, &waiting, &lint, now, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &lim, &running, &waiting, &long, now, &[])), vec!["reserved"]);
        // Other programs free 20 GB: the head fits, and goes first.
        let calm = MachineSample { mem_used_kb: 68 * GB, ..m.clone() };
        assert!(blockers(decide(&calm, &lim, &running, &waiting, &head, now, &[])).is_empty());
        assert_eq!(blockers(decide(&calm, &lim, &running, &waiting, &lint, now, &[])), vec!["reserved"]);
    }

    #[test]
    fn past_max_backfill_only_jobs_that_end_before_the_room_frees_pass() {
        // The head waits for memory the compile holds; the compile ends in
        // 100 s, and then the head fits. A job that ends sooner leaves that
        // room free in time; one that does not would take it.
        let (mut running, head) = blocked_head();
        running.est_dur_s = Some(2000.0);
        let quick = Entry { est_dur_s: Some(60.0), ..job(3, "install", 0.2, 1) };
        let slow = Entry { est_dur_s: Some(600.0), ..job(4, "build", 0.2, 1) };
        let waiting = [head.clone(), quick.clone(), slow.clone()];
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        let m = machine(2.0, 21);
        let r = std::slice::from_ref(&running);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &head, 2900.0, &[])), vec!["memory"]);
        assert!(blockers(decide(&m, &backfill, r, &waiting, &quick, 2900.0, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &slow, 2900.0, &[])), vec!["reserved"]);
        // A higher priority holds its turn the same way.
        let mut urgent = head.clone();
        urgent.bypassed_since = None;
        urgent.priority = 1;
        let waiting = [urgent, quick.clone(), slow.clone()];
        assert!(blockers(decide(&m, &backfill, r, &waiting, &quick, 2900.0, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &slow, 2900.0, &[])), vec!["priority"]);
    }

    /// A dev stack on a 12-core machine: it wants 11 cores and 6 GB while it
    /// starts and seeds, then idles at a few percent of a core. It runs until
    /// someone stops it, so it has no duration to count on.
    fn stack() -> Entry {
        job(1, "dalp:stack", 11.0, 6)
    }

    const STACK: LongLived = LongLived { startup: 300.0, steady_mem_kb: None };

    #[test]
    fn a_long_lived_job_is_admitted_against_its_start_up_peak() {
        let me = stack();
        let other = job(2, "lint", 0.5, 1);
        let w = std::slice::from_ref(&me);
        // 4 busy + 0.5 promised + 11 > 12 cores: it waits for room for its peak.
        assert_eq!(blockers(decide(&machine(4.0, 4), &LIM, std::slice::from_ref(&other), w, &me, 1000.0, &[])), vec!["cpu"]);
        assert!(blockers(decide(&machine(0.4, 4), &LIM, std::slice::from_ref(&other), w, &me, 1000.0, &[])).is_empty());

        // Started at 1000: through its start-up it holds that peak, even in a lull.
        let mut s = Entry { started_at: Some(1000.0), ..me };
        s.follow(2 * GB, 11.0, 11.0, 12.0, 1010.0, Some(STACK));
        s.follow(3 * GB, 0.2, 0.2, 12.0, 1200.0, Some(STACK));
        (s.live_cpu, s.live_mem_kb) = (0.2, 3 * GB);
        assert_eq!((s.need_cpu, s.need_mem_kb, s.steady_since), (11.0, 6 * GB, None));
        // 0.5 busy + 10.8 still promised to the stack + 3 > 12 cores.
        let build = job(3, "build", 3.0, 2);
        let r = std::slice::from_ref(&s);
        assert_eq!(blockers(decide(&machine(0.5, 4), &LIM, r, std::slice::from_ref(&build), &build, 1200.0, &[])), vec!["cpu"]);
    }

    #[test]
    fn after_its_start_up_a_long_lived_job_reserves_what_it_uses() {
        let mut s = Entry { started_at: Some(1000.0), ..stack() };
        s.follow(5 * GB, 11.0, 11.0, 12.0, 1100.0, Some(STACK));
        // 300 s in, its start-up is over. It holds 3 GB and wants 0.3 cores.
        s.follow(3 * GB, 0.2, 0.2, 12.0, 1300.0, Some(STACK));
        assert_eq!(s.steady_since, Some(1300.0));
        assert_eq!((s.need_cpu, s.need_mem_kb), (0.25, 3 * GB + 3 * GB / 4));
        (s.live_cpu, s.live_mem_kb) = (0.2, 3 * GB);
        // The build that waited for the start-up peak now starts.
        let build = job(3, "build", 3.0, 2);
        let r = std::slice::from_ref(&s);
        assert!(blockers(decide(&machine(0.5, 4), &LIM, r, std::slice::from_ref(&build), &build, 1300.0, &[])).is_empty());

        // A test run against the stack raises its CPU need, which falls back after.
        s.follow(3 * GB, 6.0, 6.0, 12.0, 1400.0, Some(STACK));
        assert_eq!(s.need_cpu, 7.5);
        s.follow(3 * GB, 0.2, 0.2, 12.0, 1420.0, Some(STACK));
        assert_eq!(s.need_cpu, 0.25);
        // Its steady memory booking follows current RSS and can fall.
        s.follow(4 * GB, 0.2, 0.3, 12.0, 1440.0, Some(STACK));
        s.follow(2 * GB, 0.2, 0.3, 12.0, 1460.0, Some(STACK));
        assert_eq!(s.need_mem_kb, 2 * GB + (2 * GB) / 4);

        // What its past runs grew to after their start-up stays reserved.
        let mut again = Entry { started_at: Some(1000.0), ..stack() };
        again.follow(3 * GB, 0.2, 0.2, 12.0, 1300.0, Some(LongLived { steady_mem_kb: Some(5 * GB), ..STACK }));
        assert_eq!(again.need_mem_kb, 3 * GB + (3 * GB) / 4);

        // An ordinary job keeps its peak needs for its whole run.
        let mut compile = Entry { started_at: Some(1000.0), ..job(4, "compile", 2.0, 6) };
        compile.follow(3 * GB, 11.0, 11.0, 12.0, 1010.0, None);
        compile.follow(3 * GB, 0.2, 0.2, 12.0, 5000.0, None);
        assert_eq!((compile.need_cpu, compile.need_mem_kb, compile.steady_since), (11.0, 6 * GB, None));
    }

    #[test]
    fn steady_cpu_ignores_runnable_wait_and_stays_under_host_capacity() {
        let mut s = Entry { started_at: Some(1000.0), ..stack() };
        s.follow(3 * GB, 20.0, 0.5, 18.0, 1300.0, Some(STACK));
        assert_eq!(s.steady_since, Some(1300.0));
        assert!((s.need_cpu - 0.625).abs() < f64::EPSILON, "{}", s.need_cpu);
        s.follow(3 * GB, 100.0, 100.0, 18.0, 1400.0, Some(STACK));
        assert_eq!(s.need_cpu, 18.0);
    }

    /// A reserved job that a stack keeps out: the stack never ends by itself,
    /// so nothing counts on it to make room. It counts as load from outside
    /// taskguard: once only stacks run, the first in line starts whatever the
    /// readings say, and past `max_backfill` a newer job passes only when it
    /// ends before that.
    #[test]
    fn backfill_never_waits_for_a_long_lived_job_to_end() {
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        // Two stacks past their start-up hold 10 GB each, other programs 6.
        let steady = |pid: i32| Entry {
            pid,
            started_at: Some(0.0),
            steady_since: Some(300.0),
            live_cpu: 0.3,
            live_mem_kb: 10 * GB,
            ..job(1, "dalp:stack", 0.3, 10)
        };
        let stacks = [steady(11), steady(12)];
        let m = machine(2.0, 26);
        // A 12 GB head would reach 38 GB of 27.2.
        let (_, head) = blocked_head();
        let quick = Entry { est_dur_s: Some(60.0), ..job(3, "install", 0.2, 1) };
        let waiting = [head.clone(), quick.clone()];
        // Nothing but the stacks runs: the head starts, as when they ran outside taskguard.
        assert!(blockers(decide(&m, &backfill, &stacks, &waiting, &head, 2900.0, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &backfill, &stacks, &waiting, &quick, 2900.0, &[])), vec!["order"]);

        // A compile that ends in 100 s runs too. Past max_backfill, the head
        // starts once the compile is done, and a job that ends before then passes.
        let mut compile = Entry { started_at: Some(1000.0), est_dur_s: Some(2000.0), ..job(4, "compile", 1.0, 1) };
        compile.live_mem_kb = GB;
        let running = [stacks[0].clone(), stacks[1].clone(), compile.clone()];
        let slow = Entry { est_dur_s: Some(600.0), ..job(5, "build", 0.2, 1) };
        let waiting = [head.clone(), quick.clone(), slow.clone()];
        assert_eq!(blockers(decide(&m, &backfill, &running, &waiting, &head, 2900.0, &[])), vec!["memory"]);
        assert!(blockers(decide(&m, &backfill, &running, &waiting, &quick, 2900.0, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &backfill, &running, &waiting, &slow, 2900.0, &[])), vec!["reserved"]);

        // A stack still in its start-up holds the room it may take: nothing
        // starts into it whatever the readings say, and nobody counts on it ending.
        let starting = Entry { steady_since: None, ..stacks[0].clone() };
        let running = [starting, stacks[1].clone()];
        assert_eq!(blockers(decide(&m, &backfill, &running, &waiting, &head, 2900.0, &[])), vec!["memory"]);
        assert_eq!(blockers(decide(&m, &backfill, &running, &waiting, &quick, 2900.0, &[])), vec!["reserved"]);
    }

    #[test]
    fn backfill_keeps_pools_and_pressure() {
        let (mut running, mut head) = blocked_head();
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        // The head is held only by its pool slot: newer jobs outside the pool
        // start, newer jobs in the same pool still wait for the slot.
        running.need_mem_kb = GB;
        running.live_mem_kb = GB;
        running.pool_key = Some("db@/repo".into());
        head.pool = Some("db".into());
        head.pool_key = Some("db@/repo".into());
        head.pool_slots = Some(1);
        let mut same_pool = job(3, "migrate", 0.2, 1);
        same_pool.pool = head.pool.clone();
        same_pool.pool_key = head.pool_key.clone();
        same_pool.pool_slots = Some(1);
        let other = job(4, "lint", 0.2, 1);
        let waiting = [head.clone(), same_pool.clone(), other.clone()];
        let m = machine(2.0, 2);
        let r = std::slice::from_ref(&running);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &head, 1300.0, &[])), vec!["slots"]);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &same_pool, 1300.0, &[])), vec!["slots"]);
        assert!(blockers(decide(&m, &backfill, r, &waiting, &other, 1300.0, &[])).is_empty());
        // Memory pressure still stops every new job.
        let mut m = m;
        m.mem_pressure = Some(50.0);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &other, 1300.0, &[])), vec!["pressure"]);
    }

    #[test]
    fn pools_count_slots_per_pool_key() {
        let mut running = job(1, "e2e-a", 0.1, 0);
        running.pool_key = Some("e2e@/repo".into());
        let mut me = job(2, "e2e-b", 0.1, 0);
        me.pool = Some("e2e".into());
        me.pool_key = Some("e2e@/repo".into());
        me.pool_slots = Some(1);
        assert_eq!(blockers(decide(&machine(0.0, 1), &LIM, &[running.clone()], &[me.clone()], &me, 1000.0, &[])), vec!["slots"]);
        me.pool_key = Some("e2e@/other-worktree".into());
        assert!(blockers(decide(&machine(0.0, 1), &LIM, &[running], &[me.clone()], &me, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_first_run_waits_while_a_group_of_first_runs_still_grows() {
        // A first run started at 1000 and still grows; its memory last rose at 1030.
        let mut growing = job(1, "packages/ci:run", 1.0, 2);
        growing.known = false;
        growing.started_at = Some(1000.0);
        growing.mem_grew_at = Some(1030.0);
        let mut me = job(2, "new", 1.0, 1);
        me.known = false;
        let r = std::slice::from_ref(&growing);
        let w = std::slice::from_ref(&me);
        // With room for many, a group of first runs may grow side by side.
        assert!(blockers(decide(&machine(1.0, 4), &LIM, r, w, &me, 1040.0, &[])).is_empty());
        // 10 of 12 cores busy: the group is one job, and that one still grows.
        let full = machine(10.0, 4);
        let d = decide(&full, &LIM, r, w, &me, 1040.0, &[]);
        assert_eq!(blockers(d.clone()), vec!["learning"]);
        assert!(
            matches!(&d, Decision::Wait { blockers } if matches!(&blockers[0], Blocker::Settling { wait_s, .. } if (*wait_s - 10.0).abs() < 1e-9))
        );
        // Steady for 20 s: it has shown what it takes.
        assert!(blockers(decide(&full, &LIM, r, w, &me, 1051.0, &[])).is_empty());
        // Still growing, but past two minutes: it no longer holds new jobs back.
        let mut late = growing.clone();
        late.mem_grew_at = Some(1119.0);
        assert!(blockers(decide(&full, &LIM, std::slice::from_ref(&late), w, &me, 1121.0, &[])).is_empty());
        // A job with history is never held back by the rule.
        let known = job(3, "old", 1.0, 1);
        assert!(blockers(decide(&full, &LIM, r, std::slice::from_ref(&known), &known, 1040.0, &[])).is_empty());
        // A first run guessed to end within seconds neither waits nor counts.
        let mut lint = me.clone();
        lint.est_dur_s = Some(1.0);
        assert!(blockers(decide(&full, &LIM, r, std::slice::from_ref(&lint), &lint, 1040.0, &[])).is_empty());
        let mut short_growing = growing.clone();
        short_growing.est_dur_s = Some(1.0);
        assert!(blockers(decide(&full, &LIM, std::slice::from_ref(&short_growing), w, &me, 1040.0, &[])).is_empty());
    }

    #[test]
    fn overage_is_measured_against_the_needs_a_job_started_with() {
        let mut e = job(1, "k", 2.0, 4);
        e.live_mem_kb = 6 * GB;
        e.need_mem_kb = 7 * GB + GB / 2; // raised to follow its peak
        e.start_need_mem_kb = Some(4 * GB);
        assert_eq!(over(&[e]).1, 2 * GB);
    }

    #[test]
    fn unknown_jobs_start_in_batches_sized_by_free_room() {
        let running = job(1, "r", 1.0, 1);
        let mut me = job(2, "new", 0.0, 0);
        me.known = false;
        let r = std::slice::from_ref(&running);
        let w = std::slice::from_ref(&me);
        // A nearly full machine: batches of one, two seconds apart.
        assert_eq!(blockers(decide(&machine(10.5, 1), &LIM, r, w, &me, 1001.0, &[1000.0])), vec!["learning"]);
        assert!(blockers(decide(&machine(10.5, 1), &LIM, r, w, &me, 1003.0, &[1000.0])).is_empty());
        // An idle machine: many new jobs may start in the same window.
        let three = [1000.0, 1000.5, 1001.0];
        assert!(blockers(decide(&machine(1.0, 1), &LIM, r, w, &me, 1001.2, &three)).is_empty());
        // Memory limits the batch too: 26 of 27.2 usable GB held leaves room for one.
        assert_eq!(blockers(decide(&machine(1.0, 26), &LIM, r, w, &me, 1001.2, &three)), vec!["learning"]);
    }

    /// Replays the freeze of 2026-09-25: a first typecheck of a whole monorepo.
    /// 60 compiles with no history arrive at once on a 32 GB machine that
    /// already holds 16 GB. Each really grows to between 0.3 and 4.2 GB over
    /// 8 seconds and runs for 30. The kernel raises its pressure level as
    /// memory fills. Returns the highest share of memory in use.
    fn replay_burst(lim: &Limits, estimate_kb: u64) -> f64 {
        const TOTAL: u64 = 32 * GB;
        const BASE: u64 = 16 * GB;
        let peaks: Vec<u64> = (0..60).map(|i| [300, 700, 4200, 500, 1200, 900][i % 6] * 1024).collect();
        let mut waiting: Vec<Entry> = (0..60)
            .map(|i| Entry {
                ticket: i + 1,
                key: format!("pkg{i}:tsc"),
                need_mem_kb: estimate_kb,
                known: false,
                queued_at: 0.0,
                ..Default::default()
            })
            .collect();
        let mut running: Vec<(Entry, f64, u64)> = Vec::new(); // entry, start, real peak
        let mut starts: Vec<f64> = Vec::new();
        let mut recent_mem: Vec<(f64, u64, u64)> = Vec::new();
        let mut worst: f64 = 0.0;
        let mut t = 0.0;
        while t < 400.0 && (!waiting.is_empty() || !running.is_empty()) {
            running.retain(|(_, s, _)| t - s < 30.0);
            for (e, s, peak) in running.iter_mut() {
                e.live_mem_kb = (*peak as f64 * ((t - *s) / 8.0).min(1.0)) as u64;
            }
            let used = BASE + running.iter().map(|(e, _, _)| e.live_mem_kb).sum::<u64>();
            worst = worst.max(used as f64 * 100.0 / TOTAL as f64);
            recent_mem.retain(|(ts, _, _)| t - ts < crate::machine::PEAK_WINDOW);
            recent_mem.push((t, used, used - BASE));
            let pct = used as f64 * 100.0 / TOTAL as f64;
            let pressure = if pct > 90.0 {
                100.0
            } else if pct > 80.0 {
                50.0
            } else {
                0.0
            };
            let m = MachineSample {
                ts: t,
                cpu_busy: 2.0,
                ncpu: 10,
                mem_used_kb: used,
                mem_total_kb: TOTAL,
                mem_pressure: Some(pressure),
                recent_mem: recent_mem.clone(),
                ..Default::default()
            };
            // Every waiting job checks once per tick, oldest first, as the runners do.
            let mut i = 0;
            while i < waiting.len() {
                let run: Vec<Entry> = running.iter().map(|(e, _, _)| e.clone()).collect();
                let me = waiting[i].clone();
                if matches!(decide(&m, lim, &run, &waiting, &me, t, &starts), Decision::Admit { .. }) {
                    let peak = peaks[me.ticket as usize - 1];
                    starts.push(t);
                    running.push((me, t, peak));
                    waiting.remove(i);
                } else {
                    i += 1;
                }
            }
            t += 0.5;
        }
        assert!(waiting.is_empty(), "every job still ran");
        worst
    }

    #[test]
    fn a_burst_of_first_runs_no_longer_fills_the_machine() {
        // The rules at the time of the freeze: a first run needed nothing,
        // batches every 2 s, and the kernel's pressure alarm was ignored.
        let old = Limits { learn_stagger: 2.0, pressure_max: f64::MAX, ..LIM };
        let before = replay_burst(&old, 0);
        assert!(before > 97.0, "the old rules fill the machine: {before:.0}%");
        // Now: an estimated need, settled batches, and the pressure stop.
        let new = Limits { learn_stagger: 5.0, pressure_max: 20.0, ..LIM };
        let after = replay_burst(&new, 1536 * 1024);
        eprintln!("burst replay: old rules peak at {before:.0}% memory, new rules at {after:.0}%");
        assert!(after < 90.0, "the new rules stay clear of the edge: {after:.0}%");
    }

    #[test]
    fn pressure_stops_every_new_job() {
        let running = job(1, "r", 1.0, 1);
        let me = job(2, "tsc", 1.0, 1);
        let mut m = machine(1.0, 8);
        m.mem_pressure = Some(50.0);
        assert_eq!(
            blockers(decide(&m, &LIM, std::slice::from_ref(&running), std::slice::from_ref(&me), &me, 1000.0, &[])),
            vec!["pressure"]
        );
        // Even with nothing of ours running: the pressure comes from others.
        assert_eq!(blockers(decide(&m, &LIM, &[], std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["pressure"]);
        m.mem_pressure = Some(0.0);
        assert!(blockers(decide(&m, &LIM, &[], std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty(), "it eases: the job starts");
    }

    #[test]
    fn admission_uses_the_peak_of_the_last_seconds() {
        let running = job(1, "r", 1.0, 1);
        let me = job(2, "tsc", 1.0, 2);
        let mut m = machine(1.0, 20);
        // It read 26 GB a few seconds ago; the dip to 20 GB now does not count.
        m.recent_mem = vec![(995.0, 26 * GB, 0), (999.0, 20 * GB, 0)];
        m.ts = 1000.0;
        assert_eq!(blockers(decide(&m, &LIM, std::slice::from_ref(&running), std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["memory"]);
    }

    #[test]
    fn short_jobs_skip_the_cpu_check_but_not_memory() {
        let running = job(1, "r", 1.0, 1);
        let r = std::slice::from_ref(&running);
        let mut me = job(2, "lint", 2.0, 1);
        me.est_dur_s = Some(0.4);
        assert!(blockers(decide(&machine(12.0, 8), &LIM, r, std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&machine(12.0, 27), &LIM, r, std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["memory"]);
        me.est_dur_s = Some(30.0);
        assert_eq!(blockers(decide(&machine(12.0, 8), &LIM, r, std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["cpu"]);
    }

    #[test]
    fn short_jobs_wait_when_running_jobs_were_promised_every_core() {
        // Eleven cores promised on a 12-core machine: one more short job of
        // two cores would make 13, whatever the CPU reading says.
        let running: Vec<Entry> = (1..=11).map(|i| job(i, &format!("lint{i}"), 1.0, 1)).collect();
        let mut me = job(20, "lint", 2.0, 1);
        me.est_dur_s = Some(0.4);
        assert_eq!(blockers(decide(&machine(0.0, 8), &LIM, &running, std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["cpu"]);
        me.need_cpu = 1.0;
        assert!(blockers(decide(&machine(12.0, 8), &LIM, &running, std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty());
        // A job paused by hand promises nothing.
        let mut paused = running.clone();
        paused[0].paused_by_hand = true;
        me.need_cpu = 2.0;
        assert!(blockers(decide(&machine(0.0, 8), &LIM, &paused, std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty());
    }

    fn in_run(mut e: Entry, pipeline: &str, since: f64) -> Entry {
        e.pipeline = Some(pipeline.into());
        e.pipeline_since = Some(since);
        e
    }

    #[test]
    fn a_job_of_an_older_run_that_can_start_goes_first() {
        let running = job(1, "r", 1.0, 1);
        let r = std::slice::from_ref(&running);
        // The older run's job joined the queue later, but its run is older.
        let newer = in_run(job(2, "b:test", 1.0, 1), "turbo:20", 900.0);
        let older = in_run(job(3, "a:test", 1.0, 1), "turbo:10", 800.0);
        let waiting = [newer.clone(), older.clone()];
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, r, &waiting, &newer, 1000.0, &[])), vec!["pipeline"]);
        assert!(blockers(decide(&machine(1.0, 8), &LIM, r, &waiting, &older, 1000.0, &[])).is_empty());
        // A job of the older run that does not fit holds nothing back.
        let big = in_run(job(3, "a:build", 1.0, 30), "turbo:10", 800.0);
        assert!(blockers(decide(&machine(1.0, 8), &LIM, r, &[newer.clone(), big], &newer, 1000.0, &[])).is_empty());
        // Jobs of the same run, and jobs outside any run, keep the old rules.
        let sibling = in_run(job(4, "b:lint", 1.0, 1), "turbo:20", 900.0);
        assert!(blockers(decide(&machine(1.0, 8), &LIM, r, &[newer.clone(), sibling], &newer, 1000.0, &[])).is_empty());
        let alone = job(5, "alone", 1.0, 1);
        assert!(blockers(decide(&machine(1.0, 8), &LIM, r, &[alone.clone(), older.clone()], &alone, 1000.0, &[])).is_empty());
        // A higher priority still beats an older run.
        let mut urgent = newer.clone();
        urgent.priority = 1;
        assert!(blockers(decide(&machine(1.0, 8), &LIM, r, &[urgent.clone(), older], &urgent, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_reservation_does_not_hold_back_a_job_of_an_older_run() {
        let running = job(1, "r", 1.0, 1);
        let r = std::slice::from_ref(&running);
        // A job of the newer run waited long, passed by the older run's jobs.
        let mut passed = in_run(job(2, "b:test", 1.0, 1), "turbo:20", 900.0);
        passed.queued_at = 700.0;
        passed.bypassed_since = Some(750.0);
        let older = in_run(job(3, "a:test", 1.0, 1), "turbo:10", 800.0);
        let waiting = [passed.clone(), older.clone()];
        // The older run's job starts; the passed one steps aside for it.
        assert!(blockers(decide(&machine(1.0, 8), &LIM, r, &waiting, &older, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, r, &waiting, &passed, 1000.0, &[])), vec!["pipeline"]);
        // A job outside any run is still held back by the reservation.
        let other = job(4, "other", 1.0, 1);
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, r, &[passed, other.clone()], &other, 1000.0, &[])), vec!["reserved"]);
    }

    /// The circle seen with several DALP gates at once, all three jobs fitting:
    /// a job outside any run held a job of an older run by its reservation,
    /// that job held a job of a newer run by the run rule, and the newer run's
    /// job held the first one by its own reservation. Nothing started.
    #[test]
    fn a_reservation_and_an_older_run_never_wait_for_each_other_in_a_circle() {
        let running = job(1, "r", 1.0, 1);
        let r = std::slice::from_ref(&running);
        let mut older_run = in_run(job(4, "a:test", 1.0, 1), "turbo:10", 800.0);
        older_run.queued_at = 950.0;
        let mut newer_run = in_run(job(2, "b:test", 1.0, 1), "turbo:20", 900.0);
        newer_run.queued_at = 905.0;
        newer_run.bypassed_since = Some(906.0);
        let mut outside = job(3, "gate", 1.0, 1);
        outside.queued_at = 910.0;
        outside.bypassed_since = Some(911.0);
        let waiting = [older_run.clone(), newer_run.clone(), outside.clone()];
        let lim = Limits { max_backfill: 1800.0, ..LIM };
        let m = machine(1.0, 8);
        // The older run's job is first in line, and starts.
        assert!(blockers(decide(&m, &lim, r, &waiting, &older_run, 1300.0, &[])).is_empty());
        // The others wait for jobs ahead of them, never for one behind.
        assert_eq!(blockers(decide(&m, &lim, r, &waiting, &newer_run, 1300.0, &[])), vec!["pipeline"]);
        assert_eq!(blockers(decide(&m, &lim, r, &waiting, &outside, 1300.0, &[])), vec!["reserved"]);
    }

    /// Whatever the queue holds, a job is only ever held back for a job ahead
    /// of it in line, so there is no circle; and when every waiter fits, the
    /// first in line starts.
    #[test]
    fn every_hold_points_to_a_job_ahead_in_line() {
        let mut seed: u64 = 0x5eed;
        let mut next = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let lim = Limits { max_backfill: 1800.0, ..LIM };
        for case in 0..2000 {
            let running = job(1000, "r", 1.0, 1);
            let r = std::slice::from_ref(&running);
            let waiting: Vec<Entry> = (0..2 + next(5))
                .map(|i| {
                    let mut e = job(1 + next(50), &format!("j{i}"), 1.0, 1 + next(2) * 29 * next(2));
                    e.pid = i as i32;
                    e.queued_at = 900.0 + next(300) as f64;
                    e.priority = (next(5) == 0) as i32;
                    if next(2) == 0 {
                        e.bypassed_since = Some(e.queued_at + 1.0);
                    }
                    if next(3) != 0 {
                        let run = next(3);
                        e = in_run(e, &format!("turbo:{run}"), 700.0 + run as f64 * 100.0);
                    }
                    e
                })
                .collect();
            let m = machine(1.0, 2);
            let now = 900.0 + next(2400) as f64;
            for me in &waiting {
                if let Decision::Wait { blockers } = decide(&m, &lim, r, &waiting, me, now, &[]) {
                    for b in blockers {
                        let holder = match b {
                            Blocker::Older { key }
                            | Blocker::Reserved { key, .. }
                            | Blocker::Priority { key, .. }
                            | Blocker::Pipeline { key, .. } => key,
                            _ => continue,
                        };
                        let holder = waiting.iter().find(|w| w.key == holder).unwrap();
                        assert!(holder.ahead_of(me), "case {case}: {} holds back {}, which is ahead of it", holder.key, me.key);
                    }
                }
            }
            let fits: Vec<&Entry> = waiting.iter().filter(|w| w.need_mem_kb == GB).collect();
            if fits.len() == waiting.len() {
                let head = waiting.iter().min_by(|a, b| a.line_order(b)).unwrap();
                assert!(blockers(decide(&m, &lim, r, &waiting, head, now, &[])).is_empty(), "case {case}: the first in line must start");
            }
        }
    }

    /// How the owner of a waiting job decides.
    #[derive(Clone, Copy)]
    enum Owner {
        /// This version: `decide`, with a `StallWatch` on the jobs ahead.
        This,
        /// taskguard 0.4.0 and older, which keep the line in ticket order.
        /// Only their rule for an empty machine is modelled: these replays
        /// stop when the first job starts.
        TicketOrder,
    }

    /// Owners of waiting jobs poll every 100 ms while nothing runs, and a job
    /// a watch reports gets marked stalled, as the runner does. Returns the
    /// first job that starts and after how long, or None within 120 s.
    fn replay_mixed(m: &MachineSample, mut waiting: Vec<(Entry, Owner)>) -> Option<(String, f64)> {
        let lim = Limits { max_backfill: 1800.0, ..LIM };
        let mut watches: Vec<StallWatch> = waiting.iter().map(|_| StallWatch::default()).collect();
        let mut t = 2000.0;
        while t < 2120.0 {
            for i in 0..waiting.len() {
                let entries: Vec<Entry> = waiting.iter().map(|(e, _)| e.clone()).collect();
                let (me, owner) = waiting[i].clone();
                let start = match owner {
                    Owner::This => {
                        let d = decide(m, &lim, &[], &entries, &me, t, &[]);
                        if let Some(s) = watches[i].step(m, &lim, &[], &entries, &me, &d, t, &[]) {
                            let (ticket, pid) = (s.ticket, s.pid);
                            waiting.iter_mut().filter(|(e, _)| e.ticket == ticket && e.pid == pid).for_each(|(e, _)| e.stalled = true);
                        }
                        matches!(d, Decision::Admit { .. })
                    }
                    Owner::TicketOrder => {
                        !entries.iter().any(|w| w.priority > me.priority || (w.priority == me.priority && w.ticket < me.ticket))
                    }
                };
                if start {
                    return Some((me.key, t - 2000.0));
                }
            }
            t += 0.1;
        }
        None
    }

    fn queued(ticket: u64, key: &str, queued_at: f64, pid: i32) -> Entry {
        Entry { pid, queued_at, ..job(ticket, key, 1.0, 1) }
    }

    /// The stall of 2026-10-04 16:11 on a DALP Mac, with nothing running and
    /// room to spare. host/ddwf and host/dapi belong to a turbo run that began
    /// before the others queued, and their owners ran taskguard 0.4.0: by
    /// ticket they wait for the solhint job. The solhint and dashboard jobs ran
    /// 0.4.1: by run age they wait for host/ddwf. Nothing started for five
    /// minutes, until host/ddwf was killed.
    #[test]
    fn a_head_whose_owner_keeps_another_order_holds_nobody_up() {
        let ddwf = in_run(queued(3, "host/ddwf:tsc", 1003.0, 30), "turbo:9", 900.0);
        let dapi = in_run(queued(5, "host/dapi:oxlint", 1010.0, 50), "turbo:9", 900.0);
        let solhint = queued(2, "onchain/evm:solhint", 1002.0, 20);
        let lint = queued(4, "web/dashboard:oxlint", 1003.0, 40);
        let m = MachineSample::fixed(3.5, 10, 19 * GB, 32 * GB);
        let waiting = vec![(ddwf, Owner::TicketOrder), (solhint, Owner::This), (lint, Owner::This), (dapi, Owner::TicketOrder)];
        let (key, after) = replay_mixed(&m, waiting).expect("a job starts");
        // Both 0.4.0 jobs ahead are passed, one after the other.
        assert_eq!(key, "onchain/evm:solhint");
        assert!(after <= 2.0 * STALL_S + 1.0, "started after {after:.1}s");
    }

    /// The stall of 2026-10-04 15:44 on the same Mac: an 8 GB integration run
    /// (0.4.1) that did not fit next to 19.7 GB held outside taskguard. Once
    /// nothing ran it should have started anyway, but by run age it waited
    /// for host/legacy-graph, and that job's owner (0.4.0) waited for it by
    /// ticket. Everything else waited behind the two for 24 minutes.
    #[test]
    fn an_integration_run_that_does_not_fit_still_starts_when_nothing_runs() {
        let mut integration = queued(1, "root:run-integration", 937.0, 10);
        integration.need_mem_kb = 8 * GB;
        integration.raised_by_min = true;
        integration.pool = Some("integration".into());
        integration.pool_key = Some("integration".into());
        integration.pool_slots = Some(1);
        let graph = in_run(queued(3, "host/legacy-graph:vitest", 1040.0, 30), "turbo:8", 800.0);
        let vitest = in_run(queued(4, "host/dapi:vitest", 1041.0, 40), "turbo:8", 800.0);
        let waiting = vec![
            (integration, Owner::This),
            (queued(2, "root:lint-ast-grep", 938.0, 20), Owner::TicketOrder),
            (graph, Owner::TicketOrder),
            (vitest, Owner::TicketOrder),
            (queued(5, "e2e/dapi-testkit:tsc", 1044.0, 50), Owner::This),
            (queued(6, "e2e/agentic:tsc", 1044.0, 60), Owner::This),
        ];
        let m = MachineSample::fixed(2.0, 10, 19 * GB + 7 * GB / 10, 32 * GB);
        let (key, after) = replay_mixed(&m, waiting).expect("a job starts");
        assert_eq!(key, "root:run-integration");
        assert!(after <= 2.0 * STALL_S + 1.0, "started after {after:.1}s");
    }

    /// A job that cannot fit even with no job of taskguard running (other
    /// programs hold too much) neither stops jobs that fit nor waits forever:
    /// they pass it for `max_backfill`, then its reservation drains the queue
    /// and it starts alone.
    #[test]
    fn a_job_that_can_never_fit_neither_blocks_the_queue_nor_waits_forever() {
        let lim = Limits { max_bypass: 60.0, max_backfill: 300.0, ..LIM };
        let other = 19 * GB + 7 * GB / 10;
        let mut head = job(1, "root:run-integration", 1.0, 8);
        head.queued_at = 0.0;
        let mut waiting = vec![head];
        // A compile already runs, so the head cannot start on an empty machine.
        let mut compile = job(100_000, "compile", 1.0, 2);
        compile.live_mem_kb = 2 * GB;
        let mut running: Vec<(Entry, f64)> = vec![(compile, 60.0)];
        let mut started: Vec<(String, f64)> = Vec::new();
        let mut t: f64 = 0.0;
        while t < 1500.0 {
            running.retain(|(_, end)| *end > t);
            // A small job every 5 s for 15 minutes; each runs 20 s.
            if t < 900.0 && t % 5.0 == 0.0 {
                let mut e = job(2 + t as u64, &format!("lint{t}"), 1.0, 1);
                e.queued_at = t;
                waiting.push(e);
            }
            let run: Vec<Entry> = running.iter().map(|(e, _)| e.clone()).collect();
            let m = MachineSample::fixed(run.len() as f64, 10, other + run.iter().map(|e| e.live_mem_kb).sum::<u64>(), 32 * GB);
            let mut i = 0;
            while i < waiting.len() {
                let me = waiting[i].clone();
                if matches!(decide(&m, &lim, &run, &waiting, &me, t, &[]), Decision::Admit { .. }) {
                    for w in waiting.iter_mut().filter(|w| w.ahead_of(&me) && w.bypassed_since.is_none()) {
                        w.bypassed_since = Some(t);
                    }
                    let mut e = waiting.remove(i);
                    e.live_mem_kb = e.need_mem_kb;
                    started.push((e.key.clone(), t));
                    running.push((e, t + 20.0));
                    break;
                }
                i += 1;
            }
            t += 0.5;
        }
        assert!(waiting.is_empty(), "every job ran");
        let head_at = started.iter().find(|(k, _)| k == "root:run-integration").map(|(_, s)| *s).unwrap();
        assert!(started.iter().filter(|(_, s)| *s < head_at).count() > 10, "jobs that fit pass it meanwhile");
        assert!(head_at <= lim.max_backfill + 20.0 + 1.0, "it starts once the reservation drained the queue: {head_at}");
    }

    #[test]
    fn the_shape_changes_when_a_job_joins_or_leaves_not_when_it_rewrites() {
        let dir = std::env::temp_dir().join(format!("tg-shape-{}", std::process::id()));
        let q = Queue::open(&dir).unwrap();
        let live = std::process::id() as i32;
        let gone = {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id() as i32;
            c.wait().unwrap();
            pid
        };
        let a = queued(1, "a", 1000.0, live);
        q.write(&q.wait_path(&a), &a).unwrap();
        let before = q.shape(live);
        q.write(&q.wait_path(&a), &Entry { blocker: Some("cpu".into()), ..a.clone() }).unwrap();
        assert_eq!(q.shape(live), before, "a rewrite is no change");
        let b = queued(2, "b", 1001.0, gone);
        q.write(&q.wait_path(&b), &b).unwrap();
        assert_ne!(q.shape(live), before, "a job joined");
        q.nudge(live, |n| n.start = true).unwrap();
        assert!(q.shape(live).contains(&"nudge".to_string()), "a nudge wakes its job");

        // One read gives what reap, running and waiting gave.
        let (reaped, run, wait) = q.reap_and_read();
        assert_eq!(reaped, vec![b.run_id]);
        assert!(run.is_empty());
        assert_eq!(wait.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(), ["a"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stall_mark_is_read_with_the_queue_and_goes_with_its_job() {
        let dir = std::env::temp_dir().join(format!("tg-stall-{}", std::process::id()));
        let q = Queue::open(&dir).unwrap();
        let live = std::process::id() as i32;
        let (a, b) = (queued(1, "a", 1000.0, live), queued(2, "b", 1001.0, live));
        q.write(&q.wait_path(&a), &a).unwrap();
        q.write(&q.wait_path(&b), &b).unwrap();
        q.mark_stalled(&a, &Stall { key: "a".into(), ..Default::default() }).unwrap();
        assert_eq!(q.waiting().iter().map(|e| e.stalled).collect::<Vec<_>>(), vec![true, false]);
        // The mark is no part of the entry its owner writes.
        assert!(!fs::read_to_string(q.wait_path(&a)).unwrap().contains("stalled"));
        q.reap();
        assert!(q.waiting()[0].stalled, "kept while the job waits");
        fs::remove_file(q.wait_path(&a)).unwrap();
        q.reap();
        assert_eq!(fs::read_dir(dir.join("stall")).unwrap().count(), 0, "gone with its job");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_run_keeps_its_age_until_its_runner_is_gone() {
        let dir = std::env::temp_dir().join(format!("tg-pipeline-{}", std::process::id()));
        let q = Queue::open(&dir).unwrap();
        let live = std::process::id() as i32;
        assert_eq!(q.pipeline_since(&format!("turbo:{live}"), live, 100.0), 100.0);
        assert_eq!(q.pipeline_since(&format!("turbo:{live}"), live, 500.0), 100.0, "a later job keeps the first time");
        let dead = i32::MAX - 1;
        q.pipeline_since(&format!("turbo:{dead}"), dead, 200.0);
        q.reap();
        assert!(dir.join("pipeline").join(format!("turbo.{live}")).exists());
        assert!(!dir.join("pipeline").join(format!("turbo.{dead}")).exists(), "a gone runner's file is dropped");
        fs::remove_dir_all(&dir).ok();
    }
}
