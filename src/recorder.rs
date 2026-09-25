//! The auto recorder: one background process per machine that samples the
//! machine every couple of seconds for the dashboard's history, keeps the
//! shared machine reading fresh for waiting jobs, and exits by itself once the
//! queue has been idle for a while and no dashboard is open.

use crate::config::{self, Config};
use crate::db::{self, Db};
use crate::machine;
use crate::queue::Queue;
use crate::sys;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::time::Duration;

const EVERY: f64 = 2.0;
const TOP_EVERY: f64 = 10.0;
const ROLLUP_EVERY: f64 = 60.0;
const PRUNE_EVERY: f64 = 3600.0;

pub fn run() -> Result<()> {
    let dir = config::state_dir();
    let q = Queue::open(&dir)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("recorder.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(()); // another recorder won the race
    }
    let cfg = Config::load(std::path::Path::new("/"), std::path::Path::new("/")).unwrap_or_default();
    let idle_exit =
        std::env::var("TASKGUARD_RECORDER_IDLE_EXIT").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(cfg.recorder_idle_exit as f64);
    let db = Db::open_dir(&dir)?;
    let started = db::now();
    let (mut last_top, mut last_rollup, mut last_prune) = (0.0, db::now(), 0.0);
    let mut prev_cpu: HashMap<i32, (u64, f64)> = HashMap::new();
    let mut prev: Option<machine::MachineSample> = machine::read_cache(&dir);
    loop {
        let now = db::now();
        let (waiting, running) = {
            let _g = q.lock()?;
            for gone in q.reap() {
                let _ = db.abandon_run(gone);
            }
            let running = q.running();
            let s = machine::measure(prev.as_ref(), running.iter().map(|e| e.live_mem_kb).sum());
            machine::write_cache(&dir, &s);
            prev = Some(s);
            (q.waiting().len(), running.len())
        };
        if let Some(s) = &prev {
            let _ = db.machine_sample(s, waiting, running);
        }
        if waiting + running > 0 {
            q.touch_activity();
        }
        if now - last_top >= TOP_EVERY {
            last_top = now;
            let _ = top_procs(&db, &q, &mut prev_cpu, now);
        }
        if now - last_rollup >= ROLLUP_EVERY {
            last_rollup = now;
            let _ = db.rollup(now, cfg.sample_every);
        }
        if now - last_prune >= PRUNE_EVERY {
            last_prune = now;
            let _ = db.prune(cfg.retention_raw_hours, cfg.retention_rollup_days);
        }
        let idle_since = q.last_activity().max(started);
        if waiting + running == 0 && q.viewers() == 0 && now - idle_since >= idle_exit {
            let _ = db.rollup(now, cfg.sample_every);
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs_f64(EVERY));
    }
}

/// The five biggest processes that taskguard did not start, by memory, with
/// their CPU since the last look. This is what the dashboard names when the
/// room went to something outside the queue.
fn top_procs(db: &Db, q: &Queue, prev_cpu: &mut HashMap<i32, (u64, f64)>, now: f64) -> Result<()> {
    let procs = sys::list_procs();
    let children = sys::children_map(&procs);
    let mut ours: HashSet<i32> = HashSet::new();
    for e in q.running() {
        ours.extend(sys::descendants(e.pid, &children));
    }
    ours.insert(std::process::id() as i32);
    let mut rows: Vec<(String, i32, u64, f64)> = Vec::new();
    let mut seen: HashMap<i32, (u64, f64)> = HashMap::new();
    for p in &procs {
        if ours.contains(&p.pid) {
            continue;
        }
        let Some(s) = sys::proc_sample(p.pid) else { continue };
        let cores = match prev_cpu.get(&p.pid) {
            Some((c, t)) if now > *t => s.cpu_ns.saturating_sub(*c) as f64 / 1e9 / (now - t),
            _ => 0.0,
        };
        seen.insert(p.pid, (s.cpu_ns, now));
        rows.push((String::new(), p.pid, s.footprint_kb, cores));
    }
    *prev_cpu = seen;
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    rows.truncate(5);
    for r in &mut rows {
        r.0 = sys::proc_name(r.1);
    }
    db.top_procs(now, &rows)
}
