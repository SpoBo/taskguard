//! The one-shot subcommands: status, history, outliers, prune, doctor, import-history, pause, resume, start.

use crate::config::{self, Config};
use crate::dash;
use crate::db::{self, Db};
use crate::key;
use crate::machine;
use crate::matcher;
use crate::queue::{Limits, Queue};
use crate::report::{self, Snapshot, dur, gb};
use crate::sys;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn limits(cfg: &Config) -> Limits {
    Limits {
        cpu_max_pct: cfg.cpu_max,
        mem_max_pct: cfg.mem_max,
        learn_stagger: cfg.learn_stagger,
        max_bypass: cfg.max_bypass as f64,
        max_backfill: cfg.max_backfill as f64,
        cpu_min_duration: cfg.cpu_min_duration,
        pressure_max: cfg.pressure_max,
        noise_mem_pct: cfg.noise_mem,
        noise_cpu: cfg.noise_cpu,
        outside_admit: cfg.outside_admit,
        outside_mem_max_pct: cfg.pause_at,
        partial_fit: cfg.partial_fit.clamp(0.0, 1.0),
    }
}

/// Everything the status screen and the dashboard's Queue view show.
pub fn snapshot(dir: &Path, cfg: &Config, db: Option<&Db>) -> Result<Snapshot> {
    let mut s = queue_snapshot(dir, cfg, db)?;
    if let Some(d) = db {
        let since = db::now() - 24.0 * 3600.0;
        if let Ok(w) = dash::warnings(d, since, cfg.cpu_max, cfg.mem_max) {
            s.warnings = w.iter().map(|w| format!("{} - {}", w.title, w.lines.first().cloned().unwrap_or_default())).collect();
            s.advice = w.iter().filter(|w| w.kind == "repeated").flat_map(|w| w.lines.iter().skip(1).cloned()).collect();
        }
    }
    Ok(s)
}

/// `snapshot` without the warnings, which the dashboard reads on its own.
pub fn queue_snapshot(dir: &Path, cfg: &Config, db: Option<&Db>) -> Result<Snapshot> {
    let q = Queue::open(dir)?;
    let (m, running, waiting, unknown_starts) = {
        let _g = q.lock()?;
        for gone in q.reap() {
            if let Some(d) = db {
                let _ = d.abandon_run(gone);
            }
        }
        let running = q.running();
        let ours = running.iter().map(|e| e.live_mem_kb).sum();
        (machine::current(dir, 2.0, ours), running, q.waiting(), q.unknown_starts())
    };
    let mut s = Snapshot::build(m, limits(cfg), running, waiting, db::now(), &unknown_starts);
    if let Some(d) = db {
        s.top_other = d.latest_top_procs().unwrap_or_default();
    }
    Ok(s)
}

/// `taskguard pause|resume JOB`: JOB is a pid, a key, or a unique part of a key.
/// The job's own process does the work; this waits until it has.
pub fn pause(args: &[String], pause: bool) -> Result<i32> {
    let verb = if pause { "pause" } else { "resume" };
    let Some(target) = args.first() else { anyhow::bail!("usage: taskguard {verb} JOB (a pid, a key, or part of a key)") };
    let q = Queue::open(&config::state_dir())?;
    let running = {
        let _g = q.lock()?;
        q.reap();
        q.running()
    };
    let exact: Vec<&crate::queue::Entry> =
        running.iter().filter(|e| target.parse::<i32>().is_ok_and(|p| p == e.pid || p == e.child_pid) || &e.key == target).collect();
    let found = if exact.is_empty() { running.iter().filter(|e| e.key.contains(target.as_str())).collect() } else { exact };
    let e = match found.as_slice() {
        [e] => *e,
        [] => {
            let keys: Vec<String> = running.iter().map(|e| format!("{} (pid {})", e.key, e.pid)).collect();
            anyhow::bail!("no running job matches {target:?}; running: {}", if keys.is_empty() { "none".into() } else { keys.join(", ") })
        }
        many => anyhow::bail!(
            "{target:?} matches {} jobs: {}; give the pid",
            many.len(),
            many.iter().map(|e| format!("{} (pid {})", e.key, e.pid)).collect::<Vec<_>>().join(", ")
        ),
    };
    if !e.pausable {
        anyhow::bail!("{} runs an older taskguard (before 0.2.0); it cannot be paused from here", e.key);
    }
    match (pause, e.paused_since.is_some(), e.paused_by_hand) {
        (true, true, true) => {
            println!("{} is already paused", e.key);
            return Ok(0);
        }
        (false, false, _) => {
            println!("{} is not paused", e.key);
            return Ok(0);
        }
        _ => {}
    }
    q.nudge(e.pid, |n| n.pause = Some(pause))?;
    let t = std::time::Instant::now();
    while t.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let now = q.running().into_iter().find(|r| r.pid == e.pid);
        match now {
            None => {
                println!("{} ended", e.key);
                return Ok(0);
            }
            Some(r) if pause && r.paused_by_hand => {
                println!("paused {} (pid {}); it stays paused until: taskguard resume {}", r.key, r.pid, r.pid);
                return Ok(0);
            }
            Some(r) if !pause && r.paused_since.is_none() => {
                println!("resumed {} (pid {})", r.key, r.pid);
                return Ok(0);
            }
            _ => {}
        }
    }
    eprintln!("taskguard: {} did not {verb} within 10 s; is its process stuck?", e.key);
    Ok(1)
}

/// `taskguard start JOB`: start a waiting job now, whatever the limits say,
/// as `g` in the dashboard does. JOB is a pid, a ticket, a key, or a unique
/// part of a key. The job's own process starts itself; this waits until it
/// has.
pub fn start(args: &[String]) -> Result<i32> {
    let Some(target) = args.first() else { anyhow::bail!("usage: taskguard start JOB (a pid, a ticket, a key, or part of a key)") };
    let q = Queue::open(&config::state_dir())?;
    let waiting = {
        let _g = q.lock()?;
        q.reap();
        q.waiting()
    };
    let number = target.parse::<i64>().ok();
    // A pid comes first: tickets count up past the pids of a busy machine, and
    // the advice names the pid, so it must never start another job.
    let by_pid: Vec<&crate::queue::Entry> = waiting.iter().filter(|e| number == Some(e.pid as i64)).collect();
    let exact =
        if by_pid.is_empty() { waiting.iter().filter(|e| number == Some(e.ticket as i64) || &e.key == target).collect() } else { by_pid };
    let found = if exact.is_empty() { waiting.iter().filter(|e| e.key.contains(target.as_str())).collect() } else { exact };
    let e = match found.as_slice() {
        [e] => *e,
        [] => {
            let keys: Vec<String> = waiting.iter().map(|e| format!("{} (pid {})", e.key, e.pid)).collect();
            anyhow::bail!("no waiting job matches {target:?}; waiting: {}", if keys.is_empty() { "none".into() } else { keys.join(", ") })
        }
        many => anyhow::bail!(
            "{target:?} matches {} jobs: {}; give the pid",
            many.len(),
            many.iter().map(|e| format!("{} (pid {})", e.key, e.pid)).collect::<Vec<_>>().join(", ")
        ),
    };
    if e.version.is_none() {
        anyhow::bail!("{} runs an older taskguard (before 0.1.3); it cannot be started from here", e.key);
    }
    q.nudge(e.pid, |n| n.start = true)?;
    let t = std::time::Instant::now();
    while t.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !q.waiting().iter().any(|w| w.pid == e.pid) {
            println!("started {} (pid {}); it runs on the cores it gets, and no limit holds it back, memory included", e.key, e.pid);
            return Ok(0);
        }
    }
    eprintln!("taskguard: {} did not start within 10 s; is its process stuck?", e.key);
    Ok(1)
}

pub fn status(json: bool) -> Result<i32> {
    let dir = config::state_dir();
    let cwd = std::env::current_dir()?;
    let cfg = Config::load(&cwd, &key::checkout_root(&cwd))?;
    let db = Db::open_dir(&dir).ok();
    let s = snapshot(&dir, &cfg, db.as_ref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        print!("{}", report::render_status(&s));
    }
    Ok(0)
}

pub fn history() -> Result<i32> {
    let dir = config::state_dir();
    let db = Db::open_dir(&dir)?;
    let cfg = Config::load(Path::new("/"), Path::new("/")).unwrap_or_default();
    let rows = dash::history(&db, &cfg.learn())?;
    if rows.is_empty() {
        println!("no runs recorded yet");
        return Ok(0);
    }
    println!("{:<52} {:<14} {:>5} {:>7} {:>9} {:>8} {:>5}", "key", "namespace", "runs", "cores", "memory", "last", "exit");
    for r in rows {
        println!(
            "{:<52} {:<14} {:>5} {:>7} {:>9} {:>8} {:>5}",
            r.key,
            r.ns,
            r.runs,
            r.cpu.map(|c| format!("{c:.1}")).unwrap_or_else(|| "-".into()),
            r.mem_kb.map(gb).unwrap_or_else(|| "-".into()),
            r.last_dur_s.map(dur).unwrap_or_else(|| "-".into()),
            r.last_exit.map(|e| e.to_string()).unwrap_or_else(|| "-".into()),
        );
    }
    Ok(0)
}

/// The options of `outliers` and `prune`.
#[derive(Debug, Default, PartialEq)]
struct PruneArgs {
    pattern: Option<String>,
    older_than_s: Option<f64>,
    outliers: bool,
    undo: bool,
    apply: bool,
    ratio: Option<f64>,
    min_kb: Option<u64>,
}

fn parse_prune(args: &[String]) -> Result<PruneArgs> {
    let mut p = PruneArgs::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if a.starts_with("--") => (n, Some(v.to_string())),
            _ => (a, None),
        };
        let mut value = || -> Result<String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match name {
            "--older-than" => p.older_than_s = Some(parse_age(&value()?)?),
            "--ratio" => {
                let v = value()?;
                p.ratio = Some(v.parse().map_err(|_| anyhow::anyhow!("--ratio needs a number, such as 1.5"))?);
            }
            "--min" => p.min_kb = Some(config::parse_size_kb(&value()?)?),
            "--outliers" => p.outliers = true,
            "--undo" => p.undo = true,
            "--apply" => p.apply = true,
            _ if a.starts_with('-') => anyhow::bail!("unknown option {a}"),
            _ if p.pattern.is_none() => p.pattern = Some(a.to_string()),
            _ => anyhow::bail!("one key pattern only; quote it so the shell leaves the * alone"),
        }
        i += 1;
    }
    Ok(p)
}

/// "30d", "12h", "90m", "45s". A bare number is days.
fn parse_age(s: &str) -> Result<f64> {
    let t = s.trim();
    let (num, mult) = match t.chars().last() {
        Some('d') => (&t[..t.len() - 1], 86400.0),
        Some('h') => (&t[..t.len() - 1], 3600.0),
        Some('m') => (&t[..t.len() - 1], 60.0),
        Some('s') => (&t[..t.len() - 1], 1.0),
        _ => (t, 86400.0),
    };
    let n: f64 = num.parse().map_err(|_| anyhow::anyhow!("bad age {s:?}; use 30d, 12h, 90m or 45s"))?;
    Ok(n * mult)
}

/// Settings for this directory, with `--ratio` and `--min` on top.
fn learn_rules(p: &PruneArgs) -> Result<db::Learn> {
    let cwd = std::env::current_dir()?;
    let cfg = Config::load(&cwd, &key::checkout_root(&cwd))?;
    let mut rules = cfg.learn();
    if let Some(r) = p.ratio {
        rules.outlier_ratio = r;
    }
    if let Some(m) = p.min_kb {
        rules.outlier_min_kb = m;
    }
    Ok(rules)
}

/// The outliers of every key that matches `pattern`.
fn find_all_outliers(db: &Db, pattern: Option<&str>, rules: &db::Learn) -> Result<Vec<(String, db::Outliers)>> {
    let mut out = Vec::new();
    for k in db.keys(pattern)? {
        if let Some(o) = db.learned(&k, rules)?.outliers {
            out.push((k, o));
        }
    }
    Ok(out)
}

fn ago(t: f64) -> String {
    let s = (db::now() - t).max(0.0);
    if s >= 2.0 * 86400.0 { format!("{:.0}d ago", s / 86400.0) } else { format!("{} ago", dur(s)) }
}

/// `taskguard outliers [PATTERN] [--ratio R] [--min SIZE]`
pub fn outliers(args: &[String]) -> Result<i32> {
    let p = parse_prune(args)?;
    let rules = learn_rules(&p)?;
    let db = Db::open_dir(&config::state_dir())?;
    let found = find_all_outliers(&db, p.pattern.as_deref(), &rules)?;
    println!(
        "a peak counts as an outlier when it is more than {}x and {} above the next peak below it (outlier_ratio, outlier_min)",
        rules.outlier_ratio,
        gb(rules.outlier_min_kb)
    );
    if found.is_empty() {
        println!("no outliers in the last {} runs of any key", rules.keep);
        return Ok(0);
    }
    println!();
    println!("{:<52} {:>6} {:>9} {:>9} {:>9} {:>6} {:>9}  why", "key", "run", "ended", "peak", "usual", "x", "next run");
    for (k, o) in &found {
        for r in &o.runs {
            println!(
                "{:<52} {:>6} {:>9} {:>9} {:>9} {:>6.1} {:>9}  {}",
                k,
                r.run_id,
                ago(r.ended_at),
                gb(r.peak_kb),
                gb(o.normal_kb),
                r.peak_kb as f64 / o.normal_kb.max(1) as f64,
                gb(o.mem_kb),
                o.why(r)
            );
        }
    }
    println!();
    println!("drop them from the history: taskguard prune --outliers [PATTERN] [--ratio R] (add --apply to do it)");
    Ok(0)
}

/// `taskguard prune`: move runs out of the history, or back.
pub fn prune(args: &[String]) -> Result<i32> {
    let p = parse_prune(args)?;
    let db = Db::open_dir(&config::state_dir())?;
    let now = db::now();
    let (runs, verb) = if p.undo {
        (db.pruned(p.pattern.as_deref().unwrap_or("*"))?, "put back")
    } else if p.outliers {
        let rules = learn_rules(&p)?;
        let mut runs = Vec::new();
        for (k, o) in find_all_outliers(&db, p.pattern.as_deref(), &rules)? {
            for r in &o.runs {
                if p.older_than_s.is_some_and(|a| r.ended_at >= now - a) {
                    continue;
                }
                runs.push(db::PrunedRun {
                    id: r.run_id,
                    key: k.clone(),
                    ended_at: Some(r.ended_at),
                    peak_mem_kb: Some(r.peak_kb),
                    exit: r.exit,
                    reason: format!(
                        "outlier: {}, {:.1}x the usual {} (outlier_ratio {})",
                        gb(r.peak_kb),
                        r.peak_kb as f64 / o.normal_kb.max(1) as f64,
                        gb(o.normal_kb),
                        rules.outlier_ratio
                    ),
                });
            }
        }
        (runs, "prune")
    } else {
        let Some(pattern) = p.pattern.as_deref() else {
            anyhow::bail!(
                "name the keys to prune: taskguard prune 'packages/api:*' [--older-than 30d], or '*' for all of them; see taskguard history"
            );
        };
        let reason = match p.older_than_s {
            Some(a) => format!("pruned by hand: older than {}", dur(a)),
            None => "pruned by hand".into(),
        };
        let mut runs = db.runs_of(pattern, p.older_than_s.map(|a| now - a))?;
        for r in &mut runs {
            r.reason = reason.clone();
        }
        (runs, "prune")
    };
    if runs.is_empty() {
        println!("nothing to {verb}");
        return Ok(0);
    }
    println!("{:<52} {:>6} {:>9} {:>9} {:>5}  reason", "key", "run", "ended", "peak", "exit");
    for r in &runs {
        println!(
            "{:<52} {:>6} {:>9} {:>9} {:>5}  {}",
            r.key,
            r.id,
            r.ended_at.map(ago).unwrap_or_else(|| "-".into()),
            r.peak_mem_kb.map(gb).unwrap_or_else(|| "-".into()),
            r.exit.map(|e| e.to_string()).unwrap_or_else(|| "-".into()),
            r.reason
        );
    }
    let keys = runs.iter().map(|r| r.key.as_str()).collect::<std::collections::BTreeSet<_>>().len();
    if !p.apply {
        println!();
        println!("dry run: would {verb} {} run(s) of {keys} key(s). Run again with --apply to do it.", runs.len());
        if !p.undo {
            println!("pruned runs are kept aside; taskguard prune --undo [PATTERN] puts them back");
        }
        return Ok(0);
    }
    let n = if p.undo { db.restore(&runs)? } else { db.prune_runs(&runs)? };
    let done = if p.undo { "put back" } else { "pruned" };
    println!();
    println!("{done} {n} run(s) of {keys} key(s)");
    if !p.undo {
        println!("taskguard prune --undo [PATTERN] --apply puts them back");
    }
    Ok(0)
}

pub fn doctor(args: &[String]) -> Result<i32> {
    let dir = config::state_dir();
    let cwd = std::env::current_dir()?;
    let checkout = key::checkout_root(&cwd);
    let cfg = Config::load(&cwd, &checkout)?;

    if let Some(i) = args.iter().position(|a| a == "--explain") {
        let Some(cmdline) = args.get(i + 1) else {
            anyhow::bail!("--explain needs a command, in quotes");
        };
        return explain(cmdline, &cwd, &cfg, &dir);
    }

    println!("taskguard {}", env!("CARGO_PKG_VERSION"));
    println!("state dir:      {}", dir.display());
    println!(
        "user config:    {}{}",
        config::user_config_path().display(),
        if config::user_config_path().exists() { "" } else { " (not present; built-in defaults)" }
    );
    println!("layers here:    {}", cfg.layers.join("  <  "));
    for u in &cfg.unknown {
        println!("unknown here:   {u} (ignored: a typo, or a setting of a newer taskguard)");
    }
    println!("namespace here: {}", key::namespace(&cwd));
    println!();
    println!(
        "limits: cpu_max {:.0}%  mem_max {:.0}%  learn_stagger {}s  max_bypass {}s  max_backfill {}s  partial_fit {}  hints {}",
        cfg.cpu_max, cfg.mem_max, cfg.learn_stagger, cfg.max_bypass, cfg.max_backfill, cfg.partial_fit, cfg.hints
    );
    for (k, v) in &cfg.origin {
        if v != "built-in" {
            println!("  {k} set by {v}");
        }
    }
    println!();
    let m = machine::measure(None, 0);
    println!(
        "machine: {} cores, {:.1} busy; memory {} of {} held ({:.0}%){}",
        m.ncpu,
        m.cpu_busy,
        gb(m.mem_used_kb),
        gb(m.mem_total_kb),
        m.mem_pct(),
        m.mem_pressure.map(|p| format!(", pressure {p:.0}")).unwrap_or_default()
    );
    let me = sys::proc_sample(std::process::id() as i32);
    println!(
        "per-process readings: {}",
        match me {
            Some(s) => format!("ok (this process: {}, {:.3}s CPU)", gb(s.footprint_kb), s.cpu_ns as f64 / 1e9),
            None => "NOT AVAILABLE".into(),
        }
    );
    let rec_running = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("recorder.lock"))
        .map(|f| f.try_lock().is_err())
        .unwrap_or(false);
    println!("recorder: {}", if rec_running { "running" } else { "not running (it starts with the next job)" });
    println!();
    println!(
        "pools: {}",
        cfg.pools.iter().map(|p| format!("{} ({})", p.name, crate::config::slots_text(p.max_slots))).collect::<Vec<_>>().join(", ")
    );
    println!("passthrough patterns: {}", cfg.passthrough.len());
    println!("[[job]] rules: {}", cfg.jobs.len());

    let shims = tsc_queue_leftovers();
    if !shims.is_empty() {
        println!();
        println!("tsc-queue shims are still installed in {} place(s), for example:", shims.len());
        for s in shims.iter().take(5) {
            println!("  {}", s.display());
        }
        println!("remove them with the old tool before relying on taskguard:");
        println!("  tsc-queue unload && tsc-queue uninstall");
    }
    Ok(0)
}

fn explain(cmdline: &str, cwd: &Path, cfg: &Config, dir: &Path) -> Result<i32> {
    let argv = matcher::split_shell(cmdline);
    let key = key::run_key(cwd, &argv);
    let cls = cfg.classify(&argv, &key)?;
    println!("command:    {cmdline}");
    println!("effective:  {}", matcher::effective(&argv).join(" "));
    println!("key:        {key}");
    println!("namespace:  {}", key::namespace(cwd));
    println!("label:      {}", cls.label.as_deref().unwrap_or("-"));
    println!(
        "queued:     {}",
        if cls.passthrough {
            "no - passthrough, it runs straight through unmeasured"
        } else if cls.now {
            "no - now = true, it skips the queue but is measured"
        } else {
            "yes"
        }
    );
    match &cls.pool {
        Some((n, s, pc)) => println!(
            "pool:       {n} ({}{})",
            s.map(|s| format!("{s} slot(s)")).unwrap_or("no slot limit".into()),
            if *pc { ", one per checkout" } else { "" }
        ),
        None => println!("pool:       none"),
    }
    if let Some(s) = cfg.long_lived(&cls, cls.pool.as_ref().map(|p| p.0.as_str())) {
        println!("long-lived: yes - it holds its start-up peak for {}, then its needs follow what it uses", dur(s));
    }
    for w in &cls.why {
        println!("  because {w}");
    }
    if let Ok(db) = Db::open_dir(dir) {
        let l = db.learned(&key, &cfg.learn())?;
        if l.runs == 0 {
            println!("learned:    nothing yet; the first run just runs");
        } else {
            println!(
                "learned:    {} runs; {} cores, {} memory{}, usually {}",
                l.runs,
                l.cpu.map(|c| format!("{c:.1}")).unwrap_or("?".into()),
                l.mem_kb.map(gb).unwrap_or("?".into()),
                if l.mem_boosted { " (+25% after a memory-starved run)" } else { "" },
                l.dur_s.map(dur).unwrap_or("?".into())
            );
            if let Some(o) = &l.outliers {
                let top = o.runs.iter().map(|r| r.peak_kb).max().unwrap_or(0);
                println!(
                    "outliers:   {} run(s) up to {} where it usually takes {}; {} (taskguard outliers)",
                    o.runs.len(),
                    gb(top),
                    gb(o.normal_kb),
                    o.why(&o.runs[0])
                );
            }
            if l.steady_cpu.is_some() || l.steady_mem_kb.is_some() {
                println!(
                    "after start-up: {} cores, {} memory",
                    l.steady_cpu.map(|c| format!("{c:.1}")).unwrap_or("?".into()),
                    l.steady_mem_kb.map(gb).unwrap_or("?".into())
                );
            }
        }
    }
    Ok(0)
}

/// Compiler paths tsc-queue wrapped, in the checkouts its old config named.
fn tsc_queue_leftovers() -> Vec<PathBuf> {
    let conf = config::home().join(".config/tsc-queue.conf");
    let Ok(text) = std::fs::read_to_string(&conf) else { return Vec::new() };
    let var = |name: &str| -> Vec<PathBuf> {
        text.lines()
            .filter_map(|l| l.trim().strip_prefix(&format!("{name}=")))
            .flat_map(|v| {
                v.split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .replace("$HOME", &config::home().to_string_lossy())
                    .split_whitespace()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let mut checkouts = var("TSC_QUEUE_ROOTS");
    for s in var("TSC_QUEUE_SCAN") {
        if let Ok(rd) = std::fs::read_dir(&s) {
            checkouts.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    let mut out = Vec::new();
    for c in checkouts {
        {
            let rel = "node_modules/typescript/bin/tsc";
            let p = c.join(rel);
            if std::fs::read(&p).map(|b| String::from_utf8_lossy(&b[..b.len().min(200)]).contains("tsc-queue")).unwrap_or(false) {
                out.push(p);
            }
        }
        for store in [".pnpm", ".bun"] {
            if let Ok(rd) = std::fs::read_dir(c.join("node_modules").join(store)) {
                for e in rd.filter_map(|e| e.ok()) {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let cands: Vec<PathBuf> = if n.starts_with("typescript@") {
                        vec![e.path().join("node_modules/typescript/bin/tsc")]
                    } else if n.starts_with("@typescript+typescript-darwin") {
                        std::fs::read_dir(e.path().join("node_modules/@typescript"))
                            .map(|rd| rd.filter_map(|x| x.ok()).map(|x| x.path().join("lib/tsc")).collect())
                            .unwrap_or_default()
                    } else {
                        vec![]
                    };
                    for p in cands {
                        if std::fs::read(&p).map(|b| String::from_utf8_lossy(&b[..b.len().min(200)]).contains("tsc-queue")).unwrap_or(false)
                        {
                            out.push(p);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Loads tsc-queue's history. Its keys were the project path with `/` turned
/// into `_`, with no command, so they are stored under `tsc-queue:<key>` and
/// used for a tsc or tsgo job that has no history of its own yet.
pub fn import_history() -> Result<i32> {
    let src =
        std::env::var_os("TSC_QUEUE_DIR").map(PathBuf::from).unwrap_or_else(|| config::home().join(".cache/tsc-queue")).join("history.tsv");
    let text = std::fs::read_to_string(&src).map_err(|e| anyhow::anyhow!("reading {}: {e}", src.display()))?;
    let db = Db::open_dir(&config::state_dir())?;
    let done: i64 = db.conn.query_row("SELECT count(*) FROM runs WHERE imported = 1", [], |r| r.get(0))?;
    if done > 0 {
        println!("already imported ({done} runs); nothing to do");
        return Ok(0);
    }
    let tx = db.conn.unchecked_transaction()?;
    let mut n = 0;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 {
            continue;
        }
        let (Ok(ts), Ok(peak), Ok(secs), Ok(exit)) = (f[0].parse::<f64>(), f[2].parse::<i64>(), f[3].parse::<f64>(), f[4].parse::<i64>())
        else {
            continue;
        };
        if peak <= 0 {
            continue;
        }
        tx.execute(
            "INSERT INTO runs (ns, key, cmd, queued_at, started_at, ended_at, exit, peak_mem_kb, imported, label)
             VALUES ('imported', ?1, 'tsc', ?2, ?2, ?3, ?4, ?5, 1, 'typecheck')",
            rusqlite::params![format!("tsc-queue:{}", f[1]), ts - secs, ts, exit, peak],
        )?;
        n += 1;
    }
    tx.commit()?;
    println!("imported {n} runs from {}", src.display());
    Ok(0)
}

/// The tsc-queue key for a cwd: the project path with `/` and spaces as `_`.
pub fn legacy_key(cwd: &Path) -> String {
    key::project_path(cwd).replace(['/', ' '], "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn prune_options() {
        let p = parse_prune(&args("packages/api:* --older-than 30d --apply")).unwrap();
        assert_eq!(p.pattern.as_deref(), Some("packages/api:*"));
        assert_eq!(p.older_than_s, Some(30.0 * 86400.0));
        assert!(p.apply && !p.outliers && !p.undo);
        let p = parse_prune(&args("--outliers --ratio=1.5 --min 512M")).unwrap();
        assert_eq!((p.outliers, p.ratio, p.min_kb, p.pattern), (true, Some(1.5), Some(512 * 1024), None));
        assert_eq!(parse_prune(&args("--older-than 12h")).unwrap().older_than_s, Some(12.0 * 3600.0));
        assert_eq!(parse_prune(&args("--older-than 7")).unwrap().older_than_s, Some(7.0 * 86400.0), "a bare number is days");
        assert!(parse_prune(&args("a b")).is_err(), "an unquoted * the shell expanded");
        assert!(parse_prune(&args("--bogus")).is_err());
        assert!(parse_prune(&args("--ratio")).is_err());
    }
}
