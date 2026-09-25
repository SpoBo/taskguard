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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    #[serde(default)]
    pub estimate_from: Option<String>,
}

pub struct Queue {
    pub dir: PathBuf,
}

pub struct Guard {
    _file: File,
}

impl Queue {
    pub fn open(dir: &Path) -> Result<Queue> {
        for d in ["wait", "run", "viewers"] {
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
        self.read_dir("wait").into_iter().map(|(_, e)| e).collect()
    }

    pub fn running(&self) -> Vec<Entry> {
        self.read_dir("run").into_iter().map(|(_, e)| e).collect()
    }

    /// Drop entries whose owner process is gone. Returns the run ids that
    /// were abandoned, so the caller can close them in the database.
    pub fn reap(&self) -> Vec<i64> {
        let mut gone = Vec::new();
        for sub in ["wait", "run"] {
            for (p, e) in self.read_dir(sub) {
                if !alive(e.pid) {
                    let _ = fs::remove_file(&p);
                    gone.push(e.run_id);
                }
            }
        }
        if let Ok(rd) = fs::read_dir(self.dir.join("viewers")) {
            for f in rd.filter_map(|e| e.ok()) {
                let pid: i32 = f.file_name().to_string_lossy().parse().unwrap_or(0);
                if !alive(pid) {
                    let _ = fs::remove_file(f.path());
                }
            }
        }
        gone
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
    /// Jobs that usually end sooner than this skip the CPU check.
    pub cpu_min_duration: f64,
    /// Memory pressure (0-100) at which nothing new starts.
    pub pressure_max: f64,
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
    Slots {
        pool: String,
        busy: u32,
        max: u32,
        holders: Vec<String>,
    },
    LearnStagger {
        wait_s: f64,
    },
    /// The kernel reports memory pressure: nothing new starts.
    Pressure {
        level: f64,
        limit: f64,
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
            Blocker::Slots { .. } => "slots",
            Blocker::LearnStagger { .. } => "learning",
            Blocker::Memory { .. } => "memory",
            Blocker::Pressure { .. } => "pressure",
            Blocker::Cpu { .. } => "cpu",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Admit { reason: String },
    Wait { blockers: Vec<Blocker> },
}

/// Memory that running jobs have not claimed yet: a job sitting at 2 GB whose
/// history says it peaks at 20 GB still has 18 GB to take, and that has to be
/// reserved now. Without this, a second job is admitted into space the first
/// one is about to eat. The same holds for CPU.
pub fn reserve(running: &[Entry]) -> (f64, u64) {
    running.iter().fold((0.0, 0), |(c, m), e| ((c + (e.need_cpu - e.live_cpu).max(0.0)), m + e.need_mem_kb.saturating_sub(e.live_mem_kb)))
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
    let older: Vec<&Entry> = waiting.iter().filter(|w| w.ticket < me.ticket).collect();

    // The kernel's own alarm comes first and holds back every job, even when
    // taskguard runs nothing: then the pressure comes from other programs, and
    // one more job only makes it worse. This cannot deadlock the queue: jobs
    // start again as soon as the pressure eases, and --now still skips it.
    if let Some(p) = m.mem_pressure.filter(|p| *p >= lim.pressure_max) {
        return Decision::Wait { blockers: vec![Blocker::Pressure { level: p, limit: lim.pressure_max }] };
    }

    // Always make progress: with nothing running, the oldest job starts,
    // whatever the readings say. This is what keeps the queue from deadlocking
    // on a machine that is busy with work outside taskguard.
    if running.is_empty() {
        return match older.first() {
            None => Decision::Admit { reason: "nothing else is running".into() },
            Some(o) => Decision::Wait { blockers: vec![Blocker::Older { key: o.key.clone() }] },
        };
    }

    let (res_cpu, res_mem) = reserve(running);
    let cpu_limit = m.ncpu as f64 * lim.cpu_max_pct / 100.0;
    let mem_limit = m.mem_total_kb as f64 * lim.mem_max_pct / 100.0;
    let mem_used = m.mem_for_admission(running.iter().map(|e| e.live_mem_kb).sum());

    let mut blockers = Vec::new();

    if let Some(r) = older.iter().find(|w| w.bypassed_since.is_some() && now - w.queued_at > lim.max_bypass) {
        blockers.push(Blocker::Reserved { key: r.key.clone(), waited_s: now - r.queued_at });
    }
    if let (Some(pk), Some(max)) = (&me.pool_key, me.pool_slots) {
        let holders: Vec<String> = running.iter().filter(|e| e.pool_key.as_ref() == Some(pk)).map(|e| e.key.clone()).collect();
        if holders.len() as u32 >= max {
            blockers.push(Blocker::Slots { pool: me.pool.clone().unwrap_or_default(), busy: holders.len() as u32, max, holders });
        }
    }
    // Jobs with no history start in small batches, and a batch only starts
    // once the jobs of the one before have run for `learn_stagger` seconds.
    // Their needs are estimates until then, and the readings need time to show
    // what the new jobs really take. A batch is as large as the free room
    // allows: one job per free core, and as many as fit in the free memory at
    // this job's estimated need.
    if !me.known && !me.raised_by_min {
        let recent: Vec<f64> = unknown_starts.iter().copied().filter(|t| now - t < lim.learn_stagger).collect();
        let free_cpu = cpu_limit - m.cpu_busy - res_cpu;
        let free_mem_kb = mem_limit - (mem_used + res_mem) as f64;
        let per_job_kb = me.need_mem_kb.max(512 * 1024) as f64;
        let batch = free_cpu.min(free_mem_kb / per_job_kb).floor().max(1.0) as usize;
        if !recent.is_empty() && recent.len() >= batch {
            let newest = recent.iter().copied().fold(f64::MIN, f64::max);
            blockers.push(Blocker::LearnStagger { wait_s: (lim.learn_stagger - (now - newest)).max(0.0) });
        }
    }
    let mem_would = (mem_used + res_mem + me.need_mem_kb) as f64;
    if mem_would > mem_limit {
        blockers.push(Blocker::Memory {
            would_pct: pct(mem_would, m.mem_total_kb as f64),
            limit_pct: lim.mem_max_pct,
            short_kb: (mem_would - mem_limit) as u64,
            used_kb: mem_used,
            reserve_kb: res_mem,
            need_kb: me.need_mem_kb,
            total_kb: m.mem_total_kb,
        });
    }
    // A job that usually ends within a few seconds is over before the CPU
    // reading could react to it, so holding it back only makes it late. Too
    // little CPU only slows a job down; memory stays a hard rule for all.
    let short = me.est_dur_s.is_some_and(|d| d < lim.cpu_min_duration);
    let cpu_would = m.cpu_busy + res_cpu + me.need_cpu;
    if !short && cpu_would > cpu_limit + 1e-9 {
        blockers.push(Blocker::Cpu {
            would: cpu_would,
            limit: cpu_limit,
            short: cpu_would - cpu_limit,
            busy: m.cpu_busy,
            reserve: res_cpu,
            need: me.need_cpu,
        });
    }
    if blockers.is_empty() {
        let cpu = if short {
            format!("CPU not checked for a job that usually takes {:.1}s", me.est_dur_s.unwrap_or(0.0))
        } else {
            format!("CPU {:.1}+{:.1}+{:.1} of {:.1} cores", m.cpu_busy, res_cpu, me.need_cpu, cpu_limit)
        };
        Decision::Admit { reason: format!("fits: {cpu}, memory {:.0}% of {:.0}%", pct(mem_would, m.mem_total_kb as f64), lim.mem_max_pct) }
    } else {
        Decision::Wait { blockers }
    }
}

fn pct(a: f64, b: f64) -> f64 {
    if b <= 0.0 { 0.0 } else { a * 100.0 / b }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024;
    const LIM: Limits =
        Limits { cpu_max_pct: 100.0, mem_max_pct: 85.0, learn_stagger: 2.0, max_bypass: 120.0, cpu_min_duration: 5.0, pressure_max: 20.0 };

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
}
