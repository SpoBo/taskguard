//! The auto recorder: one background process per machine that samples the
//! machine every couple of seconds for the dashboard's history, keeps the
//! shared machine reading fresh for waiting jobs, and exits by itself once the
//! queue has been idle for a while and no dashboard is open.

use crate::config::{self, Config};
use crate::db::{self, Db};
use crate::groups;
use crate::machine;
use crate::queue::Queue;
use crate::sys;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::time::Duration;

pub const EVERY: f64 = 2.0;
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
    let mut known: HashMap<i32, Known> = HashMap::new();
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
        let with_top = now - last_top >= TOP_EVERY;
        if with_top {
            last_top = now;
        }
        let _ = others(&db, &q, &mut prev_cpu, &mut known, now, with_top);
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

/// One group's load, added up over its processes.
#[derive(Default)]
struct Tally {
    cores: f64,
    mem_kb: u64,
    procs: usize,
    /// Memory and process count per program name.
    names: HashMap<String, (u64, usize)>,
}

/// What the recorder remembers about a process: its short name and the
/// group its own name or path puts it in. Both stay the same for a pid.
struct Known {
    name: String,
    own: Option<&'static groups::Group>,
}

/// Load that taskguard did not start: per group of well-known programs
/// (agents, browsers, ...) on every call, and the five biggest processes,
/// by memory, when `with_top` is set. This is what the dashboard names when
/// the room went to something outside the queue.
fn others(
    db: &Db,
    q: &Queue,
    prev_cpu: &mut HashMap<i32, (u64, f64)>,
    known: &mut HashMap<i32, Known>,
    now: f64,
    with_top: bool,
) -> Result<()> {
    let procs = sys::list_procs();
    let children = sys::children_map(&procs);
    let mut ours: HashSet<i32> = HashSet::new();
    for e in q.running() {
        ours.extend(sys::descendants(e.pid, &children));
    }
    ours.insert(std::process::id() as i32);
    let alive: HashSet<i32> = procs.iter().map(|p| p.pid).collect();
    known.retain(|pid, _| alive.contains(pid));
    let parents: HashMap<i32, i32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut own: HashMap<i32, Option<&'static groups::Group>> = HashMap::new();
    for p in &procs {
        let k = known.entry(p.pid).or_insert_with(|| {
            let name = sys::proc_name(p.pid);
            let path = sys::proc_path(p.pid).unwrap_or_default();
            Known { own: groups::own_group(&name, &path), name: groups::display_name(&name, &path) }
        });
        own.insert(p.pid, k.own);
    }
    let group_of = groups::assign(&parents, &own);
    // scripts/demo.sh: made-up groups, so screenshots show no real programs.
    let demo = std::env::var_os("TASKGUARD_DEMO_GROUPS").is_some();

    let mut rows: Vec<(String, i32, u64, f64)> = Vec::new();
    let mut seen: HashMap<i32, (u64, f64)> = HashMap::new();
    let mut by_group: HashMap<&str, Tally> = HashMap::new();
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
        if let Some(g) = group_of.get(&p.pid) {
            let e = by_group.entry(g).or_default();
            e.cores += cores;
            e.mem_kb += s.footprint_kb;
            e.procs += 1;
            let name = known.get(&p.pid).map(|k| k.name.clone()).unwrap_or_default();
            let n = e.names.entry(name).or_default();
            n.0 += s.footprint_kb;
            n.1 += 1;
        }
        rows.push((String::new(), p.pid, s.footprint_kb, cores));
    }
    *prev_cpu = seen;
    let group_rows: Vec<db::GroupReading> = by_group
        .into_iter()
        .map(|(g, t)| {
            let mut names: Vec<(String, (u64, usize))> = t.names.into_iter().collect();
            names.sort_by_key(|(_, (m, _))| std::cmp::Reverse(*m));
            let top =
                names
                    .iter()
                    .take(3)
                    .map(|(name, (m, c))| {
                        if *c > 1 { format!("{name} ×{c} {}", crate::report::gb(*m)) } else { format!("{name} {}", crate::report::gb(*m)) }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
            db::GroupReading { group: g.to_string(), cores: t.cores, mem_kb: t.mem_kb, procs: t.procs, top }
        })
        .collect();
    db.group_samples(now, &if demo { demo_groups(now) } else { group_rows })?;
    if !with_top {
        return Ok(());
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    rows.truncate(5);
    for r in &mut rows {
        r.0 = known.get(&r.1).map(|k| k.name.clone()).unwrap_or_else(|| sys::proc_name(r.1));
    }
    db.top_procs(now, &rows)
}

/// Made-up groups for the demo, gently changing over time.
fn demo_groups(now: f64) -> Vec<db::GroupReading> {
    let wave = |period: f64, phase: f64| 1.0 + 0.15 * ((now / period + phase) * std::f64::consts::TAU).sin();
    let gb = |v: f64| (v * 1024.0 * 1024.0) as u64;
    let g = |group: &str, cores: f64, mem: f64, procs: usize, top: &str| db::GroupReading {
        group: group.into(),
        cores,
        mem_kb: gb(mem),
        procs,
        top: top.into(),
    };
    vec![
        g("agents", 1.2 * wave(40.0, 0.0), 6.4 * wave(90.0, 0.0), 38, "claude ×6 3.1 GB, node ×14 1.9 GB, codex ×2 1.2 GB"),
        g("browsers", 0.4 * wave(30.0, 0.3), 2.6, 21, "Google Chrome ×21 2.6 GB"),
        g("editors", 0.3, 1.8 * wave(120.0, 0.5), 9, "Visual Studio Code ×9 1.8 GB"),
        g("dev services", 0.1, 0.9, 12, "postgres ×9 0.7 GB, redis-server 0.2 GB"),
    ]
}
