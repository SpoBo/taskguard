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

/// A taskguard call that runs inside a running job, under its claim.
#[derive(Debug, Clone, PartialEq)]
pub struct Nested {
    pub outer: i32,
    pub pid: i32,
    pub since: f64,
    pub key: String,
}

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
    /// A long-lived job, from its start: it runs until someone stops it.
    /// Older versions leave it out; their long-lived jobs show as such only
    /// once they are steady.
    pub long_lived: bool,
    /// For a long-lived job: the work its caller said will run on it
    /// (`--expect`), with the memory booked for each until that work starts.
    /// Older versions leave it out, and do not see the booking.
    pub expects: Vec<Expect>,
    /// The long-lived job this job is work for: one in the same checkout
    /// that expects it. Older versions leave it out.
    pub work_for: Option<WorkFor>,
    /// For a long-lived job that expects work: until then it waits, before
    /// it starts, for that work to queue (`expect_wait`), so it starts with
    /// room for both. Older versions leave it out.
    pub expect_until: Option<f64>,
    /// It started with memory over RAM: by `overcommit`, by --st, or by
    /// hand. On Linux the kernel's OOM killer picks it first. Older versions
    /// leave it out.
    pub over_ram: bool,
    /// It started by the over-commit rule itself (`Room::overcommit`), as
    /// the only job in the queue or stuck first in line, not by --st or by
    /// hand. Work that takes over what it declared for it inherits that.
    /// Older versions leave it out.
    pub overcommitted: bool,
    /// While its past runs say it can no longer reach its whole need: that
    /// whole need, which `need_mem_kb` is lowered from (`Entry::reach`).
    /// Older versions leave it out, and read the lowered need.
    pub reach_flat_kb: Option<u64>,
    /// It went above what its past runs reached at its age: it books its
    /// whole need for the rest of its run. Older versions leave it out.
    pub reach_missed: bool,
    /// Another job saw this one able to start for `STALL_S` while it still
    /// waited (`stall/<ticket>.<pid>`). It holds no job back any more. Read
    /// from the queue, never written into the entry.
    #[serde(skip)]
    pub stalled: bool,
}

/// Work a long-lived job expects: a pool name, a job key, or a command
/// pattern as in `[[job]] match`. Until a job that matches it starts, the
/// long-lived job books `mem_kb` for it, so the room the work needs is
/// there when it comes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Expect {
    pub work: String,
    pub mem_kb: u64,
    /// Where `mem_kb` comes from, in words.
    pub from: String,
    /// A job for it has started: the booking is gone.
    pub started: bool,
    /// Not declared: the history says such work followed this job before
    /// (`infer_work`). While no job links to it, it is a guess (`is_guess`):
    /// its booking ends `expire_after_s` after the job started, and no job
    /// waits for it under the progress rule.
    pub implied: bool,
    pub expire_after_s: Option<f64>,
    /// The guess did not come true in time: the booking is gone.
    pub expired: bool,
}

impl Expect {
    /// Still booked: its work has not started, and the guess has not expired.
    pub fn open(&self) -> bool {
        !self.started && !self.expired
    }

    /// A guess whose time is up, for a job that started at `started`.
    pub fn due(&self, started: f64, now: f64) -> bool {
        self.open() && self.expire_after_s.is_some_and(|a| now - started > a)
    }
}

/// Is item `i` of long-lived job `e` still a guess of the history: implied,
/// and no job among `running` and `waiting` links to it? Work that queued
/// for a guess and linked to it made it come true: from then on it is
/// declared work in every rule.
pub fn is_guess(e: &Entry, i: usize, running: &[Entry], waiting: &[Entry]) -> bool {
    e.expects.get(i).is_some_and(|x| x.implied)
        && !running.iter().chain(waiting).any(|j| j.work_for.as_ref().is_some_and(|w| w.pid == e.pid && w.item == Some(i as u32)))
}

/// A guess of the history that did not come true in time: its booking ends.
pub fn expire_guesses(e: &mut Entry, running: &[Entry], waiting: &[Entry], now: f64) -> bool {
    let started = e.started_at.unwrap_or(now);
    let due: Vec<usize> = (0..e.expects.len()).filter(|&i| e.expects[i].due(started, now) && is_guess(e, i, running, waiting)).collect();
    for &i in &due {
        e.expects[i].expired = true;
    }
    !due.is_empty()
}

/// The long-lived job a job is work for, and which of its `expects` it
/// takes over (none when every one of them has started already).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorkFor {
    pub pid: i32,
    pub key: String,
    pub item: Option<u32>,
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

    /// A job that does not end by itself: long-lived, or steady.
    pub fn lasting(&self) -> bool {
        self.long_lived || self.steady()
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
    /// The highest memory its past runs took after their start-up. `follow`
    /// does not book it: once steady, a job books what it holds now.
    pub steady_mem_kb: Option<u64>,
}

/// Room kept above a steady job's measured CPU use.
const STEADY_CPU_MARGIN: f64 = 0.25;

impl Entry {
    /// Move a running job's needs after one reading of it: `mem_kb` and the
    /// cores it used in the recent window.
    ///
    /// Past its needs, a job's needs follow its peak, with room to grow: what
    /// it promises to take must keep up with what it takes. CPU follows the
    /// cores it uses, never the time its threads wait for a core: a tool that
    /// starts one thread per core waits on every core of a busy machine.
    ///
    /// A long-lived job follows its peak through its start-up only. After that its needs start again
    /// from what it uses: a dev stack that took every core while it started
    /// and seeded, and idles at a few percent, must not keep every core for
    /// hours. Its memory need is what it holds now plus a quarter, up or
    /// down with each reading, so an idle stack releases its booking; what
    /// its past runs grew to after their start-up is not booked. Its CPU need
    /// is measured use plus a margin, up or down, because runnable wait can
    /// reflect priority and contention rather than useful work.
    pub fn follow(&mut self, mem_kb: u64, used_recent: f64, ncpu: f64, now: f64, long: Option<LongLived>) {
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
        } else if used_recent > self.need_cpu {
            self.need_cpu = used_recent.min(ncpu);
        }
    }
}

impl Entry {
    /// Lower a running job's memory need to `can_reach`, the most its past
    /// runs still reached from its age on (`reach::Envelope::need_at`),
    /// never above `flat`, the need it would book by its peak. Once it holds
    /// more than that, it books `flat` for the rest of its run. A steady
    /// long-lived job and needs set by hand are left alone. Returns true when
    /// the job has just gone above it.
    pub fn reach(&mut self, flat: u64, can_reach: Option<u64>) -> bool {
        self.need_mem_kb = flat;
        self.reach_flat_kb = None;
        if self.reach_missed || self.steady() || self.by_hand.is_some() {
            return false;
        }
        match can_reach {
            Some(r) if self.live_mem_kb > r => {
                self.reach_missed = true;
                true
            }
            Some(r) if r < flat => {
                self.need_mem_kb = r;
                self.reach_flat_kb = Some(flat);
                false
            }
            _ => false,
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
    /// To a long-lived job: work it expects has started (`Expect::started`),
    /// by index.
    pub work_started: Vec<u32>,
}

pub struct Queue {
    pub dir: PathBuf,
}

pub struct Guard {
    _file: File,
}

impl Queue {
    pub fn open(dir: &Path) -> Result<Queue> {
        for d in ["wait", "run", "viewers", "nudge", "pipeline", "stall", "nested"] {
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

    /// A taskguard call nested inside the running job of `outer` (a pid)
    /// leaves `nested/<outer>.<pid>`, so that job learns that it wraps other
    /// taskguard jobs. The note holds the call's job key, for the tree in
    /// `taskguard top`; when it was written is when the call started.
    pub fn note_nested(&self, outer: i32, pid: i32, key: &str) {
        let _ = fs::write(self.dir.join("nested").join(format!("{outer}.{pid}")), key);
    }

    /// Every nested call with a note: the outer job's pid, the call's pid,
    /// when it started, and its key (empty from a note without one).
    pub fn nested(&self) -> Vec<Nested> {
        let Ok(rd) = fs::read_dir(self.dir.join("nested")) else { return Vec::new() };
        rd.filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let (outer, pid) = name.split_once('.')?;
                let since = e.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64();
                Some(Nested {
                    outer: outer.parse().ok()?,
                    pid: pid.parse().ok()?,
                    since,
                    key: fs::read_to_string(e.path()).unwrap_or_default(),
                })
            })
            .collect()
    }

    /// How many nested calls left a note for the job of `owner`. The notes
    /// are removed, so a later job with the same pid starts from none.
    pub fn take_nested(&self, owner: i32) -> u64 {
        let prefix = format!("{owner}.");
        let Ok(rd) = fs::read_dir(self.dir.join("nested")) else { return 0 };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)))
            .filter(|e| fs::remove_file(e.path()).is_ok())
            .count() as u64
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
    /// At the kernel's warning level (under 100), jobs still start while
    /// memory fits, and while no first run still grows. Off: warning stops
    /// every new job, as critical always does.
    pub warn_admit: bool,
    /// A job short of memory by at most this share of RAM (0-100) starts.
    pub noise_mem_pct: f64,
    /// A job short of CPU by at most this many cores starts.
    pub noise_cpu: f64,
    /// A job that only programs outside taskguard keep out starts, while
    /// memory would stay under `outside_mem_max_pct` of RAM.
    pub outside_admit: bool,
    pub outside_mem_max_pct: f64,
    /// A job that cannot fit now (its CPU need is above the whole limit, or
    /// it has waited past `max_bypass`) starts on this share of its CPU need,
    /// when the machine is near its recent low. 0 turns it off.
    pub partial_fit: f64,
    /// A job that only programs outside taskguard keep out may start over RAM
    /// (`Room::overcommit`), while memory would stay under this share of RAM
    /// and the part over RAM fits in half the free swap. At or under 100 it
    /// is off.
    pub overcommit_max_pct: f64,
    /// Seconds the only job in the queue waits before it may start over RAM.
    pub overcommit_after: f64,
    /// Seconds the first job in line waits before it may start over RAM
    /// while other jobs queue too.
    pub overcommit_stuck_after: f64,
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
    /// The kernel reports memory pressure: nothing new starts. At the
    /// warning level with `warn_admit` on (`paced`), jobs still start, but
    /// not while the first run `after` still grows: for at most `next_in`
    /// seconds.
    Pressure {
        level: f64,
        limit: f64,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        paced: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_in: Option<f64>,
    },
    /// A running job is paused for memory: it resumes before anything new starts.
    Paused {
        key: String,
    },
    /// Work for a long-lived job that does not run yet: it waits for it.
    Stack {
        key: String,
    },
    /// A long-lived job waits, for at most `wait_s` more, for the work it
    /// expects to queue.
    Expect {
        work: Vec<String>,
        wait_s: f64,
    },
    /// `limit_pct` is the ceiling the job really waits for: `mem_max`, or
    /// all of RAM for the first in line with nothing of taskguard's
    /// running. `overcommit_in` is set when `Room::overcommit` would let it
    /// start over RAM in that many seconds.
    Memory {
        would_pct: f64,
        limit_pct: f64,
        short_kb: u64,
        used_kb: u64,
        reserve_kb: u64,
        need_kb: u64,
        total_kb: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        overcommit_in: Option<f64>,
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
            Blocker::Stack { .. } => "stack",
            Blocker::Expect { .. } => "expect",
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
        /// Set when the job starts on part of its CPU need (`partial_fit`):
        /// the cores it books instead of its whole need.
        #[serde(skip_serializing_if = "Option::is_none")]
        booked_cpu: Option<f64>,
        /// Memory would go over RAM: it starts by `Room::overcommit`.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        over_ram: bool,
        /// It starts by `Room::overcommit`.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        overcommitted: bool,
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
///
/// A long-lived job also reserves what it books for the work it expects.
pub fn reserve(running: &[Entry]) -> (f64, u64) {
    running.iter().fold((0.0, 0), |(c, m), e| {
        let cpu = if e.paused_by_hand { 0.0 } else { (e.need_cpu - e.live_cpu).max(0.0) };
        (c + cpu, m + e.need_mem_kb.saturating_sub(e.live_mem_kb) + booked(e, running))
    })
}

/// What `e` still books for the work it expects: each item whose work has
/// not started, and that no running job has taken over yet. A waiting job
/// books all of them, so it starts only with room for its work too.
pub fn booked(e: &Entry, running: &[Entry]) -> u64 {
    let taken = |i: usize| {
        e.started_at.is_some() && running.iter().any(|r| r.work_for.as_ref().is_some_and(|w| w.pid == e.pid && w.item == Some(i as u32)))
    };
    e.expects.iter().enumerate().filter(|(i, x)| x.open() && !taken(*i)).map(|(_, x)| x.mem_kb).sum()
}

/// The part of `booked` that only the history guessed (`is_guess`).
fn guessed(e: &Entry, running: &[Entry], waiting: &[Entry]) -> u64 {
    let expects =
        e.expects.iter().enumerate().map(|(i, x)| Expect { started: x.started || !is_guess(e, i, running, waiting), ..x.clone() });
    booked(&Entry { expects: expects.collect(), ..e.clone() }, running)
}

/// The part of `job`'s own booking that it leaves out when it starts: all
/// it guessed, when `job` is a waiting long-lived job with no declared work.
/// Such a stack starts as it would without the history.
pub fn own_guess(job: &Entry, running: &[Entry], waiting: &[Entry]) -> u64 {
    let undeclared = job.long_lived && (0..job.expects.len()).all(|i| is_guess(job, i, running, waiting));
    if undeclared && job.started_at.is_none() { guessed(job, running, waiting) } else { 0 }
}

/// The memory `job` adds when it starts: its need, plus what it books for
/// the work it expects, less what its long-lived job booked for it, which it
/// takes over.
pub fn need_with_booking(job: &Entry, running: &[Entry]) -> u64 {
    let own = if job.started_at.is_none() { booked(job, &[]) } else { 0 };
    (job.need_mem_kb + own).saturating_sub(booking_taken_over(job, running))
}

/// What the long-lived job of `job` books for it now, which `job` takes over
/// when it starts.
fn booking_taken_over(job: &Entry, running: &[Entry]) -> u64 {
    let Some(WorkFor { pid, item: Some(i), .. }) = &job.work_for else { return 0 };
    if job.started_at.is_some() {
        return 0;
    }
    let Some(stack) = running.iter().find(|e| e.pid == *pid) else { return 0 };
    let taken = running.iter().any(|r| r.pid != job.pid && r.work_for.as_ref().is_some_and(|w| w.pid == *pid && w.item == Some(*i)));
    stack.expects.get(*i as usize).filter(|x| x.open() && !taken).map_or(0, |x| x.mem_kb)
}

/// The part of `booking_taken_over` that only the history guessed: none
/// when `job` is among `waiting`, as its link made the guess come true.
fn guess_taken_over(job: &Entry, running: &[Entry], waiting: &[Entry]) -> u64 {
    let Some(WorkFor { pid, item: Some(i), .. }) = &job.work_for else { return 0 };
    let guess = running.iter().find(|e| e.pid == *pid).is_some_and(|e| is_guess(e, *i as usize, running, waiting));
    if guess { booking_taken_over(job, running) } else { 0 }
}

/// Is `job` work for a long-lived job that does not run yet? It waits for
/// it: the work needs it up.
pub fn waits_for_stack(job: &Entry, running: &[Entry]) -> bool {
    job.started_at.is_none() && job.work_for.as_ref().is_some_and(|w| !running.iter().any(|e| e.pid == w.pid))
}

/// The long-lived job in `me`'s checkout that expects it, among `running`
/// and `waiting`, by `matches` (does `me` match this declared work?). A
/// running one comes first, then the item whose work no other job has taken.
/// Returns the link and the long-lived job's priority.
pub fn link_work(me: &Entry, running: &[Entry], waiting: &[Entry], matches: impl Fn(&str) -> bool) -> Option<(WorkFor, i32)> {
    if me.long_lived {
        return None;
    }
    let claimed = |pid: i32, i: usize| {
        running
            .iter()
            .chain(waiting)
            .any(|r| r.pid != me.pid && r.work_for.as_ref().is_some_and(|w| w.pid == pid && w.item == Some(i as u32)))
    };
    let stacks = running.iter().chain(waiting).filter(|e| e.long_lived && e.pid != me.pid && e.checkout == me.checkout);
    let mut best: Option<(u8, WorkFor, i32)> = None;
    for e in stacks {
        for (i, x) in e.expects.iter().enumerate().filter(|(_, x)| matches(&x.work) && still_expected(x)) {
            let free = x.open() && !claimed(e.pid, i);
            // A stack that waits takes only work that can take over a booking.
            if e.started_at.is_none() && !free {
                continue;
            }
            let rank = u8::from(e.started_at.is_none()) * 2 + u8::from(!free);
            if best.as_ref().is_none_or(|(r, _, _)| rank < *r) {
                let item = free.then_some(i as u32);
                best = Some((rank, WorkFor { pid: e.pid, key: e.key.clone(), item }, e.priority));
            }
        }
    }
    best.map(|(_, w, p)| (w, p))
}

/// Is this work still expected: declared, or implied by the history and
/// neither expired nor started? Only then does work link to it and go
/// first. Once the run that made a guess come true has started, a second
/// matching run is not the work the history foresaw: it keeps its own
/// priority.
fn still_expected(x: &Expect) -> bool {
    !x.implied || x.open()
}

/// Has `me`'s link to its long-lived job lapsed: that job is gone, or what
/// it expected of `me` was a guess that has expired or whose work started?
/// The item `me` links to is no guess: `me` made it come true.
pub fn link_lapsed(me: &Entry, running: &[Entry], waiting: &[Entry], matches: impl Fn(&str) -> bool) -> bool {
    let Some(w) = &me.work_for else { return false };
    let expected = |e: &Entry, i: usize| matches(&e.expects[i].work) && (w.item == Some(i as u32) || still_expected(&e.expects[i]));
    running.iter().chain(waiting).find(|e| e.pid == w.pid).is_none_or(|e| !(0..e.expects.len()).any(|i| expected(e, i)))
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
    // First in line, for `overcommit`: work queued for `me` cannot start
    // before it, whatever its place.
    let first = older.iter().all(|w| w.work_for.as_ref().is_some_and(|l| l.pid == me.pid));

    // The kernel's own alarm comes first and holds back every job, even when
    // taskguard runs nothing: then the pressure comes from other programs, and
    // one more job only makes it worse. This cannot deadlock the queue: jobs
    // start again as soon as the pressure eases, and --now still skips it.
    //
    // macOS keeps its warning level for a long time, also with half of memory
    // free: a running dev stack holds it there, and the job that needs that
    // stack waited behind it. So at warning (`warn_admit`), jobs still start,
    // only while memory fits without any leeway. A job with a history has its
    // whole need booked from its first second, so the next job may start at
    // once. A first run books a guess, so nothing new starts until it has
    // shown what it takes (`settling_for`). Critical stops every new job.
    if let Some(p) = m.mem_pressure.filter(|p| *p >= lim.pressure_max) {
        let paced = p < 100.0 && lim.warn_admit;
        let growing = running.iter().filter_map(|e| e.settling_for(now).map(|w| (e, w))).max_by(|a, b| a.1.total_cmp(&b.1));
        let pressure = Blocker::Pressure {
            level: p,
            limit: lim.pressure_max,
            paced,
            after: growing.filter(|_| paced).map(|(e, _)| e.key.clone()),
            next_in: growing.filter(|_| paced).map(|(_, w)| w),
        };
        if !paced || growing.is_some() {
            return Decision::Wait { blockers: vec![pressure] };
        }
        // With only long-lived jobs past their start-up running, the progress
        // rule below decides, up to all of RAM: they do not end by themselves,
        // and the job that needs one of them would wait as long as the
        // warning lasts, which can be all morning.
        if !running.iter().all(Entry::steady) {
            let room = Room::new(m, lim, running, waiting);
            if let Some(mem) =
                room.blockers(m, lim, running, me, now, unknown_starts).into_iter().find(|b| matches!(b, Blocker::Memory { .. }))
                && !matches!(room.overcommit(m, lim, waiting, me, first, now), Overcommit::Now(_))
            {
                return Decision::Wait { blockers: vec![mem, pressure] };
            }
        }
    }

    // A job paused for memory gets its room back before anything new starts.
    if let Some(p) = running.iter().find(|e| e.auto_paused()) {
        return Decision::Wait { blockers: vec![Blocker::Paused { key: p.key.clone() }] };
    }

    // A long-lived job that expects work waits a few seconds for it to
    // queue, unless it already has: its caller may start the work right
    // after it, and then it starts with room for both, and the work goes
    // first, instead of the stack starting alone.
    if let Some(until) = me.expect_until.filter(|u| *u > now && me.started_at.is_none())
        && !waiting.iter().any(|w| w.work_for.as_ref().is_some_and(|l| l.pid == me.pid))
    {
        let work = me.expects.iter().map(|x| x.work.clone()).collect();
        return Decision::Wait { blockers: vec![Blocker::Expect { work, wait_s: until - now }] };
    }

    // Always make progress: with nothing running, the first job in line that
    // fits starts, whatever the CPU reading says and up to all of RAM. This is
    // what keeps the queue from deadlocking on a machine that is busy with
    // work outside taskguard. A long-lived job past its start-up counts as
    // such work: it may run for hours, and a job it keeps out would otherwise
    // hold up the whole queue until it is stopped.
    //
    // A job that does not fit even so holds nobody back: it waits for memory,
    // and the jobs behind it that fit start meanwhile. Next to long-lived
    // jobs, a long-lived job must fit under `mem_max`: it does not end by
    // itself, so starting it over the limit is no progress but load that
    // stays, and a second dev stack started that way kept the first one's
    // run out until it was given up. Only when programs outside taskguard
    // alone keep it out may it go up to the ceiling of `outside_admit`, and
    // with nothing of taskguard's running, up to all of RAM: nothing taskguard
    // runs would free that.
    let room = Room::new(m, lim, running, waiting);
    if running.iter().all(Entry::steady) {
        let fits = |w: &Entry| room.fits_on_progress(m, lim, running, w);
        let head = older.iter().copied().chain(std::iter::once(me)).find(|w| fits(w));
        return match head {
            Some(h) if !h.ahead_of(me) && running.is_empty() => Decision::Admit {
                reason: "nothing else is running".into(),
                early: None,
                booked_cpu: None,
                over_ram: false,
                overcommitted: false,
            },
            Some(h) if !h.ahead_of(me) => Decision::Admit {
                reason: "nothing else is running but long-lived jobs past their start-up".into(),
                early: None,
                booked_cpu: None,
                over_ram: false,
                overcommitted: false,
            },
            Some(o) => Decision::Wait { blockers: vec![Blocker::Older { key: o.key.clone() }] },
            None => {
                let mut blockers: Vec<Blocker> = room
                    .blockers(m, lim, running, me, now, unknown_starts)
                    .into_iter()
                    .filter(|b| matches!(b, Blocker::Slots { .. } | Blocker::Stack { .. }))
                    .collect();
                if blockers.is_empty() || room.mem_would_on(me, true) > room.progress_ceiling(m, lim, me) {
                    blockers.push(room.progress_memory_blocker(m, lim, me));
                }
                if blockers.iter().all(|b| matches!(b, Blocker::Memory { .. })) {
                    match room.overcommit(m, lim, waiting, me, first, now) {
                        Overcommit::Now(why) => return over_committed(m, &room, me, why),
                        Overcommit::In(s) => {
                            if let Some(Blocker::Memory { overcommit_in, .. }) = blockers.first_mut() {
                                *overcommit_in = Some(s);
                            }
                        }
                        Overcommit::No => {}
                    }
                }
                if let Some(p) = m.mem_pressure.filter(|p| *p >= lim.pressure_max) {
                    blockers.push(Blocker::Pressure { level: p, limit: lim.pressure_max, paced: true, after: None, next_in: None });
                }
                Decision::Wait { blockers }
            }
        };
    }

    let mut blockers = Vec::new();
    // With backfill on, a job ahead that goes first holds its turn only while
    // it could start itself: a job that waits for memory cannot use the room
    // it would hold, so newer jobs that fit may start meanwhile. Past
    // `max_backfill` only jobs that should end before it could start anyway
    // still do. They take no room it could use, and a stream of them cannot
    // keep it out: it starts once the running jobs have made room for it, or
    // when none runs, which they do not put off. A job that may start on part
    // of its need (`partial_fit`) waits only for that part.
    //
    // Work queued for a long-lived job is what that job exists for: the
    // long-lived job it waits for, and that work once its long-lived job
    // runs, hold their turn as a strict reservation does, also while they do
    // not fit yet. Otherwise a stream of lints keeps taking the room a stack
    // and its run need together, until the caller gives up on the run.
    let queued_for = |w: &Entry| w.work_for.is_some() || waiting.iter().any(|x| x.work_for.as_ref().is_some_and(|l| l.pid == w.pid));
    let holds = |w: &Entry| {
        if queued_for(w) {
            return true;
        }
        let (mut theirs, _) = room.left(m, lim, running, w, now, unknown_starts);
        if theirs.is_empty() {
            return true;
        }
        if partial_fit_due(lim, w, now, room.cpu_limit) {
            for b in &mut theirs {
                if let Blocker::Cpu { short, .. } = b {
                    *short = short_of_partial_fit(lim, w, *short);
                }
            }
        }
        now - w.queued_at > lim.max_backfill && !me.est_dur_s.zip(soonest_start(&theirs, running, now)).is_some_and(|(d, t)| d <= t)
    };
    // A job ahead that would not fit even once every job that ends by itself
    // has ended can only start when a long-lived job is stopped, or programs
    // outside taskguard free memory. It holds nobody back: holding the jobs
    // behind it that fit gains it nothing, and on Omarchy one such job kept
    // two dozen others out for half an hour.
    let may_hold = |w: &Entry| room.fits_once_drained(m, lim, running, w);
    // A job whose own pool is full cannot start, whatever room it holds:
    // holding others back for it gains nothing.
    let pool_full = |w: &Entry| match (&w.pool_key, w.pool_slots) {
        (Some(pk), Some(max)) => running.iter().filter(|e| e.pool_key.as_ref() == Some(pk)).count() as u32 >= max,
        _ => false,
    };
    // An older job that newer ones have passed for `max_bypass` goes first.
    // Only a job ahead in line holds `me` back, so a job of an older run is
    // never held back for one of a newer run, or for a job that queued after
    // that run began.
    if let Some(r) = older.iter().find(|w| {
        w.bypassed_since.is_some()
            && now - w.queued_at > lim.max_bypass
            && !pool_full(w)
            && may_hold(w)
            && (lim.max_backfill <= lim.max_bypass || holds(w))
    }) {
        blockers.push(Blocker::Reserved { key: r.key.clone(), waited_s: now - r.queued_at });
    }
    // A waiting job with a higher priority goes first, unless its own pool is
    // full. With backfill on, it holds them back as a reservation does.
    if let Some(h) =
        older.iter().find(|w| w.priority > me.priority && !pool_full(w) && may_hold(w) && (lim.max_backfill <= 0.0 || holds(w)))
    {
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
        let (early, booked_cpu) = early.unzip();
        Decision::Admit { reason, early, booked_cpu: booked_cpu.flatten(), over_ram: false, overcommitted: false }
    } else if blockers.iter().all(|b| matches!(b, Blocker::Memory { .. }))
        && let Overcommit::Now(why) = room.overcommit(m, lim, waiting, me, first, now)
    {
        over_committed(m, &room, me, why)
    } else {
        Decision::Wait { blockers }
    }
}

/// The admission of a job that `Room::overcommit` lets start.
fn over_committed(m: &MachineSample, room: &Room, me: &Entry, why: String) -> Decision {
    let over_ram = room.mem_would_on(me, true) > m.mem_total_kb as f64;
    let reason = "only memory keeps it out, and over-commit lets it start (overcommit)";
    Decision::Admit { reason: reason.into(), early: Some(why), booked_cpu: None, over_ram, overcommitted: true }
}

/// In how many seconds a job that `blockers` keep out could start at the
/// soonest, by the learned durations of the running jobs: once they have
/// freed what it lacks, or else once none of them runs but long-lived jobs
/// past their start-up, as then the first in line that fits starts whatever
/// the CPU reading says. None when that hangs on a job whose duration is
/// unknown.
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

/// The most memory a job may take the machine to over RAM: `overcommit_max`
/// of RAM, and no more than RAM plus half the free swap. Half, so the swap
/// keeps room for the pages the rest of the machine moves out as it fills.
/// With no swap (or none read) nothing goes over RAM.
pub fn overcommit_ceiling(m: &MachineSample, lim: &Limits) -> f64 {
    let total = m.mem_total_kb as f64;
    (total * lim.overcommit_max_pct / 100.0).min(total + m.swap_free_kb.unwrap_or(0) as f64 / 2.0)
}

/// Would `me` take memory over RAM if it started now? A job started so,
/// by any rule or by hand, is made the OOM killer's first pick on Linux.
pub fn over_ram(m: &MachineSample, lim: &Limits, running: &[Entry], waiting: &[Entry], me: &Entry) -> bool {
    Room::new(m, lim, running, waiting).mem_would_on(me, true) > m.mem_total_kb as f64
}

/// What `Room::overcommit` says of a job that only memory keeps out.
enum Overcommit {
    /// It may start over RAM now, for this reason.
    Now(String),
    /// It may, once it has waited this many seconds more.
    In(f64),
    No,
}

/// The room the running jobs leave, read once per decision.
#[derive(Clone)]
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
    /// What the running jobs that end by themselves hold, and still
    /// reserve: the memory that frees up once they have ended.
    passing_live: u64,
    passing_res: u64,
    /// The running jobs: what a job takes over from its long-lived job
    /// depends on them.
    running: Vec<Entry>,
    /// The waiting jobs linked to a long-lived job: what is still a guess
    /// depends on them.
    linked: Vec<Entry>,
    /// The part of `res_mem` that only the history guessed: what long-lived
    /// jobs book for work nobody declared and no job has queued for yet
    /// (`is_guess`).
    res_guessed: u64,
}

impl Room {
    fn new(m: &MachineSample, lim: &Limits, running: &[Entry], waiting: &[Entry]) -> Room {
        let (res_cpu, res_mem) = reserve(running);
        let linked: Vec<Entry> = waiting.iter().filter(|w| w.work_for.is_some()).cloned().collect();
        let ours_mem = running.iter().map(|e| e.live_mem_kb).sum();
        let passing: Vec<Entry> = running.iter().filter(|e| !e.lasting()).cloned().collect();
        Room {
            res_cpu,
            res_mem,
            promised_cpu: promised_cpu(running),
            cpu_limit: cpu_limit(m, lim),
            mem_limit: m.mem_total_kb as f64 * lim.mem_max_pct / 100.0,
            mem_used: m.mem_for_admission(ours_mem),
            ours_mem,
            passing_live: passing.iter().map(|e| e.live_mem_kb).sum(),
            passing_res: reserve(&passing).1,
            running: running.to_vec(),
            res_guessed: running.iter().map(|e| guessed(e, running, &linked)).sum(),
            linked,
        }
    }

    fn mem_would(&self, job: &Entry) -> f64 {
        self.mem_would_on(job, false)
    }

    /// As `mem_would`, by the progress rule (`progress`) or not. A guess of
    /// the history never keeps a job that ends by itself out by the progress
    /// rule, so it cannot stop the queue; the part of it that job takes over
    /// counts once. A long-lived job respects the guesses of others while
    /// they are open, but one started with no declared work leaves its own
    /// guess out: such a stack starts as it would without the history, and
    /// books its guess from then on.
    fn mem_would_on(&self, job: &Entry, progress: bool) -> f64 {
        let guess = if progress && !job.long_lived {
            self.res_guessed.saturating_sub(guess_taken_over(job, &self.running, &self.linked))
        } else {
            0
        };
        let own = own_guess(job, &self.running, &self.linked);
        (self.mem_used + self.res_mem - guess.min(self.res_mem) + need_with_booking(job, &self.running) - own) as f64
    }

    /// The most memory a job may take the machine to by the progress rule:
    /// all of RAM, as nothing else of taskguard's would come after it. A
    /// long-lived job, which stays, next to long-lived jobs of taskguard's,
    /// only `mem_max` (with `noise_mem`), or when only programs outside
    /// taskguard keep it out, the ceiling of `outside_admit`. With nothing of
    /// taskguard's running, it too goes up to all of RAM: nothing taskguard
    /// runs would free the rest.
    fn progress_ceiling(&self, m: &MachineSample, lim: &Limits, job: &Entry) -> f64 {
        let total = m.mem_total_kb as f64;
        if !job.long_lived || self.running.is_empty() {
            return total;
        }
        let ceiling = self.mem_limit + total * lim.noise_mem_pct / 100.0;
        let ours = self.mem_would_on(job, true) - self.mem_used.saturating_sub(self.ours_mem) as f64;
        if lim.outside_admit && ours <= self.mem_limit { ceiling.max(total * lim.outside_mem_max_pct / 100.0).min(total) } else { ceiling }
    }

    /// May `job` start by the progress rule now: its pool has a free slot
    /// and it fits under `progress_ceiling`.
    fn fits_on_progress(&self, m: &MachineSample, lim: &Limits, running: &[Entry], job: &Entry) -> bool {
        let pool_free = match (&job.pool_key, job.pool_slots) {
            (Some(pk), Some(max)) => (running.iter().filter(|e| e.pool_key.as_ref() == Some(pk)).count() as u32) < max,
            _ => true,
        };
        pool_free && !waits_for_stack(job, running) && self.mem_would_on(job, true) <= self.progress_ceiling(m, lim, job)
    }

    /// Would `job` fit by the progress rule once every running job that ends
    /// by itself has ended? If not, it starts only when a long-lived job is
    /// stopped or programs outside taskguard free memory.
    fn fits_once_drained(&self, m: &MachineSample, lim: &Limits, running: &[Entry], job: &Entry) -> bool {
        let lasting: Vec<Entry> = running.iter().filter(|e| e.lasting()).cloned().collect();
        let drained = Room {
            mem_used: self.mem_used.saturating_sub(self.passing_live),
            ours_mem: self.ours_mem.saturating_sub(self.passing_live),
            res_mem: self.res_mem.saturating_sub(self.passing_res),
            running: lasting.clone(),
            ..self.clone()
        };
        drained.fits_on_progress(m, lim, &lasting, job)
    }

    /// The memory blocker for `job`, whether or not it fits under the limit.
    fn memory_blocker(&self, m: &MachineSample, lim: &Limits, job: &Entry) -> Blocker {
        self.memory_blocker_at(m, job, self.mem_would(job), self.mem_limit, lim.mem_max_pct)
    }

    /// The memory blocker for `job` by the progress rule: measured against
    /// the ceiling that rule really holds it to, all of RAM when nothing of
    /// taskguard's runs, not `mem_max`.
    fn progress_memory_blocker(&self, m: &MachineSample, lim: &Limits, job: &Entry) -> Blocker {
        let ceiling = self.progress_ceiling(m, lim, job);
        self.memory_blocker_at(m, job, self.mem_would_on(job, true), ceiling, pct(ceiling, m.mem_total_kb as f64))
    }

    fn memory_blocker_at(&self, m: &MachineSample, job: &Entry, mem_would: f64, ceiling: f64, limit_pct: f64) -> Blocker {
        Blocker::Memory {
            would_pct: pct(mem_would, m.mem_total_kb as f64),
            limit_pct,
            short_kb: (mem_would - ceiling).max(0.0) as u64,
            used_kb: self.mem_used,
            reserve_kb: self.res_mem,
            need_kb: need_with_booking(job, &self.running),
            total_kb: m.mem_total_kb,
            overcommit_in: None,
        }
    }

    /// May `job`, which only memory keeps out, start over RAM? macOS
    /// compresses and swaps cold memory, so a job over RAM costs speed, not
    /// processes; Linux has only its swap, and the OOM killer when that is
    /// gone. So it may, never at critical pressure (on Linux, never at
    /// `pressure_max`), while memory would stay under `overcommit_max` of
    /// RAM and the part over RAM fits in half the free swap
    /// (`overcommit_ceiling`), when either:
    /// - nothing of taskguard's runs but the long-lived job it is work for,
    ///   and it is first in line (`head`): it has waited `overcommit_after`
    ///   as the only job in the queue, or `overcommit_stuck_after` with
    ///   other jobs queued behind it. Then only programs outside taskguard
    ///   keep it out, and nothing taskguard runs would ever free the room;
    /// - it takes over what its running long-lived job declared and booked
    ///   for it, adds no more than `noise_mem` to that, and that long-lived
    ///   job itself started by this rule (`overcommitted`): it started over
    ///   RAM with that room booked, so its work starts at once. A booking
    ///   the history guessed was never part of that start.
    fn overcommit(&self, m: &MachineSample, lim: &Limits, waiting: &[Entry], job: &Entry, head: bool, now: f64) -> Overcommit {
        let critical = m.mem_pressure.is_some_and(|p| p >= 100.0 || (!lim.warn_admit && p >= lim.pressure_max));
        if lim.overcommit_max_pct <= 100.0 || critical {
            return Overcommit::No;
        }
        let total = m.mem_total_kb as f64;
        let would = self.mem_would_on(job, true);
        let ceiling = overcommit_ceiling(m, lim);
        if would > ceiling {
            return Overcommit::No;
        }
        let gb = |kb: f64| kb / (1024.0 * 1024.0);
        let fit = format!(
            "memory would reach {:.0}% of RAM, under overcommit_max {:.0}%, {:.1} GB into swap with {:.1} GB free",
            pct(would, total),
            lim.overcommit_max_pct,
            gb((would - total).max(0.0)),
            gb(m.swap_free_kb.unwrap_or(0) as f64)
        );
        let stack = job.work_for.as_ref().and_then(|w| self.running.iter().find(|e| e.pid == w.pid));
        let item = job.work_for.as_ref().and_then(|w| w.item).zip(stack).and_then(|(i, s)| s.expects.get(i as usize));
        let declared = item.is_some_and(|x| !x.implied);
        let taken = booking_taken_over(job, &self.running);
        if taken > 0
            && declared
            && stack.is_some_and(|s| s.overcommitted)
            && need_with_booking(job, &self.running) as f64 <= total * lim.noise_mem_pct / 100.0
        {
            let key = job.work_for.as_ref().map(|w| w.key.as_str()).unwrap_or_default();
            return Overcommit::Now(format!(
                "it takes over the {:.1} GB {key} declared for it, and {key} started over RAM with that room by over-commit ({fit})",
                gb(taken as f64)
            ));
        }
        if self.running.iter().any(|e| stack.is_none_or(|s| s.pid != e.pid)) || !head {
            return Overcommit::No;
        }
        let alone = !waiting.iter().any(|w| w.pid != job.pid && !w.stalled && w.work_for.as_ref().is_none_or(|l| l.pid != job.pid));
        let after = if alone { lim.overcommit_after } else { lim.overcommit_stuck_after };
        let waited = now - job.queued_at;
        if waited < after {
            return Overcommit::In(after - waited);
        }
        Overcommit::Now(format!(
            "only programs outside taskguard keep it out, and it waited {} {} ({fit})",
            crate::report::dur(waited),
            if alone { "as the only job in the queue (overcommit_after)" } else { "first in line (overcommit_stuck_after)" }
        ))
    }

    /// What keeps `job` from starting, after the leeway: a job that falls
    /// short only by a little, or only because of programs outside taskguard,
    /// or that cannot fit now and gets its `partial_fit`, starts anyway. Then
    /// the second value says what it lacks and why it starts, for the job's
    /// warning line, and for a partial fit the cores it books.
    fn left(
        &self,
        m: &MachineSample,
        lim: &Limits,
        running: &[Entry],
        job: &Entry,
        now: f64,
        unknown_starts: &[f64],
    ) -> (Vec<Blocker>, Option<(String, Option<f64>)>) {
        let blockers = self.blockers(m, lim, running, job, now, unknown_starts);
        match self.early(m, lim, job, &blockers, now) {
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
    ///
    /// A job that cannot fit now (`partial_fit_due`) and lacks CPU may also
    /// start on part of its need, `partial_fit` of it at least, when the
    /// machine is near its recent low: too little CPU only makes a job
    /// slower, and a quiet moment gives it the best chance to finish well.
    /// It then books the cores that were free, not its whole need, so it
    /// does not hold every other job out while it runs.
    fn early(&self, m: &MachineSample, lim: &Limits, job: &Entry, blockers: &[Blocker], now: f64) -> Option<(String, Option<f64>)> {
        if blockers.is_empty() || !blockers.iter().all(|b| matches!(b, Blocker::Memory { .. } | Blocker::Cpu { .. })) {
            return None;
        }
        let total = m.mem_total_kb as f64;
        let gb = |kb: f64| kb / (1024.0 * 1024.0);
        let mut why = Vec::new();
        let mut booked = None;
        for b in blockers {
            match *b {
                Blocker::Memory { short_kb, .. } => {
                    let would = self.mem_would(job);
                    if would > total * lim.outside_mem_max_pct / 100.0 {
                        return None;
                    }
                    let outside = self.mem_used.saturating_sub(self.ours_mem);
                    let ours = (self.ours_mem + self.res_mem + need_with_booking(job, &self.running)) as f64;
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
                    } else if partial_fit_due(lim, job, now, self.cpu_limit)
                        && short_of_partial_fit(lim, job, short) <= 1e-9
                        && m.near_cpu_low()
                    {
                        let free = job.need_cpu - short;
                        booked = Some(free);
                        why.push(format!(
                            "CPU short by {short:.1} cores; it gets {free:.1} of the {:.1} it needs, at least partial_fit {:.0}%, with the machine near its recent low of {:.1} busy cores",
                            job.need_cpu,
                            lim.partial_fit * 100.0,
                            m.cpu_low()
                        ));
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        Some((why.join("; "), booked))
    }

    /// What keeps `job` itself from starting now, apart from the jobs ahead
    /// of it in line.
    fn blockers(&self, m: &MachineSample, lim: &Limits, running: &[Entry], job: &Entry, now: f64, unknown_starts: &[f64]) -> Vec<Blocker> {
        let mut blockers = Vec::new();
        // Work for a long-lived job that does not run yet needs it up first.
        if waits_for_stack(job, running) {
            blockers.push(Blocker::Stack { key: job.work_for.as_ref().map(|w| w.key.clone()).unwrap_or_default() });
        }
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
        if self.mem_would(job) > self.mem_limit {
            blockers.push(self.memory_blocker(m, lim, job));
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

/// The cores the CPU limit allows on this machine.
pub fn cpu_limit(m: &MachineSample, lim: &Limits) -> f64 {
    m.ncpu as f64 * lim.cpu_max_pct / 100.0
}

/// Does `job` need more cores than the whole CPU limit, so it never fits?
pub fn never_fits(job: &Entry, cpu_limit: f64) -> bool {
    job.need_cpu > cpu_limit + 1e-9
}

/// Can `job` not fit now? It never fits, or it has waited past `max_bypass`,
/// so it is the job others are held for.
pub fn cannot_fit_now(lim: &Limits, job: &Entry, now: f64, cpu_limit: f64) -> bool {
    never_fits(job, cpu_limit) || now - job.queued_at > lim.max_bypass
}

/// May `job` start on part of its CPU need? Only when partial fit is on, the
/// job is not short (short jobs skip the CPU reading anyway), and it cannot
/// fit now.
pub fn partial_fit_due(lim: &Limits, job: &Entry, now: f64, cpu_limit: f64) -> bool {
    lim.partial_fit > 0.0 && !short(lim, job) && cannot_fit_now(lim, job, now, cpu_limit)
}

/// The CPU a job of `need` cores books when it starts on decision `d`. A
/// job that fits books its need. A partial fit books the cores it got. A job
/// that CPU keeps out and starts anyway (by hand, or at a `--st` timeout)
/// books only the cores that were free: it runs on what it gets, and booking
/// its whole need would hold every other job out while it runs. From there
/// it follows what it uses.
pub fn cpu_to_book(d: &Decision, need: f64) -> f64 {
    match d {
        Decision::Admit { booked_cpu: Some(b), .. } => *b,
        Decision::Admit { .. } => need,
        Decision::Wait { blockers } => blockers
            .iter()
            .find_map(|b| match *b {
                Blocker::Cpu { limit, busy, reserve, .. } => Some((limit - busy - reserve).clamp(0.0, need)),
                _ => None,
            })
            .unwrap_or(need),
    }
}

/// How many cores `job` still lacks to start on its `partial_fit` share, when
/// it lacks `short` of its whole need.
pub fn short_of_partial_fit(lim: &Limits, job: &Entry, short: f64) -> f64 {
    (short - (1.0 - lim.partial_fit) * job.need_cpu).max(0.0)
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
        warn_admit: false,
        noise_mem_pct: 0.0,
        noise_cpu: 0.0,
        outside_admit: false,
        outside_mem_max_pct: 95.0,
        partial_fit: 0.0,
        overcommit_max_pct: 0.0,
        overcommit_after: 120.0,
        overcommit_stuck_after: 1800.0,
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
        assert!(!older.long_lived && older.expects.is_empty() && older.work_for.is_none());
        // What this version adds reads back, and leaves the rest alone.
        let stack = Entry {
            long_lived: true,
            expects: vec![Expect {
                work: "browser".into(),
                mem_kb: 4,
                from: "x".into(),
                started: true,
                implied: true,
                expire_after_s: Some(600.0),
                expired: true,
            }],
            expect_until: Some(5.0),
            work_for: Some(WorkFor { pid: 9, key: "s".into(), item: Some(0) }),
            ..older
        };
        let back: Entry = serde_json::from_str(&serde_json::to_string(&stack).unwrap()).unwrap();
        assert_eq!(back, stack);
        let nudge: Nudge = serde_json::from_str(r#"{"start":false,"work_started":[0]}"#).unwrap();
        assert_eq!(nudge.work_started, vec![0]);
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
    fn nothing_running_admits_the_first_in_line_that_fits_in_ram() {
        // Whatever the CPU reading says, and past mem_max: 20 GB held and 10
        // GB more is 94% of 32 GB, over the 85% limit, under all of RAM.
        let old = job(1, "old", 64.0, 10);
        assert!(blockers(decide(&machine(12.0, 20), &LIM, &[], std::slice::from_ref(&old), &old, 1000.0, &[])).is_empty());
        let young = job(2, "young", 1.0, 1);
        assert_eq!(blockers(decide(&machine(0.0, 1), &LIM, &[], &[old.clone(), young.clone()], &young, 1000.0, &[])), vec!["order"]);
        // A job that would not fit in all of RAM waits for memory, and holds
        // nobody back: the young job that fits starts.
        let big = job(1, "big", 64.0, 500);
        assert_eq!(blockers(decide(&machine(12.0, 31), &LIM, &[], std::slice::from_ref(&big), &big, 1000.0, &[])), vec!["memory"]);
        assert!(blockers(decide(&machine(0.0, 1), &LIM, &[], &[big.clone(), young.clone()], &young, 1000.0, &[])).is_empty());
    }

    /// Three past runs of a `ci` job on a 32 GB machine: 2 GB, an 18 GB peak
    /// from 40 s to 70 s, then 2 GB again until it ends at 300 s.
    fn ci_envelope() -> crate::reach::Envelope {
        let run = |shift: f64| -> Vec<(f64, u64)> {
            (0..=150)
                .map(|i| i as f64 * 2.0)
                .map(|t| (t, if (40.0 + shift..=70.0 + shift).contains(&t) { 18 * GB } else { 2 * GB }))
                .collect()
        };
        let curves = [0.0, 6.0, -6.0].iter().map(|s| crate::reach::Curve::from_samples(&run(*s), 300.0).unwrap()).collect();
        crate::reach::Envelope::of(curves, 3).unwrap()
    }

    /// #7: a job past its peak books what its past runs still reached from
    /// its age on, not its whole peak, so a waiting job starts. Before its
    /// peak, and after it went above the curve, it books its whole need.
    #[test]
    fn a_job_past_its_peak_reserves_only_what_it_can_still_reach() {
        let env = ci_envelope();
        let mut ci = Entry { started_at: Some(1000.0), ..job(1, "ci", 2.0, 18) };
        let test = job(2, "test", 1.0, 8);
        let at = |ci: &mut Entry, age: f64, live_gb: u64| {
            ci.live_mem_kb = live_gb * GB;
            let flat = ci.reach_flat_kb.unwrap_or(ci.need_mem_kb);
            ci.reach(flat, env.need_at(age))
        };
        let waits = |ci: &Entry| {
            blockers(decide(&machine(1.0, 6), &LIM, std::slice::from_ref(ci), std::slice::from_ref(&test), &test, 1000.0, &[]))
        };
        // At 20 s its peak is still to come: 6 + 16 reserved + 8 > 27.2 GB.
        assert!(!at(&mut ci, 20.0, 2));
        assert_eq!(ci.need_mem_kb, 18 * GB);
        assert_eq!(waits(&ci), vec!["memory"]);
        // At 120 s its past runs reached 2 GB at most: it books 2.3 GB.
        assert!(!at(&mut ci, 120.0, 2));
        assert_eq!((ci.need_mem_kb, ci.reach_flat_kb), ((2.0 * GB as f64 * crate::reach::MARGIN) as u64, Some(18 * GB)));
        assert_eq!(waits(&ci), Vec::<&str>::new(), "the test starts");
        // It goes above that curve: its whole need again, for the rest of the run.
        assert!(at(&mut ci, 140.0, 5), "it just went above the curve");
        assert_eq!((ci.need_mem_kb, ci.reach_flat_kb, ci.reach_missed), (18 * GB, None, true));
        assert!(!at(&mut ci, 200.0, 2));
        assert_eq!(ci.need_mem_kb, 18 * GB);
        // Past the end of its past runs, too.
        let mut late = Entry { started_at: Some(1000.0), ..job(1, "ci", 2.0, 18) };
        at(&mut late, 400.0, 2);
        assert_eq!(late.need_mem_kb, 18 * GB);
    }

    #[test]
    fn a_steady_stack_and_needs_set_by_hand_ignore_the_curve() {
        let env = ci_envelope();
        let mut by_hand = Entry { started_at: Some(1000.0), by_hand: Some("set by hand".into()), ..job(1, "ci", 2.0, 18) };
        by_hand.reach(18 * GB, env.need_at(120.0));
        assert_eq!(by_hand.need_mem_kb, 18 * GB);
        let mut steady = Entry { started_at: Some(1000.0), steady_since: Some(1300.0), ..job(1, "stack", 2.0, 5) };
        steady.reach(5 * GB, env.need_at(120.0));
        assert_eq!(steady.need_mem_kb, 5 * GB, "a steady stack books what it holds plus a quarter, as before");
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

    fn booked(d: &Decision) -> Option<f64> {
        match d {
            Decision::Admit { booked_cpu, .. } => *booked_cpu,
            Decision::Wait { .. } => None,
        }
    }

    /// A typecheck that wants 14 cores on a 12-core limit can never fully
    /// fit. With `partial_fit`, it starts once it can get half of that while
    /// the machine is about as quiet as it gets, and books what it got.
    #[test]
    fn a_job_that_can_never_fully_fit_starts_on_part_of_its_need() {
        let lim = Limits { partial_fit: 0.5, ..LIM };
        let mut lint = job(1, "lint", 4.0, 2);
        lint.started_at = Some(990.0);
        lint.live_cpu = 4.0;
        let r = std::slice::from_ref(&lint);
        let head = job(2, "typecheck", 14.0, 2);
        let w = std::slice::from_ref(&head);
        // 12 - 5 busy = 7 cores free: half of 14.
        let d = decide(&machine(5.0, 4), &lim, r, w, &head, 1010.0, &[]);
        assert!(early(d.clone()).is_some_and(|e| e.contains("partial_fit")), "{d:?}");
        assert_eq!(booked(&d), Some(7.0), "it books the room it started on, not 14 cores");
        // Only 5 free: too little.
        assert_eq!(blockers(decide(&machine(7.0, 4), &lim, r, w, &head, 1010.0, &[])), vec!["cpu"]);
        // 7 free, but the machine was at 1 core a minute ago: wait for quiet.
        let busy = MachineSample { recent_cpu: vec![(-60.0, 1.0)], ..machine(5.0, 4) };
        assert_eq!(blockers(decide(&busy, &lim, r, w, &head, 1010.0, &[])), vec!["cpu"]);
        // Memory stays a hard rule.
        let big = job(2, "typecheck", 14.0, 30);
        assert_eq!(blockers(decide(&machine(5.0, 4), &lim, r, std::slice::from_ref(&big), &big, 1010.0, &[])), vec!["memory", "cpu"]);
        // partial_fit = 0 turns it off.
        assert_eq!(blockers(decide(&machine(5.0, 4), &LIM, r, w, &head, 1010.0, &[])), vec!["cpu"]);
    }

    /// A start by hand (`taskguard start`, `g` in top) of a job that cannot
    /// fit books the cores that were free, not its whole need: it runs on the
    /// cores it gets, and the jobs behind it keep moving.
    #[test]
    fn a_job_started_by_hand_books_the_free_cores_and_keeps_no_job_out() {
        let mut lint = job(1, "lint", 4.0, 2);
        lint.started_at = Some(990.0);
        lint.live_cpu = 4.0;
        let head = job(2, "typecheck", 30.0, 2);
        let d = decide(&machine(5.0, 4), &LIM, std::slice::from_ref(&lint), std::slice::from_ref(&head), &head, 1010.0, &[]);
        assert_eq!(blockers(d.clone()), vec!["cpu"]);
        // 12 cores - 5 busy = 7 free.
        assert_eq!(cpu_to_book(&d, head.need_cpu), 7.0);

        // Running on those 7 cores, it leaves room for a 1-core test.
        let small = job(3, "test", 1.0, 1);
        let mut started = Entry { need_cpu: 7.0, started_at: Some(1010.0), live_cpu: 7.0, ..head.clone() };
        let ran =
            |s: &Entry| decide(&machine(11.0, 4), &LIM, &[lint.clone(), s.clone()], std::slice::from_ref(&small), &small, 1020.0, &[]);
        assert!(blockers(ran(&started)).is_empty());
        // Had it booked its whole need, nothing could start beside it.
        started.need_cpu = 30.0;
        assert_eq!(blockers(ran(&started)), vec!["cpu"]);

        // A job that fits books its need; a partial fit books what it got.
        let fits = decide(&machine(1.0, 4), &LIM, &[], std::slice::from_ref(&small), &small, 1010.0, &[]);
        assert_eq!(cpu_to_book(&fits, 1.0), 1.0);
        let partial = Decision::Admit { reason: String::new(), early: None, booked_cpu: Some(6.0), over_ram: false, overcommitted: false };
        assert_eq!(cpu_to_book(&partial, 14.0), 6.0);
    }

    #[test]
    fn a_job_that_fits_later_gets_a_partial_fit_only_once_it_waited_max_bypass() {
        let lim = Limits { partial_fit: 0.5, ..LIM };
        let mut lint = job(1, "lint", 6.0, 2);
        lint.started_at = Some(990.0);
        lint.live_cpu = 6.0;
        let r = std::slice::from_ref(&lint);
        let head = job(2, "typecheck", 8.0, 2);
        let w = std::slice::from_ref(&head);
        // 12 - 6 = 6 free of 8: it fits once the lint ends, so it waits.
        assert_eq!(blockers(decide(&machine(6.0, 4), &lim, r, w, &head, 1050.0, &[])), vec!["cpu"]);
        // Past max_bypass (120 s) it takes the 6 cores it can get.
        let d = decide(&machine(6.0, 4), &lim, r, w, &head, 1121.0, &[]);
        assert_eq!(booked(&d), Some(6.0), "{d:?}");
    }

    /// A job whose CPU need is above the whole limit never "could start":
    /// no running job frees enough. Past `max_backfill` only jobs that end
    /// before the running ones do still pass, and once none runs, it starts.
    #[test]
    fn the_drain_ends_with_a_job_bigger_than_the_whole_limit_running() {
        let lim = Limits { max_backfill: 600.0, ..LIM };
        let mut lint = job(1, "lint", 4.0, 2);
        lint.started_at = Some(990.0);
        lint.live_cpu = 4.0;
        lint.est_dur_s = Some(1000.0);
        let mut head = job(2, "typecheck", 14.0, 2);
        head.bypassed_since = Some(1001.0);
        let quick = Entry { est_dur_s: Some(30.0), ..job(3, "test-a", 1.0, 1) };
        let slow = Entry { est_dur_s: Some(600.0), ..job(4, "test-b", 1.0, 1) };
        let waiting = [head.clone(), quick.clone(), slow.clone()];
        let r = std::slice::from_ref(&lint);
        let m = machine(5.0, 4);
        // Within max_backfill both small jobs pass the head.
        assert!(blockers(decide(&m, &lim, r, &waiting, &slow, 1300.0, &[])).is_empty());
        // Past it, the lint has 290 s left: the 30 s job still passes, the
        // 600 s one waits for the head.
        assert!(blockers(decide(&m, &lim, r, &waiting, &quick, 1700.0, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &lim, r, &waiting, &slow, 1700.0, &[])), vec!["reserved"]);
        assert_eq!(blockers(decide(&m, &lim, r, &waiting, &head, 1700.0, &[])), vec!["cpu"]);
        // The lint ends: the head starts, though it needs more than the limit.
        assert!(blockers(decide(&m, &lim, &[], &waiting, &head, 2000.0, &[])).is_empty());
    }

    /// With partial fit, the head can start once half its need is free, so
    /// past `max_backfill` a newer job only passes when it ends before that.
    #[test]
    fn past_max_backfill_the_drain_counts_on_the_partial_fit() {
        let lim = Limits { max_backfill: 600.0, partial_fit: 0.5, ..LIM };
        let mut lint = Entry { est_dur_s: Some(1000.0), ..job(1, "lint", 4.0, 2) };
        lint.started_at = Some(990.0);
        lint.live_cpu = 4.0;
        let mut long = Entry { est_dur_s: Some(5000.0), ..job(5, "e2e", 4.0, 2) };
        long.started_at = Some(990.0);
        long.live_cpu = 4.0;
        let mut head = job(2, "typecheck", 14.0, 2);
        head.bypassed_since = Some(1001.0);
        let slow = Entry { est_dur_s: Some(600.0), ..job(4, "test-b", 1.0, 1) };
        let waiting = [head.clone(), slow.clone()];
        let r = [lint, long];
        // 9 busy: 3 free of the 7 the head needs to start. Once the lint ends
        // (290 s) it has 7: the 600 s job would still run then, so it waits,
        // though the e2e job runs for over an hour.
        let m = machine(9.0, 4);
        assert_eq!(blockers(decide(&m, &lim, &r, &waiting, &slow, 1700.0, &[])), vec!["reserved"]);
        let quick = Entry { est_dur_s: Some(200.0), ..slow.clone() };
        let waiting = [head, quick.clone()];
        assert!(blockers(decide(&m, &lim, &r, &waiting, &quick, 1700.0, &[])).is_empty());
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
        let mut urgent = job(3, "urgent", 1.0, 20);
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
        let mut big = job(2, "big", 2.0, 20);
        big.bypassed_since = Some(1010.0);
        let small = job(3, "small", 1.0, 1);
        let waiting = [big.clone(), small.clone()];
        // Not yet past max_bypass.
        assert!(blockers(decide(&machine(1.0, 8), &LIM, std::slice::from_ref(&running), &waiting, &small, 1100.0, &[])).is_empty());
        // Past it: nobody newer may start.
        assert_eq!(
            blockers(decide(&machine(1.0, 8), &LIM, std::slice::from_ref(&running), &waiting, &small, 1200.0, &[])),
            vec!["reserved"]
        );
        // A job that would not fit in RAM even once r has ended (8 GB held by
        // other programs and 30 GB more) waits for them, not for r: it holds
        // nobody back.
        let huge = Entry { need_mem_kb: 30 * GB, ..big };
        let waiting = [huge, small.clone()];
        assert!(blockers(decide(&machine(1.0, 8), &LIM, &[running], &waiting, &small, 1200.0, &[])).is_empty());
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
        s.follow(2 * GB, 11.0, 12.0, 1010.0, Some(STACK));
        s.follow(3 * GB, 0.2, 12.0, 1200.0, Some(STACK));
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
        s.follow(5 * GB, 11.0, 12.0, 1100.0, Some(STACK));
        // 300 s in, its start-up is over. It holds 3 GB and wants 0.3 cores.
        s.follow(3 * GB, 0.2, 12.0, 1300.0, Some(STACK));
        assert_eq!(s.steady_since, Some(1300.0));
        assert_eq!((s.need_cpu, s.need_mem_kb), (0.25, 3 * GB + 3 * GB / 4));
        (s.live_cpu, s.live_mem_kb) = (0.2, 3 * GB);
        // The build that waited for the start-up peak now starts.
        let build = job(3, "build", 3.0, 2);
        let r = std::slice::from_ref(&s);
        assert!(blockers(decide(&machine(0.5, 4), &LIM, r, std::slice::from_ref(&build), &build, 1300.0, &[])).is_empty());

        // A test run against the stack raises its CPU need, which falls back after.
        s.follow(3 * GB, 6.0, 12.0, 1400.0, Some(STACK));
        assert_eq!(s.need_cpu, 7.5);
        s.follow(3 * GB, 0.2, 12.0, 1420.0, Some(STACK));
        assert_eq!(s.need_cpu, 0.25);
        // Its steady memory booking follows current RSS and can fall.
        s.follow(4 * GB, 0.3, 12.0, 1440.0, Some(STACK));
        s.follow(2 * GB, 0.3, 12.0, 1460.0, Some(STACK));
        assert_eq!(s.need_mem_kb, 2 * GB + (2 * GB) / 4);

        // What its past runs grew to after their start-up is not booked:
        // only what it holds now, plus a quarter.
        let mut again = Entry { started_at: Some(1000.0), ..stack() };
        again.follow(3 * GB, 0.2, 12.0, 1300.0, Some(LongLived { steady_mem_kb: Some(5 * GB), ..STACK }));
        assert_eq!(again.need_mem_kb, 3 * GB + (3 * GB) / 4);

        // An ordinary job keeps its peak needs for its whole run.
        let mut compile = Entry { started_at: Some(1000.0), ..job(4, "compile", 2.0, 6) };
        compile.follow(3 * GB, 11.0, 12.0, 1010.0, None);
        compile.follow(3 * GB, 0.2, 12.0, 5000.0, None);
        assert_eq!((compile.need_cpu, compile.need_mem_kb, compile.steady_since), (11.0, 6 * GB, None));
    }

    #[test]
    fn a_running_job_books_the_cores_it_uses_not_those_it_waits_for() {
        // tsgo on a busy machine: 4 cores used, 17 wanted.
        let mut tsgo = Entry { started_at: Some(1000.0), ..job(4, "tsgo", 2.0, 2) };
        tsgo.follow(GB, 4.0, 18.0, 1010.0, None);
        assert_eq!(tsgo.need_cpu, 4.0);
    }

    #[test]
    fn steady_cpu_ignores_runnable_wait_and_stays_under_host_capacity() {
        let mut s = Entry { started_at: Some(1000.0), ..stack() };
        s.follow(3 * GB, 0.5, 18.0, 1300.0, Some(STACK));
        assert_eq!(s.steady_since, Some(1300.0));
        assert!((s.need_cpu - 0.625).abs() < f64::EPSILON, "{}", s.need_cpu);
        s.follow(3 * GB, 100.0, 18.0, 1400.0, Some(STACK));
        assert_eq!(s.need_cpu, 18.0);
    }

    /// A reserved job that a stack keeps out: the stack never ends by itself,
    /// so nothing counts on it to make room. It counts as load from outside
    /// taskguard: once only stacks run, the first in line that fits in RAM
    /// starts, and past `max_backfill` a newer job passes only when it ends
    /// before that.
    #[test]
    fn backfill_never_waits_for_a_long_lived_job_to_end() {
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        // Two stacks past their start-up hold 7 GB each, other programs 5.
        let steady = |pid: i32| Entry {
            pid,
            started_at: Some(0.0),
            steady_since: Some(300.0),
            long_lived: true,
            live_cpu: 0.3,
            live_mem_kb: 7 * GB,
            ..job(1, "dalp:stack", 0.3, 7)
        };
        let stacks = [steady(11), steady(12)];
        let m = machine(2.0, 19);
        // A 12 GB head would reach 31 GB, over the 27.2 GB limit, in RAM.
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

    /// A steady dev stack of `live_gb`, as the MacBook's at 10:09.
    fn steady_stack(pid: i32, live_gb: u64) -> Entry {
        Entry {
            pid,
            started_at: Some(0.0),
            steady_since: Some(120.0),
            long_lived: true,
            priority: 10,
            live_cpu: 0.1,
            live_mem_kb: live_gb * GB,
            ..job(pid as u64, "dev-env:stack", 0.1, live_gb + live_gb / 4)
        }
    }

    #[test]
    fn the_progress_rule_never_starts_a_stack_that_does_not_fit() {
        // The MacBook at 10:09:56: one steady stack, 24 GB held of 32, mem_max
        // 95. A second stack of 9.4 GB queued at priority 10, ahead of the run
        // of the first one (4.3 GB).
        let lim = Limits { mem_max_pct: 95.0, ..LIM };
        let a = steady_stack(11, 7);
        let stack_b = Entry { long_lived: true, priority: 10, need_mem_kb: 94 * GB / 10, ..job(2, "dev-env:stack", 3.0, 0) };
        let run_a = Entry { need_mem_kb: 43 * GB / 10, ..job(3, "pr.ts --pr 17398", 3.0, 0) };
        let waiting = [stack_b.clone(), run_a.clone()];
        let m = machine(1.0, 24);
        let r = std::slice::from_ref(&a);
        // 24 + 1.75 still reserved by the stack + 9.4 = 35 GB, over 30.4: the
        // stack waits, and holds nobody back.
        assert_eq!(blockers(decide(&m, &lim, r, &waiting, &stack_b, 1000.0, &[])), vec!["memory"]);
        // The run fits under all of RAM (30 GB of 32), and starts.
        let d = decide(&m, &lim, r, &waiting, &run_a, 1000.0, &[]);
        assert!(matches!(&d, Decision::Admit { reason, .. } if reason.contains("long-lived")), "{d:?}");
        // With nothing running at all, a stack starts up to all of RAM: 22 +
        // 9.4 GB fits in 32, 23 + 9.4 does not.
        assert!(blockers(decide(&machine(1.0, 22), &lim, &[], &waiting, &stack_b, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&machine(1.0, 23), &lim, &[], &waiting, &stack_b, 1000.0, &[])), vec!["memory"]);
        // At macOS warning the same holds: the run starts next to its stack.
        let mut warn = m.clone();
        warn.mem_pressure = Some(50.0);
        let warned = Limits { warn_admit: true, ..lim };
        assert!(blockers(decide(&warn, &warned, r, &waiting, &run_a, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&warn, &warned, r, &waiting, &stack_b, 1000.0, &[])), vec!["memory", "pressure"]);
        // Critical still stops every new job.
        warn.mem_pressure = Some(100.0);
        assert_eq!(blockers(decide(&warn, &warned, r, &waiting, &run_a, 1000.0, &[])), vec!["pressure"]);
    }

    #[test]
    fn a_job_that_only_fits_once_a_stack_stops_holds_nobody_back() {
        // Omarchy at 11:33: two stacks run, one past its start-up, and 18.9 GB
        // is held of 31.3. An oxlint of 5.5 GB has waited 55 minutes, past
        // max_backfill, and would reach 110%: it fits only once a stack is
        // stopped. A test of 189 MB queued behind it.
        let lim = Limits { mem_max_pct: 95.0, max_backfill: 1800.0, ..LIM };
        let m = MachineSample::fixed(5.7, 24, 189 * GB / 10, 313 * GB / 10);
        let steady = Entry { live_mem_kb: 82 * GB / 10, need_mem_kb: 102 * GB / 10, ..steady_stack(11, 8) };
        let starting = Entry {
            steady_since: None,
            started_at: Some(3000.0),
            live_mem_kb: 11 * GB / 10,
            need_mem_kb: 91 * GB / 10,
            ..steady_stack(12, 1)
        };
        let running = [steady.clone(), starting];
        let mut lint = job(1, "domains/token-features/app:oxlint", 3.6, 0);
        (lint.need_mem_kb, lint.queued_at, lint.bypassed_since, lint.est_dur_s) = (55 * GB / 10, 0.0, Some(60.0), Some(40.0));
        let test = Entry { need_mem_kb: 189 * 1024, est_dur_s: Some(30.0), ..job(2, "custody-ops/flows:bun_test", 1.3, 0) };
        let waiting = [lint.clone(), test.clone()];
        assert_eq!(blockers(decide(&m, &lim, &running, &waiting, &lint, 3300.0, &[])), vec!["memory"]);
        assert!(blockers(decide(&m, &lim, &running, &waiting, &test, 3300.0, &[])).is_empty(), "the test fits and starts");
        // So in the progress rule: with only the steady stack running, the
        // lint does not fit in RAM; the test behind it does.
        let m = MachineSample::fixed(1.0, 24, 28 * GB, 313 * GB / 10);
        let r = std::slice::from_ref(&steady);
        assert_eq!(blockers(decide(&m, &lim, r, &waiting, &lint, 3300.0, &[])), vec!["memory"]);
        assert!(blockers(decide(&m, &lim, r, &waiting, &test, 3300.0, &[])).is_empty());
        // A lint that fits once a job that ends by itself has ended still
        // holds back a job that would not end before then, as before.
        let compile = Entry { started_at: Some(3200.0), live_mem_kb: 6 * GB, est_dur_s: Some(600.0), ..job(3, "compile", 1.0, 6) };
        let m = MachineSample::fixed(5.0, 24, 24 * GB, 313 * GB / 10);
        let running = [steady, compile];
        let slow = Entry { est_dur_s: Some(1000.0), ..test };
        let waiting = [lint.clone(), slow.clone()];
        assert_eq!(blockers(decide(&m, &lim, &running, &waiting, &lint, 3300.0, &[])), vec!["memory"]);
        assert_eq!(blockers(decide(&m, &lim, &running, &waiting, &slow, 3300.0, &[])), vec!["reserved"]);
    }

    /// A stack that expects a browser run of 4.3 GB, in checkout /w/a.
    fn expecting(mut stack: Entry) -> Entry {
        stack.checkout = "/w/a".into();
        stack.expects = vec![Expect { work: "browser".into(), mem_kb: 43 * GB / 10, ..Default::default() }];
        stack
    }

    /// The browser run of checkout /w/a, linked to the stack of pid 11.
    fn run_for(item: Option<u32>) -> Entry {
        Entry {
            pool: Some("browser".into()),
            checkout: "/w/a".into(),
            need_mem_kb: 43 * GB / 10,
            priority: 11,
            work_for: Some(WorkFor { pid: 11, key: "dev-env:stack".into(), item }),
            ..job(3, "pr.ts --pr 17398", 3.0, 0)
        }
    }

    #[test]
    fn a_stack_starts_only_with_room_for_the_work_it_expects() {
        let lim = Limits { mem_max_pct: 95.0, ..LIM };
        // 18 GB held of 32 (limit 30.4): the stack alone (9.4 GB) fits, with
        // its 4.3 GB run it does not.
        let alone = Entry { long_lived: true, priority: 10, need_mem_kb: 94 * GB / 10, ..job(2, "dev-env:stack", 3.0, 0) };
        let stack = Entry { pid: 11, ..expecting(alone.clone()) };
        let m = machine(1.0, 18);
        let busy = [Entry { started_at: Some(990.0), ..job(5, "lint", 1.0, 1) }];
        assert!(blockers(decide(&m, &lim, &busy, std::slice::from_ref(&alone), &alone, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &lim, &busy, std::slice::from_ref(&stack), &stack, 1000.0, &[])), vec!["memory"]);
        assert_eq!(need_with_booking(&stack, &busy), 94 * GB / 10 + 43 * GB / 10);
        assert!(blockers(decide(&machine(1.0, 15), &lim, &busy, std::slice::from_ref(&stack), &stack, 1000.0, &[])).is_empty());
    }

    /// #27, with its numbers: 10 cores, 32 GB, mem_max 90 (28.8 GB). Programs
    /// outside taskguard held 17.4 GB; the stack needs 9.2 GB, its proof job
    /// 4.0 GB. The stack alone fit and started, and then held 7.3 GB idle for
    /// 39 minutes while its proof job waited, 1.8 GB short. Once the stack
    /// expects the proof job, declared or from its history, it does not start
    /// for a job that cannot; when it does start, its proof job does too.
    #[test]
    fn a_stack_is_not_started_for_a_proof_job_that_cannot_start() {
        let lim = Limits { mem_max_pct: 90.0, outside_mem_max_pct: crate::commands::outside_ceiling(90.0, 92.0), ..LIM };
        let alone = Entry { long_lived: true, priority: 10, need_mem_kb: 92 * GB / 10, ..job(2, "dev-env:stack", 3.0, 0) };
        let mut stack = Entry { pid: 11, ..expecting(alone.clone()) };
        stack.expects[0].mem_kb = 4 * GB;
        let m = MachineSample::fixed(1.0, 10, 174 * GB / 10, 32 * GB);
        // A small job of taskguard's runs, holding what it needs.
        let busy = [Entry { started_at: Some(990.0), live_mem_kb: GB / 2, ..job(5, "lint", 1.0, 0) }];
        // Before: the stack alone fits (26.6 of 28.8 GB) and starts.
        assert!(blockers(decide(&m, &lim, &busy, std::slice::from_ref(&alone), &alone, 1000.0, &[])).is_empty());
        // Expecting its proof job it waits: 30.6 GB, over 28.8 and over the
        // 29.4 GB that outside_admit allows when only other programs keep it out.
        assert_eq!(blockers(decide(&m, &lim, &busy, std::slice::from_ref(&stack), &stack, 1000.0, &[])), vec!["memory"]);
        // With 2 GB more free, the stack and its booked run fit together.
        let freer = MachineSample::fixed(1.0, 10, 154 * GB / 10, 32 * GB);
        assert!(blockers(decide(&freer, &lim, &busy, std::slice::from_ref(&stack), &stack, 1000.0, &[])).is_empty());
        // With nothing of taskguard's running, the pair may start up to all of
        // RAM; then, with the stack steady at 7.3 GB (24.7 GB held, as in the
        // issue), its proof job is the first in line and starts too.
        assert!(blockers(decide(&m, &lim, &[], std::slice::from_ref(&stack), &stack, 1000.0, &[])).is_empty());
        let up = Entry { need_mem_kb: 92 * GB / 10, ..expecting(steady_stack(11, 0)) };
        let up = Entry { live_mem_kb: 73 * GB / 10, expects: vec![Expect { mem_kb: 4 * GB, ..up.expects[0].clone() }], ..up };
        let run = Entry { need_mem_kb: 4 * GB, ..run_for(Some(0)) };
        let held = MachineSample::fixed(1.0, 10, 247 * GB / 10, 32 * GB);
        assert!(blockers(decide(&held, &lim, std::slice::from_ref(&up), std::slice::from_ref(&run), &run, 3400.0, &[])).is_empty());
    }

    #[test]
    fn a_running_stack_keeps_the_room_for_its_run_until_the_run_starts() {
        let lim = Limits { mem_max_pct: 95.0, ..LIM };
        let stack = expecting(steady_stack(11, 7));
        let r = std::slice::from_ref(&stack);
        // What the stack may still grow to (1 GB), and 4.3 GB booked for its run.
        let grows = stack.need_mem_kb - stack.live_mem_kb;
        assert_eq!(reserve(r).1, grows + 43 * GB / 10);
        let lint = Entry { need_mem_kb: 46 * GB / 10, ..job(4, "lint", 3.5, 0) };
        let run = run_for(Some(0));
        let waiting = [lint.clone(), run.clone()];
        let m = machine(1.0, 24);
        let other = [stack.clone(), Entry { started_at: Some(990.0), ..job(5, "build", 0.5, 0) }];
        // 24 GB held + 5.3 reserved: a lint that needs that room waits; the
        // run takes over the booking, and goes first.
        assert_eq!(blockers(decide(&m, &lim, &other, &waiting, &lint, 1000.0, &[])), vec!["priority", "memory"]);
        assert!(blockers(decide(&m, &lim, &other, &waiting, &run, 1000.0, &[])).is_empty());
        let alone = [lint.clone()];
        assert_eq!(blockers(decide(&m, &lim, &other, &alone, &lint, 1000.0, &[])), vec!["memory"]);
        // Once the run runs, the stack books nothing for it: the run's own
        // need counts instead.
        let running = [stack.clone(), Entry { started_at: Some(1000.0), ..run.clone() }];
        assert_eq!(reserve(&running).1, grows + 43 * GB / 10);
        assert_eq!(super::booked(&stack, &running), 0);
        // And once its owner has marked it started, it stays dropped.
        let mut done = stack.clone();
        done.expects[0].started = true;
        assert_eq!(reserve(std::slice::from_ref(&done)).1, grows);
    }

    /// The MacBook (32 GB, mem_max 95, warn_admit) with over-commit on at
    /// its defaults: 120% of RAM, after 120 s alone or 1800 s first in line.
    fn overcommit_limits() -> Limits {
        Limits {
            mem_max_pct: 95.0,
            warn_admit: true,
            noise_mem_pct: 2.0,
            outside_admit: true,
            outside_mem_max_pct: 97.5,
            overcommit_max_pct: 120.0,
            ..LIM
        }
    }

    /// `outside_gb` held by programs outside taskguard, at `pressure`, with
    /// 16 GB of swap free.
    fn macbook(outside_gb: f64, pressure: f64) -> MachineSample {
        MachineSample {
            mem_pressure: Some(pressure),
            swap_free_kb: Some(16 * GB),
            ..MachineSample::fixed(1.0, 10, (outside_gb * GB as f64) as u64, 32 * GB)
        }
    }

    /// A lone dev stack of 9.5 GB, queued at 1000, that declared its 4.3 GB
    /// browser run: it books 13.8 GB.
    fn lone_stack() -> Entry {
        let stack = Entry { pid: 11, long_lived: true, priority: 10, need_mem_kb: 95 * GB / 10, ..job(1, "dev-env:stack", 3.0, 0) };
        expecting(stack)
    }

    fn memory_of(d: &Decision) -> &Blocker {
        match d {
            Decision::Wait { blockers } => blockers.iter().find(|b| b.name() == "memory").expect("a memory blocker"),
            Decision::Admit { .. } => panic!("admitted: {d:?}"),
        }
    }

    #[test]
    fn a_lone_stack_that_only_programs_outside_keep_out_starts_over_ram_after_a_while() {
        let lim = overcommit_limits();
        let me = lone_stack();
        let w = std::slice::from_ref(&me);
        // 20 GB outside + 13.8 GB = 106% of RAM. For its first two minutes
        // it waits, and says how far it is from the real ceiling: all of
        // RAM, not mem_max.
        let d = decide(&macbook(20.0, 0.0), &lim, &[], w, &me, 1060.0, &[]);
        let b = memory_of(&d);
        assert!(
            matches!(b, Blocker::Memory { limit_pct, overcommit_in: Some(s), .. } if *limit_pct == 100.0 && (*s - 60.0).abs() < 1e-6),
            "{b:?}"
        );
        let text = crate::report::blocker_text(b);
        assert!(text.contains("(limit 100%), 1.8 GB short") && text.contains("over RAM in 1m00s"), "{text}");
        // After that it starts, at normal pressure and at macOS warning.
        for p in [0.0, 50.0] {
            let d = decide(&macbook(20.0, p), &lim, &[], w, &me, 1130.0, &[]);
            assert!(
                matches!(&d, Decision::Admit { over_ram: true, early: Some(why), .. } if why.contains("only job in the queue")),
                "{p}: {d:?}"
            );
        }
        // Up to the cap: 24 GB outside is 118%, 25 GB 121%.
        assert!(matches!(decide(&macbook(24.0, 50.0), &lim, &[], w, &me, 1130.0, &[]), Decision::Admit { .. }));
        let d = decide(&macbook(25.0, 0.0), &lim, &[], w, &me, 1130.0, &[]);
        assert!(matches!(memory_of(&d), Blocker::Memory { overcommit_in: None, .. }), "{d:?}");
        // Never at critical.
        assert_eq!(blockers(decide(&macbook(18.5, 100.0), &lim, &[], w, &me, 5000.0, &[])), vec!["pressure"]);
        // Its run, queued for it, does not count as another job in line.
        let run = Entry { queued_at: 1100.0, ..run_for(None) };
        let both = [me.clone(), run];
        assert!(matches!(decide(&macbook(20.0, 0.0), &lim, &[], &both, &me, 1130.0, &[]), Decision::Admit { over_ram: true, .. }));
    }

    #[test]
    fn over_ram_goes_only_into_half_the_free_swap() {
        let lim = overcommit_limits();
        let me = lone_stack();
        let w = std::slice::from_ref(&me);
        let with_swap = |gb: u64| MachineSample { swap_free_kb: Some(gb * GB), ..macbook(20.0, 0.0) };
        // 1.8 GB over RAM: 4 GB of swap holds it, 3 GB does not, none at all
        // keeps it out for good.
        assert!(matches!(decide(&with_swap(4), &lim, &[], w, &me, 1130.0, &[]), Decision::Admit { .. }));
        assert_eq!(blockers(decide(&with_swap(3), &lim, &[], w, &me, 1130.0, &[])), vec!["memory"]);
        let none = MachineSample { swap_free_kb: None, ..macbook(20.0, 0.0) };
        assert_eq!(blockers(decide(&none, &lim, &[], w, &me, 5000.0, &[])), vec!["memory"]);
        // Linux: no warning level, so any pressure at pressure_max (PSI)
        // keeps it out, whatever the swap.
        let linux = Limits { warn_admit: false, ..lim };
        assert!(matches!(decide(&with_swap(4), &linux, &[], w, &me, 1130.0, &[]), Decision::Admit { .. }));
        let psi = MachineSample { mem_pressure: Some(25.0), ..with_swap(16) };
        assert_eq!(blockers(decide(&psi, &linux, &[], w, &me, 5000.0, &[])), vec!["pressure"]);
    }

    #[test]
    fn with_other_jobs_queued_the_first_in_line_starts_over_ram_only_once_stuck() {
        let lim = overcommit_limits();
        let me = lone_stack();
        // A 20 GB compile queued behind it: it cannot start either.
        let compile = Entry { queued_at: 1010.0, ..job(2, "compile", 2.0, 20) };
        let waiting = [me.clone(), compile.clone()];
        let m = macbook(20.0, 0.0);
        let d = decide(&m, &lim, &[], &waiting, &me, 1130.0, &[]);
        assert!(matches!(memory_of(&d), Blocker::Memory { overcommit_in: Some(s), .. } if (*s - 1670.0).abs() < 1e-6), "{d:?}");
        let d = decide(&m, &lim, &[], &waiting, &me, 2800.0, &[]);
        assert!(matches!(&d, Decision::Admit { early: Some(why), .. } if why.contains("first in line")), "{d:?}");
        // The job behind it never starts over RAM by the first-in-line rule.
        assert_eq!(blockers(decide(&m, &lim, &[], &waiting, &compile, 5000.0, &[])), vec!["memory"]);
        // A stalled job behind it does not count.
        let stalled = [me.clone(), Entry { stalled: true, ..compile }];
        assert!(matches!(decide(&m, &lim, &[], &stalled, &me, 1130.0, &[]), Decision::Admit { .. }));
    }

    #[test]
    fn the_run_of_a_stack_started_over_ram_inherits_the_over_commit_at_once() {
        let lim = overcommit_limits();
        // The stack started by over-commit and runs, steady at 7 GB (8 GB
        // need), with 4.3 GB declared for its run; 20 GB outside:
        // 27 + 1 + 4.3 GB = 101% of RAM.
        let stack = Entry { overcommitted: true, ..expecting(steady_stack(11, 7)) };
        let r = std::slice::from_ref(&stack);
        let run = Entry { queued_at: 1000.0, ..run_for(Some(0)) };
        let w = std::slice::from_ref(&run);
        let off = Limits { overcommit_max_pct: 0.0, ..lim };
        assert_eq!(blockers(decide(&macbook(27.0, 0.0), &off, r, w, &run, 1000.0, &[])), vec!["memory"], "0.11.0-rc.2 waits");
        let d = decide(&macbook(27.0, 0.0), &lim, r, w, &run, 1000.0, &[]);
        let inherits = |why: &str| why.contains("takes over the 4.3 GB");
        assert!(matches!(&d, Decision::Admit { over_ram: true, overcommitted: true, early: Some(why), .. } if inherits(why)), "{d:?}");
        assert!(matches!(decide(&macbook(27.0, 50.0), &lim, r, w, &run, 1000.0, &[]), Decision::Admit { .. }));
        // Never at critical; never above the cap.
        assert_eq!(blockers(decide(&macbook(27.0, 100.0), &lim, r, w, &run, 1000.0, &[])), vec!["pressure"]);
        assert_eq!(blockers(decide(&macbook(34.0, 0.0), &lim, r, w, &run, 1000.0, &[])), vec!["memory"]);
        // A run that needs more than was booked for it does not inherit.
        let big = Entry { need_mem_kb: 8 * GB, ..run.clone() };
        assert_eq!(blockers(decide(&macbook(27.0, 0.0), &lim, r, std::slice::from_ref(&big), &big, 1000.0, &[])), vec!["memory"]);
        // Also while the stack is still in its start-up.
        let starting = Entry { steady_since: None, started_at: Some(990.0), ..stack.clone() };
        let d = decide(&macbook(27.0, 0.0), &lim, std::slice::from_ref(&starting), w, &run, 1000.0, &[]);
        assert!(matches!(d, Decision::Admit { .. }), "{d:?}");
    }

    #[test]
    fn the_run_of_a_stack_that_started_normally_over_commits_only_by_the_queue_rule() {
        let lim = overcommit_limits();
        let stack = expecting(steady_stack(11, 7));
        let r = std::slice::from_ref(&stack);
        let run = Entry { queued_at: 1000.0, ..run_for(Some(0)) };
        let w = std::slice::from_ref(&run);
        let d = decide(&macbook(27.0, 0.0), &lim, r, w, &run, 1000.0, &[]);
        assert!(matches!(memory_of(&d), Blocker::Memory { overcommit_in: Some(s), .. } if (*s - 120.0).abs() < 1e-6), "{d:?}");
        let d = decide(&macbook(27.0, 0.0), &lim, r, w, &run, 1130.0, &[]);
        assert!(matches!(&d, Decision::Admit { over_ram: true, early: Some(why), .. } if why.contains("only job in the queue")), "{d:?}");
        // With another job of taskguard's running, never.
        let other = Entry { pid: 21, started_at: Some(900.0), ..job(21, "lint", 0.5, 0) };
        let both = [stack.clone(), other];
        assert_eq!(blockers(decide(&macbook(27.0, 0.0), &lim, &both, w, &run, 9000.0, &[])), vec!["memory"]);
        // With another job queued, only once stuck.
        let compile = Entry { pid: 2, queued_at: 1010.0, ..job(2, "compile", 2.0, 20) };
        let queued = [run.clone(), compile];
        assert_eq!(blockers(decide(&macbook(27.0, 0.0), &lim, r, &queued, &run, 1130.0, &[])), vec!["memory"]);
        assert!(matches!(decide(&macbook(27.0, 0.0), &lim, r, &queued, &run, 2810.0, &[]), Decision::Admit { .. }));
    }

    #[test]
    fn the_run_of_a_guessed_booking_does_not_inherit_the_over_commit() {
        let lim = overcommit_limits();
        let mut stack = Entry { overcommitted: true, ..expecting(steady_stack(11, 7)) };
        stack.expects[0].implied = true;
        let r = std::slice::from_ref(&stack);
        let run = Entry { queued_at: 1000.0, ..run_for(Some(0)) };
        let w = std::slice::from_ref(&run);
        let d = decide(&macbook(27.0, 0.0), &lim, r, w, &run, 1000.0, &[]);
        assert!(matches!(memory_of(&d), Blocker::Memory { overcommit_in: Some(_), .. }), "{d:?}");
    }

    #[test]
    fn at_macos_warning_the_takeover_still_waits_for_its_slot_and_a_paused_job() {
        let lim = overcommit_limits();
        let stack = Entry { overcommitted: true, ..expecting(steady_stack(11, 7)) };
        let compile = Entry { pid: 21, started_at: Some(900.0), ..job(21, "compile", 0.5, 0) };
        let run = Entry { queued_at: 1000.0, pool_key: Some("browser".into()), pool_slots: Some(1), ..run_for(Some(0)) };
        let w = std::slice::from_ref(&run);
        let m = macbook(27.0, 50.0);
        assert!(matches!(decide(&m, &lim, &[stack.clone(), compile.clone()], w, &run, 1000.0, &[]), Decision::Admit { .. }));
        let other_run = Entry { pid: 22, started_at: Some(900.0), pool_key: Some("browser".into()), ..job(22, "other run", 0.5, 0) };
        let d = decide(&m, &lim, &[stack.clone(), compile.clone(), other_run], w, &run, 1000.0, &[]);
        assert!(!matches!(d, Decision::Admit { .. }), "{d:?}");
        let paused = Entry { paused_since: Some(950.0), ..compile };
        assert_eq!(blockers(decide(&m, &lim, &[stack, paused], w, &run, 1000.0, &[])), vec!["paused"]);
    }

    #[test]
    fn a_second_stack_next_to_a_running_one_still_waits() {
        let lim = overcommit_limits();
        let a = expecting(steady_stack(11, 7));
        let b = Entry { pid: 12, ..lone_stack() };
        let w = std::slice::from_ref(&b);
        for now in [1130.0, 2900.0, 9000.0] {
            // 9 GB outside + the first stack's 7 GB, 5.3 GB it reserves, and 13.8 GB.
            assert_eq!(blockers(decide(&macbook(16.0, 0.0), &lim, std::slice::from_ref(&a), w, &b, now, &[])), vec!["memory"]);
        }
    }

    #[test]
    fn with_over_commit_off_a_lone_job_waits_as_in_0_11_0_rc_2() {
        let lim = Limits { overcommit_max_pct: 0.0, ..overcommit_limits() };
        let me = lone_stack();
        let w = std::slice::from_ref(&me);
        for now in [1130.0, 2900.0, 90000.0] {
            let d = decide(&macbook(20.0, 0.0), &lim, &[], w, &me, now, &[]);
            assert!(matches!(memory_of(&d), Blocker::Memory { overcommit_in: None, .. }), "{d:?}");
        }
        // Under the limit it still starts at once, as before.
        assert!(matches!(decide(&macbook(18.0, 0.0), &lim, &[], w, &me, 1000.0, &[]), Decision::Admit { over_ram: false, .. }));
    }

    #[test]
    fn a_forced_start_over_ram_is_seen_as_such() {
        let lim = overcommit_limits();
        let me = lone_stack();
        let w = std::slice::from_ref(&me);
        assert!(over_ram(&macbook(20.0, 0.0), &lim, &[], w, &me));
        assert!(!over_ram(&macbook(18.0, 0.0), &lim, &[], w, &me));
        // A history guess another stack books is not memory in use: a job
        // forced next to it is over RAM only by the progress measure.
        let mut guessing = Entry { pid: 12, ..expecting(steady_stack(12, 7)) };
        guessing.expects[0].implied = true;
        let lint = job(3, "lint", 0.5, 18);
        let r = std::slice::from_ref(&guessing);
        assert!(!over_ram(&macbook(10.0, 0.0), &lim, r, std::slice::from_ref(&lint), &lint));
    }

    #[test]
    fn work_links_to_the_stack_of_its_checkout_that_expects_it() {
        let stack = Entry { pid: 11, ..expecting(steady_stack(11, 7)) };
        let me = Entry { pool: Some("browser".into()), checkout: "/w/a".into(), ..job(3, "pr.ts", 3.0, 4) };
        let is = |w: &str| w == "browser";
        let (link, priority) = link_work(&me, std::slice::from_ref(&stack), &[], is).unwrap();
        assert_eq!((link.pid, link.item, priority), (11, Some(0), 10));
        // Not from another checkout, and not work it does not expect.
        let elsewhere = Entry { checkout: "/w/b".into(), ..me.clone() };
        assert!(link_work(&elsewhere, std::slice::from_ref(&stack), &[], is).is_none());
        assert!(link_work(&me, std::slice::from_ref(&stack), &[], |w| w == "lint").is_none());
        // A second run while the first one has the booking still goes first,
        // without a booking to take over.
        let first = Entry { pid: 30, started_at: Some(990.0), ..run_for(Some(0)) };
        let (link, _) = link_work(&me, &[stack.clone(), first], &[], is).unwrap();
        assert_eq!(link.item, None);
        // A stack that still waits takes only work that can take over its booking.
        let waiting = Entry { started_at: None, ..stack.clone() };
        let claimed = Entry { pid: 30, ..run_for(Some(0)) };
        assert!(link_work(&me, &[], &[waiting.clone(), claimed], is).is_none());
        assert_eq!(link_work(&me, &[], std::slice::from_ref(&waiting), is).unwrap().0.item, Some(0));
        // A long-lived job is nobody's work.
        assert!(link_work(&Entry { long_lived: true, ..me }, std::slice::from_ref(&stack), &[], is).is_none());
    }

    #[test]
    fn work_waits_for_its_stack_and_holds_nobody_back_meanwhile() {
        let stack = Entry { pid: 11, long_lived: true, priority: 10, ..expecting(job(2, "dev-env:stack", 3.0, 9)) };
        let run = run_for(Some(0));
        let lint = job(4, "lint", 1.0, 1);
        let waiting = [stack.clone(), run.clone(), lint.clone()];
        let busy = [Entry { started_at: Some(990.0), ..job(5, "build", 0.5, 2) }];
        let m = machine(1.0, 4);
        assert_eq!(blockers(decide(&m, &LIM, &busy, &waiting, &run, 1000.0, &[])), vec!["stack"]);
        // Its priority is above the stack's, but it cannot start: the lint
        // that fits is held back only by the stack itself.
        assert_eq!(blockers(decide(&m, &LIM, &busy, &waiting, &lint, 1000.0, &[])), vec!["priority"]);
        let d = decide(&m, &LIM, &busy, &waiting, &lint, 1000.0, &[]);
        assert!(
            matches!(&d, Decision::Wait { blockers } if matches!(&blockers[0], Blocker::Priority { key, priority: 10 } if key == "dev-env:stack"))
        );
        // With nothing running, the progress rule skips it too: the stack
        // behind it in line starts.
        assert_eq!(blockers(decide(&m, &LIM, &[], &waiting, &run, 1000.0, &[])), vec!["stack"]);
        assert!(blockers(decide(&m, &LIM, &[], &waiting, &stack, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_stack_that_expects_work_waits_a_few_seconds_for_it_to_queue() {
        let stack = Entry {
            pid: 11,
            long_lived: true,
            priority: 10,
            queued_at: 1000.0,
            expect_until: Some(1005.0),
            ..expecting(job(2, "dev-env:stack", 3.0, 9))
        };
        let m = machine(1.0, 4);
        let alone = std::slice::from_ref(&stack);
        let d = decide(&m, &LIM, &[], alone, &stack, 1002.0, &[]);
        assert!(
            matches!(&d, Decision::Wait { blockers } if matches!(&blockers[0], Blocker::Expect { wait_s, .. } if *wait_s == 3.0)),
            "{d:?}"
        );
        // The run queued: the stack starts now, with room for both, and the
        // run waits for it.
        let run = run_for(Some(0));
        let both = [stack.clone(), run.clone()];
        assert!(blockers(decide(&m, &LIM, &[], &both, &stack, 1002.0, &[])).is_empty());
        assert_eq!(blockers(decide(&m, &LIM, &[], &both, &run, 1002.0, &[])), vec!["stack"]);
        // Nothing queued: after the wait it starts alone.
        assert!(blockers(decide(&m, &LIM, &[], alone, &stack, 1005.0, &[])).is_empty());
        // A stack that expects nothing never waits.
        let plain = Entry { expects: Vec::new(), expect_until: None, ..stack.clone() };
        assert!(blockers(decide(&m, &LIM, &[], std::slice::from_ref(&plain), &plain, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_stack_with_work_queued_for_it_holds_its_turn_also_before_it_fits() {
        // DALP's max_backfill: a job ahead holds its turn only while it could
        // start, for half an hour.
        let lim = Limits { max_backfill: 1800.0, ..LIM };
        let stack = Entry { pid: 11, long_lived: true, priority: 10, ..expecting(job(2, "dev-env:stack", 3.0, 9)) };
        let lint = job(4, "lint", 1.0, 1);
        let busy = [Entry { started_at: Some(990.0), est_dur_s: Some(20.0), ..job(5, "build", 0.5, 12) }];
        // 4 GB held + 12 reserved + 13.3 for the stack and its run: it waits.
        let m = machine(1.0, 4);
        // Alone, it lets the lint that fits start meanwhile, as before.
        let waiting = [stack.clone(), lint.clone()];
        assert!(blockers(decide(&m, &lim, &busy, &waiting, &lint, 1000.0, &[])).is_empty());
        // With its run queued for it, the lint waits: the room is theirs.
        let waiting = [stack.clone(), run_for(Some(0)), lint.clone()];
        assert_eq!(blockers(decide(&m, &lim, &busy, &waiting, &lint, 1000.0, &[])), vec!["priority"]);
        // Still when 14 GB is held by other programs: once the build has
        // ended nothing of taskguard's runs, and a lone job may then take
        // the machine up to all of RAM (14 + 13.3 of 32 GB), so the stack
        // and its run will start, and the room is theirs.
        let m = machine(1.0, 14);
        assert_eq!(blockers(decide(&m, &lim, &busy, &waiting, &lint, 1000.0, &[])), vec!["priority"]);
        // Not when it could not start even then: 14 GB held and 22.3 GB more
        // for a bigger stack and its run is over all of RAM.
        let big = Entry { pid: 11, long_lived: true, priority: 10, ..expecting(job(2, "dev-env:stack", 3.0, 18)) };
        let waiting = [big, run_for(Some(0)), lint.clone()];
        assert!(blockers(decide(&m, &lim, &busy, &waiting, &lint, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_guess_of_the_history_books_room_but_never_stops_the_queue() {
        let lim = Limits { mem_max_pct: 95.0, ..LIM };
        let mut stack = Entry { pid: 11, ..expecting(steady_stack(11, 7)) };
        stack.expects[0].implied = true;
        stack.expects[0].expire_after_s = Some(600.0);
        let grows = stack.need_mem_kb - stack.live_mem_kb;
        let r = std::slice::from_ref(&stack);
        // It books the run's usual 4.3 GB, and the run takes that over.
        assert_eq!(reserve(r).1, grows + 43 * GB / 10);
        let me = Entry { pool: Some("browser".into()), checkout: "/w/a".into(), ..job(3, "pr.ts", 3.0, 4) };
        assert_eq!(link_work(&me, r, &[], |w| w == "browser").unwrap().0.item, Some(0));
        // 24 GB held of 32 and only the stack runs: a 5 GB lint does not fit
        // with the guess booked (35 GB), but the progress rule ignores a
        // guess, up to all of RAM (30 GB): it starts.
        let m = machine(1.0, 24);
        let lint = job(4, "lint", 3.5, 5);
        assert!(blockers(decide(&m, &lim, r, std::slice::from_ref(&lint), &lint, 1000.0, &[])).is_empty());
        // While a job that ends by itself runs, the guess holds like a booking.
        let busy = [stack.clone(), Entry { started_at: Some(990.0), ..job(5, "build", 0.5, 0) }];
        assert_eq!(blockers(decide(&m, &lim, &busy, std::slice::from_ref(&lint), &lint, 1000.0, &[])), vec!["memory"]);
        // A second stack respects the open guess, declared work or not (24 +
        // 5.3 + 4 GB is over 30.4); one with no declared work leaves only its
        // own guess out.
        let plain = Entry { long_lived: true, priority: 10, ..job(6, "dev-env:stack", 1.0, 4) };
        assert_eq!(blockers(decide(&m, &lim, r, std::slice::from_ref(&plain), &plain, 1000.0, &[])), vec!["memory"]);
        let mut guessing = Entry { checkout: "/w/b".into(), ..plain.clone() };
        guessing.expects = vec![Expect { work: "browser".into(), mem_kb: GB, implied: true, ..Default::default() }];
        assert_eq!(blockers(decide(&m, &lim, r, std::slice::from_ref(&guessing), &guessing, 1000.0, &[])), vec!["memory"]);
        let mut declared = guessing.clone();
        declared.expects[0].implied = false;
        assert_eq!(blockers(decide(&m, &lim, r, std::slice::from_ref(&declared), &declared, 1000.0, &[])), vec!["memory"]);
        // A run that linked to the open guess keeps its link, and so does one
        // that linked while another run held the booking.
        let linked = Entry { work_for: Some(WorkFor { pid: 11, key: "dev-env:stack".into(), item: Some(0) }), ..me.clone() };
        assert!(!link_lapsed(&linked, r, &[], |w| w == "browser"));
        let linked = Entry { work_for: Some(WorkFor { pid: 11, key: "dev-env:stack".into(), item: None }), ..me.clone() };
        assert!(!link_lapsed(&linked, r, &[], |w| w == "browser"));
        // Expired, it books nothing: the stack with no declared work starts
        // (24 + 1 + 4 GB), and no run links to the guess or keeps a link
        // that did not take its booking.
        stack.expects[0].expired = true;
        let r = std::slice::from_ref(&stack);
        assert_eq!(reserve(r).1, grows);
        assert!(blockers(decide(&m, &lim, r, std::slice::from_ref(&plain), &plain, 1000.0, &[])).is_empty());
        assert!(blockers(decide(&m, &lim, r, std::slice::from_ref(&guessing), &guessing, 1000.0, &[])).is_empty());
        assert!(link_work(&me, r, &[], |w| w == "browser").is_none());
        assert!(link_lapsed(&linked, r, &[], |w| w == "browser"));
        // Nor once it came true.
        stack.expects[0] = Expect { expired: false, started: true, ..stack.expects[0].clone() };
        let r = std::slice::from_ref(&stack);
        assert!(link_work(&me, r, &[], |w| w == "browser").is_none());
        assert!(link_lapsed(&linked, r, &[], |w| w == "browser"));
        // Declared work that has started still links: the stack exists for it.
        stack.expects[0].implied = false;
        let r = std::slice::from_ref(&stack);
        assert_eq!(link_work(&me, r, &[], |w| w == "browser").unwrap().0.item, None);
        assert!(!link_lapsed(&linked, r, &[], |w| w == "browser"));
    }

    #[test]
    fn a_run_takes_over_a_guessed_booking_once_by_the_progress_rule() {
        let lim = Limits { mem_max_pct: 95.0, ..LIM };
        let mut stack = Entry { pid: 11, ..expecting(steady_stack(11, 7)) };
        stack.expects[0].implied = true;
        let r = std::slice::from_ref(&stack);
        let run = run_for(Some(0));
        // 29 GB held, 1 still to grow and the 4.3 GB the run takes over:
        // 34.3 GB of 32. The guess it takes over counts once, not as nothing.
        assert_eq!(blockers(decide(&machine(1.0, 29), &lim, r, std::slice::from_ref(&run), &run, 1000.0, &[])), vec!["memory"]);
        // 26 GB held: 31.3 GB, under all of RAM, and it starts.
        assert!(blockers(decide(&machine(1.0, 26), &lim, r, std::slice::from_ref(&run), &run, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_lone_stack_is_never_kept_out_by_other_programs_alone() {
        // 32 GB, mem_max 85 (27.2 GB). Nothing of taskguard runs; other
        // programs hold 22 GB and a 9 GB stack would take the machine to 31
        // GB: only they keep it out, and nothing taskguard runs would free
        // that. It starts up to all of RAM, whatever outside_admit and its
        // ceiling say.
        let stack = Entry { long_lived: true, priority: 10, ..job(1, "dev-env:stack", 3.0, 9) };
        let me = std::slice::from_ref(&stack);
        for lim in [
            LIM,
            Limits { outside_admit: true, ..LIM },
            Limits { outside_admit: true, outside_mem_max_pct: 92.0, ..LIM },
            Limits { outside_admit: false, outside_mem_max_pct: 50.0, ..LIM },
        ] {
            assert!(blockers(decide(&machine(1.0, 22), &lim, &[], me, &stack, 1000.0, &[])).is_empty(), "{lim:?}");
            // Never past all of RAM: 24 + 9 GB is over 32.
            assert_eq!(blockers(decide(&machine(1.0, 24), &lim, &[], me, &stack, 1000.0, &[])), vec!["memory"], "{lim:?}");
        }
        // Next to a steady stack, only programs outside taskguard keep it out
        // (4 + 1 + 9 GB of taskguard's is under 27.2): it starts up to the
        // outside_admit ceiling (30.4 GB), and without that leeway waits.
        let small = steady_stack(12, 4);
        let r = std::slice::from_ref(&small);
        let lim = Limits { outside_admit: true, ..LIM };
        assert!(blockers(decide(&machine(1.0, 20), &lim, r, me, &stack, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&machine(1.0, 21), &lim, r, me, &stack, 1000.0, &[])), vec!["memory"]);
        assert_eq!(blockers(decide(&machine(1.0, 20), &LIM, r, me, &stack, 1000.0, &[])), vec!["memory"]);
        // When a steady stack of taskguard's fills the room, outside programs
        // are no excuse: 16 + 4 + 9 GB of its own is over 27.2.
        let a = steady_stack(12, 16);
        assert_eq!(blockers(decide(&machine(1.0, 17), &lim, std::slice::from_ref(&a), me, &stack, 1000.0, &[])), vec!["memory"]);
    }

    #[test]
    fn a_guess_whose_work_queued_stays_while_that_work_waits() {
        // The history says the run follows about 6 minutes after the stack
        // starts: the guess expires 10 minutes in. The run queues at minute 6
        // and links; memory pressure keeps it out past minute 10.
        let mut stack = Entry { pid: 11, ..expecting(steady_stack(11, 7)) };
        stack.expects[0].implied = true;
        stack.expects[0].expire_after_s = Some(600.0);
        let me = Entry { pool: Some("browser".into()), checkout: "/w/a".into(), queued_at: 360.0, ..job(3, "pr.ts", 3.0, 4) };
        let (link, priority) = link_work(&me, std::slice::from_ref(&stack), &[], |w| w == "browser").unwrap();
        assert_eq!((link.item, priority + 1), (Some(0), 11));
        let run = Entry { work_for: Some(link), priority: priority + 1, ..me };
        let waiting = std::slice::from_ref(&run);
        // Minute 11: the guess came true, so it does not expire.
        assert!(!expire_guesses(&mut stack, &[], waiting, 660.0));
        let r = std::slice::from_ref(&stack);
        assert!(!link_lapsed(&run, r, waiting, |w| w == "browser"));
        assert_eq!(reserve(r).1, GB + 43 * GB / 10, "it keeps the room booked for the run");
        let mut m = machine(1.0, 20);
        m.mem_pressure = Some(50.0);
        assert_eq!(blockers(decide(&m, &LIM, r, waiting, &run, 660.0, &[])), vec!["pressure"]);
        // Once the run has left the queue, the guess that is past its time ends.
        assert!(expire_guesses(&mut stack, &[], &[], 660.0));
        assert_eq!(reserve(std::slice::from_ref(&stack)).1, GB);
    }

    #[test]
    fn a_second_run_does_not_ride_on_a_guess_the_first_run_made_come_true() {
        let mut stack = Entry { pid: 11, ..expecting(steady_stack(11, 7)) };
        stack.expects[0].implied = true;
        stack.expects[0].expire_after_s = Some(600.0);
        let is = |w: &str| w == "browser";
        // The first run linked to the guess, took over its booking and runs.
        stack.expects[0].started = true;
        let first = Entry { pid: 30, started_at: Some(990.0), ..run_for(Some(0)) };
        let running = [stack.clone(), first.clone()];
        assert!(!link_lapsed(&first, &running, &[], is), "the first run keeps its link");
        // A second matching run in the checkout is not the work the history
        // foresaw: it does not link, so it gets no lift and does not wait
        // for the stack.
        let second = Entry { pool: Some("browser".into()), checkout: "/w/a".into(), ..job(31, "pr.ts --pr 17399", 3.0, 4) };
        assert!(link_work(&second, &running, &[], is).is_none());
        // One that linked before the first run started lets go of its link.
        let linked = Entry { work_for: Some(WorkFor { pid: 11, key: "dev-env:stack".into(), item: None }), ..second.clone() };
        assert!(link_lapsed(&linked, &running, &[], is));
        // Declared work still links a second run: the caller said it comes.
        stack.expects[0].implied = false;
        let running = [stack, first];
        assert_eq!(link_work(&second, &running, &[], is).unwrap().0.item, None);
    }

    #[test]
    fn a_guess_whose_work_queued_is_declared_work_also_by_the_progress_rule() {
        // 32 GB, 28 GB held; only a steady stack runs, still to grow 1 GB,
        // and the history guessed a 4.3 GB run for it.
        let lim = Limits { mem_max_pct: 95.0, ..LIM };
        let mut stack = Entry { pid: 11, ..expecting(steady_stack(11, 7)) };
        stack.expects[0].implied = true;
        stack.expects[0].expire_after_s = Some(600.0);
        let r = std::slice::from_ref(&stack);
        let m = machine(1.0, 28);
        let lint = job(4, "lint", 0.5, 2);
        // While nothing queued for it, the guess is no reason to wait: the
        // lint starts (28 + 1 + 2 GB).
        assert!(blockers(decide(&m, &lim, r, std::slice::from_ref(&lint), &lint, 1000.0, &[])).is_empty());
        // The run queued and linked: the guess came true, and its room is
        // the run's. The lint does not start into it (28 + 1 + 4.3 + 2 GB).
        let run = run_for(Some(0));
        let waiting = [run.clone(), lint.clone()];
        assert_eq!(blockers(decide(&m, &lim, r, &waiting, &lint, 1000.0, &[])), vec!["memory"]);
        assert!(!expire_guesses(&mut stack, &[], &waiting, 2000.0));
        // A guess no job queued for still expires, and then books nothing.
        let waiting = std::slice::from_ref(&lint);
        assert!(expire_guesses(&mut stack, &[], waiting, 2000.0));
        assert!(blockers(decide(&m, &lim, std::slice::from_ref(&stack), waiting, &lint, 2000.0, &[])).is_empty());
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
    fn nested_calls_leave_notes_for_their_outer_job() {
        let tmp = tempfile::tempdir().unwrap();
        let q = Queue::open(tmp.path()).unwrap();
        q.note_nested(100, 201, "a:tsc");
        q.note_nested(100, 202, "b:tsc");
        q.note_nested(1000, 203, "c:lint");
        let mut seen: Vec<(i32, i32, String)> = q.nested().into_iter().map(|n| (n.outer, n.pid, n.key)).collect();
        seen.sort();
        assert_eq!(seen[0], (100, 201, "a:tsc".to_string()));
        assert_eq!(seen.len(), 3);
        assert_eq!(q.take_nested(100), 2, "only its own notes, not those of pid 1000");
        assert_eq!(q.take_nested(100), 0, "taken once");
        assert_eq!(q.take_nested(1000), 1);
    }

    #[test]
    fn at_warning_jobs_start_while_memory_fits_and_no_first_run_grows() {
        // A dev stack past its start-up holds macOS at warning with half of
        // memory free. The browser proof that needs it starts at once.
        let lim = Limits { warn_admit: true, ..LIM };
        let stack = Entry { started_at: Some(100.0), steady_since: Some(220.0), live_mem_kb: 8 * GB, ..job(1, "stack", 1.0, 8) };
        let proof = job(2, "proof", 2.2, 3);
        let mut m = machine(2.0, 16);
        m.mem_pressure = Some(50.0);
        let d = decide(&m, &lim, std::slice::from_ref(&stack), std::slice::from_ref(&proof), &proof, 1000.0, &[]);
        assert!(blockers(d).is_empty(), "the proof starts next to its stack");

        // A job with a history has its whole need booked: the next starts at once.
        let started = Entry { started_at: Some(999.0), ..proof.clone() };
        let lint = job(3, "lint", 1.0, 1);
        let d = decide(&m, &lim, &[stack.clone(), started], std::slice::from_ref(&lint), &lint, 1000.0, &[]);
        assert!(blockers(d).is_empty(), "known jobs need no gap");

        // A first run books a guess: nothing new starts while it still grows.
        let first = Entry { known: false, started_at: Some(990.0), mem_grew_at: Some(995.0), ..job(4, "first", 1.0, 1) };
        let both = [stack.clone(), first];
        let d = decide(&m, &lim, &both, std::slice::from_ref(&lint), &lint, 1000.0, &[]);
        assert!(
            matches!(&d, Decision::Wait { blockers } if matches!(&blockers[0], Blocker::Pressure { after: Some(k), next_in: Some(t), .. } if k == "first" && *t == 15.0))
        );
        assert!(blockers(decide(&m, &lim, &both, std::slice::from_ref(&lint), &lint, 1015.0, &[])).is_empty(), "it has settled");

        // At warning, memory has no leeway while a job that ends by itself
        // runs.
        let big = job(5, "big", 1.0, 12);
        let busy = [stack.clone(), Entry { started_at: Some(999.0), ..lint.clone() }];
        let d = decide(&m, &lim, &busy, std::slice::from_ref(&big), &big, 1000.0, &[]);
        assert_eq!(blockers(d), vec!["memory", "pressure"]);
        // With only long-lived jobs running, the progress rule decides, up to
        // all of RAM: 16 + 12 GB is 88% of 32, over the 85% limit.
        let d = decide(&m, &lim, std::slice::from_ref(&stack), std::slice::from_ref(&big), &big, 1000.0, &[]);
        assert!(blockers(d).is_empty(), "the progress rule also works at warning");
        let bigger = job(5, "bigger", 1.0, 17);
        let d = decide(&m, &lim, std::slice::from_ref(&stack), std::slice::from_ref(&bigger), &bigger, 1000.0, &[]);
        assert_eq!(blockers(d), vec!["memory", "pressure"], "over all of RAM it waits");

        // Critical stops every new job, and warn_admit off keeps the old rule.
        m.mem_pressure = Some(100.0);
        assert_eq!(
            blockers(decide(&m, &lim, std::slice::from_ref(&stack), std::slice::from_ref(&proof), &proof, 1000.0, &[])),
            vec!["pressure"]
        );
        m.mem_pressure = Some(50.0);
        assert_eq!(
            blockers(decide(&m, &LIM, std::slice::from_ref(&stack), std::slice::from_ref(&proof), &proof, 1000.0, &[])),
            vec!["pressure"]
        );
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

    /// #4: an e2e run passed for long holds nobody back while its own pool's
    /// one slot is taken: it could not start in the room it holds.
    #[test]
    fn a_reservation_whose_pool_is_full_holds_nobody_back() {
        let pooled = |e: Entry| Entry { pool_key: Some("e2e@/w".into()), pool_slots: Some(1), ..e };
        let running = pooled(Entry { started_at: Some(500.0), ..job(1, "e2e:a", 1.0, 2) });
        let mut passed = pooled(job(2, "e2e:b", 1.0, 2));
        (passed.queued_at, passed.bypassed_since) = (600.0, Some(610.0));
        let lint = job(3, "lint", 1.0, 1);
        let waiting = [passed.clone(), lint.clone()];
        assert!(blockers(decide(&machine(1.0, 8), &LIM, std::slice::from_ref(&running), &waiting, &lint, 1000.0, &[])).is_empty());
        // Once its slot is free, it holds them back again.
        let other = job(4, "other", 1.0, 1);
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, &[other], &waiting, &lint, 1000.0, &[])), vec!["reserved"]);
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
