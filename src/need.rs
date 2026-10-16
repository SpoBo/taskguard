//! How a job's history becomes the CPU and memory it reserves, step by step.
//!
//! The runner books what `derive` returns, and stores its steps with the run
//! (`runs.need_why`), so `taskguard dump` can show later why a run reserved
//! what it did. `dump` calls the same function for the next run, so the two
//! never tell different stories.

use crate::config::Config;
use crate::db::{Db, Learned};
use crate::report::gb;
use serde::Serialize;
use std::path::Path;

/// One step that set or changed the need. `cpu` and `mem_kb` are the need
/// after the step; a step that leaves one alone repeats it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    pub step: &'static str,
    pub cpu: f64,
    pub mem_kb: u64,
    pub why: String,
}

/// The need a run starts with, and how it was made.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Need {
    pub cpu: f64,
    pub mem_kb: u64,
    /// A minimum raised the need above what was learned.
    pub raised_by_min: bool,
    /// The history said more memory than `mem_max` allows; this much.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_capped_from: Option<u64>,
    /// The key whose history counted: the job's own, or tsc-queue's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_key: Option<String>,
    pub steps: Vec<Step>,
}

/// A first guess for a job with no history.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Guess {
    pub mem_kb: Option<u64>,
    pub cpu: Option<f64>,
    pub dur_s: Option<f64>,
    /// Where it comes from, in words.
    pub from: String,
}

/// Everything `derive` reads.
pub struct Inputs<'a> {
    pub learned: &'a Learned,
    pub history_key: Option<&'a str>,
    /// Set when the job has no runs of its own.
    pub guess: Option<&'a Guess>,
    pub min_cpu: Option<f64>,
    pub min_mem_kb: Option<u64>,
    pub ncpu: f64,
    pub mem_total_kb: u64,
    pub cpu_max: f64,
    pub mem_max: f64,
    pub new_job_mem_kb: u64,
    pub keep: usize,
    pub boost_runs: usize,
}

/// The need a run starts with, and each step that made it.
pub fn derive(i: &Inputs) -> Need {
    let l = i.learned;
    let mut steps = Vec::new();
    let (mut cpu, mut mem) = (0.0, 0u64);
    if l.runs > 0 {
        cpu = l.cpu.unwrap_or(0.0);
        mem = l.mem_kb.unwrap_or(0);
        steps.push(Step { step: "history", cpu, mem_kb: mem, why: history_why(l, i.keep, i.history_key) });
        if l.mem_boosted {
            mem = (mem as f64 * 1.25) as u64;
            steps.push(Step {
                step: "boost",
                cpu,
                mem_kb: mem,
                why: format!("+25%: one of the last {} runs was starved of memory and reached its need", i.boost_runs),
            });
        }
    } else if let Some(g) = i.guess {
        cpu = g.cpu.unwrap_or(1.0);
        mem = g.mem_kb.unwrap_or(i.new_job_mem_kb);
        steps.push(Step { step: "first_guess", cpu, mem_kb: mem, why: format!("no runs yet: {}", g.from) });
    }
    // What the history alone says: a first guess is no learned need.
    let learned_mem = if l.runs > 0 { mem } else { 0 };
    // A job that once wanted more cores than the machine has is capped at the
    // limit; otherwise it could only ever start on an empty machine.
    let cpu_cap = (i.ncpu * i.cpu_max / 100.0).min(i.ncpu);
    if cpu > cpu_cap {
        cpu = cpu_cap;
        steps.push(Step { step: "cpu_cap", cpu, mem_kb: mem, why: format!("capped at cpu_max {:.0}% of {} cores", i.cpu_max, i.ncpu) });
    }
    if let Some(m) = i.min_cpu.filter(|m| *m > cpu) {
        cpu = m;
        steps.push(Step { step: "min_cpu", cpu, mem_kb: mem, why: format!("raised to min_cpu {m:.1}") });
    }
    if let Some(m) = i.min_mem_kb.filter(|m| *m > mem) {
        mem = m;
        steps.push(Step { step: "min_mem", cpu, mem_kb: mem, why: format!("raised to min_mem {}", gb(m)) });
    }
    // A job that once took more memory than the limit allows is capped at the
    // limit, as CPU is: otherwise it could only ever start on an empty machine.
    let mem_cap = (i.mem_total_kb as f64 * i.mem_max / 100.0) as u64;
    let mut mem_capped_from = None;
    if mem_cap > 0 && mem > mem_cap && i.min_mem_kb.is_none_or(|m| m <= mem_cap) {
        mem_capped_from = Some(mem);
        mem = mem_cap;
        steps.push(Step {
            step: "mem_cap",
            cpu,
            mem_kb: mem,
            why: format!("capped at mem_max {:.0}% of {}", i.mem_max, gb(i.mem_total_kb)),
        });
    }
    let raised_by_min = i.min_cpu.is_some_and(|m| m > l.cpu.unwrap_or(0.0)) || i.min_mem_kb.is_some_and(|m| m > learned_mem);
    Need { cpu, mem_kb: mem, raised_by_min, mem_capped_from, history_key: i.history_key.map(str::to_string), steps }
}

/// Why the history says what it says about memory and CPU, in words.
fn history_why(l: &Learned, keep: usize, key: Option<&str>) -> String {
    let n = l.runs;
    let mut why = Vec::new();
    if let Some(k) = key.filter(|k| k.starts_with("tsc-queue:")) {
        why.push(format!("no runs of its own; the history of {k}"));
    }
    let mem = l.mem_kb.map(gb).unwrap_or("?".into());
    if let Some(o) = &l.outliers {
        let top = o.runs.iter().map(|r| r.peak_kb).max().unwrap_or(0);
        why.push(format!(
            "memory {mem}: {} outlier run(s) up to {} above the usual {}, {}",
            o.runs.len(),
            gb(top),
            gb(o.normal_kb),
            o.why(&o.runs[0])
        ));
    } else {
        why.push(format!("memory {mem}: the highest peak of the last {n} run(s) (keep {keep})"));
    }
    if !l.failed.is_empty() {
        let top = l.failed.iter().map(|r| r.peak_kb).max().unwrap_or(0);
        why.push(format!("{} failed run(s) up to {} ignored", l.failed.len(), gb(top)));
    }
    if let Some(c) = l.cpu {
        why.push(format!("cpu {c:.1}: the cores the recent runs used, newest counting most"));
    }
    why.join("; ")
}

/// The history that counts for a job: its own, or for a compile with none,
/// what tsc-queue learned. Returns it with the key it came from.
pub fn history(db: &Db, cfg: &Config, cwd: &Path, argv: &[String], key: &str) -> (Learned, String) {
    let learned = db.learned(key, &cfg.learn()).unwrap_or_default();
    let tool = tool(argv);
    if learned.runs == 0 && matches!(tool.as_str(), "tsc" | "tsgo") {
        let legacy = format!("tsc-queue:{}", crate::commands::legacy_key(cwd));
        if let Ok(l) = db.learned(&legacy, &cfg.learn())
            && l.runs > 0
        {
            return (l, legacy);
        }
    }
    (learned, key.to_string())
}

/// A first guess for a job with no history: what similar jobs needed, or
/// new_job_mem without any.
pub fn guess(db: Option<&Db>, cfg: &Config, package: Option<&str>, argv: &[String], label: Option<&str>, pool: Option<&str>) -> Guess {
    let cmdline = argv.join(" ");
    let same = package.map(|p| (p, cmdline.as_str()));
    let est = db.and_then(|d| d.estimate(same, label, &tool(argv), pool).ok().flatten());
    match est {
        Some(e) if e.mem_kb.is_some() => Guess { mem_kb: e.mem_kb, cpu: e.cpu, dur_s: e.dur_s, from: e.from },
        e => Guess {
            mem_kb: Some(cfg.new_job_mem_kb),
            cpu: e.as_ref().and_then(|e| e.cpu),
            dur_s: e.and_then(|e| e.dur_s),
            from: "the new_job_mem default".into(),
        },
    }
}

/// The program a command runs: `tsc` for `bunx tsc -p .`.
pub fn tool(argv: &[String]) -> String {
    crate::matcher::effective(argv).first().map(|c| c.rsplit('/').next().unwrap_or(c).to_string()).unwrap_or_default()
}

/// The `Inputs` of a job from the config and this machine.
pub fn inputs<'a>(
    cfg: &Config,
    learned: &'a Learned,
    history_key: Option<&'a str>,
    guess: Option<&'a Guess>,
    min_cpu: Option<f64>,
    min_mem_kb: Option<u64>,
) -> Inputs<'a> {
    Inputs {
        learned,
        history_key,
        guess: guess.filter(|_| learned.runs == 0),
        min_cpu,
        min_mem_kb,
        ncpu: crate::sys::ncpu() as f64,
        mem_total_kb: crate::sys::mem_total_kb(),
        cpu_max: cfg.cpu_max,
        mem_max: cfg.mem_max,
        new_job_mem_kb: cfg.new_job_mem_kb,
        keep: cfg.hist_keep,
        boost_runs: cfg.boost_runs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024;

    fn base(l: &Learned) -> Inputs<'_> {
        Inputs {
            learned: l,
            history_key: Some("k"),
            guess: None,
            min_cpu: None,
            min_mem_kb: None,
            ncpu: 10.0,
            mem_total_kb: 32 * GB,
            cpu_max: 100.0,
            mem_max: 85.0,
            new_job_mem_kb: 1536 * 1024,
            keep: 10,
            boost_runs: 5,
        }
    }

    fn names(n: &Need) -> Vec<&str> {
        n.steps.iter().map(|s| s.step).collect()
    }

    #[test]
    fn the_history_sets_the_need_and_each_step_says_why() {
        let l = Learned { runs: 4, mem_kb: Some(3 * GB), cpu: Some(2.0), ..Default::default() };
        let n = derive(&base(&l));
        assert_eq!((n.cpu, n.mem_kb, n.raised_by_min), (2.0, 3 * GB, false));
        assert_eq!(names(&n), ["history"]);
        assert!(n.steps[0].why.contains("highest peak of the last 4 run(s)"), "{}", n.steps[0].why);
    }

    #[test]
    fn the_boost_min_and_cap_each_leave_a_step() {
        let l = Learned { runs: 4, mem_kb: Some(24 * GB), cpu: Some(12.0), mem_boosted: true, ..Default::default() };
        let mut i = base(&l);
        i.min_cpu = Some(11.0);
        let n = derive(&i);
        // 24 GB * 1.25 = 30 GB, above mem_max 85% of 32 GB (27.2 GB).
        assert_eq!(names(&n), ["history", "boost", "cpu_cap", "min_cpu", "mem_cap"]);
        assert_eq!(n.mem_capped_from, Some(30 * GB));
        assert_eq!(n.mem_kb, ((32 * GB) as f64 * 85.0 / 100.0) as u64);
        assert_eq!(n.cpu, 11.0);
        // The minimum only lifts the capped need, not what was learned (12).
        assert!(!n.raised_by_min);
    }

    #[test]
    fn a_min_above_the_cap_is_kept() {
        let l = Learned::default();
        let mut i = base(&l);
        i.min_mem_kb = Some(30 * GB);
        let n = derive(&i);
        assert_eq!((n.mem_kb, n.mem_capped_from), (30 * GB, None));
        assert!(n.raised_by_min);
    }

    #[test]
    fn a_first_run_takes_the_guess() {
        let l = Learned::default();
        let g = Guess { mem_kb: Some(2 * GB), cpu: None, dur_s: None, from: "typical of 3 tsc jobs".into() };
        let mut i = base(&l);
        i.guess = Some(&g);
        let n = derive(&i);
        assert_eq!((n.cpu, n.mem_kb), (1.0, 2 * GB));
        assert_eq!(n.steps[0].why, "no runs yet: typical of 3 tsc jobs");
    }

    #[test]
    fn ignored_failed_runs_are_named() {
        let failed = crate::db::Outlier { run_id: 7, peak_kb: 14 * GB, ended_at: 0.0, exit: Some(1) };
        let l = Learned { runs: 3, mem_kb: Some(GB / 2), failed: vec![failed], ..Default::default() };
        let n = derive(&base(&l));
        assert!(n.steps[0].why.contains("1 failed run(s) up to 14.0 GB ignored"), "{}", n.steps[0].why);
    }
}
